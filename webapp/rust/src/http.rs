// レスポンスボディ型と小さなレスポンス生成ヘルパ。
use bytes::Bytes;
use hyper::body::{Frame, SizeHint};
use hyper::header::{HeaderValue, CONTENT_TYPE, SET_COOKIE};
use hyper::{Response, StatusCode};
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

pub enum Body {
    Full(Option<Bytes>),
    Stream(mpsc::UnboundedReceiver<Bytes>),
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Body::Full(b) => Poll::Ready(b.take().map(|b| Ok(Frame::data(b)))),
            Body::Stream(rx) => rx.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Body::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Body::Full(Some(b)) => SizeHint::with_exact(b.len() as u64),
            Body::Full(None) => SizeHint::with_exact(0),
            Body::Stream(_) => SizeHint::default(),
        }
    }
}

pub type Resp = Response<Body>;

static JSON_CT: HeaderValue = HeaderValue::from_static("application/json;charset=utf-8");

pub fn json(status: u16, body: Vec<u8>) -> Resp {
    let mut r = Response::new(Body::Full(Some(Bytes::from(body))));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r.headers_mut().insert(CONTENT_TYPE, JSON_CT.clone());
    r
}

pub fn json_cookie(status: u16, body: Vec<u8>, name: &str, value: &str) -> Resp {
    let mut r = json(status, body);
    if let Ok(v) = HeaderValue::from_str(&format!("{name}={value}; Path=/")) {
        r.headers_mut().insert(SET_COOKIE, v);
    }
    r
}

pub fn err(status: u16, msg: &str) -> Resp {
    json(status, crate::util::error_json(msg))
}

/// w.WriteHeader(status) だけのレスポンス
pub fn bare(status: u16) -> Resp {
    let mut r = Response::new(Body::Full(None));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r
}

pub fn text(status: u16, body: &'static str) -> Resp {
    let mut r = Response::new(Body::Full(Some(Bytes::from_static(body.as_bytes()))));
    *r.status_mut() = StatusCode::from_u16(status).unwrap();
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    r
}

pub fn not_found() -> Resp {
    text(404, "404 page not found\n")
}

pub fn method_not_allowed(allow: &'static str) -> Resp {
    let mut r = text(405, "Method Not Allowed\n");
    r.headers_mut().insert("allow", HeaderValue::from_static(allow));
    r
}

pub fn sse(rx: mpsc::UnboundedReceiver<Bytes>) -> Resp {
    let mut r = Response::new(Body::Stream(rx));
    let h = r.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert("cache-control", HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}
