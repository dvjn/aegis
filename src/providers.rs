use flate2::read::GzDecoder;
use serde_json::Value;
use std::io::Read;

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

pub fn requested_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("model")?
        .as_str()
        .map(str::to_owned)
}

pub fn extract_usage(provider: Provider, body: &[u8]) -> Usage {
    let decoded = decode_body(body);
    let values = json_values(&decoded);
    let usage = values.iter().rev().find_map(find_usage);
    let Some(value) = usage else {
        return Usage::default();
    };

    match provider {
        Provider::Anthropic => Usage {
            input_tokens: integer(value, "input_tokens"),
            output_tokens: integer(value, "output_tokens"),
            cache_read_tokens: integer(value, "cache_read_input_tokens"),
            cache_write_tokens: integer(value, "cache_creation_input_tokens"),
            reasoning_tokens: None,
            raw_json: serde_json::to_string(value).ok(),
        },
        Provider::Codex => Usage {
            input_tokens: integer(value, "input_tokens"),
            output_tokens: integer(value, "output_tokens"),
            cache_read_tokens: value
                .get("input_tokens_details")
                .and_then(|details| integer(details, "cached_tokens")),
            cache_write_tokens: None,
            reasoning_tokens: value
                .get("output_tokens_details")
                .and_then(|details| integer(details, "reasoning_tokens")),
            raw_json: serde_json::to_string(value).ok(),
        },
    }
}

fn decode_body(body: &[u8]) -> Vec<u8> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return body.to_vec();
    }
    let mut decoded = Vec::new();
    if GzDecoder::new(body).read_to_end(&mut decoded).is_ok() {
        decoded
    } else {
        body.to_vec()
    }
}

fn json_values(body: &[u8]) -> Vec<Value> {
    if let Ok(value) = serde_json::from_slice(body) {
        return vec![value];
    }

    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

fn find_usage(value: &Value) -> Option<&Value> {
    if let Some(usage) = value.get("usage") {
        return Some(usage);
    }
    if let Some(response) = value.get("response")
        && let Some(usage) = response.get("usage")
    {
        return Some(usage);
    }
    if let Some(message) = value.get("message")
        && let Some(usage) = message.get("usage")
    {
        return Some(usage);
    }
    None
}

fn integer(value: &Value, key: &str) -> Option<i64> {
    value.get(key)?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn extracts_gzip_compressed_anthropic_sse_usage() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;

        let body = br#"event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":42,"input_tokens":10}}

"#;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).unwrap();
        let usage = extract_usage(Provider::Anthropic, &encoder.finish().unwrap());
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(42));
    }

    #[test]
    fn extracts_codex_response_usage() {
        let body = br#"{"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":5}}}"#;
        let usage = extract_usage(Provider::Codex, body);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.reasoning_tokens, Some(5));
    }
}
