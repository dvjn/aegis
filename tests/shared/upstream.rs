use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
const RETAINED_BODY_LIMIT: usize = 8 * 1024 * 1024;
const UPSTREAM_BODY_LIMIT: usize = 64 * 1024 * 1024;
const FRAME_CACHE_ENTRIES: usize = 32;

type FrameCache = HashMap<(usize, usize, bool), Bytes>;

#[derive(Clone)]
pub struct Upstream {
    base_url: String,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    received_count: Arc<AtomicUsize>,
    active_streams: Arc<AtomicUsize>,
    barriers: Arc<Mutex<HashMap<String, Arc<tokio::sync::Barrier>>>>,
    frames: Arc<Mutex<FrameCache>>,
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
            received_count: Arc::new(AtomicUsize::new(0)),
            active_streams: Arc::new(AtomicUsize::new(0)),
            barriers: Arc::new(Mutex::new(HashMap::new())),
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

    pub fn received_count(&self) -> usize {
        self.received_count.load(Ordering::Relaxed)
    }

    pub fn active_streams(&self) -> usize {
        self.active_streams.load(Ordering::Relaxed)
    }

    pub async fn wait_until_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.active_streams() == 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.active_streams() == 0
    }

    pub fn last_body(&self) -> String {
        self.received()
            .pop()
            .expect("fake upstream recorded no requests")
            .body
    }

    pub fn reset(&self) {
        self.received.lock().expect("upstream lock").clear();
        self.received_count.store(0, Ordering::Relaxed);
        self.barriers.lock().expect("barrier lock").clear();
    }

    fn record(&self, uri: &Uri, headers: &HeaderMap, body: &Bytes) {
        self.received_count.fetch_add(1, Ordering::Relaxed);
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

    async fn wait_at_barrier(&self, knobs: &Knobs) {
        let Some(id) = &knobs.barrier else {
            return;
        };
        let barrier = self
            .barriers
            .lock()
            .expect("barrier lock")
            .entry(id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Barrier::new(knobs.participants.max(1))))
            .clone();
        barrier.wait().await;
    }

    fn delta_frame(&self, index: usize, size: usize, openai: bool) -> Bytes {
        let key = (index, size, openai);
        if let Some(cached) = self.frames.lock().expect("frame lock").get(&key) {
            return cached.clone();
        }
        let frame = if openai {
            event_frame(
                "response.output_text.delta",
                &json!({
                    "type":"response.output_text.delta",
                    "item_id":"msg_load",
                    "output_index":0,
                    "content_index":0,
                    "sequence_number":index,
                    "delta":"x".repeat(size)
                }),
            )
        } else {
            event_frame(
                "content_block_delta",
                &json!({
                    "type":"content_block_delta",
                    "index":0,
                    "delta":{"type":"text_delta","text":"x".repeat(size)}
                }),
            )
        };
        let mut cache = self.frames.lock().expect("frame lock");
        if cache.len() < FRAME_CACHE_ENTRIES {
            cache.insert(key, frame.clone());
        }
        frame
    }

    fn stream_sse(&self, knobs: Knobs, openai: bool, echoed: String) -> Response {
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let upstream = self.clone();
        self.active_streams.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _active = ActiveStream::new(Arc::clone(&upstream.active_streams));
            if !openai {
                let start = event_frame(
                    "content_block_start",
                    &json!({
                        "type":"content_block_start",
                        "index":0,
                        "content_block":{"type":"text","text":""}
                    }),
                );
                if sender.send(Ok(start)).await.is_err() {
                    return;
                }
            }

            let abort_after = knobs.abort.then_some(knobs.chunks / 2);
            let base_size = knobs.bytes / knobs.chunks;
            let remainder = knobs.bytes % knobs.chunks;
            for index in 0..knobs.chunks {
                if abort_after.is_some_and(|limit| index >= limit) {
                    let _ = sender
                        .send(Err(std::io::Error::other("upstream aborted mid-stream")))
                        .await;
                    return;
                }
                let size = base_size + usize::from(index < remainder);
                let frame = upstream.delta_frame(index, size, openai);
                if sender.send(Ok(frame)).await.is_err() {
                    return;
                }
                if knobs.stall_after.is_some_and(|limit| index + 1 >= limit) {
                    sender.closed().await;
                    return;
                }
                if !knobs.delay.is_zero() {
                    tokio::time::sleep(knobs.delay).await;
                }
            }

            if !echoed.is_empty() {
                let frame = if openai {
                    event_frame(
                        "response.output_text.delta",
                        &json!({
                            "type":"response.output_text.delta",
                            "item_id":"msg_load",
                            "output_index":0,
                            "content_index":0,
                            "sequence_number":knobs.chunks,
                            "delta":echoed
                        }),
                    )
                } else {
                    event_frame(
                        "content_block_delta",
                        &json!({
                            "type":"content_block_delta",
                            "index":0,
                            "delta":{"type":"text_delta","text":echoed}
                        }),
                    )
                };
                if sender.send(Ok(frame)).await.is_err() {
                    return;
                }
            }
            if openai {
                if knobs.usage {
                    let completed = event_frame(
                        "response.completed",
                        &json!({
                            "type":"response.completed",
                            "sequence_number":knobs.chunks + 1,
                            "response":{"id":"resp_load","status":"completed","usage":usage(true)}
                        }),
                    );
                    if sender.send(Ok(completed)).await.is_err() {
                        return;
                    }
                }
            } else {
                if !knobs.no_stop {
                    let stop = event_frame(
                        "content_block_stop",
                        &json!({"type":"content_block_stop","index":0}),
                    );
                    if sender.send(Ok(stop)).await.is_err() {
                        return;
                    }
                }
                if knobs.usage {
                    let usage = event_frame(
                        "message_delta",
                        &json!({"type":"message_delta","usage":usage(false)}),
                    );
                    if sender.send(Ok(usage)).await.is_err() {
                        return;
                    }
                }
            }
            let _ = sender
                .send(Ok(Bytes::from_static(b"data: [DONE]\n\n")))
                .await;
        });

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(ReceiverStream::new(receiver)))
            .expect("sse response")
    }
}

