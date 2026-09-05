use super::Replacement;
use super::sse::{Frame, FrameParser};
use serde_json::Value;

/// Rewrites masking placeholders back to their originals in a streamed
/// response, holding back text that could still be the start of a placeholder
/// split across frames.
pub struct StreamRestorer {
    parser: FrameParser,
    replacements: Vec<Replacement>,
    blocks: Vec<Block>,
}

struct Block {
    key: String,
    pending: String,
    field: FieldPath,
    encoding: Encoding,
    last_delta: Frame,
}

type FieldPath = &'static [&'static str];

/// Whether a text field holds JSON source (so a restored secret must be
/// written as a JSON string literal) or plain text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Plain,
    Json,
}

const JSON_ENCODED_FIELD: &str = "arguments";

enum Role {
    Delta {
        key: String,
        field: FieldPath,
        encoding: Encoding,
    },
    Stop {
        key: String,
    },
    Other,
}

impl StreamRestorer {
    pub fn new(replacements: Vec<Replacement>) -> Self {
        Self {
            parser: FrameParser::default(),
            replacements,
            blocks: Vec::new(),
        }
    }

    pub fn rewrite_chunk(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.replacements.is_empty() {
            return chunk.to_vec();
        }
        let mut output = Vec::with_capacity(chunk.len());
        for frame in self.parser.push(chunk) {
            self.rewrite_frame(&frame, &mut output);
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        for block in std::mem::take(&mut self.blocks) {
            self.flush_block(block, &mut output);
        }
        let unterminated = self.parser.finish();
        output.extend(self.restore_document(unterminated));
        output
    }

    fn restore_document(&self, body: Vec<u8>) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(&body) else {
            return body;
        };
        if let Ok(mut document) = serde_json::from_str::<Value>(text) {
            if substitute_value(&mut document, &self.replacements) {
                return document.to_string().into_bytes();
            }
            return body;
        }
        substitute_text(text, &self.replacements, Encoding::Plain).into_bytes()
    }

    fn rewrite_frame(&mut self, frame: &Frame, output: &mut Vec<u8>) {
        let Some(mut data) = frame
            .data()
            .and_then(|data| serde_json::from_str::<Value>(&data).ok())
        else {
            output.extend_from_slice(frame.bytes());
            return;
        };
        match classify(&data) {
            Role::Delta {
                key,
                field,
                encoding,
            } => {
                let Some(delta_text) = field_mut(&mut data, field)
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                else {
                    output.extend_from_slice(frame.bytes());
                    return;
                };
                let position = self.block_position(key, field, encoding, frame);
                let block = &mut self.blocks[position];
                block.pending.push_str(&delta_text);
                let emitted = take_emittable(&mut block.pending, &self.replacements, encoding);
                if emitted == delta_text {
                    output.extend_from_slice(frame.bytes());
                } else {
                    set_field(&mut data, field, emitted);
                    output.extend(frame.with_data(&data.to_string()));
                }
            }
            Role::Stop { key } => {
                if let Some(position) = self.blocks.iter().position(|block| block.key == key) {
                    let block = self.blocks.remove(position);
                    self.flush_block(block, output);
                }
                self.substitute_frame(frame, data, output);
            }
            Role::Other => self.substitute_frame(frame, data, output),
        }
    }

    fn block_position(
        &mut self,
        key: String,
        field: FieldPath,
        encoding: Encoding,
        frame: &Frame,
    ) -> usize {
        match self.blocks.iter().position(|block| block.key == key) {
            Some(position) => {
                self.blocks[position].last_delta = frame.clone();
                position
            }
            None => {
                self.blocks.push(Block {
                    key,
                    pending: String::new(),
                    field,
                    encoding,
                    last_delta: frame.clone(),
                });
                self.blocks.len() - 1
            }
        }
    }

    fn flush_block(&self, block: Block, output: &mut Vec<u8>) {
        let restored = substitute_text(&block.pending, &self.replacements, block.encoding);
        if restored.is_empty() {
            return;
        }
        let Some(data) = block.last_delta.data() else {
            return;
        };
        let Ok(mut data) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        set_field(&mut data, block.field, restored);
        output.extend(block.last_delta.with_data(&data.to_string()));
    }

