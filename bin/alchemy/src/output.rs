//! Where a command's results go.

use std::fmt::Display;
use std::io::Write;

use crate::error::ClientError;

/// A command's results sink: stdout in the binary, a buffer in tests.
///
/// Results only: diagnostics and errors are reported by the caller, not
/// written here.
pub(crate) struct Output<W> {
    out: W,
    json: bool,
}

impl<W: Write> Output<W> {
    /// Results written to `out`, as JSON when `json` is set.
    pub(crate) fn new(out: W, json: bool) -> Self {
        Self { out, json }
    }

    /// Whether `--json` was given: commands then write one JSON document,
    /// or one per line for a stream.
    pub(crate) fn json(&self) -> bool {
        self.json
    }

    /// Write `line` and a newline, then flush so a follower sees it now.
    pub(crate) fn line(&mut self, line: impl Display) -> Result<(), ClientError> {
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
impl<W> Output<W> {
    /// The sink, to inspect what was written.
    pub(crate) fn get_ref(&self) -> &W {
        &self.out
    }
}
