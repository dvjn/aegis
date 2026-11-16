//! The private loopback proxy of one launch. It forwards the CLI's requests to the launch's
//! providers on the paired gateway and adds the machine's token, so the CLI never holds it. Each
//! provider has its own path prefix, which a launch on one provider leaves empty.
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
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
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
    origin: String,
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

/// What the proxy does with requests under one path prefix.
pub(crate) struct Route {
    /// What the CLI's base URL adds after the proxy's address, such as `/<id>`; empty when the
    /// launch has one provider.
    pub prefix: String,
    pub to: To,
}

pub(crate) enum To {
    /// Provider `id` on the gateway; `base_path` is what the CLI adds that the gateway does not expect.
    Provider { id: String, base_path: &'static str },
    /// Answers every request with this message.
    Refusal(String),
}

impl Route {
    pub(crate) fn provider(prefix: String, id: &str, base_path: &'static str) -> Self {
        Self {
            prefix,
            to: To::Provider {
                id: id.to_owned(),
                base_path,
            },
        }
    }

    /// The path that comes before what the CLI sends.
    fn base(&self) -> String {
        match &self.to {
            To::Provider { base_path, .. } => format!("{}{base_path}", self.prefix),
            To::Refusal(_) => self.prefix.clone(),
        }
    }
}

struct Target {
    store: Store,
    server: String,
    routes: Vec<Route>,
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
        routes: Vec<Route>,
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
            routes,
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
            origin: format!("http://{host}"),
            secret,
            target,
            task,
        })
    }

    /// The proxy's scheme and address, which each route's base URL starts with.
    pub(crate) fn origin(&self) -> &str {
        &self.origin
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
    fn is_from_launch(&self, uri: &Uri, headers: &HeaderMap) -> bool {
        // HTTP/2 names the host in the request's authority instead of a header.
        let host = match headers.get(header::HOST) {
            Some(host) => Some(host.as_bytes()),
            None => uri
                .authority()
                .map(|authority| authority.as_str().as_bytes()),
        };
        host == Some(self.host.as_bytes())
            && !headers.contains_key(header::ORIGIN)
            && headers
                .get(SECRET_HEADER)
                .is_some_and(|secret| bool::from(secret.as_bytes().ct_eq(self.secret.as_bytes())))
    }

    /// The route of a request, and its path and query after the route's base.
    fn route<'a>(&self, uri: &'a Uri) -> Option<(&To, &'a str)> {
        let path = uri.path_and_query().map_or("/", |value| value.as_str());
        self.routes.iter().find_map(|route| {
            let base = route.base();
            let rest = path
                .as_bytes()
                .starts_with(base.as_bytes())
                .then(|| path.get(base.len()..))
                .flatten()?;
            (rest.is_empty() || rest.starts_with(['/', '?'])).then_some((&route.to, rest))
        })
    }

    /// The gateway URL of provider `id` for `rest`, which must stay under the provider's route.
    fn url(&self, id: &str, rest: &str) -> Option<Url> {
        let route = format!("/providers/{id}");
        let url = Url::parse(&format!("{}{route}{rest}", self.server)).ok()?;
        let path = url.path();
        let inside = path
            .strip_prefix(&route)
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

impl Target {
    /// The stored token, when another ags replaced the `used` one, as by pairing again.
    async fn newer_token(&self, used: &str) -> Option<String> {
        let stored = self.store.load().ok()??;
        if stored.server != self.server || stored.access_token == used {
            return None;
        }
        let mut login = self.login.lock().await;
        *login = stored;
        Some(login.access_token.clone())
    }
}

async fn forward(State(target): State<Arc<Target>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    if !target.is_from_launch(&parts.uri, &parts.headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // HTTP/2's extended CONNECT is a CONNECT too, with its protocol in an extension.
    if parts.method == Method::CONNECT {
        return failure(
            StatusCode::NOT_IMPLEMENTED,
            "the launch proxy does not open tunnels (CONNECT)",
        );
    }
    let url = match target.route(&parts.uri) {
        Some((To::Provider { id, .. }, rest)) => target.url(id, rest),
        Some((To::Refusal(message), _)) => return failure(StatusCode::FORBIDDEN, message),
        None => None,
    };
    let Some(url) = url else {
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
    // Kept whole, so that a request refused with a replaced token can be sent again.
    let body = match body.size_hint().exact() {
        Some(0) => None,
        _ => match axum::body::to_bytes(body, usize::MAX).await {
            Ok(body) => Some(body),
            Err(error) => {
                return failure(
                    StatusCode::BAD_REQUEST,
                    &format!("could not read the request: {error}"),
                );
            }
        },
    };
    let send = |token: String| {
        let mut outbound = target.http.request(parts.method.clone(), url.clone());
        for (name, value) in &parts.headers {
            if passes(name, &parts.headers) {
                outbound = outbound.header(name, value);
            }
        }
        outbound = outbound
            .header(CREDENTIAL_HEADER, token)
            .header(LAUNCH_ID_HEADER, target.launch_id.clone());
        if let Some(body) = &body {
            outbound = outbound.body(body.clone());
        }
        outbound.send()
    };
    let mut sent = send(token.clone()).await;
    if sent
        .as_ref()
        .is_ok_and(|response| response.status() == StatusCode::UNAUTHORIZED)
        && let Some(newer) = target.newer_token(&token).await
    {
        sent = send(newer).await;
    }
    let response = match sent {
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

    fn target(base_path: &'static str) -> Target {
        routed(vec![Route::provider(String::new(), "claude", base_path)])
    }

    fn routed(routes: Vec<Route>) -> Target {
        Target {
            store: Store::at("unused"),
            server: "https://aegis.example".into(),
            routes,
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
                lifetime: 3600,
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

    #[tokio::test]
    async fn a_tunnel_is_refused() {
        let target = Arc::new(target(""));
        let connect = |uri: &'static str, host: Option<&'static str>| {
            let mut request = Request::builder()
                .method(Method::CONNECT)
                .uri(uri)
                .header(SECRET_HEADER, SECRET);
            if let Some(host) = host {
                request = request.header(header::HOST, host);
            }
            request.body(Body::empty()).unwrap()
        };
        for request in [
            connect("127.0.0.1:4000", Some("127.0.0.1:4000")),
            connect("http://127.0.0.1:4000/v1/messages", None),
        ] {
            let response = forward(State(target.clone()), request).await;
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        }
        let elsewhere = connect("evil.example:443", Some("evil.example:443"));
        let response = forward(State(target), elsewhere).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
        let origin_form = Uri::from_static("/v1/messages");
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut pairs = vec![("host", "127.0.0.1:4000"), ("x-ags-launch-secret", SECRET)];
            pairs.extend_from_slice(extra);
            headers(&pairs)
        };
        assert!(target.is_from_launch(&origin_form, &with(&[])));
        assert!(
            !target.is_from_launch(&origin_form, &with(&[("origin", "http://127.0.0.1:4000")]))
        );
        assert!(!target.is_from_launch(&origin_form, &headers(&[("host", "127.0.0.1:4000")])));
        let secret_only = headers(&[("x-ags-launch-secret", SECRET)]);
        assert!(target.is_from_launch(
            &Uri::from_static("http://127.0.0.1:4000/model/m/converse"),
            &secret_only
        ));
        assert!(!target.is_from_launch(&origin_form, &secret_only));
        assert!(!target.is_from_launch(
            &Uri::from_static("http://evil.example:4000/model/m/converse"),
            &secret_only
        ));
        for (host, secret) in [
            ("localhost:4000", SECRET),
            ("evil.example:4000", SECRET),
            ("127.0.0.1:4000", "0123456789abcdef0123456789abcdee"),
            ("127.0.0.1:4000", "0123456789abcdef"),
            ("127.0.0.1:4000", ""),
        ] {
            let request = headers(&[("host", host), ("x-ags-launch-secret", secret)]);
            assert!(
                !target.is_from_launch(&origin_form, &request),
                "{host} {secret}"
            );
        }
    }

    /// The gateway URL a request to `path` goes to.
    fn forwarded(target: &Target, path: &'static str) -> Option<String> {
        match target.route(&Uri::from_static(path))? {
            (To::Provider { id, .. }, rest) => target.url(id, rest).map(String::from),
            (To::Refusal(_), _) => None,
        }
    }

    #[test]
    fn the_base_path_is_removed_and_nothing_else_matches() {
        let plain = target("");
        let rest = |target: &Target, path: &'static str| {
            target
                .route(&Uri::from_static(path))
                .map(|(_, rest)| rest.to_owned())
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
            target
                .url("claude", "/v1/messages?beta=true")
                .unwrap()
                .as_str(),
            "https://aegis.example/providers/claude/v1/messages?beta=true"
        );
        assert_eq!(
            target.url("claude", "").unwrap().as_str(),
            "https://aegis.example/providers/claude"
        );
        for escape in [
            "/../codex/responses",
            "/%2e%2e/%2E%2E/api/machine",
            "/..",
            "x",
        ] {
            assert!(target.url("claude", escape).is_none(), "{escape}");
        }
    }

    #[tokio::test]
    async fn each_provider_has_its_own_prefix_and_a_refused_one_says_why() {
        let target = Arc::new(routed(vec![
            Route::provider("/work".into(), "work", ""),
            Route::provider("/chat".into(), "chat", "/codex"),
            Route {
                prefix: "/.off/google-vertex".into(),
                to: To::Refusal("no vertex provider".into()),
            },
        ]));
        assert_eq!(
            forwarded(&target, "/work/v1/messages?beta=true").as_deref(),
            Some("https://aegis.example/providers/work/v1/messages?beta=true")
        );
        assert_eq!(
            forwarded(&target, "/chat/codex/responses").as_deref(),
            Some("https://aegis.example/providers/chat/responses")
        );
        for path in [
            "/v1/messages",
            "/workshop/v1/messages",
            "/chat/responses",
            "/work/../chat/codex/responses",
            "/.off",
        ] {
            assert_eq!(forwarded(&target, path), None, "{path}");
        }
        let request = Request::builder()
            .method(Method::POST)
            .uri("/.off/google-vertex/v1/projects/p")
            .header(header::HOST, "127.0.0.1:4000")
            .header(SECRET_HEADER, SECRET)
            .body(Body::empty())
            .unwrap();
        let response = forward(State(target), request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["message"],
            "ags: no vertex provider"
        );
    }

    #[tokio::test]
    async fn a_refused_request_is_sent_again_once_with_a_replaced_login() {
        use axum::routing::post;
        let gateway = Router::new().route(
            "/providers/claude/v1/messages",
            post(|headers: HeaderMap, body: String| async move {
                match headers
                    .get(CREDENTIAL_HEADER)
                    .and_then(|token| token.to_str().ok())
                {
                    Some("new") => (StatusCode::OK, body),
                    _ => (StatusCode::UNAUTHORIZED, String::new()),
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, gateway).await.unwrap() });
        let dir = crate::store::tests::scratch();
        let login = |token: &str| Login {
            server: server.clone(),
            client_id: "client".into(),
            access_token: token.into(),
            refresh_token: None,
            expires_at: crate::store::now() + 3600,
            lifetime: 3600,
        };
        let mut target = target("");
        target.store = Store::at(&dir);
        target.server = server.clone();
        target.login = Mutex::new(login("old"));
        let target = Arc::new(target);
        let request = || {
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header(header::HOST, "127.0.0.1:4000")
                .header(SECRET_HEADER, SECRET)
                .body(Body::from("hello"))
                .unwrap()
        };

        target.store.save(&login("old")).unwrap();
        let refused = forward(State(target.clone()), request()).await;
        assert_eq!(
            refused.status(),
            StatusCode::UNAUTHORIZED,
            "the same login is not retried"
        );
        assert!(target.refused.load(Ordering::Relaxed));

        target.refused.store(false, Ordering::Relaxed);
        target.store.save(&login("new")).unwrap();
        let response = forward(State(target.clone()), request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body, "hello", "the request is sent again whole");
        assert!(!target.refused.load(Ordering::Relaxed));
        assert_eq!(target.login.lock().await.access_token, "new");
        std::fs::remove_dir_all(dir).unwrap();
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
