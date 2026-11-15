use anyhow::{Result, bail};
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Anthropic,
    OpenAi,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SemanticSpec {
    pub protocol: Protocol,
    pub request_bytes: usize,
    pub parts: usize,
    pub response_bytes: usize,
}

pub struct GeneratedPayload {
    pub body: Bytes,
    pub bytes: usize,
}

const WEIGHTS: [usize; 7] = [23, 12, 10, 9, 9, 30, 7];

pub fn generate(spec: SemanticSpec, tail: usize) -> Result<GeneratedPayload> {
    let counts = part_counts(spec.parts);
    let empty = build(spec.protocol, counts, [0; 6], 0, tail);
    let base_bytes = serde_json::to_vec(&empty)?.len();
    if base_bytes > spec.request_bytes {
        bail!(
            "{} parts require {base_bytes} bytes, above {} byte target",
            spec.parts,
            spec.request_bytes
        );
    }

    let available = spec.request_bytes - base_bytes;
    let mut category_bytes = [0; 6];
    for index in 0..6 {
        category_bytes[index] = available * WEIGHTS[index] / 100;
    }
    let envelope_bytes = available - category_bytes.iter().sum::<usize>();
    let document = build(spec.protocol, counts, category_bytes, envelope_bytes, tail);
    let encoded = serde_json::to_vec(&document)?;
    if encoded.len() != spec.request_bytes {
        bail!(
            "generated {} bytes for {} byte target",
            encoded.len(),
            spec.request_bytes
        );
    }
    Ok(GeneratedPayload {
        bytes: encoded.len(),
        body: Bytes::from(encoded),
    })
}

fn part_counts(parts: usize) -> [usize; 6] {
    let mut counts = [0; 6];
    let mut used = 0;
    for index in 0..5 {
        counts[index] = (parts * WEIGHTS[index] / 93).max(1);
        used += counts[index];
    }
    counts[5] = parts.saturating_sub(used).max(1);
    counts
}

fn build(
    protocol: Protocol,
    counts: [usize; 6],
    category_bytes: [usize; 6],
    envelope_bytes: usize,
    tail: usize,
) -> Value {
    match protocol {
        Protocol::Anthropic => build_anthropic(counts, category_bytes, envelope_bytes, tail),
        Protocol::OpenAi => build_openai(counts, category_bytes, envelope_bytes, tail),
    }
}

fn build_anthropic(
    counts: [usize; 6],
    category_bytes: [usize; 6],
    envelope_bytes: usize,
    tail: usize,
) -> Value {
    let tools = texts(category_bytes[0], counts[0], "tool definition", 0)
        .into_iter()
        .enumerate()
        .map(|(index, description)| {
            json!({
                "name": format!("tool_{index:04}"),
                "description": description,
                "input_schema": {"type":"object","properties":{"query":{"type":"string"}}}
            })
        })
        .collect::<Vec<_>>();
    let system = texts(category_bytes[1], counts[1], "system policy", 0)
        .into_iter()
        .map(|text| json!({"type":"text","text":text}))
        .collect::<Vec<_>>();
    let user = texts(category_bytes[2], counts[2], "user request", 0)
        .into_iter()
        .map(|text| json!({"type":"text","text":text}))
        .collect::<Vec<_>>();
    let thinking = texts(category_bytes[3], counts[3], "thinking", 0)
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            json!({"type":"thinking","thinking":text,"signature":format!("signature-{index:04}")})
        })
        .collect::<Vec<_>>();
    let tool_uses = texts(category_bytes[4], counts[4], "tool arguments", 0)
        .into_iter()
        .enumerate()
        .map(|(index, arguments)| {
            json!({
                "type":"tool_use",
                "id":format!("toolu_{index:04}"),
                "name":format!("tool_{:04}", index % counts[0]),
                "input":{"query":arguments}
            })
        })
        .collect::<Vec<_>>();
    let results = texts(category_bytes[5], counts[5], "tool result", tail)
        .into_iter()
        .enumerate()
        .map(|(index, content)| {
            json!({
                "type":"tool_result",
                "tool_use_id":format!("toolu_{:04}", index % counts[4]),
                "content":content
            })
        })
        .collect::<Vec<_>>();

    let mut assistant = thinking;
    assistant.extend(tool_uses);
    json!({
        "model":"claude-sonnet-4-5-20250929",
        "max_tokens":4096,
        "stream":true,
        "system":system,
        "tools":tools,
        "messages":[
            {"role":"user","content":user},
            {"role":"assistant","content":assistant},
            {"role":"user","content":results}
        ],
        "metadata":{"user_id":"load-harness","padding":exact_text(envelope_bytes,"envelope",0,0)}
    })
}

