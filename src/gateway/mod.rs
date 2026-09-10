use anyhow::Context;

use crate::{
    api_keys::{AuthenticationError, KeyStore},
    config::{ProviderConfig, ProviderKind},
    policies::{
        Decision, Pipeline, PolicyError, PolicyFailure, RequestContext, restore::StreamRestorer,
    },
    pricing::cost,
    providers::{Provider, extract_usage, requested_model},
    request_id::RequestId,
    telemetry::{CompletionRecord, SqliteSink, StartRecord, timestamp},
};

mod http;

use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
pub use http::router;
use serde_json::json;
use std::{collections::HashMap, io, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const BLOCKING_POLICY_BYTES: usize = 256 * 1024;
const POLICY_HEADER: HeaderName = HeaderName::from_static("x-aegis-policy");

pub(crate) fn webpki_roots_tls_config() -> anyhow::Result<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(config)
}

#[derive(Clone)]
pub struct Gateway {
    client: reqwest::Client,
    sink: SqliteSink,
    keys: KeyStore,
    policies: Pipeline,
    providers: Arc<HashMap<String, ProviderTarget>>,
    max_capture_bytes: usize,
}

#[derive(Clone)]
struct ProviderTarget {
    kind: Provider,
    origin: Arc<str>,
    base_path: Arc<str>,
}

impl ProviderTarget {
    fn upstream_url(&self, path: &str) -> String {
        if self.base_path.is_empty() || begins_with_segments(path, &self.base_path) {
            format!("{}{path}", self.origin)
        } else {
            format!("{}{}{path}", self.origin, self.base_path)
        }
    }
}

fn begins_with_segments(path: &str, prefix: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')
}

