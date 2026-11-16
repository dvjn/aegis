//! The private loopback proxy of one launch. It forwards the CLI's requests to one provider on
//! the paired gateway and adds the machine's token, so the CLI never holds it.
//!
//! Loopback is shared by every user and browser on the machine, so the proxy answers only
//! requests that name its address as their host (no DNS rebinding), carry no `Origin` (no web
//! page), and carry the launch's random secret in a header (no other local user).
use std::{
    net::Ipv4Addr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use aegis_wire::{CREDENTIAL_HEADER, LAUNCH_ID_HEADER};
use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, HttpBody},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use url::Url;

use crate::{LoggedOut, Login, Store, current_login};

/// Carries the launch's secret from the CLI to the proxy.
pub(crate) const SECRET_HEADER: &str = "x-ags-launch-secret";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Proxy {
    base_url: String,
    secret: String,
    target: Arc<Target>,
    task: JoinHandle<()>,
}

/// What the proxy learned about the login while the CLI ran.
pub(crate) struct Ended {
    /// The gateway or a vendor answered 401 at least once.
    pub refused: bool,
    /// Refreshing the token found that the server logged this machine out.
    pub logged_out: Option<anyhow::Error>,
}

struct Target {
    store: Store,
    server: String,
    /// `/providers/<id>` on the server.
    route: String,
    /// The path the CLI adds that the gateway does not expect.
    base_path: String,
    host: String,
    secret: HeaderValue,
    launch_id: HeaderValue,
    http: reqwest::Client,
    login: Mutex<Login>,
    refused: AtomicBool,
    logged_out: std::sync::Mutex<Option<anyhow::Error>>,
}

impl Proxy {
    pub(crate) async fn start(
        store_dir: &Path,
        login: Login,
        provider: &str,
        base_path: &str,
        launch_id: &str,
    ) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("starting the proxy")?;
        let host = listener.local_addr()?.to_string();
        let secret = new_secret()?;
        let target = Arc::new(Target {
            store: Store::at(store_dir),
            server: login.server.clone(),
            route: format!("/providers/{provider}"),
            base_path: base_path.to_owned(),
            host: host.clone(),
            secret: HeaderValue::from_str(&secret)?,
            launch_id: HeaderValue::from_str(launch_id).context("an invalid launch ID")?,
            http: crate::server::client()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .context("starting the HTTP client")?,
            login: Mutex::new(login),
            refused: AtomicBool::new(false),
            logged_out: std::sync::Mutex::new(None),
        });
        let app = Router::new().fallback(forward).with_state(target.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            base_url: format!("http://{host}{base_path}"),
            secret,
            target,
            task,
        })
    }

    /// The URL the CLI uses in place of its provider's.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The value the CLI sends in [`SECRET_HEADER`].
    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }

    pub(crate) fn end(self) -> Ended {
        self.task.abort();
        let logged_out = self
            .target
            .logged_out
            .lock()
            .expect("the proxy's state is intact")
            .take();
        Ended {
            refused: self.target.refused.load(Ordering::Relaxed),
            logged_out,
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 128 bits from the operating system, as hex.
fn new_secret() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("no secure random source: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Target {
    fn is_from_launch(&self, headers: &HeaderMap) -> bool {
        headers.get(header::HOST).map(HeaderValue::as_bytes) == Some(self.host.as_bytes())
            && !headers.contains_key(header::ORIGIN)
            && headers
                .get(SECRET_HEADER)
                .is_some_and(|secret| bool::from(secret.as_bytes().ct_eq(self.secret.as_bytes())))
    }

    /// The path and query after the base path.
    fn rest<'a>(&self, uri: &'a Uri) -> Option<&'a str> {
        let path = uri.path_and_query().map_or("/", |value| value.as_str());
        let rest = path
            .as_bytes()
            .starts_with(self.base_path.as_bytes())
            .then(|| path.get(self.base_path.len()..))
            .flatten()?;
        (rest.is_empty() || rest.starts_with(['/', '?'])).then_some(rest)
    }

    /// The gateway URL for `rest`, which must stay under this provider's route.
    fn url(&self, rest: &str) -> Option<Url> {
        let url = Url::parse(&format!("{}{}{rest}", self.server, self.route)).ok()?;
        let path = url.path();
        let inside = path
            .strip_prefix(&self.route)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with('/'));
        inside.then_some(url)
    }

    async fn token(&self) -> Result<String, (StatusCode, String)> {
        let mut login = self.login.lock().await;
        if !login.expires_soon() {
            return Ok(login.access_token.clone());
        }
        match current_login(&self.store).await {
            Ok(Some(current)) if current.server == self.server => {
                *login = current;
                Ok(login.access_token.clone())
            }
            Ok(_) => Err((
                StatusCode::UNAUTHORIZED,
                format!("this machine is no longer logged in to {}", self.server),
            )),
            Err(error) if error.is::<LoggedOut>() => {
                let message = error.to_string();
                *self.logged_out.lock().expect("the proxy's state is intact") = Some(error);
                Err((StatusCode::UNAUTHORIZED, message))
            }
            Err(error) => Err((StatusCode::BAD_GATEWAY, format!("{error:#}"))),
        }
    }
}

