//! App logs: turning a container's raw log bytes into lines, merging
//! several containers' tails in time order, and resuming a followed log
//! without repeating a line.
//!
//! The daemon asks the engine for each container's log with a timestamp
//! on every line, as Docker writes them (RFC 3339, UTC, nanoseconds). A
//! [`LineSplitter`] cuts the bytes into lines, [`split_timestamp`] takes
//! the timestamp off the front, [`merge_tail`] orders the containers'
//! tails into one, and a [`Cursor`] remembers where a container's log was
//! left so a reconnect can skip what was already sent.

use std::cmp::Ordering;
use std::fmt;

/// Lines sent when a request names no `tail`.
pub const DEFAULT_TAIL: u32 = 200;

/// The most lines a request may ask for. A larger `tail` is cut to this.
pub const MAX_TAIL: u32 = 5000;

/// The most bytes kept of one line. The rest of a longer line is dropped
/// and [`TRUNCATED`] marks the cut.
///
/// Below Docker's own 16 KiB split of long lines, so a line Docker splits
/// is always cut within its first piece.
pub const MAX_LINE: usize = 8 * 1024;

/// Appended to a line cut at [`MAX_LINE`].
pub const TRUNCATED: &str = "…";

/// Why a logs query was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LogQueryError {
    /// `tail` is not a whole number of lines.
    #[error("tail must be a whole number of lines, not {0:?}")]
    Tail(String),
    /// `follow` is not a yes or no.
    #[error("follow must be true or false, not {0:?}")]
    Follow(String),
}

/// The number of lines a `tail` query value asks for: [`DEFAULT_TAIL`]
/// when absent or empty, at most [`MAX_TAIL`].
pub fn parse_tail(value: Option<&str>) -> Result<u32, LogQueryError> {
    match value.map(str::trim) {
        None | Some("") => Ok(DEFAULT_TAIL),
        Some(text) => match text.parse::<u64>() {
            Ok(n) => Ok(u32::try_from(n).unwrap_or(MAX_TAIL).min(MAX_TAIL)),
            Err(_) => Err(LogQueryError::Tail(text.to_owned())),
        },
    }
}

/// Whether a `follow` query value asks to follow: `false` when absent,
/// `true` for an empty value, `true`, or `1`.
pub fn parse_follow(value: Option<&str>) -> Result<bool, LogQueryError> {
    match value {
        None | Some("false" | "0") => Ok(false),
        Some("" | "true" | "1") => Ok(true),
        Some(other) => Err(LogQueryError::Follow(other.to_owned())),
    }
}

/// A point in time to the nanosecond, as Docker stamps log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    secs: i64,
    nanos: u32,
}

impl Timestamp {
    /// `secs` and `nanos` since the Unix epoch. Nanoseconds past a
    /// second are carried into `secs`.
    pub fn new(secs: i64, nanos: u32) -> Self {
        Self {
            secs: secs.saturating_add(i64::from(nanos / 1_000_000_000)),
            nanos: nanos % 1_000_000_000,
        }
    }

    /// Whole seconds since the Unix epoch, rounded down.
    pub fn secs(self) -> i64 {
        self.secs
    }

    /// Nanoseconds past [`Self::secs`].
    pub fn nanos(self) -> u32 {
        self.nanos
    }