impl Gateway {
    pub fn new(
        sink: SqliteSink,
        keys: KeyStore,
        policies: Pipeline,
        providers: Vec<ProviderConfig>,
        max_capture_bytes: usize,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .tls_backend_preconfigured(webpki_roots_tls_config()?)
            .build()?;
        let providers = providers
            .into_iter()
            .map(|config| {
                let (kind, base_url) = match config.kind {
                    ProviderKind::ClaudeSubscription { base_url } => {
                        (Provider::Anthropic, base_url)
                    }
                    ProviderKind::CodexSubscription { base_url } => (Provider::Codex, base_url),
                };
                let parsed = url::Url::parse(&base_url)
                    .with_context(|| format!("provider {:?} has an invalid base_url", config.id))?;
                Ok((
                    config.id,
                    ProviderTarget {
                        kind,
                        origin: Arc::from(parsed.origin().ascii_serialization()),
                        base_path: Arc::from(parsed.path().trim_end_matches('/')),
                    },
                ))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        Ok(Self {
            client,
            sink,
            keys,
            policies,
            providers: Arc::new(providers),
            max_capture_bytes,
        })
    }

    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids = self.providers.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids
    }

    pub async fn forward(&self, provider_id: &str, request: Request) -> Response {
        let Some(target) = self.providers.get(provider_id) else {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "provider_not_found"})),
            )
                .into_response();
        };
        match self.try_forward(provider_id, target, request).await {
            Ok(response) => response,
            Err(error) => {
                tracing::error!(%error, %provider_id, "gateway request failed");
                (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(json!({
                        "error": "gateway_error",
                        "message": "the upstream request could not be completed"
                    })),
                )
                    .into_response()
            }
        }
    }

    async fn try_forward(
        &self,
        provider_id: &str,
        target: &ProviderTarget,
        request: Request,
    ) -> anyhow::Result<Response> {
        let started = std::time::Instant::now();
        let provider = target.kind;
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .map(RequestId::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let (mut parts, body) = request.into_parts();
        let authenticated = match self
            .keys
            .authenticate(&mut parts.headers, provider_id)
            .await
        {
            Ok(key) => key,
            Err(error) => return Ok(authentication_error(error)),
        };
        let body = to_bytes(body, MAX_REQUEST_BYTES).await?;
        let model = requested_model(&body);
        let context = RequestContext {
            body: body.clone(),
            content_encoding: parts
                .headers
                .get(header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        };
        let decision = match self.evaluate_policies(context).await? {
            Ok(decision) => decision,
            Err(failure) => return Ok(policy_failure(failure)),
        };
        let transformed = decision.body.as_ptr() != body.as_ptr();
        let Decision {
            body,
            evaluations,
            restore,
        } = decision;
        let mut restorer =
            (!restore.replacements.is_empty()).then(|| StreamRestorer::new(restore.replacements));
        if transformed {
            parts.headers.remove(header::CONTENT_ENCODING);
            parts.headers.remove(header::CONTENT_LENGTH);
        }
        let endpoint = parts
            .uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or(parts.uri.path());
        let route_prefix = format!("/providers/{provider_id}");
        let upstream_path = endpoint.strip_prefix(&route_prefix).unwrap_or(endpoint);
        let upstream_url = target.upstream_url(upstream_path);
        let span = tracing::Span::current();
        span.record("user_id", tracing::field::display(&authenticated.user_id));
        span.record("key_id", tracing::field::display(&authenticated.id));
        span.record("provider_id", tracing::field::display(provider_id));
        if let Some(model) = model.as_deref() {
            span.record("model", tracing::field::display(model));
        }
        let capture_id = self
            .sink
            .start(StartRecord {
                request_id: &request_id,
                key_id: &authenticated.id,
                key_version_id: &authenticated.version_id,
                provider_id,
                provider,
                method: parts.method.as_str(),
                endpoint,
                requested_model: model.as_deref(),
                request_body: &body,
            })
            .await?;
        self.sink
            .record_evaluations(capture_id, &evaluations)
            .await?;

        let mut outbound = self
            .client
            .request(parts.method.clone(), upstream_url)
            .body(body.clone());
        for (name, value) in &parts.headers {
            if !is_hop_by_hop(name) && name != header::HOST && name != header::ACCEPT_ENCODING {
                outbound = outbound.header(name, value);
            }
        }

        let upstream = match outbound.send().await {
            Ok(response) => response,
            Err(error) => {
                self.sink.fail(capture_id, &error.to_string()).await;
                return Err(error.into());
            }
        };
        let status = upstream.status();
        let mut response_headers = filtered_headers(upstream.headers());
        if restorer.is_some() {
            // Restoring a placeholder changes the body length, and the rewritten
            // body is streamed, so the final length is unknown here.
            response_headers.remove(header::CONTENT_LENGTH);
        }
        let sink = self.sink.clone();
        let max_capture_bytes = self.max_capture_bytes;
        let (sender, receiver) = mpsc::channel::<Result<Bytes, io::Error>>(16);

        let completion_span = span.clone();
        tokio::spawn(async move {
            let mut stream = upstream.bytes_stream();
            let mut capture = Vec::new();
            let mut response_bytes = 0usize;
            let mut truncated = false;
            let mut disconnected = false;
            let mut first_byte_at = None;
            let mut stream_error = None;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => {
                        if first_byte_at.is_none() {
                            first_byte_at = Some(timestamp());
                        }
                        let remaining = max_capture_bytes.saturating_sub(capture.len());
                        if remaining > 0 {
                            capture.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                        }
                        truncated |= chunk.len() > remaining;
                        let chunk = match &mut restorer {
                            Some(restorer) => Bytes::from(restorer.rewrite_chunk(&chunk)),
                            None => chunk,
                        };
                        response_bytes = response_bytes.saturating_add(chunk.len());
                        if sender.send(Ok(chunk)).await.is_err() {
                            disconnected = true;
                            break;
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        stream_error = Some(message.clone());
                        let _ = sender.send(Err(io::Error::other(message))).await;
                        break;
                    }
                }
            }

            if let Some(restorer) = &mut restorer {
                let tail = restorer.finish();
                if !tail.is_empty() && !disconnected && stream_error.is_none() {
                    response_bytes = response_bytes.saturating_add(tail.len());
                    if sender.send(Ok(Bytes::from(tail))).await.is_err() {
                        disconnected = true;
                    }
                }
            }

            let usage = extract_usage(provider, &capture);
            let cost = cost(model.as_deref(), &usage);
            for (field, value) in [
                ("input_tokens", usage.input_tokens),
                ("output_tokens", usage.output_tokens),
                ("cache_read_tokens", usage.cache_read_tokens),
                ("cache_write_tokens", usage.cache_write_tokens),
                ("reasoning_tokens", usage.reasoning_tokens),
            ] {
                if let Some(value) = value {
                    completion_span.record(field, value);
                }
            }
            let _entered = completion_span.enter();
            tracing::info!(
                target: crate::access_log::GATEWAY,
                status = status.as_u16(),
                latency_ms = started.elapsed().as_millis() as u64,
                "request completed"
            );
            drop(_entered);
            if let Err(error) = sink
                .complete(CompletionRecord {
                    id: capture_id,
                    status: status.as_u16(),
                    first_byte_at: first_byte_at.as_deref(),
                    response_body: &capture,
                    response_bytes,
                    response_truncated: truncated,
                    client_disconnected: disconnected,
                    usage: &usage,
                    cost,
                    error_message: stream_error.as_deref(),
                })
                .await
            {
                tracing::error!(%error, %capture_id, "failed to persist gateway completion");
            }
        });

        let mut response = Response::new(Body::from_stream(ReceiverStream::new(receiver)));
        *response.status_mut() = status;
        *response.headers_mut() = response_headers;
        response
            .extensions_mut()
            .insert(crate::access_log::DeferredCompletion);
        Ok(response)
    }
}

