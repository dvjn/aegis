use crate::compression::{decode_body, decode_brotli_unsniffable};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub enum Provider {
    Anthropic,
    Codex,
    TypeSafe,
}

impl Provider {
    pub fn protocol(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic_messages",
            Self::Codex => "openai_responses",
            Self::TypeSafe => "typesafe_systemone",
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

/// What the response reported: the tokens it spent, and the model that spent
/// them, which is the resolved id behind an alias like `claude-sonnet-4-5` or
/// `jev-latest`.
#[derive(Debug, Default)]
pub struct Completion {
    pub usage: Usage,
    pub model: Option<String>,
}

pub fn extract_completion(provider: Provider, body: &[u8]) -> Completion {
    let found = decoded_response(body);
    Completion {
        usage: found
            .usage
            .map(|usage| usage_from(provider, &usage))
            .unwrap_or_default(),
        model: found.model,
    }
}

/// The model a stored response reported, for rows captured before the gateway
/// recorded it.
pub fn response_model(body: &[u8]) -> Option<String> {
    decoded_response(body).model
}

pub fn extract_usage(provider: Provider, body: &[u8]) -> Usage {
    extract_completion(provider, body).usage
}

fn usage_from(provider: Provider, value: &Value) -> Usage {
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
        Provider::TypeSafe => Usage {
            input_tokens: integer(value, "input_tokens"),
            output_tokens: integer(value, "output_tokens"),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            raw_json: serde_json::to_string(value).ok(),
        },
    }
}

/// The last usage a body reported and the first model it named. Usage comes
/// last because a stream totals it as it goes; the model comes first because it
/// is named in the opening frame and never changes, and a capture that hit its
/// byte limit keeps the head of the body but not the tail.
#[derive(Default)]
struct Found {
    usage: Option<Value>,
    model: Option<String>,
}

enum Scan {
    NoValues,
    Found(Found),
}

fn decoded_response(body: &[u8]) -> Found {
    match scan_response(&decode_body(body)) {
        Scan::Found(found) => found,
        Scan::NoValues => match decode_brotli_unsniffable(body).map(|body| scan_response(&body)) {
            Some(Scan::Found(found)) => found,
            _ => Found::default(),
        },
    }
}

fn scan_response(body: &[u8]) -> Scan {
    if let Ok(mut value) = serde_json::from_slice::<Value>(body) {
        return Scan::Found(Found {
            model: take_model(&value),
            usage: take_usage(&mut value),
        });
    }

    let mut parsed_any = false;
    let mut found = Found::default();
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
        if found.model.is_none() {
            found.model = take_model(&value);
        }
        if let Some(usage) = take_usage(&mut value) {
            found.usage = Some(usage);
        }
    }

    if parsed_any {
        Scan::Found(found)
    } else {
        Scan::NoValues
    }
}

/// The three dialects name the model in the same three places they carry usage:
/// at the top level, under `response`, or under `message`.
fn take_model(value: &Value) -> Option<String> {
    [Some(value), value.get("response"), value.get("message")]
        .into_iter()
        .flatten()
        .find_map(|owner| owner.get("model").and_then(Value::as_str))
        .map(str::to_owned)
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

    const TYPESAFE_BODY: &[u8] = br#"{"model":"jev-1.13.0","answers":{"blue":{"type":"noul","noul":0.97}},"usage":{"input_tokens":123,"output_tokens":7}}"#;

    #[test]
    fn extracts_typesafe_usage() {
        let usage = extract_usage(Provider::TypeSafe, TYPESAFE_BODY);
        assert_eq!(usage.input_tokens, Some(123));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.cache_write_tokens, None);
        assert_eq!(usage.reasoning_tokens, None);
    }

    #[test]
    fn extracts_compressed_typesafe_usage() {
        let usage = extract_usage(
            Provider::TypeSafe,
            &crate::compression::tests::gzip(TYPESAFE_BODY),
        );
        assert_eq!(usage.input_tokens, Some(123));
        assert_eq!(usage.output_tokens, Some(7));
    }

    #[test]
    fn typesafe_aliases_are_read_as_the_requested_model() {
        let body = br#"{"model":"jev-latest","state":"hello","questions":{}}"#;
        assert_eq!(requested_model(body).as_deref(), Some("jev-latest"));
    }

    const ANTHROPIC_STREAM: &[u8] = br#"event: message_start
