//! Splitting a child's output into lines as it arrives.

/// The longest line kept, in bytes. Anything past it, up to the next
/// newline, is dropped, so a child that never writes a newline cannot grow
/// the daemon's memory.
pub(super) const MAX_LINE: usize = alchemy_core::operation::MAX_OUTPUT_LINE;

/// Turns chunks of bytes into complete lines.
///
/// Lines end at `\n`; a trailing `\r` is dropped, and invalid UTF-8 is
/// replaced rather than rejected.
#[derive(Debug)]
pub(super) struct LineSplitter {
    max: usize,
    pending: Vec<u8>,
    overflowed: bool,
}

impl LineSplitter {
    pub(super) fn new(max: usize) -> Self {
        Self {
            max,
            pending: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed `chunk` and return the lines it completed.
    pub(super) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            let (line, tail) = rest.split_at(end);
            self.append(line);
            lines.push(self.take());
            rest = tail.get(1..).unwrap_or_default();
        }
        self.append(rest);
        lines
    }

    /// The last line, if the output did not end with a newline.
    pub(super) fn finish(mut self) -> Option<String> {
        (!self.pending.is_empty() || self.overflowed).then(|| self.take())
    }

    fn append(&mut self, bytes: &[u8]) {
        let room = self.max.saturating_sub(self.pending.len());
        if bytes.len() > room {
            self.overflowed = true;
        }
        self.pending
            .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
    }

    fn take(&mut self) -> String {
        let mut line = std::mem::take(&mut self.pending);
        self.overflowed = false;
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line)
            .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
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

    type Chunks = &'static [&'static [u8]];

    fn split(max: usize, chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = LineSplitter::new(max);
        let mut lines: Vec<String> = chunks.iter().flat_map(|c| splitter.push(c)).collect();
        lines.extend(splitter.finish());
        lines
    }

    #[test]
    fn splits_chunks_into_lines() {
        let cases: [(usize, Chunks, &[&str]); 12] = [
            (64, &[], &[]),
            (64, &[b""], &[]),
            (64, &[b"one\ntwo\n"], &["one", "two"]),
            (64, &[b"one\ntwo"], &["one", "two"]),
            (64, &[b"on", b"e\nt", b"wo\n"], &["one", "two"]),
            (64, &[b"\n\n"], &["", ""]),
            (64, &[b"crlf\r\n"], &["crlf"]),
            (64, &[b"a\rb\n"], &["a\rb"]),
            (64, &[b"bad \xff byte\n"], &["bad \u{fffd} byte"]),
            (4, &[b"abcdefgh\nij\n"], &["abcd", "ij"]),
            (4, &[b"ab", b"cdef", b"gh\nij"], &["abcd", "ij"]),
            (4, &[b"abcdefgh"], &["abcd"]),
        ];
        for (max, chunks, want) in cases {
            assert_eq!(split(max, chunks), want, "{chunks:?}");
        }
    }

    #[test]
    fn splits_a_multibyte_character_across_chunks() {
        assert_eq!(split(64, &[b"caf\xc3", b"\xa9\n"]), ["café"]);
    }
}