async fn forward(State(target): State<Arc<Target>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    if !target.is_from_launch(&parts.headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(url) = target.rest(&parts.uri).and_then(|rest| target.url(rest)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if parts.headers.contains_key(header::UPGRADE) {
        return failure(
            StatusCode::NOT_IMPLEMENTED,
            "Aegis does not carry WebSocket connections",
        );
    }
    let token = match target.token().await {
        Ok(token) => token,
        Err((status, message)) => return failure(status, &message),
    };
    let mut outbound = target.http.request(parts.method, url);
    for (name, value) in &parts.headers {
        if passes(name, &parts.headers) {
            outbound = outbound.header(name, value);
        }
    }
    outbound = outbound
        .header(CREDENTIAL_HEADER, token)
        .header(LAUNCH_ID_HEADER, target.launch_id.clone());
    if body.size_hint().exact() != Some(0) {
        outbound = outbound.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let response = match outbound.send().await {
        Ok(response) => response,
        Err(error) => {
            return failure(
                StatusCode::BAD_GATEWAY,
                &format!("could not reach {}: {error}", target.server),
            );
        }
    };
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        target.refused.store(true, Ordering::Relaxed);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        if passes(name, response.headers()) {
            headers.append(name.clone(), value.clone());
        }
    }
    (status, headers, Body::from_stream(response.bytes_stream())).into_response()
}

/// Whether a header crosses the proxy: hop-by-hop headers and ags's own never do.
fn passes(name: &HeaderName, headers: &HeaderMap) -> bool {
    let named_by_connection = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case(name.as_str()));
    !named_by_connection
        && !matches!(
            name.as_str(),
            "connection"
                | "host"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
        && name != CREDENTIAL_HEADER
        && name != LAUNCH_ID_HEADER
        && name != SECRET_HEADER
}

/// An error in the shape Anthropic and OpenAI clients both show.
fn failure(status: StatusCode, message: &str) -> Response {
    let body = json!({"error": {"type": "ags_error", "message": format!("ags: {message}")}});
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn target(base_path: &str) -> Target {
        Target {
            store: Store::at("unused"),
            server: "https://aegis.example".into(),
            route: "/providers/claude".into(),
            base_path: base_path.into(),
            host: "127.0.0.1:4000".into(),
            secret: HeaderValue::from_static(SECRET),
            launch_id: HeaderValue::from_static("launch"),
            http: crate::server::client().build().unwrap(),
            login: Mutex::new(Login {
                server: "https://aegis.example".into(),
                client_id: "client".into(),
                access_token: "token".into(),
                refresh_token: None,
                expires_at: 0,
            }),
            refused: AtomicBool::new(false),
            logged_out: std::sync::Mutex::new(None),
        }
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                )
            })
            .collect()
    }

    #[test]
    fn a_secret_is_128_random_bits() {
        let (first, second) = (new_secret().unwrap(), new_secret().unwrap());
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn only_the_launched_process_reaches_the_proxy() {
        let target = target("");
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut pairs = vec![("host", "127.0.0.1:4000"), ("x-ags-launch-secret", SECRET)];
            pairs.extend_from_slice(extra);
            headers(&pairs)
        };
        assert!(target.is_from_launch(&with(&[])));
        assert!(!target.is_from_launch(&with(&[("origin", "http://127.0.0.1:4000")])));
        assert!(!target.is_from_launch(&headers(&[("host", "127.0.0.1:4000")])));
        for (host, secret) in [
            ("localhost:4000", SECRET),
            ("evil.example:4000", SECRET),
            ("127.0.0.1:4000", "0123456789abcdef0123456789abcdee"),
            ("127.0.0.1:4000", "0123456789abcdef"),
            ("127.0.0.1:4000", ""),
        ] {
            let request = headers(&[("host", host), ("x-ags-launch-secret", secret)]);
            assert!(!target.is_from_launch(&request), "{host} {secret}");
        }
    }

    #[test]
    fn the_base_path_is_removed_and_nothing_else_matches() {
        let plain = target("");
        let rest = |target: &Target, path: &'static str| {
            target.rest(&Uri::from_static(path)).map(str::to_owned)
        };
        assert_eq!(
            rest(&plain, "/v1/messages").as_deref(),
            Some("/v1/messages")
        );
        assert_eq!(rest(&plain, "/?beta=true").as_deref(), Some("/?beta=true"));
        let codex = target("/codex");
        assert_eq!(
            rest(&codex, "/codex/responses").as_deref(),
            Some("/responses")
        );
        assert_eq!(rest(&codex, "/codex").as_deref(), Some(""));
        assert_eq!(rest(&codex, "/codexx/responses"), None);
        assert_eq!(rest(&codex, "/responses"), None);
    }

    #[test]
    fn a_request_stays_under_its_provider() {
        let target = target("");
        assert_eq!(
            target.url("/v1/messages?beta=true").unwrap().as_str(),
            "https://aegis.example/providers/claude/v1/messages?beta=true"
        );
        assert_eq!(
            target.url("").unwrap().as_str(),
            "https://aegis.example/providers/claude"
        );
        for escape in [
            "/../codex/responses",
            "/%2e%2e/%2E%2E/api/machine",
            "/..",
            "x",
        ] {
            assert!(target.url(escape).is_none(), "{escape}");
        }
    }

    #[test]
    fn credentials_and_hop_by_hop_headers_never_cross() {
        let incoming = headers(&[
            ("connection", "keep-alive, x-private"),
            ("x-private", "1"),
            ("host", "127.0.0.1:4000"),
            ("transfer-encoding", "chunked"),
            ("x-aegis-api-key", "the user's own key"),
            ("x-aegis-launch-id", "forged"),
            ("x-ags-launch-secret", SECRET),
            ("authorization", "Bearer vendor-sign-in"),
            ("anthropic-version", "2023-06-01"),
        ]);
        let crossing: Vec<_> = incoming
            .keys()
            .filter(|name| passes(name, &incoming))
            .map(HeaderName::as_str)
            .collect();
        assert_eq!(crossing, ["authorization", "anthropic-version"]);
    }
}
