//! Reasoning that models write into the answer itself: Qwen on Groq and the R1
//! distills stream a literal `<think>…</think>` block inside `content`, removed
//! here as the stream flows. A `reasoning_content` field never reaches here —
//! neither provider deserializes one.

use futures::stream::{BoxStream, StreamExt};

use crate::chat::{ChatError, ChatEvent};

/// Matched case-insensitively, attributes allowed.
const TAGS: [&str; 6] = [
    "think",
    "thinking",
    "reason",
    "reasoning",
    "reflection",
    "scratchpad",
];

/// Removes reasoning blocks delta by delta. Deltas do not arrive on tag
/// boundaries, so a tag can straddle two of them (`"<thi"` then `"nk>"`).
#[derive(Debug, Default)]
pub struct ReasoningFilter {
    /// Inside a block: everything is dropped until it closes.
    inside: bool,
    /// A trailing `<…` fragment awaiting the rest of its tag.
    held: String,
    seen: bool,
}

impl ReasoningFilter {
    /// The visible part of one delta; empty means nothing should be emitted.
    pub fn push(&mut self, delta: &str) -> String {
        let text = if self.held.is_empty() {
            delta.to_string()
        } else {
            format!("{}{delta}", std::mem::take(&mut self.held))
        };
        self.held.clear();
        if let Some(at) = text.rfind('<') {
            if partial_tag(&text[at..]) {
                self.held = text[at..].to_string();
                return self.take(&text[..at]);
            }
        }
        self.take(&text)
    }

    /// Whatever is still held when the stream ends: a `<` that never became a
    /// tag is ordinary text and must not be swallowed.
    pub fn flush(&mut self) -> String {
        let held = std::mem::take(&mut self.held);
        self.take(&held)
    }

    fn take(&mut self, text: &str) -> String {
        let mut out = String::new();
        let mut copied = 0;
        let mut at = 0;
        while let Some(next) = text[at..].find('<') {
            let start = at + next;
            let Some((closing, len)) = tag_at(&text[start..]) else {
                at = start + 1;
                continue;
            };
            if !self.inside {
                if closing && !self.seen {
                    // No open tag: the provider stripped it, so what came before
                    // was the thinking.
                    out.clear();
                } else {
                    out.push_str(&text[copied..start]);
                }
            }
            self.inside = !closing;
            at = start + len;
            copied = at;
        }
        if !self.inside {
            out.push_str(&text[copied..]);
        }
        // Models leave a blank line after the block; the answer must not open
        // with it.
        let visible = if self.seen {
            out
        } else {
            out.trim_start().to_string()
        };
        if !visible.is_empty() {
            self.seen = true;
        }
        visible
    }
}

/// The reasoning tag at the start of `text`: whether it closes a block, and
/// its byte length. `None` for any other tag.
fn tag_at(text: &str) -> Option<(bool, usize)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let mut at = 1;
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    let closing = bytes.get(at) == Some(&b'/');
    if closing {
        at += 1;
    }
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    let start = at;
    while bytes.get(at).is_some_and(u8::is_ascii_alphanumeric) {
        at += 1;
    }
    let name = text.get(start..at)?.to_ascii_lowercase();
    if !TAGS.contains(&name.as_str()) {
        return None;
    }
    // Attributes are tolerated: everything up to `>` is tag.
    let end = bytes[at..].iter().position(|byte| *byte == b'>')?;
    Some((closing, at + end + 1))
}

/// Whether a trailing `<…` could still become a reasoning tag: `<thi` waits,
/// `<h` goes out now. Being too permissive only delays text by one delta.
fn partial_tag(fragment: &str) -> bool {
    let Some(rest) = fragment.strip_prefix('<') else {
        return false;
    };
    // Already closed: a complete tag, not a fragment.
    if rest.contains('>') {
        return false;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('/').unwrap_or(rest).trim_start();
    let name: String = rest
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    if name.len() < rest.len() {
        // The name is finished, so only a real one is still waiting on its `>`.
        TAGS.contains(&name.as_str())
    } else {
        TAGS.iter().any(|tag| tag.starts_with(&name))
    }
}

/// Wraps a provider stream so reasoning never reaches the caller. Deltas left
/// empty are dropped; anything held back is released before `Done`.
pub fn strip_reasoning<'a>(
    inner: BoxStream<'a, Result<ChatEvent, ChatError>>,
) -> BoxStream<'a, Result<ChatEvent, ChatError>> {
    futures::stream::unfold(Strip::new(inner), |mut state| async move {
        if let Some(event) = state.pending.take() {
            return Some((Ok(event), state));
        }
        if state.ended {
            return None;
        }
        loop {
            let Some(event) = state.inner.next().await else {
                // Ending without `Done` must not swallow held text.
                state.ended = true;
                let rest = state.filter.flush();
                if rest.is_empty() {
                    return None;
                }
                return Some((Ok(ChatEvent::TextDelta(rest)), state));
            };
            match event {
                Ok(ChatEvent::TextDelta(delta)) => {
                    let visible = state.filter.push(&delta);
                    // All reasoning: wait rather than emit an empty delta.
                    if visible.is_empty() {
                        continue;
                    }
                    return Some((Ok(ChatEvent::TextDelta(visible)), state));
                }
                Ok(done @ ChatEvent::Done { .. }) => {
                    let rest = state.filter.flush();
                    if rest.is_empty() {
                        return Some((Ok(done), state));
                    }
                    state.pending = Some(done);
                    return Some((Ok(ChatEvent::TextDelta(rest)), state));
                }
                other => return Some((other, state)),
            }
        }
    })
    .boxed()
}

