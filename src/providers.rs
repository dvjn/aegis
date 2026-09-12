use crate::compression::{decode_body, decode_brotli_unsniffable};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub enum Provider {
    Anthropic,
    Codex,
}

impl Provider {
    pub fn protocol(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic_messages",
            Self::Codex => "openai_responses",
        }
    }
}

#[derive(Debug, Default)]
pub struct Usage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub raw_json: Option<String>,
}

/// Deserializing into this instead of a `Value` lets serde skip the rest of the
/// body rather than building a tree that is dropped after one field is read.
#[derive(Deserialize)]
struct ModelOnly {
    #[serde(default)]
    model: Option<String>,
}

pub fn requested_model(body: &[u8]) -> Option<String> {
    let decoded = decode_body(body);
    serde_json::from_slice::<ModelOnly>(&decoded)
        .ok()
        .or_else(|| {
            decode_brotli_unsniffable(body)
                .and_then(|decoded| serde_json::from_slice(&decoded).ok())
        })?
        .model
}

pub fn extract_usage(provider: Provider, body: &[u8]) -> Usage {
    let Some(owned_usage) = decoded_last_usage(body) else {
        return Usage::default();
    };
    let value = &owned_usage;

    match provider {
        Provider::Anthropic => Usage {
            input_tokens: integer(value, "input_tokens"),
            output_tokens: integer(value, "output_tokens"),
            cache_read_tokens: integer(value, "cache_read_input_tokens"),
            cache_write_tokens: integer(value, "cache_creation_input_tokens"),
            reasoning_tokens: None,
            raw_json: serde_json::to_string(value).ok(),
        },
        Provider::Codex => {
            let cache_read_tokens = value
                .get("input_tokens_details")
                .and_then(|details| integer(details, "cached_tokens"));
            Usage {
                // The Responses API counts cached tokens inside input_tokens,
                // while the Messages API keeps them apart. Subtract here so the
                // stored columns are disjoint for every provider and a total
                // never counts a cached token twice. Clamped because an
                // inconsistent upstream payload must not yield a negative count.
                input_tokens: integer(value, "input_tokens")
                    .map(|total| (total - cache_read_tokens.unwrap_or(0)).max(0)),
                output_tokens: integer(value, "output_tokens"),
                cache_read_tokens,
                cache_write_tokens: None,
                reasoning_tokens: value
                    .get("output_tokens_details")
                    .and_then(|details| integer(details, "reasoning_tokens")),
                raw_json: serde_json::to_string(value).ok(),
            }
        }
    }
}

enum Scan {
    NoValues,
    LastUsage(Option<Value>),
}

fn decoded_last_usage(body: &[u8]) -> Option<Value> {
    match scan_last_usage(&decode_body(body)) {
        Scan::LastUsage(usage) => usage,
        Scan::NoValues => match scan_last_usage(&decode_brotli_unsniffable(body)?) {
            Scan::LastUsage(usage) => usage,
            Scan::NoValues => None,
        },
    }
}

fn scan_last_usage(body: &[u8]) -> Scan {
    if let Ok(mut value) = serde_json::from_slice::<Value>(body) {
        return Scan::LastUsage(take_usage(&mut value));
    }

    let mut parsed_any = false;
    let mut last_usage = None;
    let frames = String::from_utf8_lossy(body);
    let frames = frames
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| *data != "[DONE]");
    for data in frames {
        let Ok(mut value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        parsed_any = true;
        if let Some(usage) = take_usage(&mut value) {
            last_usage = Some(usage);
        }
    }

    if parsed_any {
        Scan::LastUsage(last_usage)
    } else {
        Scan::NoValues
    }
}

fn take_usage(value: &mut Value) -> Option<Value> {
    if let Some(usage) = value.get_mut("usage") {
        return Some(usage.take());
    }
    if let Some(usage) = value.get_mut("response").and_then(|it| it.get_mut("usage")) {
        return Some(usage.take());
    }
    if let Some(usage) = value.get_mut("message").and_then(|it| it.get_mut("usage")) {
        return Some(usage.take());
    }
    None
}

fn integer(value: &Value, key: &str) -> Option<i64> {
    value.get(key)?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &[u8] = br#"{"model":"gpt-5.6-sol","input":"hello"}"#;

    #[test]
    fn extracts_requested_model_from_compressed_bodies() {
        assert_eq!(requested_model(REQUEST).as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            requested_model(&crate::compression::tests::gzip(REQUEST)).as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            requested_model(&zstd::encode_all(REQUEST, 0).unwrap()).as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            requested_model(&crate::compression::tests::brotli(REQUEST)).as_deref(),
            Some("gpt-5.6-sol")
        );
    }

    #[test]
    fn extracts_anthropic_sse_usage() {
        let body = br#"event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":42,"input_tokens":10,"cache_read_input_tokens":8}}

"#;
        let usage = extract_usage(Provider::Anthropic, body);
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(42));
        assert_eq!(usage.cache_read_tokens, Some(8));
    }

    const SSE_BODY: &[u8] = br#"event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":42,"input_tokens":10}}

"#;

    fn assert_sse_usage(body: &[u8]) {
        let usage = extract_usage(Provider::Anthropic, body);
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(42));
    }

    #[test]
    fn extracts_gzip_compressed_anthropic_sse_usage() {
        assert_sse_usage(&crate::compression::tests::gzip(SSE_BODY));
    }

    #[test]
    fn extracts_zstd_compressed_anthropic_sse_usage() {
        assert_sse_usage(&zstd::encode_all(SSE_BODY, 0).unwrap());
    }

    #[test]
    fn extracts_brotli_compressed_anthropic_sse_usage() {
        assert_sse_usage(&crate::compression::tests::brotli(SSE_BODY));
    }

    #[test]
    fn an_undecodable_body_yields_no_usage() {
        assert!(
            extract_usage(Provider::Anthropic, b"\x00not a body")
                .raw_json
                .is_none()
        );
    }

    #[test]
    fn extracts_codex_response_usage() {
        let body = br#"{"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":5}}}"#;
        let usage = extract_usage(Provider::Codex, body);
        assert_eq!(usage.input_tokens, Some(60));
        assert_eq!(usage.reasoning_tokens, Some(5));
    }

    #[test]
    fn codex_cached_tokens_are_counted_once() {
        let body = br#"{"usage":{"input_tokens":1000,"output_tokens":20,"input_tokens_details":{"cached_tokens":400}}}"#;
        let usage = extract_usage(Provider::Codex, body);
        assert_eq!(usage.input_tokens, Some(600));
        assert_eq!(usage.cache_read_tokens, Some(400));
        assert_eq!(
            usage.input_tokens.unwrap()
                + usage.cache_read_tokens.unwrap()
                + usage.output_tokens.unwrap(),
            1020,
            "the four counters must add up to the tokens the upstream reported"
        );
    }

    #[test]
    fn codex_cached_tokens_above_the_input_total_clamp_to_zero() {
        let body = br#"{"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":400}}}"#;
        let usage = extract_usage(Provider::Codex, body);
        assert_eq!(usage.input_tokens, Some(0));
        assert_eq!(usage.cache_read_tokens, Some(400));
    }

    #[test]
    fn codex_raw_json_keeps_the_upstream_input_total() {
        let body = br#"{"usage":{"input_tokens":1000,"output_tokens":20,"input_tokens_details":{"cached_tokens":400}}}"#;
        let usage = extract_usage(Provider::Codex, body);
        let raw: Value = serde_json::from_str(&usage.raw_json.unwrap()).unwrap();
        assert_eq!(raw["input_tokens"], 1000);
    }
}
