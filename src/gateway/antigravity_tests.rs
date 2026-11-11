use super::*;
use crate::{
    config::{GuardrailsConfig, GuardrailsMode, RegexGuardrailConfig},
    migration::Migrator,
    policies::pipeline,
};
use axum::{Router, body::to_bytes, routing::any};
use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
use sea_orm_migration::MigratorTrait;
use serde_json::Value;
use std::collections::BTreeMap;
use tower::ServiceExt;

#[tokio::test]
async fn subscription_inference_is_accounted_and_control_plane_is_private() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (sender, mut received) = tokio::sync::mpsc::channel(4);
    let backend = tokio::spawn(async move {
        axum::serve(listener,Router::new().fallback(any(move |request:Request| {
            let sender = sender.clone();
            async move {
                assert_eq!(request.headers()[header::AUTHORIZATION],"Bearer google-subscription");
                assert!(!request.headers().contains_key("x-aegis-api-key"));
                assert!(!request.headers().contains_key(crate::ags_wire::ACCESS_TOKEN_HEADER));
                let path = request.uri().to_string();
                let body = to_bytes(request.into_body(),1024*1024).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                sender.send((path.clone(),body.clone())).await.unwrap();
                if path.contains("loadCodeAssist") { return axum::Json(json!({"account":"private-account"})).into_response(); }
                let masked = body["request"]["contents"][0]["parts"][0]["text"].as_str().unwrap();
                let (head,tail) = masked.split_at(masked.len()/2);
                let stream = [
                    json!({"response":{"modelVersion":"gemini-3.8-flash-n","candidates":[{"index":0,"content":{"parts":[{"text":head}]}},{"index":1,"content":{"parts":[{"text":head}]}}]}}),
                    json!({"response":{"candidates":[{"index":0,"content":{"parts":[{"text":tail,"thoughtSignature":"signed-response"}]}},{"index":1,"content":{"parts":[{"text":tail}]}}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":20,"thoughtsTokenCount":5,"totalTokenCount":125}}}),
                ].into_iter().map(|value| format!("data: {value}\n\n")).collect::<String>();
                ([(header::CONTENT_TYPE,"text/event-stream")],stream).into_response()
            }
        }))).await.unwrap();
    });
    let database = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&database, None).await.unwrap();
    let user = uuid::Uuid::now_v7();
    database.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','agy@example.com','agy@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
    let keys = KeyStore::new(database.clone());
    let (_, key) = keys.create(user, "agy", &["google".into()]).await.unwrap();
    let guardrails = GuardrailsConfig {
        enabled: true,
        mode: GuardrailsMode::Mask,
        regex: RegexGuardrailConfig {
            enabled: true,
            detectors: BTreeMap::from([("test_secret".into(), "unit-secret-123".into())]),
        },
        ..Default::default()
    };
    let gateway = Gateway::new(
        SqliteSink::new(database.clone()),
        keys,
        pipeline(&guardrails, [7; 32]),
        vec![ProviderConfig {
            id: "google".into(),
            kind: ProviderKind::AntigravitySubscription { base_url: origin },
        }],
        1024 * 1024,
    )
    .unwrap();
    let app = router(gateway);
    let send = |endpoint: &str, body: Value| {
        app.clone().oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/providers/google/{endpoint}"))
                .header("x-aegis-api-key", &key)
                .header(header::AUTHORIZATION, "Bearer google-subscription")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
    };
    let response = send(
        "v1internal:loadCodeAssist",
        json!({"account":"unit-secret-123"}),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024).await.unwrap();
    let (path, control) = received.recv().await.unwrap();
    assert_eq!(path, "/v1internal:loadCodeAssist");
    assert_eq!(control["account"], "unit-secret-123");
    let response = send("v1internal:streamGenerateContent?alt=sse",json!({"model":"gemini-3.8-flash-low","request":{
        "systemInstruction":{"parts":[{"text":"unit-secret-123"}]},
        "contents":[{"role":"user","parts":[{"text":"unit-secret-123"},{"inlineData":{"mimeType":"image/png","data":"unit-secret-123"}}]},{"role":"model","parts":[{"text":"unit-secret-123","thoughtSignature":"signed-request"}]}],
        "tools":[{"functionDeclarations":[{"name":"Read","description":"unit-secret-123","parameters":{"type":"OBJECT"}}]}]
    }})).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let output = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let (path, body) = received.recv().await.unwrap();
    assert_eq!(path, "/v1internal:streamGenerateContent?alt=sse");
    assert!(
        body["request"]["contents"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("AEGIS_MASKED_")
    );
    assert_eq!(
        body["request"]["contents"][1]["parts"][0]["text"],
        "unit-secret-123"
    );
    assert_eq!(
        body["request"]["contents"][0]["parts"][1]["inlineData"]["data"],
        "unit-secret-123"
    );
    let output = String::from_utf8(output.to_vec()).unwrap();
    assert!(output.contains("signed-response"));
    assert!(output.contains("unit-secret-123"));
    let usage = tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            if let Some(row) = database.query_one_raw(Statement::from_string(DbBackend::Sqlite,"SELECT u.*,r.protocol,r.resolved_model FROM gateway_usage u JOIN gateway_requests r ON r.id=u.request_id")).await.unwrap() { break row; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(usage.try_get::<i64>("", "input_tokens").unwrap(), 60);
    assert_eq!(usage.try_get::<i64>("", "cache_read_tokens").unwrap(), 40);
    assert_eq!(usage.try_get::<i64>("", "output_tokens").unwrap(), 25);
    assert_eq!(usage.try_get::<i64>("", "reasoning_tokens").unwrap(), 5);
    assert_eq!(
        usage.try_get::<String>("", "resolved_model").unwrap(),
        "gemini-3.8-flash-n"
    );
    assert_eq!(
        usage.try_get::<String>("", "protocol").unwrap(),
        "google_code_assist"
    );
    let row = database
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM gateway_requests",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.try_get::<i64>("", "n").unwrap(),
        1,
        "account/quota calls must not be captured as model requests"
    );
    backend.abort();
}
