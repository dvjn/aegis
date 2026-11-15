//! AWS event stream framing (`application/vnd.amazon.eventstream`), which
//! Bedrock uses for streamed responses instead of SSE.
//!
//! A frame is a 12-byte prelude (total length, headers length, prelude CRC),
//! typed headers, a payload and a CRC of everything before it. Bedrock's
//! InvokeModel stream wraps each Anthropic event as base64 under `bytes`;
//! Converse sends each event's JSON as the payload and names it in the
//! `:event-type` header.
//!
//! The gateway reads these frames as SSE, so usage extraction and masked-value
//! restoration work on one representation, and writes restored events back as
//! frames.
use crate::policies::{restore::StreamRestorer, sse::FrameParser};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::collections::HashMap;

pub const CONTENT_TYPE: &str = "application/vnd.amazon.eventstream";

/// The SSE event name that marks an InvokeModel chunk, whose payload is
/// wrapped as base64 under `bytes`.
const CHUNK: &str = "chunk";
const PRELUDE: usize = 12;
const CHECKSUM: usize = 4;
/// Larger than any frame Bedrock sends; a bigger length means corruption.
const MAX_FRAME: usize = 24 * 1024 * 1024;

pub struct Message<'a> {
    headers: &'a [u8],
    pub payload: &'a [u8],
    raw: &'a [u8],
}

pub enum Decoded<'a> {
    Message(Message<'a>),
    Incomplete,
    Invalid,
}

pub fn decode(buffer: &[u8]) -> Decoded<'_> {
    if buffer.len() < PRELUDE {
        return Decoded::Incomplete;
    }
    let total = u32::from_be_bytes(buffer[0..4].try_into().expect("4 bytes")) as usize;
    let headers = u32::from_be_bytes(buffer[4..8].try_into().expect("4 bytes")) as usize;
    let prelude_crc = u32::from_be_bytes(buffer[8..12].try_into().expect("4 bytes"));
    if crc32fast::hash(&buffer[..8]) != prelude_crc
        || total > MAX_FRAME
        || total < PRELUDE + headers + CHECKSUM
    {
        return Decoded::Invalid;
    }
    if buffer.len() < total {
        return Decoded::Incomplete;
    }
    let message_crc =
        u32::from_be_bytes(buffer[total - CHECKSUM..total].try_into().expect("4 bytes"));
    if crc32fast::hash(&buffer[..total - CHECKSUM]) != message_crc {
        return Decoded::Invalid;
    }
    Decoded::Message(Message {
        headers: &buffer[PRELUDE..PRELUDE + headers],
        payload: &buffer[PRELUDE + headers..total - CHECKSUM],
        raw: &buffer[..total],
    })
}

pub fn encode(headers: &[u8], payload: &[u8]) -> Vec<u8> {
    let total = PRELUDE + headers.len() + payload.len() + CHECKSUM;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame.extend_from_slice(headers);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}

impl Message<'_> {
    /// A string-valued header, such as `:event-type` or `:message-type`.
    pub fn header(&self, name: &str) -> Option<&str> {
        let mut rest = self.headers;
        while let [length, tail @ ..] = rest {
            let length = *length as usize;
            let (key, tail) = tail.split_at_checked(length)?;
            let (&kind, tail) = tail.split_first()?;
            let size = match kind {
                0 | 1 => 0,
                2 => 1,
                3 => 2,
                4 => 4,
                5 | 8 => 8,
                9 => 16,
                6 | 7 => {
                    let (length, _) = tail.split_at_checked(2)?;
                    2 + u16::from_be_bytes(length.try_into().ok()?) as usize
                }
                _ => return None,
            };
            let (value, tail) = tail.split_at_checked(size)?;
            if kind == 7 && key == name.as_bytes() {
                return std::str::from_utf8(&value[2..]).ok();
            }
            rest = tail;
        }
        None
    }

    fn is_event(&self) -> bool {
        self.header(":message-type")
            .is_none_or(|kind| kind == "event")
    }

    /// The event as one SSE frame: InvokeModel chunks unwrapped from base64
    /// under the name `chunk`, Converse events under their own names.
    fn to_sse(&self) -> Option<(&str, Vec<u8>)> {
        let payload: Value = serde_json::from_slice(self.payload).ok()?;
        let (name, data) = match payload.get("bytes").and_then(Value::as_str) {
            Some(encoded) => (
                CHUNK,
                String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?,
            ),
            None => (
                self.header(":event-type")?,
                String::from_utf8(self.payload.to_vec()).ok()?,
            ),
        };
        Some((
            name,
            format!("event: {name}\ndata: {data}\n\n").into_bytes(),
        ))
    }
}

