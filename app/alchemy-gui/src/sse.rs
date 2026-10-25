//! An incremental parser for server-sent events, as `GET /api/events`
//! sends them.
//!
//! The same WHATWG event-stream rules as the CLI's parser: lines end in
//! CRLF, LF, or CR; a blank line dispatches the event; `:` starts a
//! comment, such as the daemon's keep-alives; several `data:` lines join
//! with newlines; `id` and `retry` are dropped. Bytes arrive in whatever
//! chunks the transport delivers, so a line may be split across calls to
//! [`SseParser::push`].

use thiserror::Error;

/// Upper bound on one event's bytes, comments and all, so a daemon that
/// never ends a line cannot exhaust memory.
pub const MAX_EVENT: usize = 1 << 20;

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// One dispatched event: its `event:` name and its joined `data:` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, or `message` when none was sent.
    pub event: String,
    /// Every `data:` line of the event, joined with `\n`.
    pub data: String,
}

/// Why a stream could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SseError {
    /// An event grew past [`MAX_EVENT`] bytes before it was dispatched.
    #[error("event stream: an event is longer than {MAX_EVENT} bytes")]
    TooLong,
}

/// Turns chunks of an event stream into [`SseEvent`]s.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
    pending: usize,
    after_cr: bool,
    started: bool,
}

impl SseParser {
    /// A parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next `chunk` and return the events it completed, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        let mut events = Vec::new();
        for &byte in chunk {
            let after_cr = std::mem::replace(&mut self.after_cr, false);
            if after_cr && byte == b'\n' {
                continue;
            }
            self.pending += 1;
            if self.pending > MAX_EVENT {
                return Err(SseError::TooLong);
            }
            if byte == b'\n' || byte == b'\r' {
                self.after_cr = byte == b'\r';
                let line = std::mem::take(&mut self.line);
                events.extend(self.line_ended(&line));
            } else {
                self.line.push(byte);
            }
        }
        Ok(events)
    }

    fn line_ended(&mut self, line: &[u8]) -> Option<SseEvent> {
        let first = !std::mem::replace(&mut self.started, true);
        let line = if first {
            line.strip_prefix(BOM).unwrap_or(line)
        } else {
            line
        };
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(b":") {
            return None;
        }
        let (name, value) = match line.iter().position(|b| *b == b':') {
            Some(colon) => {
                let (name, rest) = line.split_at(colon);
                let value = rest.get(1..).unwrap_or_default();
                (name, value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &[][..]),
        };
        let value = String::from_utf8_lossy(value);
        match name {
            b"event" => self.event = Some(value.into_owned()),
            b"data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(&value);
                }
                None => self.data = Some(value.into_owned()),
            },
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        self.pending = 0;
        let event = self.event.take();
        let data = self.data.take()?;
        Some(SseEvent {
            event: event
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "message".to_owned()),
            data,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.to_owned(),
            data: data.to_owned(),
        }
    }

    fn parse_chunks(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        chunks
            .iter()
            .flat_map(|chunk| parser.push(chunk).unwrap())
            .collect()
    }

    #[test]
    fn parses_whole_streams() {
        let cases: Vec<(&str, &[u8], Vec<SseEvent>)> = vec![
            (
                "engine event",
                b"event: engine\ndata: {\"state\":\"ready\"}\n\n",
                vec![ev("engine", "{\"state\":\"ready\"}")],
            ),
            ("no event name", b"data: x\n\n", vec![ev("message", "x")]),
            (
                "multi-line data",
                b"data: a\ndata: b\n\n",
                vec![ev("message", "a\nb")],
            ),
            ("keep-alive comment", b":\n\n", vec![]),
            ("event without data", b"event: engine\n\n", vec![]),
            ("crlf", b"event: a\r\ndata: x\r\n\r\n", vec![ev("a", "x")]),
            ("cr", b"event: a\rdata: x\r\r", vec![ev("a", "x")]),
            (
                "leading bom",
                b"\xEF\xBB\xBFdata: x\n\n",
                vec![ev("message", "x")],
            ),
            ("unterminated event", b"data: x\n", vec![]),
            (
                "two events",
                b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n",
                vec![ev("a", "1"), ev("b", "2")],
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(parse_chunks(&[input]), want, "{name}");
        }
    }

    #[test]
    fn any_split_gives_the_same_events() {
        let stream: &[u8] = b"event: engine\r\ndata: {\"a\":1}\r\n\r\n:\r\n\r\ndata: x\r\n\r\n";
        let whole = parse_chunks(&[stream]);
        assert_eq!(whole.len(), 2);
        for at in 0..=stream.len() {
            let (a, b) = stream.split_at(at);
            assert_eq!(parse_chunks(&[a, b]), whole, "split at {at}");
        }
    }

    #[test]
    fn an_event_longer_than_the_limit_is_an_error() {
        let mut parser = SseParser::new();
        assert_eq!(parser.push(b"data: "), Ok(vec![]));
        assert_eq!(parser.push(&vec![b'x'; MAX_EVENT]), Err(SseError::TooLong));
    }
}