data: {"type":"message_start","message":{"model":"claude-sonnet-4-5-20250929","usage":{"input_tokens":10}}}

event: message_delta
data: {"type":"message_delta","usage":{"input_tokens":10,"output_tokens":42}}

"#;

    const CODEX_STREAM: &[u8] = br#"event: response.created
data: {"type":"response.created","response":{"model":"gpt-5.6-luna","usage":null}}

event: response.completed
data: {"type":"response.completed","response":{"model":"gpt-5.6-luna","usage":{"input_tokens":100,"output_tokens":20}}}

"#;

    #[test]
    fn reads_the_model_each_dialect_reports() {
        for (provider, body, expected) in [
            (
                Provider::Anthropic,
                ANTHROPIC_STREAM,
                "claude-sonnet-4-5-20250929",
            ),
            (Provider::Codex, CODEX_STREAM, "gpt-5.6-luna"),
            (Provider::TypeSafe, TYPESAFE_BODY, "jev-1.13.0"),
        ] {
            let completion = extract_completion(provider, body);
            assert_eq!(completion.model.as_deref(), Some(expected));
            assert!(
                completion.usage.output_tokens.is_some(),
                "usage extraction must be unchanged for {expected}"
            );
        }
    }

    #[test]
    fn the_model_survives_a_capture_that_lost_its_tail() {
        // Capture keeps the head of a body and drops the tail, so a stream cut
        // short still carries the frame that names the model, but not the one
        // that totals usage.
        let first_frame_end = ANTHROPIC_STREAM
            .windows(6)
            .position(|window| window == b"event:")
            .and_then(|start| {
                ANTHROPIC_STREAM[start + 6..]
                    .windows(6)
                    .position(|window| window == b"event:")
                    .map(|next| start + 6 + next)
            })
            .expect("the fixture has two frames");
        let completion =
            extract_completion(Provider::Anthropic, &ANTHROPIC_STREAM[..first_frame_end]);
        assert_eq!(
            completion.model.as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
        assert_eq!(
            completion.usage.output_tokens, None,
            "the frame totalling usage never arrived"
        );
    }

    #[test]
    fn a_capture_cut_inside_the_opening_frame_recovers_nothing() {
        let completion = extract_completion(Provider::Anthropic, &ANTHROPIC_STREAM[..90]);
        assert_eq!(completion.model, None);
        assert_eq!(completion.usage.input_tokens, None);
    }

    #[test]
    fn a_response_naming_no_model_resolves_to_none() {
        let body = br#"{"usage":{"input_tokens":1,"output_tokens":2}}"#;
        let completion = extract_completion(Provider::TypeSafe, body);
        assert_eq!(completion.model, None);
        assert_eq!(completion.usage.input_tokens, Some(1));
    }

    #[test]
    fn the_model_is_read_from_a_compressed_body() {
        let completion = extract_completion(
            Provider::TypeSafe,
            &crate::compression::tests::gzip(TYPESAFE_BODY),
        );
        assert_eq!(completion.model.as_deref(), Some("jev-1.13.0"));
    }

    #[test]
    fn codex_raw_json_keeps_the_upstream_input_total() {
        let body = br#"{"usage":{"input_tokens":1000,"output_tokens":20,"input_tokens_details":{"cached_tokens":400}}}"#;
        let usage = extract_usage(Provider::Codex, body);
        let raw: Value = serde_json::from_str(&usage.raw_json.unwrap()).unwrap();
        assert_eq!(raw["input_tokens"], 1000);
    }
}
