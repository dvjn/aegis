//! Gemini may attach a signature after the text it signs. Mask-mode restoration
//! waits for the response before editing unsigned candidates. Normal forwarding
//! and observe mode keep streaming. Oversized responses pass through unchanged.
use super::{Encoding, Replacement, substitute_text, substitute_value};
use crate::policies::sse::{Frame, FrameParser};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct GeminiRestorer {
    body: Vec<u8>,
    replacements: Vec<Replacement>,
    passthrough: bool,
}

struct Document {
    value: Option<Value>,
    changed: bool,
}

impl GeminiRestorer {
    pub fn new(replacements: Vec<Replacement>) -> Self {
        Self {
            body: Vec::new(),
            replacements,
            passthrough: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.passthrough {
            return chunk.to_vec();
        }
        if self.body.len().saturating_add(chunk.len()) > MAX_BUFFER_BYTES {
            self.passthrough = true;
            let mut body = std::mem::take(&mut self.body);
            body.extend_from_slice(chunk);
            return body;
        }
        self.body.extend_from_slice(chunk);
        Vec::new()
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let body = std::mem::take(&mut self.body);
        if self.passthrough || body.is_empty() {
            return body;
        }
        if let Ok(value) = serde_json::from_slice(&body) {
            let mut documents = vec![Document {
                value: Some(value),
                changed: false,
            }];
            restore(&mut documents, &self.replacements);
            return if documents[0].changed {
                serde_json::to_vec(documents[0].value.as_ref().unwrap()).expect("JSON value")
            } else {
                body
            };
        }
        let mut parser = FrameParser::default();
        let mut frames = parser.push(&body);
        let tail = parser.finish();
        if !tail.is_empty() {
            frames.push(Frame::from_bytes(tail));
        }
        let mut documents: Vec<_> = frames
            .iter()
            .map(|frame| Document {
                value: frame
                    .data()
                    .and_then(|data| serde_json::from_str(&data).ok()),
                changed: false,
            })
            .collect();
        restore(&mut documents, &self.replacements);
        let mut output = Vec::with_capacity(body.len());
        for (frame, document) in frames.into_iter().zip(documents) {
            if document.changed {
                output.extend(frame.with_data(&document.value.unwrap().to_string()));
            } else {
                output.extend_from_slice(frame.bytes());
            }
        }
        output
    }
}

fn response(value: &Value) -> &Value {
    value.get("response").unwrap_or(value)
}

fn restore(documents: &mut [Document], replacements: &[Replacement]) {
    let mut signed = BTreeSet::new();
    for value in documents
        .iter()
        .filter_map(|document| document.value.as_ref())
    {
        for (position, candidate) in response(value)["candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|part| {
                    part.get("thoughtSignature").is_some()
                        || part.get("thought_signature").is_some()
                })
            {
                signed.insert(candidate["index"].as_u64().unwrap_or(position as u64));
            }
        }
    }
    // Buffer by candidate, part, and thought/text channel so placeholders split
    // across SSE events can be restored without ever rewriting a signed candidate.
    type TextPieces = BTreeMap<(u64, usize, bool), Vec<(usize, String, String)>>;
    let mut pieces = TextPieces::new();
    for (document_index, document) in documents.iter().enumerate() {
        let Some(value) = &document.value else {
            continue;
        };
        let prefix = if value.get("response").is_some() {
            "/response"
        } else {
            ""
        };
        for (position, candidate) in response(value)["candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let id = candidate["index"].as_u64().unwrap_or(position as u64);
            if signed.contains(&id) {
                continue;
            }
            for (part_index, part) in candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(text) = part["text"].as_str() {
                    pieces
                        .entry((id, part_index, part["thought"] == true))
                        .or_default()
                        .push((
                            document_index,
                            format!(
                                "{prefix}/candidates/{position}/content/parts/{part_index}/text"
                            ),
                            text.to_owned(),
                        ));
                }
            }
        }
    }
    for pieces in pieces.values() {
        let text: String = pieces.iter().map(|(_, _, text)| text.as_str()).collect();
        let restored = substitute_text(&text, replacements, Encoding::Plain);
        if restored == text {
            continue;
        }
        for (index, (document_index, path, _)) in pieces.iter().enumerate() {
            let document = &mut documents[*document_index];
            if let Some(field) = document
                .value
                .as_mut()
                .and_then(|value| value.pointer_mut(path))
            {
                *field = Value::String(if index == 0 {
                    restored.clone()
                } else {
                    String::new()
                });
                document.changed = true;
            }
        }
    }
    for document in documents {
        let Some(value) = &mut document.value else {
            continue;
        };
        let owner = if value.get("response").is_some() {
            &mut value["response"]
        } else {
            value
        };
        if let Some(candidates) = owner.get_mut("candidates").and_then(Value::as_array_mut) {
            for (position, candidate) in candidates.iter_mut().enumerate() {
                if !signed.contains(&candidate["index"].as_u64().unwrap_or(position as u64)) {
                    document.changed |= substitute_value(candidate, replacements);
                }
            }
        } else {
            document.changed |= substitute_value(owner, replacements);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn replacement() -> Vec<Replacement> {
        vec![Replacement {
            placeholder: "AEGIS_MASKED_TEST_END".into(),
            original: "original-secret".into(),
        }]
    }
    fn frame(part: Value) -> String {
        format!(
            "data: {}\n\n",
            json!({"response":{"candidates":[{"index":0,"content":{"parts":[part]}}]}})
        )
    }

    #[test]
    fn split_placeholders_and_tool_arguments_are_restored() {
        let source = frame(json!({"text":"AEGIS_MASKED_"}))
            + &frame(
                json!({"text":"TEST_END","functionCall":{"name":"Read","args":{"path":"AEGIS_MASKED_TEST_END"}}}),
            );
        let mut restorer = GeminiRestorer::new(replacement());
        for chunk in source.as_bytes().chunks(7) {
            assert!(restorer.push(chunk).is_empty());
        }
        let output = String::from_utf8(restorer.finish()).unwrap();
        assert!(!output.contains("AEGIS_MASKED_"));
        assert_eq!(output.matches("original-secret").count(), 2);
    }

    #[test]
    fn a_signature_arriving_late_preserves_every_byte_of_the_candidate() {
        let source = frame(json!({"text":"AEGIS_MASKED_TEST_END"}))
            + &frame(json!({"thoughtSignature":"opaque-signed-data"}));
        let mut restorer = GeminiRestorer::new(replacement());
        restorer.push(source.as_bytes());
        assert_eq!(restorer.finish(), source.as_bytes());
    }

    #[test]
    fn oversized_signed_streams_are_forwarded_without_rewriting() {
        let source = vec![b'x'; MAX_BUFFER_BYTES + 1];
        let mut restorer = GeminiRestorer::new(replacement());
        assert_eq!(restorer.push(&source), source);
        assert_eq!(restorer.push(b"tail"), b"tail");
        assert!(restorer.finish().is_empty());
    }
}
