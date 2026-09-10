//! Fake Claude/Codex upstream. The response shape is chosen per request through
//! query parameters, so one instance serves every case:
//!
//!     mode=sse     Anthropic-style `event:`/`data:` frames, then a final
//!                  message_delta carrying usage.
//!     mode=json    One plain JSON document with no SSE framing, sent with an
//!                  explicit content-length.
//!     bytes=N      Approximate filler bytes in the response.
//!     chunks=N     Number of writes the streamed body is split across.
//!     delay_ms=N   Sleep between writes, to hold a stream open.
//!     echo=1       Include the received request text in the response, so
//!                  placeholders travel back and the restore path runs.
//!
//! Usage counts are always present so the gateway records a complete row. Field
//! paths follow src/providers.rs: Anthropic reads usage.input_tokens and
//! friends, Codex reads usage.input_tokens_details.cached_tokens.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{Uri, header},
    response::Response,
    routing::any,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Clone, Default)]
struct Recorder {
    bodies: Arc<Mutex<Vec<String>>>,
}

pub struct FakeUpstream {
    base_url: String,
    recorder: Recorder,
}

impl FakeUpstream {
    pub async fn start() -> Self {
        let recorder = Recorder::default();
        let router = Router::new()
            .fallback(any(respond))
            .with_state(recorder.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            base_url: format!("http://{address}"),
            recorder,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn received(&self) -> Vec<String> {
        self.recorder.bodies.lock().unwrap().clone()
    }

    pub fn last_body(&self) -> String {
        self.received()
            .pop()
            .expect("fake upstream recorded no requests")
    }

    pub fn reset(&self) {
        self.recorder.bodies.lock().unwrap().clear();
    }
}

struct ResponseShape {
    streamed: bool,
    filler_bytes: usize,
    chunks: usize,
    delay_ms: u64,
    echo: bool,
    codex: bool,
}

impl ResponseShape {
    fn from_request(uri: &Uri) -> Self {
        let query = uri.query().unwrap_or_default();
        let parameter = |name: &str| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == name).then(|| value.to_owned())
            })
        };
        let number = |name: &str, fallback: usize| {
            parameter(name)
                .and_then(|value| value.parse().ok())
                .unwrap_or(fallback)
        };

        Self {
            streamed: parameter("mode").unwrap_or_else(|| "sse".into()) != "json",
            filler_bytes: number("bytes", 65536),
            chunks: number("chunks", 16).max(1),
            delay_ms: number("delay_ms", 0) as u64,
            echo: parameter("echo").as_deref() == Some("1"),
            codex: uri.path().contains("codex"),
        }
    }

    fn usage(&self) -> Value {
        if self.codex {
            json!({
                "input_tokens": 1000,
                "output_tokens": 500,
                "input_tokens_details": {"cached_tokens": 200},
                "output_tokens_details": {"reasoning_tokens": 50},
            })
        } else {
            json!({
                "input_tokens": 1000,
                "output_tokens": 500,
                "cache_read_input_tokens": 200,
                "cache_creation_input_tokens": 100,
            })
        }
    }
}

async fn respond(State(recorder): State<Recorder>, uri: Uri, body: Bytes) -> Response {
    let received = String::from_utf8_lossy(&body).into_owned();
    recorder.bodies.lock().unwrap().push(received.clone());

    let shape = ResponseShape::from_request(&uri);
    let echoed = if shape.echo { received } else { String::new() };
    if shape.streamed {
        streamed_response(shape, echoed)
    } else {
        json_response(shape, echoed)
    }
}

fn json_response(shape: ResponseShape, echoed: String) -> Response {
    let document = json!({
        "id": "resp_fake",
        "model": "fake-model",
        "content": "x".repeat(shape.filler_bytes) + &echoed,
        "usage": shape.usage(),
    })
    .to_string();

    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, document.len())
        .body(Body::from(document))
        .unwrap()
}

fn frame(event: &str, payload: Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

fn text_delta(text: &str) -> String {
    frame(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": text},
        }),
    )
}

/// A body written incrementally, so `chunks` and `delay_ms` reach the wire as
/// separate frames instead of one flush.
fn streamed_response(shape: ResponseShape, echoed: String) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(1);

    tokio::spawn(async move {
        let mut frames = vec![frame(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        )];
        let per_chunk = (shape.filler_bytes / shape.chunks).max(1);
        frames.extend((0..shape.chunks).map(|_| text_delta(&"x".repeat(per_chunk))));
        if !echoed.is_empty() {
            frames.push(text_delta(&echoed));
        }
        frames.push(frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ));
        frames.push(frame(
            "message_delta",
            json!({"type": "message_delta", "usage": shape.usage()}),
        ));
        frames.push("data: [DONE]\n\n".to_owned());

        for chunk in frames {
            if sender.send(Ok(chunk)).await.is_err() {
                return;
            }
            if shape.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(shape.delay_ms)).await;
            }
        }
    });

    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(ReceiverStream::new(receiver)))
        .unwrap()
}
