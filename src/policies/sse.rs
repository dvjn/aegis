/// A complete server-sent event, kept as the exact bytes it arrived in.
/// Field syntax follows https://html.spec.whatwg.org/multipage/server-sent-events.html
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    bytes: Vec<u8>,
}

struct Line<'a> {
    content: &'a [u8],
    terminator: &'a [u8],
}

impl<'a> Line<'a> {
    fn field(&self) -> Option<(&'a [u8], &'a [u8])> {
        if self.content.is_empty() || self.content[0] == b':' {
            return None;
        }
        match self.content.iter().position(|byte| *byte == b':') {
            Some(colon) => {
                let value = &self.content[colon + 1..];
                let value = value.strip_prefix(b" ").unwrap_or(value);
                Some((&self.content[..colon], value))
            }
            None => Some((self.content, b"")),
        }
    }
}

impl Frame {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg(test)]
    pub fn event(&self) -> Option<&str> {
        self.lines()
            .filter_map(|line| line.field())
            .find(|(name, _)| *name == b"event")
            .and_then(|(_, value)| std::str::from_utf8(value).ok())
    }

    pub fn data(&self) -> Option<String> {
        let mut data: Option<String> = None;
        for (_, value) in self
            .lines()
            .filter_map(|line| line.field())
            .filter(|(name, _)| *name == b"data")
        {
            let value = String::from_utf8_lossy(value);
            match data.as_mut() {
                Some(joined) => {
                    joined.push('\n');
                    joined.push_str(&value);
                }
                None => data = Some(value.into_owned()),
            }
        }
        data
    }

    pub fn with_data(&self, data: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.bytes.len() + data.len());
        let mut data_written = false;
        for line in self.lines() {
            let is_data = matches!(line.field(), Some((name, _)) if name == b"data");
            if !is_data {
                bytes.extend_from_slice(line.content);
                bytes.extend_from_slice(line.terminator);
            } else if !data_written {
                bytes.extend_from_slice(b"data: ");
                bytes.extend_from_slice(data.as_bytes());
                bytes.extend_from_slice(line.terminator);
                data_written = true;
            }
        }
        bytes
    }

    fn lines(&self) -> impl Iterator<Item = Line<'_>> {
        let mut rest = self.bytes.as_slice();
        std::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let newline = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(rest.len() - 1);
            let (line, remaining) = rest.split_at(newline + 1);
            rest = remaining;
            let terminator_len = if line.ends_with(b"\r\n") {
                2
            } else if line.ends_with(b"\n") {
                1
            } else {
                0
            };
            let (content, terminator) = line.split_at(line.len() - terminator_len);
            Some(Line {
                content,
                terminator,
            })
        })
    }
}

/// Splits arbitrary byte chunks into complete frames and buffers the partial
/// trailing frame until more bytes arrive.
#[derive(Debug, Default)]
pub struct FrameParser {
    buffer: Vec<u8>,
}

impl FrameParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = frame_end(&self.buffer) {
            let rest = self.buffer.split_off(end);
            let bytes = std::mem::replace(&mut self.buffer, rest);
            frames.push(Frame { bytes });
        }
        frames
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffer)
    }
}

fn frame_end(buffer: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    loop {
        let newline = line_start
            + buffer[line_start..]
                .iter()
                .position(|byte| *byte == b'\n')?;
        let content_end = if newline > line_start && buffer[newline - 1] == b'\r' {
            newline - 1
        } else {
            newline
        };
        if content_end == line_start {
            return Some(newline + 1);
        }
        line_start = newline + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n: keep-alive\n\nevent: delta\r\ndata: {\"a\":1}\r\n\r\n";

    fn parse_in_chunks(input: &[u8], chunk_len: usize) -> (Vec<Frame>, Vec<u8>) {
        let mut parser = FrameParser::default();
        let mut frames = Vec::new();
        for chunk in input.chunks(chunk_len) {
            frames.extend(parser.push(chunk));
        }
        (frames, parser.finish())
    }

    #[test]
    fn frames_are_split_at_blank_lines_and_keep_their_exact_bytes() {
        let (frames, leftover) = parse_in_chunks(STREAM, STREAM.len());

        assert_eq!(frames.len(), 3);
        assert!(leftover.is_empty());
        let rejoined: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame.bytes().to_vec())
            .collect();
        assert_eq!(rejoined, STREAM);
        assert_eq!(frames[0].event(), Some("ping"));
        assert_eq!(frames[0].data().as_deref(), Some(r#"{"type":"ping"}"#));
        assert_eq!(frames[1].event(), None);
        assert_eq!(frames[1].data(), None);
        assert_eq!(frames[2].event(), Some("delta"));
        assert_eq!(frames[2].data().as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn a_frame_split_mid_line_across_chunks_parses_to_the_same_frames() {
        let (whole, _) = parse_in_chunks(STREAM, STREAM.len());
        for chunk_len in 1..STREAM.len() {
            let (chunked, leftover) = parse_in_chunks(STREAM, chunk_len);
            assert_eq!(chunked, whole, "chunk length {chunk_len}");
            assert!(leftover.is_empty());
        }
    }

    #[test]
    fn a_partial_trailing_frame_stays_buffered_until_finish() {
        let mut parser = FrameParser::default();
        assert!(parser.push(b"event: delta\ndata: {\"a\"").is_empty());
        let frames = parser.push(b":1}\n\nevent: half");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data().as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(parser.finish(), b"event: half");
    }

    #[test]
    fn with_data_replaces_only_the_data_line_and_joins_multiline_data() {
        let mut parser = FrameParser::default();
        let frames = parser.push(b"event: delta\r\ndata: one\r\ndata: two\r\nid: 7\r\n\r\n");
        let [frame] = frames.as_slice() else {
            panic!("one frame expected");
        };

        assert_eq!(frame.data().as_deref(), Some("one\ntwo"));
        assert_eq!(
            frame.with_data("replaced"),
            b"event: delta\r\ndata: replaced\r\nid: 7\r\n\r\n"
        );
    }
}
