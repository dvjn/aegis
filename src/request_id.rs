use axum::{
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;
use uuid::Uuid;

pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Clone, Debug)]
pub struct RequestId(Arc<str>);

tokio::task_local! {
    static CURRENT: RequestId;
}

pub fn current() -> Option<String> {
    CURRENT.try_with(|id| id.as_str().to_owned()).ok()
}

impl RequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub async fn propagate(mut request: Request, next: Next) -> Response {
    let value = supplied(request.headers()).unwrap_or_else(new_id);
    let header = HeaderValue::from_str(&value).ok();
    request
        .extensions_mut()
        .insert(RequestId(Arc::from(value.as_str())));
    let mut response = CURRENT
        .scope(RequestId(Arc::from(value.as_str())), next.run(request))
        .await;
    if let Some(header) = header {
        response.headers_mut().insert(REQUEST_ID_HEADER, header);
    }
    response
}

fn new_id() -> String {
    Uuid::now_v7().simple().to_string()
}

fn supplied(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(&REQUEST_ID_HEADER)?.to_str().ok()?;
    let acceptable = (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    acceptable.then(|| value.to_owned())
}
