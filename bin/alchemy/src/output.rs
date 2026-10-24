//! Where a command's results go, and whether they may be styled.

use std::ffi::OsStr;
use std::fmt::Display;
use std::io::Write;

use crate::error::ClientError;

/// Whether results may carry ANSI styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Plain text only.
    Off,
    /// ANSI styling is allowed.
    On,
}

impl Color {
    /// Style only when stdout is a terminal and `NO_COLOR` is unset or
    /// empty, per <https://no-color.org>.
    pub fn detect(stdout_is_terminal: bool, no_color: Option<&OsStr>) -> Self {
        let opted_out = no_color.is_some_and(|value| !value.is_empty());
        if stdout_is_terminal && !opted_out {
            Self::On
        } else {
            Self::Off
        }
    }
}

/// A command's results sink: stdout in the binary, a buffer in tests.
///
/// Results go to `out`. Errors go to stderr, and are reported by the
/// caller of [`crate::run`], not written here; progress a person watches
/// while a command works goes to stderr through [`Self::progress`].
pub(crate) struct Output<W> {
    out: W,
    progress: Box<dyn Write + Send>,
    json: bool,
    color: Color,
    #[cfg(test)]
    captured: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl<W: Write> Output<W> {
    /// Results written to `out`, as JSON when `json` is set, styled only
    /// when `color` allows it.
    pub(crate) fn new(out: W, json: bool, color: Color) -> Self {
        #[cfg(test)]
        {
            let captured = std::sync::Arc::default();
            Self {
                out,
                progress: Box::new(Captured(std::sync::Arc::clone(&captured))),
                json,
                color,
                captured,
            }
        }
        #[cfg(not(test))]
        Self {
            out,
            progress: Box::new(std::io::stderr()),
            json,
            color,
        }
    }

    /// Whether `--json` was given: commands then write one JSON document,
    /// or one per line for a stream.
    pub(crate) fn json(&self) -> bool {
        self.json
    }

    /// Whether results may use ANSI styling. Never with `--json`.
    pub(crate) fn color(&self) -> bool {
        self.color == Color::On && !self.json
    }

    /// Write `line` and a newline, then flush so a follower sees it now.
    pub(crate) fn line(&mut self, line: impl Display) -> Result<(), ClientError> {
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        Ok(())
    }

    /// Write `line` to stderr as progress. Failing to is not an error: the
    /// command's results still go to stdout.
    pub(crate) fn progress(&mut self, line: impl Display) {
        let _ = writeln!(self.progress, "{line}").and_then(|()| self.progress.flush());
    }
}

/// A progress sink that keeps what is written, for tests.
#[cfg(test)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl<W> Output<W> {
    /// The sink, to inspect what was written.
    pub(crate) fn get_ref(&self) -> &W {
        &self.out
    }

    /// Everything written as progress so far.
    pub(crate) fn progress_written(&self) -> String {
        String::from_utf8_lossy(
            &self
                .captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_needs_a_terminal_and_no_no_color() {
        let cases = [
            (true, None, Color::On),
            (true, Some(""), Color::On),
            (true, Some("1"), Color::Off),
            (true, Some("0"), Color::Off),
            (false, None, Color::Off),
            (false, Some("1"), Color::Off),
        ];
        for (terminal, no_color, want) in cases {
            assert_eq!(
                Color::detect(terminal, no_color.map(OsStr::new)),
                want,
                "{terminal} {no_color:?}"
            );
        }
    }

    #[test]
    fn json_output_is_never_styled() {
        let cases = [
            (false, Color::On, true),
            (true, Color::On, false),
            (false, Color::Off, false),
            (true, Color::Off, false),
        ];
        for (json, color, want) in cases {
            assert_eq!(
                Output::new(Vec::new(), json, color).color(),
                want,
                "{json} {color:?}"
            );
        }
    }
}