    fn substitute_frame(&self, frame: &Frame, mut data: Value, output: &mut Vec<u8>) {
        if substitute_value(&mut data, &self.replacements) {
            output.extend(frame.with_data(&data.to_string()));
        } else {
            output.extend_from_slice(frame.bytes());
        }
    }
}

fn classify(data: &Value) -> Role {
    let Some(kind) = data["type"].as_str() else {
        return Role::Other;
    };
    match kind {
        "content_block_delta" => {
            let key = format!("index:{}", data["index"]);
            match data["delta"]["type"].as_str() {
                Some("text_delta") => Role::Delta {
                    key,
                    field: &["delta", "text"],
                    encoding: Encoding::Plain,
                },
                Some("input_json_delta") => Role::Delta {
                    key,
                    field: &["delta", "partial_json"],
                    encoding: Encoding::Json,
                },
                Some("thinking_delta") => Role::Delta {
                    key,
                    field: &["delta", "thinking"],
                    encoding: Encoding::Plain,
                },
                _ => Role::Other,
            }
        }
        "content_block_stop" => Role::Stop {
            key: format!("index:{}", data["index"]),
        },
        "response.output_text.delta"
        | "response.reasoning_text.delta"
        | "response.custom_tool_call_input.delta" => Role::Delta {
            key: item_key(data),
            field: &["delta"],
            encoding: Encoding::Plain,
        },
        "response.function_call_arguments.delta" => Role::Delta {
            key: item_key(data),
            field: &["delta"],
            encoding: Encoding::Json,
        },
        "response.output_text.done"
        | "response.reasoning_text.done"
        | "response.custom_tool_call_input.done"
        | "response.function_call_arguments.done" => Role::Stop {
            key: item_key(data),
        },
        _ => Role::Other,
    }
}

fn item_key(data: &Value) -> String {
    format!("{}:{}", data["item_id"], data["content_index"])
}

fn field_mut(value: &mut Value, path: FieldPath) -> Option<&mut Value> {
    path.iter()
        .try_fold(value, |current, segment| current.get_mut(segment))
}

fn set_field(value: &mut Value, path: FieldPath, text: String) {
    if let Some(field) = field_mut(value, path) {
        *field = Value::String(text);
    }
}

fn substitute_text(text: &str, replacements: &[Replacement], encoding: Encoding) -> String {
    replacements
        .iter()
        .filter(|replacement| text.contains(&replacement.placeholder))
        .fold(text.to_owned(), |text, replacement| {
            text.replace(
                &replacement.placeholder,
                &encoded(&replacement.original, encoding),
            )
        })
}

fn encoded(original: &str, encoding: Encoding) -> String {
    match encoding {
        Encoding::Plain => original.to_owned(),
        Encoding::Json => {
            let literal = serde_json::to_string(original).expect("strings always serialize");
            literal[1..literal.len() - 1].to_owned()
        }
    }
}

fn substitute_value(value: &mut Value, replacements: &[Replacement]) -> bool {
    substitute_value_as(value, replacements, Encoding::Plain)
}

fn substitute_value_as(
    value: &mut Value,
    replacements: &[Replacement],
    encoding: Encoding,
) -> bool {
    match value {
        Value::String(text) => {
            let restored = substitute_text(text, replacements, encoding);
            let changed = restored != *text;
            *text = restored;
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= substitute_value(item, replacements);
            }
            changed
        }
        Value::Object(fields) => {
            let mut changed = false;
            for (name, field) in fields.iter_mut() {
                let encoding = if name == JSON_ENCODED_FIELD && field.is_string() {
                    Encoding::Json
                } else {
                    Encoding::Plain
                };
                changed |= substitute_value_as(field, replacements, encoding);
            }
            changed
        }
        _ => false,
    }
}

fn take_emittable(
    pending: &mut String,
    replacements: &[Replacement],
    encoding: Encoding,
) -> String {
    let restored = substitute_text(pending, replacements, encoding);
    let held_from = holdback_start(&restored, replacements);
    *pending = restored[held_from..].to_owned();
    restored[..held_from].to_owned()
}

