use anyhow::Context;

use crate::{
    api_keys::{AuthenticationError, KeyStore},
    config::{ProviderConfig, ProviderKind},
    policies::{Decision, Pipeline, PolicyFailure, RequestContext},
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
const POLICY_HEADER: HeaderName = HeaderName::from_static("x-aegis-policy");
const APPLIED_POLICIES_HEADER: HeaderName = HeaderName::from_static("x-aegis-applied-policies");

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
        let decision = match self.policies.evaluate(RequestContext {
            provider,
            model: model.as_deref(),
            user_id: &authenticated.user_id,
            key_id: &authenticated.id,
            body: &body,
        }) {
            Ok(decision) => decision,
            Err(failure) => return Ok(policy_failure(failure)),
        };
        if let Some(blocked) = &decision.blocked {
            return Ok(policy_block(&decision, blocked));
        }
        let Decision {
            body, evaluations, ..
        } = decision;
        let applied_policies = evaluations
            .iter()
            .map(|evaluation| evaluation.policy)
            .collect::<Vec<_>>()
            .join(",");
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
        let response_headers = filtered_headers(upstream.headers());
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
                        response_bytes = response_bytes.saturating_add(chunk.len());
                        let remaining = max_capture_bytes.saturating_sub(capture.len());
                        if remaining > 0 {
                            capture.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                        }
                        truncated |= response_bytes > max_capture_bytes;
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
        if let Ok(value) = applied_policies.parse() {
            response
                .headers_mut()
                .insert(APPLIED_POLICIES_HEADER, value);
        }
        response
            .extensions_mut()
            .insert(crate::access_log::DeferredCompletion);
        Ok(response)
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
    tracing::error!(error = %failure, "request policy failed; refusing to forward");
    let mut response = (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({
            "error": "policy_failure",
            "policy": failure.policy,
            "message": "a request policy failed, so the request was not forwarded"
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(POLICY_HEADER, HeaderValue::from_static("failed"));
    response
}

fn policy_block(decision: &Decision, blocked: &crate::policies::Blocked) -> Response {
    let mut response = (
        blocked.status,
        axum::Json(json!({
            "error": blocked.code,
            "policy": blocked.policy,
            "message": "the request was blocked by a gateway policy"
        })),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(POLICY_HEADER, HeaderValue::from_static("blocked"));
    if let Ok(value) = decision.applied_policies().parse() {
        headers.insert(APPLIED_POLICIES_HEADER, value);
    }
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
