//! Incremental `text/event-stream` parser, hand-rolled because the subset
//! OpenAI-compatible endpoints use is this small.

use crate::chat::ChatError;

/// No legitimate SSE line approaches this; growth past it means the endpoint
/// is not an event stream, and buffering it whole would betray the RAM rules.
const MAX_LINE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseEvent {
    /// A `data:` payload, multi-line values already joined with `\n`.
    Data(String),
    /// The `data: [DONE]` sentinel.
    Done,
}

/// Bytes in, complete events out; no IO, so chunk boundaries are unit testable.
///
/// Errors ride beside the events already decoded from the same bytes, so a bad
/// line never costs the good ones before it. After an error the parser is dead
/// and further feeds return nothing.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    /// An unterminated line, kept as bytes because a chunk boundary can fall
    /// inside a multi-byte character. Splitting on `\n` before decoding is safe:
    /// UTF-8 continuation bytes are all >= 0x80.
    line: Vec<u8>,
    /// `data:` values of the event being assembled. A `Vec` (not a `String`) so
    /// that no field at all stays distinguishable from one empty field.
    data: Vec<String>,
    started: bool,
    dead: bool,
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> (Vec<SseEvent>, Option<ChatError>) {
        if self.dead {
            return (Vec::new(), None);
        }
        let mut out = Vec::new();
        let mut rest = chunk;
        while let Some(idx) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..idx]);
            rest = &rest[idx + 1..];
            let raw = std::mem::take(&mut self.line);
            if let Err(err) = self.handle_line(&raw, &mut out) {
                self.dead = true;
                return (out, Some(err));
            }
        }
        self.line.extend_from_slice(rest);
        if self.line.len() > MAX_LINE_BYTES {
            self.dead = true;
            return (
                out,
                Some(ChatError::Parse(
                    "stream line exceeded 1MiB; endpoint is not an event stream".to_string(),
                )),
            );
        }
        (out, None)
    }

    /// Flushes a trailing event left unterminated by a server that just closed
    /// the connection.
    pub(crate) fn finish(&mut self) -> (Vec<SseEvent>, Option<ChatError>) {
        if self.dead {
            return (Vec::new(), None);
        }
        let mut out = Vec::new();
        if !self.line.is_empty() {
            let raw = std::mem::take(&mut self.line);
            if let Err(err) = self.handle_line(&raw, &mut out) {
                self.dead = true;
                return (out, Some(err));
            }
        }
        self.dispatch(&mut out);
        (out, None)
    }

    fn handle_line(&mut self, raw: &[u8], out: &mut Vec<SseEvent>) -> Result<(), ChatError> {
        // CRLF streams leave the CR behind. A lone-CR delimiter, also legal per
        // the spec, is unsupported: no such server exists in practice, and it
        // would mean holding back a trailing CR until the next chunk arrives.
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let mut line = std::str::from_utf8(raw)
            .map_err(|_| ChatError::Parse("stream contained invalid UTF-8".to_string()))?;
        // The spec says strip a leading BOM; line level catches it even when a
        // chunk boundary split it.
        if !self.started {
            self.started = true;
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }

        if line.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        // A leading colon marks a comment or keep-alive.
        if line.starts_with(':') {
            return Ok(());
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        if field == "data" {
            self.data.push(value.to_string());
        }
        Ok(())
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            return;
        }
        let payload = self.data.join("\n");
        self.data.clear();
        if payload.trim() == "[DONE]" {
            out.push(SseEvent::Done);
        } else {
            out.push(SseEvent::Data(payload));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(payload: &str) -> SseEvent {
        SseEvent::Data(payload.to_string())
    }

    /// Feed that must not error.
    fn ok(parser: &mut SseParser, chunk: &[u8]) -> Vec<SseEvent> {
        let (events, err) = parser.feed(chunk);
        assert!(err.is_none(), "unexpected error: {err:?}");
        events
    }

    fn done(parser: &mut SseParser) -> Vec<SseEvent> {
        let (events, err) = parser.finish();
        assert!(err.is_none(), "unexpected error: {err:?}");
        events
    }

    #[test]
    fn parses_a_single_event() {
        let mut parser = SseParser::default();
        assert_eq!(
            ok(&mut parser, b"data: {\"a\":1}\n\n"),
            vec![data("{\"a\":1}")]
        );
    }

    #[test]
    fn parses_several_events_in_one_chunk() {
        let mut parser = SseParser::default();
        assert_eq!(
            ok(&mut parser, b"data: one\n\ndata: two\n\ndata: three\n\n"),
            vec![data("one"), data("two"), data("three")]
        );
    }

    #[test]
    fn value_keeps_everything_after_the_single_optional_space() {
        let mut parser = SseParser::default();
        // Only one space is stripped, so the second space survives.
        assert_eq!(
            ok(&mut parser, b"data:  leading\n\ndata:tight\n\n"),
            vec![data(" leading"), data("tight")]
        );
    }

    #[test]
    fn event_split_mid_line_across_chunks() {
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, b"data: {\"cont").is_empty());
        assert!(ok(&mut parser, b"ent\":\"hi\"}").is_empty());
        assert_eq!(ok(&mut parser, b"\n\n"), vec![data("{\"content\":\"hi\"}")]);
    }

    #[test]
    fn event_split_inside_the_data_prefix() {
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, b"da").is_empty());
        assert_eq!(ok(&mut parser, b"ta: hello\n\n"), vec![data("hello")]);
    }

    #[test]
    fn event_split_between_the_two_delimiter_newlines() {
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, b"data: hello\n").is_empty());
        assert_eq!(ok(&mut parser, b"\n"), vec![data("hello")]);
    }

    #[test]
    fn multi_byte_character_split_across_chunks() {
        let payload = "data: 日本語\n\n".as_bytes();
        // Byte 6 is the first byte of `日`; split inside that character.
        let (head, tail) = payload.split_at(7);
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, head).is_empty());
        assert_eq!(ok(&mut parser, tail), vec![data("日本語")]);
    }

    #[test]
    fn byte_wise_feed_of_a_realistic_stream() {
        let stream = concat!(
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"héllo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"🌍\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in stream.as_bytes() {
            events.extend(ok(&mut parser, &[*byte]));
        }
        assert_eq!(
            events,
            vec![
                data("{\"choices\":[{\"delta\":{\"content\":\"héllo\"}}]}"),
                data("{\"choices\":[{\"delta\":{\"content\":\"🌍\"}}]}"),
                SseEvent::Done,
            ]
        );
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut parser = SseParser::default();
        assert_eq!(
            ok(
                &mut parser,
                b"data: line one\ndata: line two\ndata: line three\n\n"
            ),
            vec![data("line one\nline two\nline three")]
        );
    }

    #[test]
    fn empty_data_field_is_an_event_with_an_empty_payload() {
        let mut parser = SseParser::default();
        assert_eq!(ok(&mut parser, b"data:\n\n"), vec![data("")]);
    }

    #[test]
    fn comments_and_keep_alives_are_ignored() {
        let mut parser = SseParser::default();
        assert_eq!(
            ok(
                &mut parser,
                b": ping\n\n:\n\n: OPENROUTER PROCESSING\ndata: payload\n\n"
            ),
            vec![data("payload")]
        );
    }

    #[test]
    fn non_data_fields_are_ignored() {
        let mut parser = SseParser::default();
        assert_eq!(
            ok(
                &mut parser,
                b"event: message\nid: 42\nretry: 1000\nbare-field\ndata: payload\n\n"
            ),
            vec![data("payload")]
        );
    }

    #[test]
    fn blank_lines_without_data_dispatch_nothing() {
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, b"\n\n\n").is_empty());
        assert!(ok(&mut parser, b"event: ping\n\n").is_empty());
    }

    #[test]
    fn done_sentinel_is_recognised() {
        let mut parser = SseParser::default();
        assert_eq!(ok(&mut parser, b"data: [DONE]\n\n"), vec![SseEvent::Done]);
    }

    #[test]
    fn crlf_delimiters_are_supported() {
        let mut parser = SseParser::default();
        assert_eq!(
            ok(
                &mut parser,
                b": keep-alive\r\n\r\ndata: first\r\ndata: second\r\n\r\ndata: [DONE]\r\n\r\n"
            ),
            vec![data("first\nsecond"), SseEvent::Done]
        );
    }

    #[test]
    fn crlf_stream_survives_a_split_inside_the_delimiter() {
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, b"data: hi\r").is_empty());
        assert!(ok(&mut parser, b"\n\r").is_empty());
        assert_eq!(ok(&mut parser, b"\n"), vec![data("hi")]);
    }

    #[test]
    fn finish_flushes_an_event_that_lost_its_trailing_blank_line() {
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, b"data: truncated").is_empty());
        assert_eq!(done(&mut parser), vec![data("truncated")]);
    }

    #[test]
    fn finish_on_a_clean_stream_yields_nothing() {
        let mut parser = SseParser::default();
        assert_eq!(ok(&mut parser, b"data: x\n\n"), vec![data("x")]);
        assert!(done(&mut parser).is_empty());
    }

    #[test]
    fn invalid_utf8_is_a_parse_error_that_keeps_prior_events() {
        let mut parser = SseParser::default();
        let (events, err) = parser.feed(b"data: good\n\ndata: \xff\xfe\n\n");
        assert_eq!(events, vec![data("good")]);
        assert!(matches!(err, Some(ChatError::Parse(_))), "got {err:?}");
        // Dead after an error: nothing more comes out.
        assert!(parser.feed(b"data: after\n\n").0.is_empty());
        assert!(parser.finish().0.is_empty());
    }

    #[test]
    fn leading_bom_is_stripped() {
        let mut parser = SseParser::default();
        assert_eq!(
            ok(&mut parser, b"\xef\xbb\xbfdata: first\n\n"),
            vec![data("first")]
        );
    }

    #[test]
    fn bom_split_across_chunks_is_still_stripped() {
        let mut parser = SseParser::default();
        assert!(ok(&mut parser, b"\xef").is_empty());
        assert!(ok(&mut parser, b"\xbb\xbf").is_empty());
        assert_eq!(ok(&mut parser, b"data: first\n\n"), vec![data("first")]);
    }

    #[test]
    fn a_newline_free_flood_is_cut_off_instead_of_buffered() {
        let mut parser = SseParser::default();
        let flood = vec![b'a'; MAX_LINE_BYTES + 2];
        let (events, err) = parser.feed(&flood);
        assert!(events.is_empty());
        assert!(matches!(err, Some(ChatError::Parse(_))), "got {err:?}");
    }
}
