use anyhow::Result;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

struct Target {
    client: reqwest::Client,
    prefix: String,
    upstream: String,
    auth: super::auth::TokenSource,
    host: String,
}

pub struct Relay {
    pub url: String,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    pub async fn start(
        server: &str,
        provider: &str,
        auth: super::auth::TokenSource,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let host = listener.local_addr()?.to_string();
        let prefix = format!("/{}", uuid::Uuid::new_v4().simple());
        let url = format!("http://{host}{prefix}");
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()?;
        let state = Arc::new(Target {
            client,
            prefix: prefix.clone(),
            upstream: format!("{server}/providers/{provider}"),
            auth,
            host,
        });
        let router = Router::new()
            .route(&format!("{prefix}/{{*path}}"), any(forward))
            .with_state(state);
        let cancellation = CancellationToken::new();
        let shutdown = cancellation.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await;
        });
        Ok(Self {
            url,
            cancellation,
            task,
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

fn allowed(name: &HeaderName, headers: &HeaderMap) -> bool {
    !matches!(
        name.as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "x-aegis-api-key"
            | "x-aegis-access-token"
    ) && !headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(name.as_str()))
}

async fn forward(State(target): State<Arc<Target>>, request: Request) -> Response {
    if request.headers().contains_key(header::ORIGIN)
        || request
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            != Some(target.host.as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/");
    let Some(suffix) = path.strip_prefix(&target.prefix) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(url) = url::Url::parse(&format!("{}{suffix}", target.upstream)) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // URL normalization must not turn a local route into another provider route.
    let expected = url::Url::parse(&target.upstream).expect("validated server and provider");
    if !url.path().starts_with(&format!("{}/", expected.path())) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let token = match target.auth.access_token().await {
        Ok(token) => token,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                "Aegis login could not be renewed; reconnect with ags setup",
            )
                .into_response();
        }
    };
    let mut outbound = target
        .client
        .request(parts.method, url)
        .header(crate::ags_wire::ACCESS_TOKEN_HEADER, token);
    for (name, value) in &parts.headers {
        if allowed(name, &parts.headers) {
            outbound = outbound.header(name, value);
        }
    }
    let response = match outbound
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                "Aegis could not be reached; no direct-provider fallback was used",
            )
                .into_response();
        }
    };
    let status = response.status();
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        if allowed(name, response.headers()) {
            headers.append(name.clone(), value.clone());
        }
    }
    (status, headers, Body::from_stream(response.bytes_stream())).into_response()
}

#[cfg(test)]
mod tests;
