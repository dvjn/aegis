//! Bedrock and Vertex providers end to end against a stand-in upstream.
use super::*;
use crate::{
    config::{GuardrailsConfig, GuardrailsMode, ProviderKind, RegexGuardrailConfig},
    eventstream::{
        self,
        tests::{chunk, converse},
    },
    migration::Migrator,
};
use axum::{Router, body::to_bytes, routing::any};
use base64::{Engine, engine::general_purpose::STANDARD};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use sea_orm_migration::MigratorTrait;
use serde_json::Value;
use std::collections::BTreeMap;
use tower::ServiceExt;

const SECRET: &str = "unit-secret-123";

struct Upstream {
    origin: String,
    received: mpsc::Receiver<(String, Value)>,
}

/// Records each request and answers with whatever `reply` builds from the
/// path and the masked text it received.
async fn upstream(reply: fn(&str, &str) -> Response) -> Upstream {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (sender, received) = mpsc::channel(8);
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |request: Request| {
                let sender = sender.clone();
                async move {
                    assert_eq!(
                        request.headers()[header::AUTHORIZATION],
                        "Bearer cloud-token"
                    );
                    assert!(!request.headers().contains_key("x-aegis-api-key"));
                    let path = request.uri().to_string();
                    let body = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
                    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let masked = masked_text(&body);
                    sender.send((path.clone(), body)).await.unwrap();
                    reply(&path, &masked)
                }
            })),
        )
        .await
        .unwrap();
    });
    Upstream { origin, received }
}

/// The first user text of an Anthropic, Converse or Gemini request.
fn masked_text(body: &Value) -> String {
    [
        &body["messages"][0]["content"][0]["text"],
        &body["messages"][0]["content"],
        &body["contents"][0]["parts"][0]["text"],
    ]
    .into_iter()
    .find_map(|value| value.as_str())
    .unwrap_or_default()
    .to_owned()
}

async fn gateway(kind: ProviderKind) -> (Router, DatabaseConnection, String) {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&database, None).await.unwrap();
    let user = uuid::Uuid::now_v7();
    database.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','cloud@example.com','cloud@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
    let keys = KeyStore::new(database.clone());
    let (_, key) = keys.create(user, "cloud", &["cloud".into()]).await.unwrap();
    let guardrails = GuardrailsConfig {
        enabled: true,
        mode: GuardrailsMode::Mask,
        regex: RegexGuardrailConfig {
            enabled: true,
            detectors: BTreeMap::from([("test_secret".into(), SECRET.into())]),
        },
        ..Default::default()
    };
    let gateway = Gateway::new(
        SqliteSink::new(database.clone()),
        keys,
        guardrails,
        [7; 32],
        vec![ProviderConfig {
            id: "cloud".into(),
            kind,
        }],
        1024 * 1024,
    )
    .unwrap();
    (router(gateway), database, key)
}

async fn send(app: &Router, key: &str, path: &str, body: Value) -> (StatusCode, Bytes) {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/providers/cloud{path}"))
                .header("x-aegis-api-key", key)
                .header(header::AUTHORIZATION, "Bearer cloud-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
    )
}

/// The recorded request for `path`, once its usage is stored.
async fn recorded(database: &DatabaseConnection, path: &str) -> sea_orm::QueryResult {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = database
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "SELECT u.*, r.protocol, r.requested_model, r.resolved_model FROM gateway_usage u
                     JOIN gateway_requests r ON r.id = u.request_id WHERE r.endpoint LIKE ?",
                    [format!("%{path}").into()],
                ))
                .await
                .unwrap();
            if let Some(row) = row {
                break row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

fn event_stream(frames: Vec<Vec<u8>>) -> Response {
    (
        [(header::CONTENT_TYPE, eventstream::CONTENT_TYPE)],
        frames.concat(),
    )
        .into_response()
}

/// Every text delta in an event stream response, decoded and joined.
fn streamed_text(body: &[u8]) -> String {
    let mut rest = body;
    let mut text = String::new();
    while let eventstream::Decoded::Message(message) = eventstream::decode(rest) {
        let mut payload: Value = serde_json::from_slice(message.payload).unwrap();
        if let Some(encoded) = payload.get("bytes").and_then(Value::as_str) {
            payload = serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
        }
        text.push_str(payload["delta"]["text"].as_str().unwrap_or_default());
        let length = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        rest = &rest[length..];
    }
    assert!(rest.is_empty());
    text
}

fn bedrock_reply(path: &str, masked: &str) -> Response {
    let (head, tail) = masked.split_at(masked.len() / 2);
    if path.ends_with("/invoke-with-response-stream") {
        let delta = |text: &str| {
            chunk(
                &json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            )
        };
        return event_stream(vec![
            chunk(
                &json!({"type":"message_start","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"cache_read_input_tokens":4}}}),
            ),
            delta(head),
            delta(tail),
            chunk(&json!({"type":"content_block_stop","index":0})),
            chunk(
                &json!({"type":"message_delta","usage":{"input_tokens":10,"cache_read_input_tokens":4,"output_tokens":7}}),
            ),
        ]);
    }
    if path.ends_with("/converse-stream") {
        let delta = |text: &str| {
            converse(
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"text":text}}),
            )
        };
        return event_stream(vec![
            converse("messageStart", &json!({"role":"assistant"})),
            delta(head),
            delta(tail),
            converse("contentBlockStop", &json!({"contentBlockIndex":0})),
            converse(
                "metadata",
                &json!({"usage":{"inputTokens":20,"outputTokens":3,"cacheReadInputTokens":6,"totalTokens":29}}),
            ),
        ]);
    }
    if path.ends_with("/chat/completions") {
        let chunk = |text: &str| json!({"object":"chat.completion.chunk","model":"zai.glm-4.7","choices":[{"index":0,"delta":{"content":text}}]});
        let events = [
            chunk(head),
            chunk(tail),
            json!({"object":"chat.completion.chunk","model":"zai.glm-4.7","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            json!({"object":"chat.completion.chunk","model":"zai.glm-4.7","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3}}),
        ];
        let body: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .chain(["data: [DONE]\n\n".to_owned()])
            .collect();
        return ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response();
    }
    if path.ends_with("/count-tokens") {
        return axum::Json(json!({"inputTokens": 5, "echo": masked})).into_response();
    }
    axum::Json(json!({"usage":{"input_tokens":30,"output_tokens":2}})).into_response()
}

