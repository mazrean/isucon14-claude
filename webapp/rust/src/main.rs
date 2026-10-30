// isuride (ISUCON14) の Rust 実装。webapp/go と同じ外部動作 (オンメモリ + write-behind WAL)。
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod engine;
mod handlers;
mod http;
mod payment;
mod persist;
mod stats;
mod store;
mod tls;
mod util;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::net::{TcpListener, UnixListener};
use tokio::signal::unix::{SignalKind, signal};

pub static SHUTDOWN: LazyLock<tokio::sync::watch::Sender<bool>> = LazyLock::new(|| tokio::sync::watch::channel(false).0);

fn env_or(key: &str, def: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| def.to_string())
}

fn setup_db() {
    let host = env_or("ISUCON_DB_HOST", "127.0.0.1");
    let port = env_or("ISUCON_DB_PORT", "3306");
    let Ok(port) = port.parse::<u16>() else {
        panic!("failed to convert DB port number from ISUCON_DB_PORT environment variable into int: {port}");
    };
    let user = env_or("ISUCON_DB_USER", "isucon");
    let password = env_or("ISUCON_DB_PASSWORD", "isucon");
    let dbname = env_or("ISUCON_DB_NAME", "isuride");
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some(host))
        .tcp_port(port)
        .user(Some(user))
        .pass(Some(password))
        .db_name(Some(dbname))
        .tcp_nodelay(true)
        .init(vec!["SET NAMES utf8mb4 COLLATE utf8mb4_general_ci"])
        .pool_opts(mysql::PoolOpts::default().with_constraints(mysql::PoolConstraints::new(1, 8).unwrap()));
    let mut i = 0;
    loop {
        match mysql::Pool::new(opts.clone()).and_then(|p| p.get_conn().map(|_| p)) {
            Ok(p) => {
                let _ = persist::DB.set(p);
                return;
            }
            Err(e) => {
                if i > 60 {
                    panic!("{e}");
                }
                log_error!("failed to connect db, retrying", "err=\"{}\"", e);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        i += 1;
    }
}

fn main() {
    if std::env::var("STATS").map(|v| !v.is_empty()).unwrap_or(false) {
        std::thread::Builder::new().name("stats".into()).spawn(stats::stats_loop).unwrap();
    }
    setup_db();
    // 前回の WAL を MySQL に反映してから読み込む
    if let Err(e) = persist::replay_wal() {
        log_error!("failed to replay wal", "err=\"{}\"", e);
    }
    if persist::wal_mode() {
        if let Err(e) = persist::open_wal() {
            panic!("{e}");
        }
    }
    {
        let mut st = store::STATE.lock();
        if let Err(e) = st.load_all() {
            panic!("{e}");
        }
    }
    LazyLock::force(&engine::NEARBY);
    std::thread::Builder::new().name("persist".into()).spawn(persist::persist_loop).unwrap();
    std::thread::Builder::new().name("matching".into()).spawn(engine::matching_loop).unwrap();

    let workers = util::env_int("RUST_WORKERS", std::thread::available_parallelism().map(|n| n.get() as i64).unwrap_or(2)).max(1) as usize;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(workers).enable_all().build().unwrap();
    rt.block_on(serve());
    // 稼働中に積まれた分を書き切ってから終了する
    persist::flush_persist();
    {
        let mut w = persist::FLUSH.lock();
        persist::wal_close(&mut w);
    }
    log_info!("bye");
    std::process::exit(0);
}

fn serve_conn<S>(graceful: &GracefulShutdown, io: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut b = http1::Builder::new();
    b.timer(TokioTimer::new()).keep_alive(true).header_read_timeout(None);
    let conn = b.serve_connection(TokioIo::new(io), service_fn(handlers::handle));
    let fut = graceful.watch(conn);
    tokio::spawn(async move {
        let _ = fut.await;
    });
}

async fn serve() {
    let tcp = match TcpListener::bind("[::]:8080").await {
        Ok(l) => l,
        Err(_) => TcpListener::bind("0.0.0.0:8080").await.expect("listen :8080"),
    };
    // 同一ホストの nginx からは unix domain socket で受ける (別ホストの nginx 用に TCP も残す)
    let uds = match std::env::var("ISUCON_SOCKET") {
        Ok(sock) if !sock.is_empty() => {
            let _ = std::fs::remove_file(&sock);
            let l = UnixListener::bind(&sock).expect("listen unix socket");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).expect("chmod socket");
            Some(l)
        }
        _ => None,
    };
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();
    let mut sigquit = signal(SignalKind::quit()).unwrap();
    let graceful = GracefulShutdown::new();
    // TLS_ADDR があれば TLS + h2 を直受けする (app ノードの nginx を省く)
    let tls = tls::bind().await;
    log_info!("Listening on :8080");
    loop {
        tokio::select! {
            r = tcp.accept() => {
                if let Ok((s, _)) = r {
                    let _ = s.set_nodelay(true);
                    serve_conn(&graceful, s);
                }
            }
            r = async { match &uds { Some(l) => l.accept().await, None => std::future::pending().await } } => {
                if let Ok((s, _)) = r {
                    serve_conn(&graceful, s);
                }
            }
            _ = async { match &tls { Some(t) => t.accept(&graceful).await, None => std::future::pending().await } } => {}
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = sigquit.recv() => break,
        }
    }
    log_info!("shutting down");
    SHUTDOWN.send_replace(true);
    // SSE のボディを終わらせる (graceful shutdown が通知ストリームを待ち続けないように)
    store::close_streams();
    drop(tcp);
    drop(uds);
    drop(tls);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(Duration::from_secs(10)) => {}
    }
}
