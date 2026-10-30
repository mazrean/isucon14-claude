// TLS (1.3) + HTTP/2 の直受け。nginx (sites-available/isuride.conf) の代わりに API と静的ファイルを返す。
// TLS_ADDR (例 ":443") を設定したときだけ有効。
use crate::http::{Body, Resp};
use crate::util::{http_date, query_unescape, wall_us};
use crate::{log_error, log_info};
use bytes::Bytes;
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, ETAG, EXPIRES, LAST_MODIFIED};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use rustc_hash::FxHashMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::UNIX_EPOCH;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;

#[derive(Clone, Copy, PartialEq)]
enum Loc {
    Root,
    Assets,
    Images,
}

struct File {
    body: Bytes,
    ctype: &'static str,
    etag: HeaderValue,
    last_modified: HeaderValue,
    /// gzip_static 用 (.gz があれば)
    gz: Option<Box<File>>,
}

struct Static {
    files: FxHashMap<String, File>,
}

fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, e)| e).unwrap_or("") {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "svg" | "svgz" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" => "text/plain",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn load_file(p: &Path, url: &str) -> Option<File> {
    let meta = std::fs::metadata(p).ok()?;
    if !meta.is_file() {
        return None;
    }
    let body = std::fs::read(p).ok()?;
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
    Some(File {
        ctype: mime(url),
        etag: HeaderValue::from_str(&format!("\"{:x}-{:x}\"", mtime, body.len())).unwrap(),
        last_modified: HeaderValue::from_str(&http_date(mtime)).unwrap(),
        body: Bytes::from(body),
        gz: None,
    })
}

fn walk(dir: &Path, prefix: &str, out: &mut FxHashMap<String, File>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        let url = format!("{prefix}/{name}");
        if p.is_dir() {
            walk(&p, &url, out);
        } else if let Some(mut f) = load_file(&p, &url) {
            let gzp = p.with_file_name(format!("{name}.gz"));
            if let Some(mut g) = load_file(&gzp, &url) {
                g.ctype = f.ctype;
                f.gz = Some(Box::new(g));
            }
            out.insert(url, f);
        }
    }
}

static STATIC: LazyLock<Static> = LazyLock::new(|| {
    let root = std::env::var("PUBLIC_DIR").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/home/isucon/webapp/public".into());
    let mut files = FxHashMap::default();
    walk(Path::new(&root), "", &mut files);
    log_info!("static files loaded", "root={} files={}", root, files.len());
    Static { files }
});

fn nginx_page(status: u16) -> Resp {
    let title = match status {
        403 => "403 Forbidden",
        404 => "404 Not Found",
        405 => "405 Not Allowed",
        _ => "500 Internal Server Error",
    };
    let body = format!("<html>\r\n<head><title>{title}</title></head>\r\n<body>\r\n<center><h1>{title}</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n");
    let mut r = Response::new(Body::Full(Some(Bytes::from(body))));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
    r
}

static CC_DAY: HeaderValue = HeaderValue::from_static("max-age=86400");
static CC_MAX: HeaderValue = HeaderValue::from_static("max-age=315360000");
static EXPIRES_MAX: HeaderValue = HeaderValue::from_static("Thu, 31 Dec 2037 23:55:55 GMT");

fn serve_static(req: &Request<Incoming>) -> Resp {
    let st = &*STATIC;
    let raw = req.uri().path();
    let path = query_unescape(raw, false).unwrap_or_else(|| raw.to_string());
    let loc = if path.starts_with("/assets/") {
        Loc::Assets
    } else if path.starts_with("/images/") {
        Loc::Images
    } else {
        Loc::Root
    };
    let file = match st.files.get(&path) {
        Some(f) => f,
        None if loc == Loc::Root => match st.files.get("/index.html") {
            Some(f) => f,
            None => return nginx_page(404),
        },
        None => return nginx_page(404),
    };
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return nginx_page(405);
    }
    let gzip_ok = loc != Loc::Root
        && req.headers().get_all(hyper::header::ACCEPT_ENCODING).iter().any(|v| v.to_str().map(|s| s.contains("gzip")).unwrap_or(false));
    let (f, gz) = match (&file.gz, gzip_ok) {
        (Some(g), true) => (&**g, true),
        _ => (file, false),
    };
    let not_modified = req.headers().get(hyper::header::IF_NONE_MATCH).is_some_and(|v| v == f.etag)
        || req.headers().get(hyper::header::IF_MODIFIED_SINCE).is_some_and(|v| v == f.last_modified);
    let mut r = if not_modified {
        let mut r = Response::new(Body::Full(None));
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        r
    } else {
        Response::new(Body::Full(Some(f.body.clone())))
    };
    let h = r.headers_mut();
    if !not_modified {
        h.insert(CONTENT_TYPE, HeaderValue::from_static(f.ctype));
    }
    h.insert(LAST_MODIFIED, f.last_modified.clone());
    h.insert(ETAG, f.etag.clone());
    if gz {
        h.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    match loc {
        Loc::Root => {
            h.insert(EXPIRES, HeaderValue::from_str(&http_date(wall_us() / 1_000_000 + 86400)).unwrap());
            h.insert(CACHE_CONTROL, CC_DAY.clone());
        }
        Loc::Assets | Loc::Images => {
            h.insert(EXPIRES, EXPIRES_MAX.clone());
            h.insert(CACHE_CONTROL, CC_MAX.clone());
            h.append(CACHE_CONTROL, HeaderValue::from_static(if loc == Loc::Assets { "public, immutable" } else { "public" }));
        }
    }
    h.insert("accept-ranges", HeaderValue::from_static("bytes"));
    r
}

