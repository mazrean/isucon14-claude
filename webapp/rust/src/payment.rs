// 決済ゲートウェイ (payment_gateway.go): Idempotency-Key (ride ID) 付きで成功するまでリトライする。
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static CLIENT: LazyLock<Client<HttpConnector, Full<Bytes>>> = LazyLock::new(|| {
    let mut c = HttpConnector::new();
    c.set_connect_timeout(Some(Duration::from_secs(2)));
    c.set_keepalive(Some(Duration::from_secs(30)));
    c.set_nodelay(true);
    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(512)
        .pool_idle_timeout(Duration::from_secs(90))
        .build(c)
});

async fn attempt(url: &str, token: &str, ride_id: &str, body: &Bytes) -> Result<(), String> {
    let req = hyper::Request::post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("Idempotency-Key", ride_id)
        .body(Full::new(body.clone()))
        .map_err(|e| e.to_string())?;
    let fut = async {
        let res = CLIENT.request(req).await.map_err(|e| format!("Post \"{url}\": {e}"))?;
        let status = res.status().as_u16();
        let _ = res.into_body().collect().await;
        if status / 100 != 2 {
            return Err(format!("payment status {status}"));
        }
        Ok(())
    };
    match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("Post \"{url}\": context deadline exceeded (Client.Timeout exceeded while awaiting headers)")),
    }
}

pub async fn post_payment(gateway: &str, token: &str, ride_id: &str, amount: i64) -> Result<(), String> {
    let body = Bytes::from(format!("{{\"amount\":{amount}}}"));
    let url = format!("{gateway}/payments");
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut last_err = String::new();
    let mut attempt_no = 0;
    while Instant::now() < deadline {
        match attempt(&url, token, ride_id, &body).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
        if attempt_no >= 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        attempt_no += 1;
    }
    Err(last_err)
}