fn build_openai(
    counts: [usize; 6],
    category_bytes: [usize; 6],
    envelope_bytes: usize,
    tail: usize,
) -> Value {
    let tools = texts(category_bytes[0], counts[0], "tool definition", 0)
        .into_iter()
        .enumerate()
        .map(|(index, description)| {
            json!({
                "type":"function",
                "name":format!("tool_{index:04}"),
                "description":description,
                "parameters":{"type":"object","properties":{"query":{"type":"string"}}}
            })
        })
        .collect::<Vec<_>>();
    let mut input = texts(category_bytes[1], counts[1], "system policy", 0)
        .into_iter()
        .map(|text| json!({"role":"system","content":[{"type":"input_text","text":text}]}))
        .collect::<Vec<_>>();
    input.extend(
        texts(category_bytes[2], counts[2], "user request", 0)
            .into_iter()
            .map(|text| json!({"role":"user","content":[{"type":"input_text","text":text}]})),
    );
    input.extend(
        texts(category_bytes[3], counts[3], "reasoning summary", 0)
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                json!({
                    "type":"reasoning",
                    "id":format!("rs_{index:04}"),
                    "encrypted_content":format!("encrypted-{index:04}"),
                    "summary":[{"type":"summary_text","text":text}]
                })
            }),
    );
    input.extend(
        texts(category_bytes[4], counts[4], "tool arguments", 0)
            .into_iter()
            .enumerate()
            .map(|(index, arguments)| {
                json!({
                    "type":"function_call",
                    "id":format!("fc_{index:04}"),
                    "call_id":format!("call_{index:04}"),
                    "name":format!("tool_{:04}", index % counts[0]),
                    "arguments":arguments
                })
            }),
    );
    input.extend(
        texts(category_bytes[5], counts[5], "tool result", tail)
            .into_iter()
            .enumerate()
            .map(|(index, output)| {
                json!({
                    "type":"function_call_output",
                    "call_id":format!("call_{:04}", index % counts[4]),
                    "output":output
                })
            }),
    );

    json!({
        "model":"gpt-5-codex",
        "stream":true,
        "tools":tools,
        "input":input,
        "metadata":{"source":"load-harness","padding":exact_text(envelope_bytes,"envelope",0,0)}
    })
}

fn texts(total: usize, count: usize, label: &str, tail: usize) -> Vec<String> {
    let each = total / count;
    let remainder = total % count;
    (0..count)
        .map(|index| {
            exact_text(
                each + usize::from(index < remainder),
                label,
                index,
                if index + 1 == count { tail } else { 0 },
            )
        })
        .collect()
}

fn exact_text(length: usize, label: &str, index: usize, tail: usize) -> String {
    let prefix = format!("{label} {index:04} deterministic semantic content ");
    let mut text = String::with_capacity(length);
    while text.len() < length {
        let remaining = length - text.len();
        text.push_str(&prefix[..prefix.len().min(remaining)]);
    }
    if tail > 0 {
        let marker = format!(" tail-{tail:08}");
        let start = text.len().saturating_sub(marker.len());
        text.replace_range(
            start..,
            &marker[marker.len().saturating_sub(text.len() - start)..],
        );
    }
    text
}