impl Gateway {
    async fn evaluate_policies(
        &self,
        context: RequestContext,
    ) -> anyhow::Result<Result<Decision, PolicyFailure>> {
        if context.body.len() <= BLOCKING_POLICY_BYTES {
            return Ok(self.policies.evaluate(context));
        }
        let policies = self.policies.clone();
        Ok(tokio::task::spawn_blocking(move || policies.evaluate(context)).await?)
    }
}

fn authentication_error(error: AuthenticationError) -> Response {
    let status = match error {
        AuthenticationError::ProviderNotAllowed(_) => StatusCode::FORBIDDEN,
        AuthenticationError::Missing | AuthenticationError::Invalid => StatusCode::UNAUTHORIZED,
        AuthenticationError::Backend(ref backend) => {
            tracing::error!(error = %backend, "API key validation failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (
        status,
        axum::Json(json!({
            "error": if status == StatusCode::FORBIDDEN { "provider_not_allowed" } else { "authentication_failed" },
            "message": error.to_string()
        })),
    )
        .into_response()
}

fn policy_failure(failure: PolicyFailure) -> Response {
    let (status, code, message) = match &failure.error {
        PolicyError::InvalidRequest(message) => {
            tracing::warn!(error = %failure, "request policy rejected the request body");
            (StatusCode::BAD_REQUEST, "invalid_request", message.clone())
        }
        PolicyError::Internal(_) => {
            tracing::error!(error = %failure, "request policy failed; refusing to forward");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "policy_failure",
                "a request policy failed, so the request was not forwarded".to_owned(),
            )
        }
    };
    let mut response = (
        status,
        axum::Json(json!({
            "error": code,
            "policy": failure.policy,
            "message": message
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(POLICY_HEADER, HeaderValue::from_static("failed"));
    response
}

fn filtered_headers(headers: &HeaderMap) -> HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::{Provider, ProviderTarget};
    use std::sync::Arc;

    fn target(base_url: &str) -> ProviderTarget {
        let parsed = url::Url::parse(base_url).expect("valid base_url");
        ProviderTarget {
            kind: Provider::Codex,
            origin: Arc::from(parsed.origin().ascii_serialization()),
            base_path: Arc::from(parsed.path().trim_end_matches('/')),
        }
    }

    #[test]
    fn endpoint_only_paths_go_under_the_configured_base_path() {
        let codex = target("https://chatgpt.com/backend-api/codex");

        assert_eq!(
            codex.upstream_url("/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex.upstream_url("/responses?stream=true"),
            "https://chatgpt.com/backend-api/codex/responses?stream=true"
        );
    }

    #[test]
    fn paths_that_restate_the_base_path_are_not_doubled() {
        let codex = target("https://chatgpt.com/backend-api/codex");

        assert_eq!(
            codex.upstream_url("/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex.upstream_url("/backend-api/codex"),
            "https://chatgpt.com/backend-api/codex"
        );
        assert_eq!(
            codex.upstream_url("/backend-api/codex?stream=true"),
            "https://chatgpt.com/backend-api/codex?stream=true"
        );
    }

    #[test]
    fn a_partial_segment_match_is_not_treated_as_the_base_path() {
        let codex = target("https://chatgpt.com/backend-api/codex");

        assert_eq!(
            codex.upstream_url("/backend-api/codexes/responses"),
            "https://chatgpt.com/backend-api/codex/backend-api/codexes/responses"
        );
    }

    #[test]
    fn a_base_url_without_a_path_forwards_the_path_unchanged() {
        let anthropic = target("https://api.anthropic.com");

        assert_eq!(
            anthropic.upstream_url("/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn a_base_url_with_a_port_keeps_the_port() {
        let local = target("http://127.0.0.1:4000/backend-api/codex");

        assert_eq!(
            local.upstream_url("/responses"),
            "http://127.0.0.1:4000/backend-api/codex/responses"
        );
        assert_eq!(
            local.upstream_url("/backend-api/codex/responses"),
            "http://127.0.0.1:4000/backend-api/codex/responses"
        );
    }
}

#[cfg(test)]
mod guardrail_tests {
    use super::Gateway;
    use crate::{
        api_keys::{API_KEY_HEADER, KeyStore},
        compression::decode_body,
        config::{
            GuardrailConfig, GuardrailsConfig, GuardrailsMode, ProviderConfig, ProviderKind,
            RegexGuardrailConfig,
        },
        migration::Migrator,
        policies::{
            mask::{PLACEHOLDER_PREFIX, PLACEHOLDER_SUFFIX},
            pipeline,
            sse::{Frame, FrameParser},
        },
        telemetry::SqliteSink,
    };
    use axum::{
        Router,
        body::{Body, Bytes, to_bytes},
        extract::{Request, State},
        http::{HeaderMap, StatusCode, header},
        response::Response,
        routing::post,
    };
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::net::TcpListener;
    use uuid::Uuid;

    const SECRET: &str = "sk-ant-api03-TESTONLYTESTONLYTESTONLYTESTONLYTEST0000";
    const PROVIDER_ID: &str = "claude";

    #[derive(Clone, Copy)]
    enum Reply {
        SplitStream,
        Json,
    }

    #[derive(Clone)]
    struct Upstream {
        received: Arc<Mutex<Vec<Bytes>>>,
        reply: Reply,
    }

    struct Harness {
        gateway: Gateway,
        database: DatabaseConnection,
        api_key: String,
        received: Arc<Mutex<Vec<Bytes>>>,
    }

    fn request_body() -> String {
        json!({
            "model": "claude-test",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": format!("run export ANTHROPIC_API_KEY={SECRET}")}]
        })
        .to_string()
    }

    fn placeholder_in(text: &str) -> Option<&str> {
        let start = text.find(PLACEHOLDER_PREFIX)?;
        let end = start + text[start..].find(PLACEHOLDER_SUFFIX)? + PLACEHOLDER_SUFFIX.len();
        text.get(start..end)
    }

    fn text_delta(text: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {}\n\n",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            })
        )
    }

    async fn respond(State(upstream): State<Upstream>, body: Bytes) -> Response {
        upstream.received.lock().unwrap().push(body.clone());
        let text = String::from_utf8_lossy(&body);
        let echoed = placeholder_in(&text).unwrap_or(SECRET).to_owned();
        match upstream.reply {
            Reply::SplitStream => {
                let (head, tail) = echoed.split_at(echoed.len() / 2);
                let stream = format!(
                    "{}{}{}event: content_block_stop\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
                    text_delta("token "),
                    text_delta(head),
                    text_delta(tail),
                    json!({"type": "content_block_stop", "index": 0}),
                    json!({"type": "message_stop"}),
                );
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(stream))
                    .unwrap()
            }
            Reply::Json => Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "id": "msg_0123",
                        "type": "message",
                        "content": [{"type": "text", "text": format!("token {echoed}")}],
                        "usage": {"input_tokens": 3, "output_tokens": 2}
                    })
                    .to_string(),
                ))
                .unwrap(),
        }
    }

    async fn serve_upstream(reply: Reply) -> (String, Arc<Mutex<Vec<Bytes>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let router = Router::new()
            .route("/v1/messages", post(respond))
            .with_state(Upstream {
                received: received.clone(),
                reply,
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}"), received)
    }

    async fn harness(mode: GuardrailsMode, reply: Reply) -> Harness {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        let user = Uuid::now_v7();
        database.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','user@example.com','user@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
        let keys = KeyStore::new(database.clone());
        let (_, api_key) = keys
            .create(user, "guardrails", &[PROVIDER_ID.into()])
            .await
            .unwrap();
        let (base_url, received) = serve_upstream(reply).await;
        let guardrails = GuardrailsConfig {
            enabled: true,
            mode,
            secrets: GuardrailConfig {
                enabled: true,
                detectors: None,
            },
            regex: RegexGuardrailConfig::default(),
        };
        let gateway = Gateway::new(
            SqliteSink::new(database.clone()),
            keys,
            pipeline(&guardrails, [7; 32]),
            vec![ProviderConfig {
                id: PROVIDER_ID.into(),
                kind: ProviderKind::ClaudeSubscription { base_url },
            }],
            1024 * 1024,
        )
        .unwrap();
        Harness {
            gateway,
            database,
            api_key,
            received,
        }
    }

    impl Harness {
        async fn send(&self) -> (StatusCode, HeaderMap, String) {
            let request = Request::post(format!("/providers/{PROVIDER_ID}/v1/messages"))
                .header(API_KEY_HEADER, &self.api_key)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(request_body()))
                .unwrap();
            let response = self.gateway.forward(PROVIDER_ID, request).await;
            let (parts, body) = response.into_parts();
            let body = to_bytes(body, 1024 * 1024).await.unwrap();
            self.wait_for_completion().await;
            (
                parts.status,
                parts.headers,
                String::from_utf8(body.to_vec()).unwrap(),
            )
        }

        async fn wait_for_completion(&self) {
            for _ in 0..200 {
                let completed: i64 = self
                    .database
                    .query_one_raw(Statement::from_string(
                        DbBackend::Sqlite,
                        "SELECT COUNT(*) completed FROM gateway_requests WHERE completed_at IS NOT NULL",
                    ))
                    .await
                    .unwrap()
                    .unwrap()
                    .try_get("", "completed")
                    .unwrap();
                if completed == 1 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the capture never completed");
        }

        fn upstream_body(&self) -> String {
            let received = self.received.lock().unwrap();
            let [body] = received.as_slice() else {
                panic!("the upstream should see exactly one request");
            };
            String::from_utf8(body.to_vec()).unwrap()
        }

        async fn stored_bodies(&self) -> Vec<String> {
            self.decoded_blobs("SELECT body FROM gateway_payload_blobs")
                .await
        }

        async fn stored_response(&self) -> String {
            let bodies = self
                .decoded_blobs(
                    "SELECT blobs.body FROM gateway_payload_blobs blobs JOIN gateway_payloads payloads ON payloads.response_body_id = blobs.id",
                )
                .await;
            let [body] = bodies.as_slice() else {
                panic!("exactly one response body should be stored");
            };
            body.clone()
        }

        async fn decoded_blobs(&self, sql: &str) -> Vec<String> {
            self.database
                .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql))
                .await
                .unwrap()
                .into_iter()
                .map(|row| {
                    let body: Vec<u8> = row.try_get("", "body").unwrap();
                    String::from_utf8(decode_body(&body)).unwrap()
                })
                .collect()
        }

        async fn evaluation(&self) -> (String, i64) {
            let row = self
                .database
                .query_one_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    "SELECT outcome, match_count FROM policy_evaluations WHERE policy = 'secrets'",
                ))
                .await
                .unwrap()
                .expect("the secrets guardrail records one evaluation");
            (
                row.try_get("", "outcome").unwrap(),
                row.try_get("", "match_count").unwrap(),
            )
        }
    }

    fn assert_masked_everywhere_but_the_client(harness: &Harness, stored: &[String]) {
        let upstream = harness.upstream_body();
        let placeholder = placeholder_in(&upstream).expect("the upstream sees a placeholder");
        assert!(placeholder.starts_with("AEGIS_MASKED_ANTHROPIC_API_KEY_"));
        assert_eq!(
            placeholder.len(),
            "AEGIS_MASKED_ANTHROPIC_API_KEY_".len() + 22 + 4
        );
        assert!(!upstream.contains(SECRET));
        assert!(!stored.is_empty());
        for body in stored {
            assert!(
                !body.contains(SECRET),
                "stored body leaks the secret: {body}"
            );
        }
    }

    #[tokio::test]
    async fn a_split_stream_is_restored_for_the_client_and_stored_with_the_placeholder() {
        let harness = harness(GuardrailsMode::Mask, Reply::SplitStream).await;

        let (status, _headers, client_body) = harness.send().await;

        assert_eq!(status, StatusCode::OK);
        assert!(client_body.contains(SECRET), "{client_body}");
        assert!(!client_body.contains(PLACEHOLDER_PREFIX), "{client_body}");
        assert!(
            client_body.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
        );

        let stored = harness.stored_bodies().await;
        assert_masked_everywhere_but_the_client(&harness, &stored);
        let response = harness.stored_response().await;
        let upstream = harness.upstream_body();
        let placeholder = placeholder_in(&upstream).unwrap();
        assert_eq!(delta_text(&response), format!("token {placeholder}"));
        assert_eq!(delta_text(&client_body), format!("token {SECRET}"));
        assert_eq!(harness.evaluation().await, ("transform".to_owned(), 1));
    }

    fn delta_text(stream: &str) -> String {
        let mut parser = FrameParser::default();
        let frames = parser.push(stream.as_bytes());
        assert!(
            parser.finish().is_empty(),
            "stream ends on a frame boundary"
        );
        frames
            .iter()
            .filter_map(Frame::data)
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(&data).ok())
            .filter_map(|data| data["delta"]["text"].as_str().map(str::to_owned))
            .collect()
    }

    #[tokio::test]
    async fn a_json_reply_is_restored_for_the_client_and_stored_with_the_placeholder() {
        let harness = harness(GuardrailsMode::Mask, Reply::Json).await;

        let (status, _headers, client_body) = harness.send().await;

        assert_eq!(status, StatusCode::OK);
        let document: serde_json::Value = serde_json::from_str(&client_body).unwrap();
        assert_eq!(
            document["content"][0]["text"].as_str().unwrap(),
            format!("token {SECRET}")
        );

        let stored = harness.stored_bodies().await;
        assert_masked_everywhere_but_the_client(&harness, &stored);
        let response = harness.stored_response().await;
        let upstream = harness.upstream_body();
        assert!(response.contains(placeholder_in(&upstream).unwrap()));
        assert!(response.contains(r#""output_tokens":2"#));
    }

    #[tokio::test]
    async fn observe_mode_forwards_the_body_unchanged_and_records_the_match() {
        let harness = harness(GuardrailsMode::Observe, Reply::Json).await;

        let (status, _headers, client_body) = harness.send().await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(harness.upstream_body(), request_body());
        assert!(client_body.contains(SECRET));
        assert_eq!(harness.evaluation().await, ("allow".to_owned(), 1));
    }
}
