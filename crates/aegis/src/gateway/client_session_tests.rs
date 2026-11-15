use super::{Gateway, router};
use crate::{
    api_keys::{API_KEY_HEADER, KeyStore},
    config::{GuardrailsConfig, ProviderConfig, ProviderKind},
    migration::Migrator,
    telemetry::SqliteSink,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

struct Harness {
    app: Router,
    database: DatabaseConnection,
    key: String,
    upstream: Arc<Mutex<Vec<HeaderMap>>>,
}

async fn upstream() -> (String, Arc<Mutex<Vec<HeaderMap>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let recorder = received.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |request: Request| {
                let recorder = recorder.clone();
                async move {
                    recorder.lock().unwrap().push(request.headers().clone());
                    let reply: Response = if request.uri().path().ends_with("/responses") {
                        axum::Json(json!({
                            "object": "response",
                            "model": "gpt-test",
                            "usage": {"input_tokens": 3, "output_tokens": 2}
                        }))
                        .into_response()
                    } else {
                        axum::Json(json!({
                            "type": "message",
                            "model": "claude-test",
                            "content": [],
                            "usage": {"input_tokens": 3, "output_tokens": 2}
                        }))
                        .into_response()
                    };
                    reply
                }
            })),
        )
        .await
        .unwrap();
    });
    (origin, received)
}

async fn harness() -> Harness {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let database = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&database, None).await.unwrap();
    let user = uuid::Uuid::now_v7();
    database.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','sessions@example.com','sessions@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
    let keys = KeyStore::new(database.clone());
    let (_, key) = keys
        .create(user, "sessions", &["claude".into(), "codex".into()])
        .await
        .unwrap();
    let (origin, upstream) = upstream().await;
    let gateway = Gateway::new(
        SqliteSink::new(database.clone()),
        keys,
        GuardrailsConfig::default(),
        [7; 32],
        vec![
            ProviderConfig {
                id: "claude".into(),
                kind: ProviderKind::ClaudeSubscription {
                    base_url: origin.clone(),
                },
            },
            ProviderConfig {
                id: "codex".into(),
                kind: ProviderKind::CodexSubscription {
                    base_url: format!("{origin}/backend-api/codex"),
                },
            },
        ],
        1024 * 1024,
    )
    .unwrap();
    Harness {
        app: router(gateway),
        database,
        key,
        upstream,
    }
}

impl Harness {
    async fn send(&self, path: &str, extra: &[(&str, &str)]) -> Option<String> {
        let mut request = Request::post(path)
            .header(API_KEY_HEADER, &self.key)
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = self
            .app
            .clone()
            .oneshot(
                request
                    .body(Body::from(
                        json!({"model": "test", "input": "hi"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        self.database
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT session_id FROM gateway_requests ORDER BY seq DESC LIMIT 1",
            ))
            .await
            .unwrap()
            .expect("the request is recorded")
            .try_get("", "session_id")
            .unwrap()
    }

    fn last_upstream_headers(&self) -> HeaderMap {
        self.upstream
            .lock()
            .unwrap()
            .last()
            .expect("the upstream saw the request")
            .clone()
    }
}

#[tokio::test]
async fn claude_code_session_is_recorded_and_forwarded_unchanged() {
    let harness = harness().await;

    let stored = harness
        .send(
            "/providers/claude/v1/messages",
            &[
                ("x-claude-code-session-id", "4f0c1d52-claude-session"),
                ("x-session-affinity", "pi-session"),
            ],
        )
        .await;

    assert_eq!(stored.as_deref(), Some("4f0c1d52-claude-session"));
    let upstream = harness.last_upstream_headers();
    assert_eq!(
        upstream["x-claude-code-session-id"],
        "4f0c1d52-claude-session"
    );
    assert_eq!(upstream["x-session-affinity"], "pi-session");
}

#[tokio::test]
async fn codex_sessions_are_recorded_through_the_responses_route() {
    let harness = harness().await;

    let current = harness
        .send(
            "/providers/codex/responses",
            &[
                ("session-id", "019a7c2e-codex-current"),
                ("conversation_id", "019a7c2e-codex-conversation"),
            ],
        )
        .await;
    assert_eq!(current.as_deref(), Some("019a7c2e-codex-current"));
    let upstream = harness.last_upstream_headers();
    assert_eq!(upstream["session-id"], "019a7c2e-codex-current");
    assert_eq!(upstream["conversation_id"], "019a7c2e-codex-conversation");

    let legacy = harness
        .send(
            "/providers/codex/responses",
            &[("session_id", "019a7c2e-codex-legacy")],
        )
        .await;
    assert_eq!(legacy.as_deref(), Some("019a7c2e-codex-legacy"));
    assert_eq!(
        harness.last_upstream_headers()["session_id"],
        "019a7c2e-codex-legacy"
    );
}

#[tokio::test]
async fn an_unusable_session_is_not_recorded_but_still_forwarded() {
    let harness = harness().await;
    let too_long = "a".repeat(super::client_session::MAX_SESSION_ID_LEN + 1);

    let stored = harness
        .send(
            "/providers/codex/responses",
            &[("session-id", too_long.as_str())],
        )
        .await;

    assert_eq!(stored, None);
    assert_eq!(
        harness.last_upstream_headers()["session-id"],
        too_long.as_str()
    );
}