    /// Parse an RFC 3339 date and time, such as
    /// `2024-05-06T07:08:09.123456789Z` or `2024-05-06T09:08:09+02:00`.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let year = digits(bytes, 0, 4)?;
        let month = digits(bytes, 5, 2)?;
        let day = digits(bytes, 8, 2)?;
        let hour = digits(bytes, 11, 2)?;
        let minute = digits(bytes, 14, 2)?;
        let second = digits(bytes, 17, 2)?;
        let separators = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
        if separators
            .iter()
            .any(|&(at, want)| bytes.get(at) != Some(&want))
            || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
            || !(1..=12).contains(&month)
            || !(1..=days_in_month(year, month)).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return None;
        }
        let mut at = 19;
        let mut nanos = 0u32;
        if bytes.get(at) == Some(&b'.') {
            at += 1;
            let start = at;
            while let Some(d) = bytes.get(at).filter(|b| b.is_ascii_digit()) {
                if at - start < 9 {
                    nanos = nanos * 10 + u32::from(d - b'0');
                }
                at += 1;
            }
            let len = at - start;
            if len == 0 {
                return None;
            }
            for _ in len..9 {
                nanos *= 10;
            }
        }
        let offset = match bytes.get(at..)? {
            b"Z" | b"z" => 0,
            [sign @ (b'+' | b'-'), rest @ ..] if rest.len() == 5 && rest.get(2) == Some(&b':') => {
                let hours = digits(rest, 0, 2)?;
                let minutes = digits(rest, 3, 2)?;
                if hours > 23 || minutes > 59 {
                    return None;
                }
                let offset = (hours * 60 + minutes) * 60;
                if *sign == b'-' { -offset } else { offset }
            }
            _ => return None,
        };
        let days = days_from_civil(year, month, day);
        let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset;
        Some(Self::new(secs, nanos))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:09}", self.secs, self.nanos)
    }
}