async fn handle_tls(req: Request<Incoming>) -> Result<Resp, Infallible> {
    let path = req.uri().path();
    if path.starts_with("/api/") {
        if path.starts_with("/api/internal/") {
            // localhost からのみ (nginx の allow 127.0.0.1; deny all)
            return Ok(nginx_page(403));
        }
        return crate::handlers::handle(req).await;
    }
    Ok(serve_static(&req))
}

fn tls_config() -> Result<rustls::ServerConfig, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let cert_path = std::env::var("TLS_CERT").unwrap_or_else(|_| "/etc/nginx/tls/_.xiv.isucon.net.crt".into());
    let key_path = std::env::var("TLS_KEY").unwrap_or_else(|_| "/etc/nginx/tls/_.xiv.isucon.net.key".into());
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(&cert_path).map_err(|e| format!("{cert_path}: {e}"))?.collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    let key = PrivateKeyDer::from_pem_file(&key_path).map_err(|e| format!("{key_path}: {e}"))?;
    // 鍵交換は X25519 のみ (Go 1.26 のクライアントが提案する X25519MLKEM768 の大きな鍵共有を避ける。nginx と同じ)
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.kx_groups = vec![rustls::crypto::aws_lc_rs::kx_group::X25519];
    let provider = Arc::new(provider);
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    cfg.session_storage = rustls::server::ServerSessionMemoryCache::new(65536);
    cfg.ticketer = rustls::crypto::aws_lc_rs::Ticketer::new().map_err(|e| e.to_string())?;
    Ok(cfg)
}

pub struct TlsServer {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    builder: Arc<auto::Builder<TokioExecutor>>,
}

pub async fn bind() -> Option<TlsServer> {
    let addr = std::env::var("TLS_ADDR").ok().filter(|s| !s.is_empty())?;
    let addr = if addr.starts_with(':') { format!("[::]{addr}") } else { addr };
    let cfg = match tls_config() {
        Ok(c) => c,
        Err(e) => {
            log_error!("tls disabled", "err=\"{}\"", e);
            return None;
        }
    };
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            log_error!("tls listen failed", "addr={} err=\"{}\"", addr, e);
            return None;
        }
    };
    LazyLock::force(&STATIC);
    let mut b = auto::Builder::new(TokioExecutor::new());
    b.http1().timer(TokioTimer::new()).keep_alive(true).header_read_timeout(None);
    b.http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(1024)
        .initial_stream_window_size(1 << 20)
        .initial_connection_window_size(4 << 20)
        .max_send_buf_size(1 << 20);
    log_info!("Listening (TLS)", "addr={}", addr);
    Some(TlsServer { listener, acceptor: TlsAcceptor::from(Arc::new(cfg)), builder: Arc::new(b) })
}

impl TlsServer {
    pub async fn accept(&self, graceful: &GracefulShutdown) {
        let Ok((s, _)) = self.listener.accept().await else { return };
        let _ = s.set_nodelay(true);
        let acceptor = self.acceptor.clone();
        let builder = self.builder.clone();
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(s).await else { return };
            let conn = builder.serve_connection(TokioIo::new(tls), hyper::service::service_fn(handle_tls)).into_owned();
            let _ = watcher.watch(conn).await;
        });
    }
}
