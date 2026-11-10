use super::*;
use axum::body::to_bytes;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[tokio::test]
async fn forwards_credentials_query_and_streams_without_buffering_or_redirects() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let (observed_tx, mut observed_rx) = mpsc::channel(4);
    let (frames_tx, frames_rx) = mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(4);
    let frames = Arc::new(tokio::sync::Mutex::new(Some(frames_rx)));
    let router = Router::new().fallback(any(move |request: Request| {
        let observed = observed_tx.clone();
        let frames = frames.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = to_bytes(body, 1024).await.unwrap();
            observed.send((parts, bytes)).await.unwrap();
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(ReceiverStream::new(
                    frames.lock().await.take().unwrap(),
                )))
                .unwrap()
        }
    }));
    let backend = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let relay = Relay::start(
        &server,
        "work",
        super::super::auth::TokenSource::Fixed("aegis-oauth".into()),
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let blocked = client
        .post(format!("{}/responses", relay.url))
        .header("origin", "https://untrusted.example")
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    let request = client
        .post(format!("{}/responses?stream=true", relay.url))
        .header("authorization", "Bearer provider-oauth")
        .header("x-aegis-api-key", "must-be-replaced")
        .header("connection", "x-remove")
        .header("x-remove", "private")
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter([
            Ok::<_, std::io::Error>("hello"),
        ])));
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    let (parts, body) = observed_rx.recv().await.unwrap();
    assert_eq!(
        parts.uri.to_string(),
        "/providers/work/responses?stream=true"
    );
    assert_eq!(parts.headers["authorization"], "Bearer provider-oauth");
    assert!(!parts.headers.contains_key("x-aegis-api-key"));
    assert_eq!(parts.headers["x-aegis-access-token"], "aegis-oauth");
    assert!(!parts.headers.contains_key("x-remove"));
    assert_eq!(&body[..], b"hello");
    frames_tx.send(Ok("data: first\n\n".into())).await.unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    let mut stream = response.bytes_stream();
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(&chunk[..], b"data: first\n\n");
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(2), frames_tx.closed())
        .await
        .unwrap();
    let address = relay.url.clone();
    drop(relay);
    assert!(
        client
            .get(format!("{address}/responses"))
            .send()
            .await
            .is_err()
    );
    backend.abort();
}