#[tokio::test]
async fn bedrock_serves_claude_code_pi_and_codex_paths() {
    let mut upstream = upstream(bedrock_reply).await;
    let (app, database, key) = gateway(ProviderKind::Bedrock {
        base_url: upstream.origin.clone(),
    })
    .await;

    // Claude Code: InvokeModel, model in the path, Anthropic events in frames.
    let (status, body) = send(
        &app,
        &key,
        "/model/eu.anthropic.claude-sonnet-4-6-v1%3A0/invoke-with-response-stream",
        json!({"anthropic_version":"bedrock-2023-05-31","messages":[{"role":"user","content":[{"type":"text","text":SECRET}]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (path, sent) = upstream.received.recv().await.unwrap();
    assert!(path.ends_with("/invoke-with-response-stream"));
    assert!(
        masked_text(&sent).starts_with("AEGIS_MASKED_"),
        "the secret is masked upstream"
    );
    assert_eq!(
        streamed_text(&body),
        SECRET,
        "and restored in the re-framed stream"
    );
    let row = recorded(&database, "/invoke-with-response-stream").await;
    assert_eq!(
        row.try_get::<String>("", "protocol").unwrap(),
        "anthropic_messages"
    );
    assert_eq!(
        row.try_get::<String>("", "requested_model").unwrap(),
        "eu.anthropic.claude-sonnet-4-6-v1:0"
    );
    assert_eq!(
        row.try_get::<String>("", "resolved_model").unwrap(),
        "claude-sonnet-4-6"
    );
    assert_eq!(row.try_get::<i64>("", "output_tokens").unwrap(), 7);
    assert_eq!(row.try_get::<i64>("", "cache_read_tokens").unwrap(), 4);

    // pi: Converse, its own event names and usage fields.
    let (status, body) = send(
        &app,
        &key,
        "/model/eu.anthropic.claude-sonnet-4-6-v1%3A0/converse-stream",
        json!({"messages":[{"role":"user","content":[{"text":SECRET}]}],"toolConfig":{"tools":[]}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, sent) = upstream.received.recv().await.unwrap();
    assert!(masked_text(&sent).starts_with("AEGIS_MASKED_"));
    assert_eq!(streamed_text(&body), SECRET);
    let row = recorded(&database, "/converse-stream").await;
    assert_eq!(
        row.try_get::<String>("", "protocol").unwrap(),
        "bedrock_converse"
    );
    assert_eq!(
        row.try_get::<String>("", "resolved_model").unwrap(),
        "eu.anthropic.claude-sonnet-4-6-v1:0",
        "Converse names no model, so the requested one stands in"
    );
    assert_eq!(row.try_get::<i64>("", "input_tokens").unwrap(), 20);
    assert_eq!(row.try_get::<i64>("", "cache_read_tokens").unwrap(), 6);
    assert_eq!(row.try_get::<i64>("", "output_tokens").unwrap(), 3);

    // Codex: the OpenAI-compatible Responses endpoint.
    let (status, _) = send(
        &app,
        &key,
        "/openai/v1/responses",
        json!({"model":"openai.gpt-5.6-sol","input":"hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (path, _) = upstream.received.recv().await.unwrap();
    assert_eq!(path, "/openai/v1/responses");
    let row = recorded(&database, "/openai/v1/responses").await;
    assert_eq!(
        row.try_get::<String>("", "protocol").unwrap(),
        "openai_responses"
    );
    assert_eq!(
        row.try_get::<String>("", "requested_model").unwrap(),
        "openai.gpt-5.6-sol"
    );
}

#[tokio::test]
async fn bedrock_partner_models_are_recorded_and_unknown_calls_are_still_masked() {
    let mut upstream = upstream(bedrock_reply).await;
    let (app, database, key) = gateway(ProviderKind::Bedrock {
        base_url: upstream.origin.clone(),
    })
    .await;

    // Partner models: Chat Completions under `/v1`, recorded and restored.
    let (status, body) = send(
        &app,
        &key,
        "/v1/chat/completions",
        json!({"model":"zai.glm-4.7","stream":true,"messages":[{"role":"user","content":SECRET}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (path, sent) = upstream.received.recv().await.unwrap();
    assert_eq!(path, "/v1/chat/completions");
    assert!(masked_text(&sent).starts_with("AEGIS_MASKED_"));
    assert!(String::from_utf8(body.to_vec()).unwrap().contains(SECRET));
    let row = recorded(&database, "/v1/chat/completions").await;
    assert_eq!(
        row.try_get::<String>("", "protocol").unwrap(),
        "openai_chat_completions"
    );
    assert_eq!(row.try_get::<i64>("", "input_tokens").unwrap(), 11);
    assert_eq!(row.try_get::<i64>("", "output_tokens").unwrap(), 3);

    // A call Aegis cannot read usage from is masked all the same, and its
    // response restored, but it is not recorded.
    let (status, body) = send(
        &app,
        &key,
        "/model/eu.anthropic.claude-sonnet-4-6-v1%3A0/count-tokens",
        json!({"messages":[{"role":"user","content":[{"type":"text","text":SECRET}]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, sent) = upstream.received.recv().await.unwrap();
    assert!(masked_text(&sent).starts_with("AEGIS_MASKED_"));
    assert!(String::from_utf8(body.to_vec()).unwrap().contains(SECRET));

    // The bypass list: a request without a body goes through untouched.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/providers/cloud/v1/models?filter=unit-secret-123")
                .header("x-aegis-api-key", &key)
                .header(header::AUTHORIZATION, "Bearer cloud-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (path, _) = upstream.received.recv().await.unwrap();
    assert_eq!(path, "/v1/models?filter=unit-secret-123");

    let recorded: i64 = database
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT COUNT(*) n FROM gateway_requests",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap();
    assert_eq!(recorded, 1, "only the Chat Completions call is recorded");
}

fn vertex_reply(path: &str, masked: &str) -> Response {
    let (head, tail) = masked.split_at(masked.len() / 2);
    let events: Vec<Value> = if path.contains(":streamRawPredict") {
        vec![
            json!({"type":"message_start","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":12}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":head}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":tail}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","usage":{"input_tokens":12,"output_tokens":4}}),
        ]
    } else {
        vec![
            json!({"modelVersion":"gemini-3-pro","candidates":[{"index":0,"content":{"parts":[{"text":head}]}}]}),
            json!({"candidates":[{"index":0,"content":{"parts":[{"text":tail}]}}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":2}}),
        ]
    };
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

#[tokio::test]
async fn vertex_serves_claude_code_and_gemini_paths() {
    let mut upstream = upstream(vertex_reply).await;
    let (app, database, key) = gateway(ProviderKind::Vertex {
        base_url: upstream.origin.clone(),
    })
    .await;

    // Claude Code: rawPredict, the version prefix added when the path lacks it.
    let (status, body) = send(
        &app,
        &key,
        "/projects/p/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-6:streamRawPredict",
        json!({"anthropic_version":"vertex-2023-10-16","messages":[{"role":"user","content":SECRET}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (path, sent) = upstream.received.recv().await.unwrap();
    assert_eq!(
        path,
        "/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-6:streamRawPredict"
    );
    assert!(masked_text(&sent).starts_with("AEGIS_MASKED_"));
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .contains("unit-secret")
    );
    let row = recorded(&database, ":streamRawPredict").await;
    assert_eq!(
        row.try_get::<String>("", "protocol").unwrap(),
        "anthropic_messages"
    );
    assert_eq!(
        row.try_get::<String>("", "requested_model").unwrap(),
        "claude-sonnet-4-6"
    );
    assert_eq!(row.try_get::<i64>("", "output_tokens").unwrap(), 4);

    // pi: Gemini through the publisher path.
    let (status, body) = send(
        &app,
        &key,
        "/v1/projects/p/locations/global/publishers/google/models/gemini-3-pro:streamGenerateContent?alt=sse",
        json!({"contents":[{"role":"user","parts":[{"text":SECRET}]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, sent) = upstream.received.recv().await.unwrap();
    assert!(masked_text(&sent).starts_with("AEGIS_MASKED_"));
    assert!(String::from_utf8(body.to_vec()).unwrap().contains(SECRET));
    let row = recorded(&database, ":streamGenerateContent?alt=sse").await;
    assert_eq!(
        row.try_get::<String>("", "protocol").unwrap(),
        "google_gemini"
    );
    assert_eq!(
        row.try_get::<String>("", "requested_model").unwrap(),
        "gemini-3-pro"
    );
    assert_eq!(row.try_get::<i64>("", "input_tokens").unwrap(), 9);
}