fn digits(bytes: &[u8], at: usize, len: usize) -> Option<i64> {
    let slice = bytes.get(at..at + len)?;
    slice.iter().try_fold(0i64, |n, b| {
        b.is_ascii_digit().then(|| n * 10 + i64::from(b - b'0'))
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given civil date, after Howard Hinnant's
/// `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Split the timestamp Docker puts before each line from the rest. A
/// line that starts with no timestamp comes back whole.
pub fn split_timestamp(line: &str) -> (Option<(&str, Timestamp)>, &str) {
    if let Some((first, rest)) = line.split_once(' ')
        && let Some(at) = Timestamp::parse(first)
    {
        return (Some((first, at)), rest);
    }
    match Timestamp::parse(line) {
        Some(at) => (Some((line, at)), ""),
        None => (None, line),
    }
}

/// Cuts one stream of log bytes, delivered in any chunks, into lines.
///
/// Lines end at `\n`, with a `\r` before it dropped. Bytes that are not
/// UTF-8 become U+FFFD. A line longer than [`MAX_LINE`] bytes is cut at a
/// character boundary and marked with [`TRUNCATED`].
#[derive(Debug, Default, Clone)]
pub struct LineSplitter {
    line: Vec<u8>,
    cut: bool,
}

impl LineSplitter {
    /// A splitter at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next `chunk` and return the lines it completed, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut pieces = chunk.split(|b| *b == b'\n');
        let last = pieces.next_back().unwrap_or_default();
        for piece in pieces {
            self.keep(piece);
            lines.push(self.take());
        }
        self.keep(last);
        lines
    }

    /// The last line, if the stream ended without a final `\n`.
    pub fn finish(&mut self) -> Option<String> {
        (!self.line.is_empty() || self.cut).then(|| self.take())
    }

    fn keep(&mut self, bytes: &[u8]) {
        if self.cut {
            return;
        }
        let room = MAX_LINE.saturating_sub(self.line.len());
        if bytes.len() <= room {
            self.line.extend_from_slice(bytes);
            return;
        }
        self.line
            .extend_from_slice(bytes.get(..room).unwrap_or(bytes));
        let next = bytes.get(room).copied();
        if next.is_some_and(is_continuation) {
            let lead = self
                .line
                .iter()
                .rposition(|b| !is_continuation(*b))
                .unwrap_or(0);
            self.line.truncate(lead);
        }
        self.cut = true;
    }

    fn take(&mut self) -> String {
        let mut bytes = std::mem::take(&mut self.line);
        if !self.cut && bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let mut line = String::from_utf8_lossy(&bytes).into_owned();
        if std::mem::take(&mut self.cut) {
            line.push_str(TRUNCATED);
        }
        line
    }
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// Merge each source's lines into one list in time order and keep the
/// last `tail`.
///
/// Each source is one container's lines in the order it logged them,
/// which is kept. `at` gives a line's timestamp; a line without one sorts
/// with the line before it in its source. Lines at the same time keep
/// the order of their sources.
pub fn merge_tail<T>(
    sources: Vec<Vec<T>>,
    tail: usize,
    at: impl Fn(&T) -> Option<Timestamp>,
) -> Vec<T> {
    let total: usize = sources.iter().map(Vec::len).sum();
    let mut heads: Vec<(
        Option<Timestamp>,
        std::iter::Peekable<std::vec::IntoIter<T>>,
    )> = sources
        .into_iter()
        .map(|lines| (None, lines.into_iter().peekable()))
        .collect();
    let mut merged = Vec::with_capacity(total.min(tail));
    let skip = total.saturating_sub(tail);
    let mut seen = 0;
    loop {
        let mut best: Option<(usize, Option<Timestamp>)> = None;
        for (index, (last, lines)) in heads.iter_mut().enumerate() {
            let Some(line) = lines.peek() else { continue };
            let key = at(line).or(*last);
            let better = match best {
                None => true,
                Some((_, best_key)) => key.cmp(&best_key) == Ordering::Less,
            };
            if better {
                best = Some((index, key));
            }
        }
        let Some((index, key)) = best else { break };
        let Some((last, lines)) = heads.get_mut(index) else {
            break;
        };
        *last = key;
        if let Some(line) = lines.next() {
            if seen >= skip {
                merged.push(line);
            }
            seen += 1;
        }
    }
    merged
}

/// Where one container's log was left, so following it again after its
/// stream ends skips every line already sent.
///
/// The engine can only start a log at a whole second, so a resumed
/// stream repeats the lines from the start of that second; the cursor
/// drops them by their timestamps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    last: Option<Timestamp>,
    at_last: usize,
    skip: usize,
}

impl Cursor {
    /// A cursor before the first line.
    pub fn new() -> Self {
        Self::default()
    }

    /// The second to ask the engine to start from: the last line's,
    /// rounded down, or `None` from the beginning.
    pub fn since(&self) -> Option<i64> {
        self.last.map(Timestamp::secs)
    }

    /// Note that a line at `at` was sent, or dropped as too old for the
    /// tail, without checking it.
    pub fn record(&mut self, at: Option<Timestamp>) {
        let Some(at) = at else { return };
        match self.last {
            Some(last) if at < last => {}
            Some(last) if at == last => self.at_last += 1,
            _ => {
                self.last = Some(at);
                self.at_last = 1;
            }
        }
    }

    /// Start reading a stream that begins at [`Self::since`] again.
    pub fn resume(&mut self) {
        self.skip = self.at_last;
    }

    /// Whether a line at `at` on a resumed stream is new, noting it if so.
    /// A line without a timestamp is always new.
    pub fn admit(&mut self, at: Option<Timestamp>) -> bool {
        let Some(at) = at else { return true };
        match self.last {
            Some(last) if at < last => false,
            Some(last) if at == last && self.skip > 0 => {
                self.skip -= 1;
                false
            }
            _ => {
                self.skip = 0;
                self.record(Some(at));
                true
            }
        }
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

    #[test]
    fn parses_tail() {
        let cases = [
            (None, Ok(200)),
            (Some(""), Ok(200)),
            (Some("0"), Ok(0)),
            (Some("50"), Ok(50)),
            (Some(" 7 "), Ok(7)),
            (Some("5000"), Ok(5000)),
            (Some("5001"), Ok(5000)),
            (Some("99999999999999"), Ok(5000)),
            (Some("-1"), Err(LogQueryError::Tail("-1".into()))),
            (Some("all"), Err(LogQueryError::Tail("all".into()))),
            (Some("1.5"), Err(LogQueryError::Tail("1.5".into()))),
        ];
        for (input, want) in cases {
            assert_eq!(parse_tail(input), want, "{input:?}");
        }
    }

    #[test]
    fn parses_follow() {
        let cases = [
            (None, Ok(false)),
            (Some(""), Ok(true)),
            (Some("true"), Ok(true)),
            (Some("1"), Ok(true)),
            (Some("false"), Ok(false)),
            (Some("0"), Ok(false)),
            (Some("yes"), Err(LogQueryError::Follow("yes".into()))),
        ];
        for (input, want) in cases {
            assert_eq!(parse_follow(input), want, "{input:?}");
        }
    }

    #[test]
    fn parses_timestamps() {
        let ts = |secs, nanos| Some(Timestamp::new(secs, nanos));
        let cases = [
            ("1970-01-01T00:00:00Z", ts(0, 0)),
            (
                "2024-05-06T07:08:09.123456789Z",
                ts(1_714_979_289, 123_456_789),
            ),
            ("2024-05-06T07:08:09.5Z", ts(1_714_979_289, 500_000_000)),
            (
                "2024-05-06T07:08:09.1234567891Z",
                ts(1_714_979_289, 123_456_789),
            ),
            ("2024-05-06T09:08:09+02:00", ts(1_714_979_289, 0)),
            ("2024-05-06T06:08:09.000000001-01:00", ts(1_714_979_289, 1)),
            ("2024-02-29T00:00:00Z", ts(1_709_164_800, 0)),
            ("1969-12-31T23:59:59Z", ts(-1, 0)),
            ("2023-02-29T00:00:00Z", None),
            ("2024-13-01T00:00:00Z", None),
            ("2024-05-06T24:00:00Z", None),
            ("2024-05-06T07:08:09", None),
            ("2024-05-06T07:08:09.Z", None),
            ("2024-05-06T07:08:09+0200", None),
            ("2024-05-06X07:08:09Z", None),
            ("hello", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(Timestamp::parse(input), want, "{input}");
        }
        assert!(
            Timestamp::parse("2024-05-06T07:08:09.5Z") > Timestamp::parse("2024-05-06T07:08:09Z")
        );
    }

    #[test]
    fn splits_timestamps_from_lines() {
        let at = Timestamp::parse("2024-05-06T07:08:09.1Z").unwrap();
        let cases = [
            (
                "2024-05-06T07:08:09.1Z hello world",
                Some(("2024-05-06T07:08:09.1Z", at)),
                "hello world",
            ),
            (
                "2024-05-06T07:08:09.1Z ",
                Some(("2024-05-06T07:08:09.1Z", at)),
                "",
            ),
            (
                "2024-05-06T07:08:09.1Z",
                Some(("2024-05-06T07:08:09.1Z", at)),
                "",
            ),
            ("hello world", None, "hello world"),
            ("", None, ""),
            ("not-a-time x", None, "not-a-time x"),
        ];
        for (input, time, line) in cases {
            assert_eq!(split_timestamp(input), (time, line), "{input:?}");
        }
    }

    type SplitCase<'a> = (&'a str, Vec<&'a [u8]>, Vec<&'a str>);
    type Line = (String, Option<Timestamp>);
    type MergeCase<'a> = (&'a str, Vec<Vec<Line>>, usize, Vec<&'a str>);

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = LineSplitter::new();
        let mut lines: Vec<String> = chunks.iter().flat_map(|c| splitter.push(c)).collect();
        lines.extend(splitter.finish());
        lines
    }

    #[test]
    fn splits_lines() {
        let long = "x".repeat(MAX_LINE + 10);
        let at_limit = "y".repeat(MAX_LINE);
        let mut straddling = "z".repeat(MAX_LINE - 1);
        straddling.push('é');
        let want_long = format!("{}{TRUNCATED}", "x".repeat(MAX_LINE));
        let want_straddling = format!("{}{TRUNCATED}", "z".repeat(MAX_LINE - 1));
        let cases: Vec<SplitCase> = vec![
            ("one line", vec![b"a\n"], vec!["a"]),
            ("two lines", vec![b"a\nb\n"], vec!["a", "b"]),
            (
                "split across chunks",
                vec![b"he", b"llo\nwo", b"rld\n"],
                vec!["hello", "world"],
            ),
            ("no final newline", vec![b"a\nb"], vec!["a", "b"]),
            ("crlf", vec![b"a\r\nb\r\n"], vec!["a", "b"]),
            ("cr split from lf", vec![b"a\r", b"\n"], vec!["a"]),
            ("empty lines", vec![b"\n\n"], vec!["", ""]),
            ("nothing", vec![b""], vec![]),
            ("invalid utf-8", vec![b"a\xffb\n"], vec!["a\u{fffd}b"]),
            (
                "utf-8 split across chunks",
                vec![b"\xc3", b"\xa9\n"],
                vec!["é"],
            ),
            (
                "too long",
                vec![long.as_bytes(), b"\nnext\n"],
                vec![&want_long, "next"],
            ),
            (
                "at the limit",
                vec![at_limit.as_bytes(), b"\n"],
                vec![&at_limit],
            ),
            (
                "cut mid character",
                vec![straddling.as_bytes(), b"\n"],
                vec![&want_straddling],
            ),
            (
                "too long without newline",
                vec![long.as_bytes()],
                vec![&want_long],
            ),
        ];
        for (name, chunks, want) in cases {
            assert_eq!(split(&chunks), want, "{name}");
        }
    }

    #[test]
    fn any_split_gives_the_same_lines() {
        let input = b"2024-05-06T07:08:09Z hi\r\n\xc3\xa9t\xc3\xa9\nlast";
        let whole = split(&[input]);
        for at in 0..=input.len() {
            let (a, b) = input.split_at(at);
            assert_eq!(split(&[a, b]), whole, "split at {at}");
        }
    }

    fn line(source: &str, secs: Option<i64>) -> (String, Option<Timestamp>) {
        (source.to_owned(), secs.map(|s| Timestamp::new(s, 0)))
    }

    #[test]
    fn merges_tails_in_time_order() {
        let web = vec![
            line("web1", Some(1)),
            line("web3", Some(3)),
            line("web5", Some(5)),
        ];
        let db = vec![
            line("db2", Some(2)),
            line("db3", Some(3)),
            line("db4", Some(4)),
        ];
        let untimed = vec![line("u1", Some(1)), line("u?", None), line("u6", Some(6))];
        let names =
            |lines: Vec<Line>| -> Vec<String> { lines.into_iter().map(|(name, _)| name).collect() };
        let cases: Vec<MergeCase> = vec![
            (
                "interleaved",
                vec![web.clone(), db.clone()],
                10,
                vec!["web1", "db2", "web3", "db3", "db4", "web5"],
            ),
            (
                "tail keeps the newest",
                vec![web.clone(), db.clone()],
                3,
                vec!["db3", "db4", "web5"],
            ),
            ("tail of zero", vec![web.clone(), db.clone()], 0, vec![]),
            ("one source", vec![web.clone()], 2, vec!["web3", "web5"]),
            ("no sources", vec![], 5, vec![]),
            ("empty sources", vec![vec![], vec![]], 5, vec![]),
            (
                "a line without a time stays after its predecessor",
                vec![untimed, db.clone()],
                10,
                vec!["u1", "u?", "db2", "db3", "db4", "u6"],
            ),
            (
                "out of order source keeps its order",
                vec![
                    vec![line("a5", Some(5)), line("a1", Some(1))],
                    vec![line("b3", Some(3))],
                ],
                10,
                vec!["b3", "a5", "a1"],
            ),
        ];
        for (name, sources, tail, want) in cases {
            assert_eq!(
                names(merge_tail(sources, tail, |(_, at)| *at)),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn a_cursor_skips_what_was_sent() {
        let t = |secs, nanos| Some(Timestamp::new(secs, nanos));
        let mut cursor = Cursor::new();
        assert_eq!(cursor.since(), None);
        for at in [t(9, 0), t(10, 1), t(10, 5), t(10, 5), None] {
            cursor.record(at);
        }
        assert_eq!(cursor.since(), Some(10));

        cursor.resume();
        let resumed = [
            (t(10, 0), false),
            (t(10, 1), false),
            (t(10, 5), false),
            (t(10, 5), false),
            (t(10, 5), true),
            (None, true),
            (t(10, 6), true),
            (t(10, 6), true),
            (t(10, 2), false),
        ];
        for (at, want) in resumed {
            assert_eq!(cursor.admit(at), want, "{at:?}");
        }
        assert_eq!(cursor.since(), Some(10));

        cursor.resume();
        let again = [(t(10, 6), false), (t(10, 6), false), (t(11, 0), true)];
        for (at, want) in again {
            assert_eq!(cursor.admit(at), want, "again {at:?}");
        }

        let mut fresh = Cursor::new();
        fresh.resume();
        assert!(fresh.admit(t(1, 0)));
        assert!(fresh.admit(t(1, 0)));
    }
}