/// A captured event stream body as SSE, or `None` when the body is not an
/// event stream. A capture cut short keeps the frames before the cut.
pub fn to_sse(body: &[u8]) -> Option<Vec<u8>> {
    let mut rest = body;
    let mut sse = Vec::new();
    let mut frames = 0;
    while let Decoded::Message(message) = decode(rest) {
        frames += 1;
        if message.is_event()
            && let Some((_, event)) = message.to_sse()
        {
            sse.extend(event);
        }
        rest = &rest[message.raw.len()..];
    }
    (frames > 0).then_some(sse)
}

/// Restores masked values in an event stream by restoring its events as SSE
/// and writing each restored event back as a frame. Frames that are not
/// events, such as exceptions, pass through unchanged. After a frame that
/// does not decode, everything passes through unchanged.
pub struct Restorer {
    inner: StreamRestorer,
    buffer: Vec<u8>,
    restored: FrameParser,
    /// The raw headers last seen for each SSE event name, reused when the
    /// restored event is written back.
    headers: HashMap<String, Vec<u8>>,
    broken: bool,
}

impl Restorer {
    pub fn new(inner: StreamRestorer) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            restored: FrameParser::default(),
            headers: HashMap::new(),
            broken: false,
        }
    }

    pub fn rewrite_chunk(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.broken {
            return chunk.to_vec();
        }
        self.buffer.extend_from_slice(chunk);
        let mut output = Vec::new();
        let mut consumed = 0;
        loop {
            match decode(&self.buffer[consumed..]) {
                Decoded::Incomplete => break,
                Decoded::Invalid => {
                    self.broken = true;
                    output.extend_from_slice(&self.buffer[consumed..]);
                    consumed = self.buffer.len();
                    break;
                }
                Decoded::Message(message) => {
                    consumed += message.raw.len();
                    let event = message.is_event().then(|| message.to_sse()).flatten();
                    let Some((name, event)) = event else {
                        output.extend_from_slice(message.raw);
                        continue;
                    };
                    self.headers
                        .insert(name.to_owned(), message.headers.to_vec());
                    let restored = self.inner.rewrite_chunk(&event);
                    output.extend(self.encode(&restored));
                }
            }
        }
        self.buffer.drain(..consumed);
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if self.broken {
            return std::mem::take(&mut self.buffer);
        }
        let tail = self.inner.finish();
        let mut output = self.encode(&tail);
        // A body that was never framed, such as a JSON error, ends here.
        output.extend(self.restored.finish());
        output.extend(std::mem::take(&mut self.buffer));
        output
    }

    fn encode(&mut self, sse: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        for frame in self.restored.push(sse) {
            let (Some(name), Some(data)) = (frame.event(), frame.data()) else {
                continue;
            };
            let Some(headers) = self.headers.get(name) else {
                continue;
            };
            let payload = if name == CHUNK {
                serde_json::json!({ "bytes": STANDARD.encode(data.as_bytes()) }).to_string()
            } else {
                data
            };
            output.extend(encode(headers, payload.as_bytes()));
        }
        output
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::policies::Replacement;

    pub fn string_header(name: &str, value: &str) -> Vec<u8> {
        let mut header = vec![name.len() as u8];
        header.extend_from_slice(name.as_bytes());
        header.push(7);
        header.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header.extend_from_slice(value.as_bytes());
        header
    }

    fn event_headers(event_type: &str) -> Vec<u8> {
        [
            string_header(":event-type", event_type),
            string_header(":content-type", "application/json"),
            string_header(":message-type", "event"),
        ]
        .concat()
    }

    /// An InvokeModel stream frame carrying one Anthropic event.
    pub fn chunk(event: &Value) -> Vec<u8> {
        let payload = serde_json::json!({
            "bytes": STANDARD.encode(event.to_string()),
            "p": "abcdefgh",
        });
        encode(&event_headers("chunk"), payload.to_string().as_bytes())
    }

    /// A Converse stream frame.
    pub fn converse(event_type: &str, payload: &Value) -> Vec<u8> {
        encode(&event_headers(event_type), payload.to_string().as_bytes())
    }

    fn events(body: &[u8]) -> Vec<(String, Value)> {
        let mut rest = body;
        let mut events = Vec::new();
        while let Decoded::Message(message) = decode(rest) {
            let name = message.header(":event-type").unwrap().to_owned();
            let mut payload: Value = serde_json::from_slice(message.payload).unwrap();
            if let Some(encoded) = payload.get("bytes").and_then(Value::as_str) {
                payload = serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
            }
            events.push((name, payload));
            rest = &rest[message.raw.len()..];
        }
        assert!(rest.is_empty(), "every byte decodes as a frame");
        events
    }

    fn replacement() -> Vec<Replacement> {
        vec![Replacement {
            placeholder: "AEGIS_MASKED_TOKEN_abc_END".into(),
            original: "sk-real-secret".into(),
        }]
    }

    #[test]
    fn frames_round_trip_and_reject_corruption() {
        let frame = chunk(&serde_json::json!({"type": "message_stop"}));
        let Decoded::Message(message) = decode(&frame) else {
            panic!("frame decodes");
        };
        assert_eq!(message.header(":event-type"), Some("chunk"));
        assert_eq!(message.header(":missing"), None);
        assert!(matches!(
            decode(&frame[..frame.len() - 1]),
            Decoded::Incomplete
        ));
        let mut corrupt = frame.clone();
        let last = corrupt.len() - 6;
        corrupt[last] ^= 1;
        assert!(matches!(decode(&corrupt), Decoded::Invalid));
        assert!(to_sse(b"{\"not\":\"a stream\"}").is_none());
    }

    #[test]
    fn a_placeholder_split_across_invoke_chunks_is_restored() {
        let delta = |text: &str| {
            chunk(&serde_json::json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }))
        };
        let stream = [
            delta("key: AEGIS_MASKED_TO"),
            delta("KEN_abc_END done"),
            chunk(&serde_json::json!({"type": "content_block_stop", "index": 0})),
        ]
        .concat();
        let mut restorer = Restorer::new(StreamRestorer::new(replacement()));
        // Feed in uneven pieces, splitting frames too.
        let mut output = Vec::new();
        for piece in stream.chunks(7) {
            output.extend(restorer.rewrite_chunk(piece));
        }
        output.extend(restorer.finish());
        let text: String = events(&output)
            .iter()
            .filter_map(|(_, event)| event["delta"]["text"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(text, "key: sk-real-secret done");
    }

    #[test]
    fn converse_deltas_are_restored_under_their_event_names() {
        let delta = |text: &str| {
            converse(
                "contentBlockDelta",
                &serde_json::json!({"contentBlockIndex": 0, "delta": {"text": text}}),
            )
        };
        let stream = [
            converse("messageStart", &serde_json::json!({"role": "assistant"})),
            delta("AEGIS_MASKED_TOKEN_"),
            delta("abc_END!"),
            converse(
                "contentBlockStop",
                &serde_json::json!({"contentBlockIndex": 0}),
            ),
            converse(
                "metadata",
                &serde_json::json!({"usage": {"inputTokens": 5, "outputTokens": 2}}),
            ),
        ]
        .concat();
        let mut restorer = Restorer::new(StreamRestorer::new(replacement()));
        let mut output = restorer.rewrite_chunk(&stream);
        output.extend(restorer.finish());
        let events = events(&output);
        assert_eq!(events.first().unwrap().0, "messageStart");
        assert_eq!(events.last().unwrap().0, "metadata");
        let text: String = events
            .iter()
            .filter(|(name, _)| name == "contentBlockDelta")
            .filter_map(|(_, event)| event["delta"]["text"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(text, "sk-real-secret!");
    }

    #[test]
    fn a_non_stream_body_passes_through_the_restorer() {
        let mut restorer = Restorer::new(StreamRestorer::new(replacement()));
        let body = br#"{"message":"AEGIS_MASKED_TOKEN_abc_END"}"#;
        let mut output = restorer.rewrite_chunk(body);
        output.extend(restorer.finish());
        assert_eq!(output, body, "an unframed error body is left as it came");
    }
}