struct Strip<'a> {
    inner: BoxStream<'a, Result<ChatEvent, ChatError>>,
    filter: ReasoningFilter,
    /// A `Done` held back while the last of the text goes out first.
    pending: Option<ChatEvent>,
    /// The inner stream is exhausted; polling it again is not allowed.
    ended: bool,
}

impl<'a> Strip<'a> {
    fn new(inner: BoxStream<'a, Result<ChatEvent, ChatError>>) -> Self {
        Self {
            inner,
            filter: ReasoningFilter::default(),
            pending: None,
            ended: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Usage;
    use futures::executor::block_on;

    fn filter(deltas: &[&str]) -> String {
        let mut filter = ReasoningFilter::default();
        let mut out = String::new();
        for delta in deltas {
            out.push_str(&filter.push(delta));
        }
        out.push_str(&filter.flush());
        out
    }

    #[test]
    fn a_whole_block_in_one_delta_is_dropped() {
        assert_eq!(
            filter(&["<think>the user wants 4</think>\n\n4"]),
            "4",
            "the block and the blank line after it both go"
        );
    }

    #[test]
    fn a_tag_split_across_deltas_is_still_a_tag() {
        assert_eq!(filter(&["<thi", "nk>secret</thi", "nk>answer"]), "answer");
    }

    #[test]
    fn text_before_and_after_a_block_survives() {
        assert_eq!(filter(&["a", "<think>x</think>", "b"]), "ab");
    }

    #[test]
    fn a_close_tag_with_no_opener_discards_what_came_before_it() {
        // Only what has not been emitted yet can be taken back, so this
        // holds within a delta, not across them.
        assert_eq!(filter(&["reasoning about it</think>\n\nanswer"]), "answer");
    }

    #[test]
    fn a_close_tag_after_visible_text_does_not_eat_the_answer() {
        assert_eq!(filter(&["answer ", "</think>", " more"]), "answer  more");
    }

    #[test]
    fn tags_are_matched_case_insensitively_and_with_attributes() {
        assert_eq!(filter(&["<THINK type=\"x\">x</Think>ok"]), "ok");
        assert_eq!(filter(&["< think >x< / think >ok"]), "ok");
    }

    #[test]
    fn every_named_reasoning_tag_is_stripped() {
        for tag in TAGS {
            assert_eq!(filter(&[&format!("<{tag}>x</{tag}>ok")]), "ok", "{tag}");
        }
    }

    #[test]
    fn ordinary_angle_brackets_are_left_alone() {
        assert_eq!(filter(&["if a < b and c > d"]), "if a < b and c > d");
        assert_eq!(filter(&["<html>", "<div>"]), "<html><div>");
        assert_eq!(filter(&["use `Vec<T>` here"]), "use `Vec<T>` here");
    }

    #[test]
    fn a_trailing_bracket_that_never_becomes_a_tag_is_released() {
        assert_eq!(filter(&["a <"]), "a <", "flush lets go of what it held");
        assert_eq!(filter(&["a <", "b"]), "a <b");
    }

    #[test]
    fn an_unterminated_block_hides_the_rest_of_the_stream() {
        assert_eq!(filter(&["<think>on and on"]), "");
    }

    #[test]
    fn leading_whitespace_is_trimmed_only_until_the_answer_starts() {
        assert_eq!(
            filter(&["<think>x</think>", "\n\n", "a", "\n\nb"]),
            "a\n\nb"
        );
    }

    fn drive(events: Vec<Result<ChatEvent, ChatError>>) -> Vec<Result<ChatEvent, ChatError>> {
        let inner = futures::stream::iter(events).boxed();
        block_on(strip_reasoning(inner).collect())
    }

    fn delta(text: &str) -> Result<ChatEvent, ChatError> {
        Ok(ChatEvent::TextDelta(text.to_string()))
    }

    #[test]
    fn the_stream_drops_deltas_that_were_all_reasoning() {
        let done = Ok(ChatEvent::Done {
            usage: Some(Usage {
                input_tokens: 1,
                output_tokens: 2,
            }),
        });
        let events = drive(vec![
            delta("<think>"),
            delta("thinking hard"),
            delta("</think>answer"),
            done.clone(),
        ]);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "answer"));
        assert_eq!(events[1].as_ref().ok(), done.as_ref().ok());
    }

    #[test]
    fn held_text_is_released_before_done() {
        let events = drive(vec![delta("a <"), Ok(ChatEvent::Done { usage: None })]);
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "a "));
        assert!(matches!(&events[1], Ok(ChatEvent::TextDelta(text)) if text == "<"));
        assert!(matches!(&events[2], Ok(ChatEvent::Done { usage: None })));
    }

    #[test]
    fn an_error_passes_through_and_ends_the_stream() {
        let events = drive(vec![
            delta("<think>x</think>ok"),
            Err(ChatError::Network("reset".to_string())),
        ]);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "ok"));
        assert!(matches!(&events[1], Err(ChatError::Network(_))));
    }

    #[test]
    fn a_reply_that_is_only_reasoning_yields_no_text_at_all() {
        let events = drive(vec![
            delta("<think>all of it</think>"),
            Ok(ChatEvent::Done { usage: None }),
        ]);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(&events[0], Ok(ChatEvent::Done { .. })));
    }
}
