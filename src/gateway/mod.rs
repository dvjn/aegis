use crate::{
    api_keys::{AuthenticationError, KeyStore},
    config::{ProviderConfig, ProviderKind},
    providers::{Provider, extract_usage, requested_model},
    request_id::RequestId,
    telemetry::{CompletionRecord, SqliteSink, StartRecord, timestamp},
};

mod http;

use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Request,
    http::{HeaderMap, HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
pub use http::router;
use serde_json::json;
use std::{collections::HashMap, io, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

fn webpki_roots_tls_config() -> anyhow::Result<rustls::ClientConfig> {
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
    providers: Arc<HashMap<String, ProviderTarget>>,
    max_capture_bytes: usize,
}

#[derive(Clone)]
struct ProviderTarget {
    kind: Provider,
    base_url: Arc<str>,
}

impl Gateway {
    pub fn new(
        sink: SqliteSink,
        keys: KeyStore,
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
                (
                    config.id,
                    ProviderTarget {
                        kind,
                        base_url: Arc::from(base_url.trim_end_matches('/')),
                    },
                )
            })
            .collect();
        Ok(Self {
            client,
            sink,
            keys,
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
        let endpoint = parts
            .uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or(parts.uri.path());
        let route_prefix = format!("/providers/{provider_id}");
        let upstream_path = endpoint.strip_prefix(&route_prefix).unwrap_or(endpoint);
        let upstream_url = format!("{}{upstream_path}", target.base_url);
        let model = requested_model(&body);
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