struct ActiveStream {
    count: Arc<AtomicUsize>,
}

impl ActiveStream {
    fn new(count: Arc<AtomicUsize>) -> Self {
        Self { count }
    }
}

impl Drop for ActiveStream {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

struct Knobs {
    mode: String,
    bytes: usize,
    chunks: usize,
    delay: Duration,
    release_delay: Duration,
    echo: bool,
    status: u16,
    abort: bool,
    stall_after: Option<usize>,
    no_stop: bool,
    usage: bool,
    barrier: Option<String>,
    participants: usize,
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
            release_delay: Duration::from_millis(number("release_ms", 0) as u64),
            echo: flag("echo"),
            status: number("status", 200) as u16,
            abort: flag("abort"),
            stall_after: query
                .get("stall_after")
                .and_then(|value| value.parse().ok()),
            no_stop: flag("no_stop"),
            usage: query.get("usage").map(String::as_str) != Some("0"),
            barrier: query.get("barrier").cloned(),
            participants: number("participants", 1),
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
    upstream.wait_at_barrier(&knobs).await;
    if !knobs.release_delay.is_zero() {
        tokio::time::sleep(knobs.release_delay).await;
    }
    let openai = uri.path().contains("codex");
    let echoed = if knobs.echo {
        String::from_utf8_lossy(&body).into_owned()
    } else {
        String::new()
    };

    if knobs.status != 200 {
        let document = json!({
            "type":"error",
            "error":{"message":"x".repeat(knobs.bytes)}
        });
        return (
            StatusCode::from_u16(knobs.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            axum::Json(document),
        )
            .into_response();
    }
    if knobs.mode == "json" {
        return json_response(&knobs, openai, &echoed);
    }
    upstream.stream_sse(knobs, openai, echoed)
}

fn json_response(knobs: &Knobs, openai: bool, echoed: &str) -> Response {
    let document = json!({
        "id":"resp_fake",
        "model":"fake-model",
        "content":"x".repeat(knobs.bytes) + echoed,
        "usage":usage(openai),
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

fn usage(openai: bool) -> Value {
    if openai {
        json!({
            "input_tokens":1000,
            "output_tokens":500,
            "input_tokens_details":{"cached_tokens":200},
            "output_tokens_details":{"reasoning_tokens":50},
        })
    } else {
        json!({
            "input_tokens":1000,
            "output_tokens":500,
            "cache_read_input_tokens":200,
            "cache_creation_input_tokens":100,
        })
    }
}
