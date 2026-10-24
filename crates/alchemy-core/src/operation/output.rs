//! An operation's raw output, bounded.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// One line of an operation's raw output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputLine {
    /// The line's place in the operation's output, counting up from 0. A
    /// gap means older lines were dropped.
    pub seq: u64,
    /// The index of the step that wrote it, in
    /// [`Operation::steps`](super::Operation::steps).
    pub step: usize,
    /// The line, without its newline.
    pub text: String,
}

/// The newest lines of an operation's output, up to a fixed count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBuffer {
    lines: VecDeque<OutputLine>,
    next: u64,
    capacity: usize,
}

impl OutputBuffer {
    /// An empty buffer keeping at most `capacity` lines, and at least one.
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            next: 0,
            capacity: capacity.max(1),
        }
    }

    /// Add `text` from step `step`, dropping the oldest line when full, and
    /// return the new line's [`OutputLine::seq`].
    pub fn push(&mut self, step: usize, text: impl Into<String>) -> u64 {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        let seq = self.next;
        self.lines.push_back(OutputLine {
            seq,
            step,
            text: text.into(),
        });
        self.next += 1;
        seq
    }

    /// The kept lines from `seq` on, oldest first.
    pub fn since(&self, seq: u64) -> Vec<OutputLine> {
        self.lines
            .iter()
            .filter(|line| line.seq >= seq)
            .cloned()
            .collect()
    }

    /// The `seq` the next line will get.
    pub fn next_seq(&self) -> u64 {
        self.next
    }

    /// How many lines are kept.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether no line is kept.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
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

    fn texts(lines: &[OutputLine]) -> Vec<(u64, &str)> {
        lines.iter().map(|l| (l.seq, l.text.as_str())).collect()
    }

    #[test]
    fn keeps_the_newest_lines() {
        type Case = (usize, usize, u64, &'static [(u64, &'static str)]);
        let cases: [Case; 5] = [
            (3, 0, 0, &[]),
            (3, 2, 0, &[(0, "0"), (1, "1")]),
            (3, 5, 0, &[(2, "2"), (3, "3"), (4, "4")]),
            (3, 5, 4, &[(4, "4")]),
            (0, 2, 0, &[(1, "1")]),
        ];
        for (capacity, pushes, from, want) in cases {
            let mut buffer = OutputBuffer::new(capacity);
            for i in 0..pushes {
                assert_eq!(buffer.push(0, i.to_string()), i as u64);
            }
            assert_eq!(
                texts(&buffer.since(from)),
                want,
                "{capacity} {pushes} {from}"
            );
            assert_eq!(buffer.next_seq(), pushes as u64);
            assert_eq!(buffer.len(), pushes.min(capacity.max(1)));
            assert_eq!(buffer.is_empty(), pushes == 0);
        }
    }

    #[test]
    fn reading_past_the_end_is_empty() {
        let mut buffer = OutputBuffer::new(2);
        buffer.push(1, "a");
        assert_eq!(buffer.since(1), []);
        assert_eq!(buffer.since(0)[0].step, 1);
    }
}
