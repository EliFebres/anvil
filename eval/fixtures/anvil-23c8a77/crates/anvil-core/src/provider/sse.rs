//! Server-Sent Events framing, independent of any provider.
//!
//! Bytes go in, frames come out. Lines are only decoded once complete, so a
//! UTF-8 sequence split across two network chunks is never mangled.

/// One parsed line of an SSE stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseLine<'a> {
    /// `event: <name>`.
    Event(&'a str),
    /// `data: <payload>`.
    Data(&'a str),
    /// An empty line, which ends the current frame.
    Blank,
    /// A comment, `id:`, `retry:` or anything else the parser does not use.
    Ignored,
}

/// Classifies one line.
///
/// A single space after the colon is stripped, as the SSE spec requires, and a
/// trailing carriage return is ignored.
///
/// # Arguments
///
/// * `line` - One line without its line feed.
///
/// # Returns
///
/// The line's role in the stream.
pub fn parse_sse_line(line: &str) -> SseLine<'_> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        return SseLine::Blank;
    }
    if let Some(value) = line.strip_prefix("data:") {
        return SseLine::Data(value.strip_prefix(' ').unwrap_or(value));
    }
    if let Some(value) = line.strip_prefix("event:") {
        return SseLine::Event(value.strip_prefix(' ').unwrap_or(value));
    }
    SseLine::Ignored
}

/// One complete event: an optional name and its data.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    /// The `event:` name, if the frame had one.
    pub event: Option<String>,
    /// All `data:` lines joined with line feeds.
    pub data: String,
}

/// Turns byte chunks into frames.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current incomplete line.
    buf: Vec<u8>,
    /// The frame being assembled.
    current: SseFrame,
    /// Whether `current` has received any data line.
    has_data: bool,
}

impl SseParser {
    /// Feeds bytes and returns every frame they complete.
    ///
    /// # Arguments
    ///
    /// * `chunk` - The next bytes from the network, split anywhere.
    ///
    /// # Returns
    ///
    /// The frames completed by this chunk, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        for byte in chunk {
            if *byte != b'\n' {
                self.buf.push(*byte);
                continue;
            }
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            if let Some(frame) = self.take_line(&line) {
                frames.push(frame);
            }
        }
        frames
    }

    /// Flushes a final frame that was not followed by a blank line.
    ///
    /// # Returns
    ///
    /// The pending frame, if it has any data.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            if let Some(frame) = self.take_line(&line) {
                return Some(frame);
            }
        }
        self.take_frame()
    }

    /// Applies one complete line; returns a frame when a blank line closes one.
    fn take_line(&mut self, line: &str) -> Option<SseFrame> {
        match parse_sse_line(line) {
            SseLine::Event(name) => {
                self.current.event = Some(name.to_owned());
                None
            }
            SseLine::Data(value) => {
                if self.has_data {
                    self.current.data.push('\n');
                }
                self.current.data.push_str(value);
                self.has_data = true;
                None
            }
            SseLine::Blank => self.take_frame(),
            SseLine::Ignored => None,
        }
    }

    /// Returns the pending frame if it has data, and resets.
    fn take_frame(&mut self) -> Option<SseFrame> {
        if !self.has_data {
            self.current = SseFrame::default();
            return None;
        }
        self.has_data = false;
        Some(std::mem::take(&mut self.current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_data_strips_single_space() {
        assert_eq!(
            parse_sse_line("data: {\"a\":1}"),
            SseLine::Data("{\"a\":1}")
        );
        assert_eq!(parse_sse_line("data:{"), SseLine::Data("{"));
        assert_eq!(parse_sse_line("data:  two"), SseLine::Data(" two"));
    }

    #[test]
    fn parse_line_event_comment_blank_cr() {
        assert_eq!(parse_sse_line("event: ping"), SseLine::Event("ping"));
        assert_eq!(parse_sse_line(": keepalive"), SseLine::Ignored);
        assert_eq!(parse_sse_line("id: 7"), SseLine::Ignored);
        assert_eq!(parse_sse_line(""), SseLine::Blank);
        assert_eq!(parse_sse_line("\r"), SseLine::Blank);
        assert_eq!(parse_sse_line("data: x\r"), SseLine::Data("x"));
    }

    #[test]
    fn parser_frame_split_across_chunks() {
        let mut parser = SseParser::default();
        let text = "data: {\"t\":\"héllo\"}\n\n".as_bytes();
        // Split inside the two-byte "é".
        let cut = text.iter().position(|b| *b == 0xC3).expect("multibyte");
        assert!(parser.push(&text[..cut + 1]).is_empty());
        assert!(parser.push(&text[cut + 1..cut + 5]).is_empty());
        let frames = parser.push(&text[cut + 5..]);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "{\"t\":\"héllo\"}".to_owned()
            }]
        );
    }

    #[test]
    fn parser_multiline_data_joined() {
        let mut parser = SseParser::default();
        let frames = parser.push(b"data: a\ndata: b\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "a\nb");
    }

    #[test]
    fn parser_finish_flushes_unterminated_frame() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: tail").is_empty());
        assert_eq!(
            parser.finish(),
            Some(SseFrame {
                event: None,
                data: "tail".to_owned()
            })
        );
        assert_eq!(parser.finish(), None);
    }

    #[test]
    fn parser_event_and_data_pair() {
        let mut parser = SseParser::default();
        let frames = parser.push(b"event: x\ndata: y\n\n: comment\n\ndata: z\n\n");
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: Some("x".to_owned()),
                    data: "y".to_owned()
                },
                SseFrame {
                    event: None,
                    data: "z".to_owned()
                },
            ]
        );
    }

    #[test]
    fn parser_event_without_data_is_dropped() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: ping\n\n").is_empty());
        let frames = parser.push(b"data: ok\n\n");
        assert_eq!(frames[0].event, None);
    }
}
