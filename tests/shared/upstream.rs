use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;

const RETAINED_REQUEST_LIMIT: usize = 64;
const RETAINED_BODY_LIMIT: usize = 65536;
const UPSTREAM_BODY_LIMIT: usize = 64 * 1024 * 1024;

const FRAME_CACHE_ENTRIES: usize = 32;

#[derive(Clone)]
pub struct Upstream {
    base_url: String,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    frames: Arc<Mutex<HashMap<(usize, usize), Bytes>>>,
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Upstream {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake upstream port");
        Self::serve(listener)
    }

    pub fn serve(listener: TcpListener) -> Self {
        let address = listener.local_addr().expect("fake upstream address");
        let upstream = Self {
            base_url: format!("http://{address}"),
            received: Arc::new(Mutex::new(Vec::new())),
            frames: Arc::new(Mutex::new(HashMap::new())),
        };
        let router = Router::new()
            .fallback(any(handle))
            .layer(DefaultBodyLimit::max(UPSTREAM_BODY_LIMIT))
            .with_state(upstream.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        upstream
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn received(&self) -> Vec<RecordedRequest> {
        self.received.lock().expect("upstream lock").clone()
    }

    pub fn last_body(&self) -> String {
        self.received()
            .pop()
            .expect("fake upstream recorded no requests")
            .body
    }

    pub fn reset(&self) {
        self.received.lock().expect("upstream lock").clear();
    }

    fn record(&self, uri: &Uri, headers: &HeaderMap, body: &Bytes) {
        let truncated = &body[..body.len().min(RETAINED_BODY_LIMIT)];
        let record = RecordedRequest {
            path: uri.to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_string(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            body: String::from_utf8_lossy(truncated).into_owned(),
        };
        let mut retained = self.received.lock().expect("upstream lock");
        if retained.len() >= RETAINED_REQUEST_LIMIT {
            retained.remove(0);
        }
        retained.push(record);
    }

    fn delta_frame(&self, index: usize, size: usize) -> Bytes {
        let key = (index, size);
        if let Some(cached) = self.frames.lock().expect("frame lock").get(&key) {
            return cached.clone();
        }
        let frame = event_frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "text_delta", "text": "x".repeat(size) },
            }),
        );
        let mut cache = self.frames.lock().expect("frame lock");
        if cache.len() < FRAME_CACHE_ENTRIES {
            cache.insert(key, frame.clone());
        }
        frame
    }

    fn stream_sse(&self, knobs: Knobs, is_codex: bool, echoed: String) -> Response {
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let upstream = self.clone();
        tokio::spawn(async move {
            for index in 0..knobs.blocks {
                let frame = event_frame(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": { "type": "text", "text": "" },
                    }),
                );
                if sender.send(Ok(frame)).await.is_err() {
                    return;
                }
            }

            let per_chunk = (knobs.bytes / knobs.chunks).max(1);
            let abort_after = knobs.abort.then_some(knobs.chunks / 2);
            for written in 0..knobs.chunks {
                if abort_after.is_some_and(|limit| written >= limit) {
                    // hyper drops the connection without a terminating chunk
                    // when the body stream errors.
                    let _ = sender
                        .send(Err(std::io::Error::other("upstream aborted mid-stream")))
                        .await;
                    return;
                }
                let frame = upstream.delta_frame(written % knobs.blocks, per_chunk);
                if sender.send(Ok(frame)).await.is_err() {
                    return;
                }
                if !knobs.delay.is_zero() {
                    tokio::time::sleep(knobs.delay).await;
                }
            }

            let mut tail = Vec::new();
            if !knobs.no_stop {
                if !echoed.is_empty() {
                    tail.push(event_frame(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": { "type": "text_delta", "text": echoed },
                        }),
                    ));
                }
                for index in 0..knobs.blocks {
                    tail.push(event_frame(
                        "content_block_stop",
                        &json!({ "type": "content_block_stop", "index": index }),
                    ));
                }
            }
            tail.push(event_frame(
                "message_delta",
                &json!({ "type": "message_delta", "usage": usage(is_codex) }),
            ));
            tail.push(Bytes::from_static(b"data: [DONE]\n\n"));
            for frame in tail {
                if sender.send(Ok(frame)).await.is_err() {
                    return;
                }
            }
        });

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(ReceiverStream::new(receiver)))
            .expect("sse response")
    }
}

struct Knobs {
    mode: String,
    bytes: usize,
    chunks: usize,
    delay: Duration,
    echo: bool,
    status: u16,
    abort: bool,
    blocks: usize,
    no_stop: bool,
}

impl Knobs {
    fn from_query(query: &HashMap<String, String>) -> Self {
        let flag = |name: &str| query.get(name).map(String::as_str) == Some("1");
        let number = |name: &str, fallback: usize| {
            query
                .get(name)
                .and_then(|value| value.parse().ok())
                .unwrap_or(fallback)
        };
        Self {
            mode: query.get("mode").cloned().unwrap_or_else(|| "sse".into()),
            bytes: number("bytes", 65536),
            chunks: number("chunks", 16).max(1),
            delay: Duration::from_millis(number("delay_ms", 0) as u64),
            echo: flag("echo"),
            status: number("status", 200) as u16,
            abort: flag("abort"),
            blocks: number("blocks", 1).max(1),
            no_stop: flag("no_stop"),
        }
    }
}

async fn handle(
    State(upstream): State<Upstream>,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    upstream.record(&uri, &headers, &body);

    let knobs = Knobs::from_query(&query);
    let is_codex = uri.path().contains("codex");
    let echoed = if knobs.echo {
        String::from_utf8_lossy(&body).into_owned()
    } else {
        String::new()
    };

    if knobs.status != 200 {
        let document = json!({
            "type": "error",
            "error": { "message": "x".repeat(knobs.bytes) },
        });
        return (
            StatusCode::from_u16(knobs.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            axum::Json(document),
        )
            .into_response();
    }

    if knobs.mode == "json" {
        return json_response(&knobs, is_codex, &echoed);
    }

    upstream.stream_sse(knobs, is_codex, echoed)
}

fn json_response(knobs: &Knobs, is_codex: bool, echoed: &str) -> Response {
    let document = json!({
        "id": "resp_fake",
        "model": "fake-model",
        "content": "x".repeat(knobs.bytes) + echoed,
        "usage": usage(is_codex),
    })
    .to_string();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, document.len())
        .body(Body::from(document))
        .expect("json response")
}

fn event_frame(event: &str, payload: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {payload}\n\n"))
}

fn usage(is_codex: bool) -> Value {
    if is_codex {
        json!({
            "input_tokens": 1000,
            "output_tokens": 500,
            "input_tokens_details": { "cached_tokens": 200 },
            "output_tokens_details": { "reasoning_tokens": 50 },
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