/// The held-back tail is the longest proper suffix that is still a prefix of
/// some placeholder, so at most (longest placeholder - 1) chars ever wait.
fn holdback_start(text: &str, replacements: &[Replacement]) -> usize {
    let longest = replacements
        .iter()
        .map(|replacement| replacement.placeholder.chars().count())
        .max()
        .unwrap_or(0);
    let mut start = text.len();
    for (held_chars, (index, _)) in text.char_indices().rev().enumerate() {
        if held_chars + 1 >= longest {
            break;
        }
        let suffix = &text[index..];
        if replacements
            .iter()
            .any(|replacement| replacement.placeholder.starts_with(suffix))
        {
            start = index;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC: &[u8] = include_bytes!("fixtures/anthropic_tool_use.sse");
    const CODEX: &[u8] = include_bytes!("fixtures/codex_custom_tool.sse");
    const PLACEHOLDER: &str = "AEGIS_SECRET_0123456789abcdef012345_END";
    const SECRET: &str = "ghp_TESTONLYTESTONLYTESTONLYTESTONLYTEST12";
    const MASKED_COMMAND: &str =
        r#"{"command":"export TOKEN=AEGIS_SECRET_0123456789abcdef012345_END"}"#;
    const MASKED_TEXT: &str = "Set AEGIS_SECRET_0123456789abcdef012345_END in your shell.";

    fn replacements() -> Vec<Replacement> {
        vec![Replacement {
            placeholder: PLACEHOLDER.to_owned(),
            original: SECRET.to_owned(),
        }]
    }

    fn restored(masked: &str) -> String {
        masked.replace(PLACEHOLDER, SECRET)
    }

    fn frames(bytes: &[u8]) -> Vec<Frame> {
        let mut parser = FrameParser::default();
        let frames = parser.push(bytes);
        assert!(
            parser.finish().is_empty(),
            "stream must end on a frame boundary"
        );
        frames
    }

    fn json(frame: &Frame) -> Option<Value> {
        frame
            .data()
            .and_then(|data| serde_json::from_str(&data).ok())
    }

    fn kind(frame: &Frame) -> String {
        json(frame)
            .and_then(|data| data["type"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn is_delta(frame: &Frame) -> bool {
        matches!(
            classify(&json(frame).unwrap_or(Value::Null)),
            Role::Delta { .. }
        )
    }

    fn split_into(text: &str, pieces: usize) -> Vec<String> {
        if pieces == 0 {
            return Vec::new();
        }
        let step = text.len().div_ceil(pieces);
        let mut out: Vec<String> = text
            .as_bytes()
            .chunks(step)
            .map(|piece| String::from_utf8(piece.to_vec()).unwrap())
            .collect();
        out.resize(pieces, String::new());
        out
    }

    /// Rewrites `path` on every frame accepted by `select`; delta frames take
    /// the pieces of `text` in order, other selected frames take the whole text.
    fn rewrite_fixture(
        fixture: &[u8],
        select: impl Fn(&Value) -> bool,
        path: FieldPath,
        text: &str,
    ) -> Vec<u8> {
        let all = frames(fixture);
        let delta_count = all
            .iter()
            .filter(|frame| json(frame).is_some_and(|data| select(&data)) && is_delta(frame))
            .count();
        let mut pieces = split_into(text, delta_count).into_iter();
        let mut output = Vec::new();
        for frame in &all {
            match json(frame) {
                Some(mut data) if select(&data) => {
                    let value = if is_delta(frame) {
                        pieces.next().unwrap()
                    } else {
                        text.to_owned()
                    };
                    set_field(&mut data, path, value);
                    output.extend(frame.with_data(&data.to_string()));
                }
                _ => output.extend_from_slice(frame.bytes()),
            }
        }
        output
    }

    fn collect_text(output: &[u8], select: impl Fn(&Value) -> bool, path: FieldPath) -> String {
        frames(output)
            .iter()
            .filter_map(json)
            .filter(|data| select(data))
            .filter_map(|mut data| {
                field_mut(&mut data, path)
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .collect()
    }

    fn field_of(output: &[u8], select: impl Fn(&Value) -> bool, path: FieldPath) -> String {
        let mut matches = frames(output)
            .into_iter()
            .filter_map(|frame| json(&frame))
            .filter(|data| select(data))
            .filter_map(|mut data| {
                field_mut(&mut data, path)
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            });
        let found = matches.next().expect("one matching frame");
        assert!(matches.next().is_none(), "exactly one matching frame");
        found
    }

    fn non_delta_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
        frames(bytes)
            .iter()
            .filter(|frame| !is_delta(frame))
            .map(|frame| frame.bytes().to_vec())
            .collect()
    }

    fn run(chunks: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
        let mut restorer = StreamRestorer::new(replacements());
        let mut output = Vec::new();
        for chunk in chunks {
            output.extend(restorer.rewrite_chunk(&chunk));
        }
        output.extend(restorer.finish());
        output
    }

    fn run_every_chunking(input: &[u8]) -> Vec<u8> {
        let whole = run(std::iter::once(input.to_vec()));
        let per_frame = run(frames(input)
            .into_iter()
            .map(|frame| frame.bytes().to_vec()));
        let byte_by_byte = run(input.iter().map(|byte| vec![*byte]));
        let mid_line = run(input.chunks(13).map(<[u8]>::to_vec));
        assert_eq!(per_frame, whole, "per frame chunking differs");
        assert_eq!(byte_by_byte, whole, "byte by byte chunking differs");
        assert_eq!(mid_line, whole, "mid-line chunking differs");
        frames(&whole);
        assert!(
            !whole
                .windows(PLACEHOLDER.len())
                .any(|window| window == PLACEHOLDER.as_bytes())
        );
        whole
    }

    fn anthropic_block(index: u64) -> impl Fn(&Value) -> bool {
        move |data| data["type"] == "content_block_delta" && data["index"] == index
    }

    fn codex_item(kind: &'static str) -> impl Fn(&Value) -> bool {
        move |data| {
            data["type"]
                .as_str()
                .is_some_and(|value| value.starts_with(kind))
        }
    }

    #[test]
    fn an_anthropic_tool_input_placeholder_split_across_frames_is_restored() {
        let path: FieldPath = &["delta", "partial_json"];
        let input = rewrite_fixture(ANTHROPIC, anthropic_block(1), path, MASKED_COMMAND);
        assert_eq!(
            collect_text(&input, anthropic_block(1), path),
            MASKED_COMMAND
        );

        let output = run_every_chunking(&input);

        assert_eq!(
            collect_text(&output, anthropic_block(1), path),
            restored(MASKED_COMMAND)
        );
        assert_eq!(non_delta_frames(&output), non_delta_frames(&input));
        let kinds: Vec<String> = frames(&output).iter().map(kind).collect();
        assert_eq!(kinds, frames(&input).iter().map(kind).collect::<Vec<_>>());
    }

    #[test]
    fn an_anthropic_text_placeholder_split_across_frames_is_restored() {
        let path: FieldPath = &["delta", "text"];
        let input = rewrite_fixture(ANTHROPIC, anthropic_block(0), path, MASKED_TEXT);

        let output = run_every_chunking(&input);

        assert_eq!(
            collect_text(&output, anthropic_block(0), path),
            restored(MASKED_TEXT)
        );
        assert_eq!(non_delta_frames(&output), non_delta_frames(&input));
    }

    #[test]
    fn a_stream_without_placeholders_is_emitted_byte_for_byte() {
        let output = run_every_chunking(ANTHROPIC);

        assert_eq!(output, ANTHROPIC);
    }

    const PEM_SECRET: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";

    fn pem_replacements() -> Vec<Replacement> {
        vec![Replacement {
            placeholder: PLACEHOLDER.to_owned(),
            original: PEM_SECRET.to_owned(),
        }]
    }

    #[test]
    fn a_multi_line_secret_restored_inside_partial_json_keeps_the_input_valid_json() {
        let path: FieldPath = &["delta", "partial_json"];
        let masked_input = format!(r#"{{"command":"cat > key <<EOF\n{PLACEHOLDER}\nEOF"}}"#);
        let input = rewrite_fixture(ANTHROPIC, anthropic_block(1), path, &masked_input);
        let mut restorer = StreamRestorer::new(pem_replacements());

        let mut output = Vec::new();
        for frame in frames(&input) {
            output.extend(restorer.rewrite_chunk(frame.bytes()));
        }
        output.extend(restorer.finish());

        let tool_input = collect_text(&output, anthropic_block(1), path);
        let parsed: Value = serde_json::from_str(&tool_input)
            .unwrap_or_else(|error| panic!("{error}: {tool_input}"));
        assert_eq!(
            parsed["command"].as_str().unwrap(),
            format!("cat > key <<EOF\n{PEM_SECRET}\nEOF")
        );
        assert!(!tool_input.contains(PLACEHOLDER));
        assert_eq!(non_delta_frames(&output), non_delta_frames(&input));
    }

    #[test]
    fn function_call_arguments_are_restored_as_json_string_literals() {
        let arguments_done = format!(
            "event: response.function_call_arguments.done\ndata: {}\n\n",
            serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "fc_0123",
                "output_index": 0,
                "arguments": format!(r#"{{"key":"{PLACEHOLDER}"}}"#)
            })
        );
        let item_done = format!(
            "event: response.output_item.done\ndata: {}\n\n",
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "fc_0123",
                    "arguments": format!(r#"{{"key":"{PLACEHOLDER}"}}"#)
                }
            })
        );
        let mut restorer = StreamRestorer::new(pem_replacements());

        let mut output = restorer.rewrite_chunk(arguments_done.as_bytes());
        output.extend(restorer.rewrite_chunk(item_done.as_bytes()));
        output.extend(restorer.finish());

        for frame in frames(&output) {
            let data = json(&frame).expect("json frames");
            let arguments = data["arguments"]
                .as_str()
                .or_else(|| data["item"]["arguments"].as_str())
                .expect("arguments present");
            let parsed: Value = serde_json::from_str(arguments).expect("valid JSON arguments");
            assert_eq!(parsed["key"].as_str().unwrap(), PEM_SECRET);
        }
    }

    #[test]
    fn a_codex_custom_tool_input_is_restored_in_the_deltas_and_the_done_frame() {
        let masked = rewrite_fixture(
            CODEX,
            codex_item("response.custom_tool_call_input."),
            &["delta"],
            MASKED_COMMAND,
        );
        let input = rewrite_fixture(
            &masked,
            |data| data["type"] == "response.custom_tool_call_input.done",
            &["input"],
            MASKED_COMMAND,
        );
        let input = rewrite_fixture(
            &input,
            |data| {
                data["type"] == "response.output_item.done"
                    && data["item"]["type"] == "custom_tool_call"
            },
            &["item", "input"],
            MASKED_COMMAND,
        );

        let output = run_every_chunking(&input);

        let deltas = collect_text(
            &output,
            |data| data["type"] == "response.custom_tool_call_input.delta",
            &["delta"],
        );
        let done = field_of(
            &output,
            |data| data["type"] == "response.custom_tool_call_input.done",
            &["input"],
        );
        assert_eq!(deltas, restored(MASKED_COMMAND));
        assert_eq!(done, deltas);
        assert_eq!(
            field_of(
                &output,
                |data| data["type"] == "response.output_item.done"
                    && data["item"]["type"] == "custom_tool_call",
                &["item", "input"]
            ),
            restored(MASKED_COMMAND)
        );
        assert_eq!(
            non_delta_frames(&output).len(),
            non_delta_frames(&input).len()
        );
    }

    #[test]
    fn a_codex_output_text_is_restored_in_the_deltas_and_the_done_frame() {
        let input = rewrite_fixture(
            CODEX,
            codex_item("response.output_text.delta"),
            &["delta"],
            MASKED_TEXT,
        );
        let input = rewrite_fixture(
            &input,
            |data| data["type"] == "response.output_text.done",
            &["text"],
            MASKED_TEXT,
        );

        let output = run_every_chunking(&input);

        let deltas = collect_text(
            &output,
            |data| data["type"] == "response.output_text.delta",
            &["delta"],
        );
        assert_eq!(deltas, restored(MASKED_TEXT));
        assert_eq!(
            field_of(
                &output,
                |data| data["type"] == "response.output_text.done",
                &["text"]
            ),
            deltas
        );
        let untouched = |data: &Value| {
            let kind = data["type"].as_str().unwrap_or_default();
            !kind.starts_with("response.output_text.")
        };
        let untouched_frames = |bytes: &[u8]| -> Vec<Vec<u8>> {
            frames(bytes)
                .iter()
                .filter(|frame| json(frame).is_some_and(|data| untouched(&data)))
                .map(|frame| frame.bytes().to_vec())
                .collect()
        };
        assert_eq!(untouched_frames(&output), untouched_frames(&input));
    }

    #[test]
    fn a_chunk_boundary_inside_a_data_line_matches_the_whole_buffer_result() {
        let input = rewrite_fixture(
            ANTHROPIC,
            anthropic_block(1),
            &["delta", "partial_json"],
            MASKED_COMMAND,
        );
        let whole = run(std::iter::once(input.to_vec()));
        let prefix = b"AEGIS_";
        let data_offset = input
            .windows(prefix.len())
            .position(|window| window == prefix)
            .expect("placeholder prefix present")
            + 3;

        let split = run([input[..data_offset].to_vec(), input[data_offset..].to_vec()].into_iter());

        assert_eq!(split, whole);
    }

    #[test]
    fn empty_replacements_return_every_chunk_unchanged() {
        let mut restorer = StreamRestorer::new(Vec::new());
        let chunk = &ANTHROPIC[..40];
        assert_eq!(restorer.rewrite_chunk(chunk), chunk);
        assert!(restorer.finish().is_empty());
    }

    #[test]
    fn finish_flushes_text_still_held_back_when_the_stream_ends_without_a_stop() {
        let frame = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"token AEGIS_SECRET_0123456789abcdef0123\"}}\n\n";
        let mut restorer = StreamRestorer::new(replacements());

        let first = restorer.rewrite_chunk(frame);
        let flushed = restorer.finish();

        assert_eq!(
            field_of(
                &first,
                |data| data["type"] == "content_block_delta",
                &["delta", "text"]
            ),
            "token "
        );
        assert_eq!(
            field_of(
                &flushed,
                |data| data["type"] == "content_block_delta",
                &["delta", "text"]
            ),
            "AEGIS_SECRET_0123456789abcdef0123"
        );
    }

    #[test]
    fn held_back_text_is_emitted_before_the_stop_frame_and_the_stop_keeps_its_bytes() {
        let delta = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"AEGIS_SECRET_0123\"}}\n\n";
        let stop =
            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
        let mut restorer = StreamRestorer::new(replacements());

        let first = restorer.rewrite_chunk(delta);
        let second = restorer.rewrite_chunk(stop);

        assert_eq!(
            field_of(
                &first,
                |data| data["type"] == "content_block_delta",
                &["delta", "text"]
            ),
            ""
        );
        let emitted = frames(&second);
        assert_eq!(emitted.len(), 2);
        assert_eq!(
            field_of(
                emitted[0].bytes(),
                |data| data["type"] == "content_block_delta",
                &["delta", "text"]
            ),
            "AEGIS_SECRET_0123"
        );
        assert_eq!(emitted[1].bytes(), stop);
    }

    #[test]
    fn the_holdback_only_keeps_a_tail_that_could_still_start_a_placeholder() {
        let replacements = replacements();
        assert_eq!(
            holdback_start("hello world", &replacements),
            "hello world".len()
        );
        assert_eq!(holdback_start("run AEGIS_SEC", &replacements), "run ".len());
        assert_eq!(holdback_start("run A", &replacements), "run ".len());
        assert_eq!(holdback_start("run B", &replacements), "run B".len());
        assert_eq!(
            holdback_start(PLACEHOLDER, &replacements),
            PLACEHOLDER.len()
        );
    }

    #[test]
    fn a_non_streaming_json_body_is_restored_when_the_stream_ends() {
        let body = format!(
            r#"{{"id":"msg_0123","content":[{{"type":"text","text":"token {}"}}]}}"#,
            replacements()[0].placeholder
        );
        let mut restorer = StreamRestorer::new(replacements());

        let mut output = restorer.rewrite_chunk(body.as_bytes());
        output.extend(restorer.finish());

        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                r#"{{"id":"msg_0123","content":[{{"type":"text","text":"token {}"}}]}}"#,
                replacements()[0].original
            )
        );
    }
}
