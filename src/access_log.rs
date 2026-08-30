pub const AUTH: &str = "aegis::access_log::auth";

pub const OAUTH: &str = "aegis::access_log::oauth";

pub const GATEWAY: &str = "aegis::access_log::gateway";

pub const MCP: &str = "aegis::access_log::mcp";

pub const ASSETS: &str = "aegis::access_log::assets";

pub const HEALTHZ: &str = "aegis::access_log::healthz";

pub const DEFAULT_FILTER: &str =
    "aegis=info,aegis::access_log::assets=off,aegis::access_log::healthz=off";

pub fn filter() -> tracing_subscriber::EnvFilter {
    match std::env::var("RUST_LOG") {
        Ok(value) if !value.trim().is_empty() => tracing_subscriber::EnvFilter::try_new(&value)
            .unwrap_or_else(|error| {
                eprintln!("RUST_LOG is invalid ({error}); using {DEFAULT_FILTER}");
                tracing_subscriber::EnvFilter::new(DEFAULT_FILTER)
            }),
        _ => tracing_subscriber::EnvFilter::new(DEFAULT_FILTER),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DeferredCompletion;

macro_rules! layer {
    ($target:expr $(, $field:ident)* $(,)?) => {
        tower_http::trace::TraceLayer::new_for_http()
            .make_span_with(|request: &axum::http::Request<_>| {
                tracing::info_span!(
                    target: $target,
                    "request",
                    method = %request.method(),
                    path = request.uri().path(),
                    version = ?request.version(),
                    request_id = request
                        .extensions()
                        .get::<$crate::request_id::RequestId>()
                        .map($crate::request_id::RequestId::as_str)
                        .unwrap_or_default(),
                    $($field = tracing::field::Empty,)*
                )
            })
            .on_request(())
            .on_response(
                |response: &axum::response::Response<_>,
                 latency: std::time::Duration,
                 _span: &tracing::Span| {
                    if response
                        .extensions()
                        .get::<$crate::access_log::DeferredCompletion>()
                        .is_some()
                    {
                        return;
                    }
                    tracing::info!(
                        target: $target,
                        status = response.status().as_u16(),
                        latency_ms = latency.as_millis() as u64,
                        "request completed"
                    );
                },
            )
            .on_failure(
                |failure: tower_http::classify::ServerErrorsFailureClass,
                 latency: std::time::Duration,
                 _span: &tracing::Span| {
                    tracing::warn!(
                        target: $target,
                        %failure,
                        latency_ms = latency.as_millis() as u64,
                        "request failed"
                    );
                },
            )
            .on_body_chunk(())
            .on_eos(())
    };
}

pub(crate) use layer;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, response::IntoResponse, routing::get};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer is not poisoned").clone())
                .expect("log output is utf-8")
        }
    }

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("buffer is not poisoned")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture(router: Router) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");
        let request = axum::http::Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .expect("request builds");
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async { router.oneshot(request).await.expect("router responds") });
        });
        buffer.contents()
    }

    #[test]
    fn recorded_domain_fields_render_flat_on_the_span() {
        let router = Router::new()
            .route(
                "/",
                get(|| async {
                    let span = tracing::Span::current();
                    span.record("user_id", tracing::field::display("user-7"));
                    span.record("model", tracing::field::display("opus"));
                    "ok"
                }),
            )
            .layer(layer!(GATEWAY, user_id, model));

        let logs = capture(router);
        assert!(logs.contains("user_id=user-7"), "{logs}");
        assert!(logs.contains("model=opus"), "{logs}");
        assert!(logs.contains("request completed"), "{logs}");
    }

    #[test]
    fn unrecorded_fields_are_omitted() {
        let router = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(layer!(GATEWAY, user_id, model));

        let logs = capture(router);
        assert!(logs.contains("request completed"), "{logs}");
        assert!(!logs.contains("user_id"), "{logs}");
        assert!(!logs.contains("model"), "{logs}");
    }

    #[test]
    fn a_deferred_response_is_not_logged_by_the_layer() {
        let router = Router::new()
            .route(
                "/",
                get(|| async {
                    let mut response = "ok".into_response();
                    response.extensions_mut().insert(DeferredCompletion);
                    response
                }),
            )
            .layer(layer!(GATEWAY));

        let logs = capture(router);
        assert!(!logs.contains("request completed"), "{logs}");
    }
}
