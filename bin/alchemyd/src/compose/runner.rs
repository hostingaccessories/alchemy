//! The [`ComposeRunner`] seam: what running a plan reports, and how it is
//! cancelled.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::{ComposeAction, ComposeCommand, ComposePlan, ProgressEvent, parse_progress};

/// Runs Compose lifecycle actions.
pub trait ComposeRunner {
    /// Run every step of `plan` in order, stopping at the first that fails.
    ///
    /// Progress goes to `events` as it happens: each step's start, every
    /// output line as it is written, and each step's successful end. The
    /// channel closes when the run ends, so a reader can loop until `None`
    /// and then look at the result. A closed or slow reader never fails
    /// the run; a full channel only slows it down.
    ///
    /// Once `cancel` fires, the running step is interrupted and no further
    /// step starts. Dropping the future stops the run at once, without the
    /// grace a cancellation gets.
    fn run(
        &self,
        plan: &ComposePlan,
        events: mpsc::Sender<ComposeEvent>,
        cancel: CancelToken,
    ) -> impl Future<Output = Result<(), ComposeError>> + Send;
}

/// Something that happened while a plan ran.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ComposeEvent {
    /// A step started.
    StepStarted {
        /// The step's index in [`ComposePlan::steps`].
        step: usize,
        /// The subcommand it runs.
        command: ComposeCommand,
    },
    /// A step wrote a line.
    Output(OutputLine),
    /// A step exited successfully.
    StepFinished {
        /// The step's index in [`ComposePlan::steps`].
        step: usize,
        /// The subcommand it ran.
        command: ComposeCommand,
    },
}

/// Which of the child's output streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error, where Compose writes its progress.
    Stderr,
}

/// One line of a step's output: the raw text, and its meaning when it is
/// a progress line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    /// The step's index in [`ComposePlan::steps`].
    pub step: usize,
    /// Which stream it was written to.
    pub stream: Stream,
    /// The line, without its newline. Invalid UTF-8 is replaced, and very
    /// long lines are cut short.
    pub text: String,
    /// The line parsed by [`parse_progress`], if it is progress.
    pub progress: Option<ProgressEvent>,
}

impl OutputLine {
    /// The line `text` from `stream` of step `step`, parsed.
    pub fn new(step: usize, stream: Stream, text: impl Into<String>) -> Self {
        let text = text.into();
        let progress = parse_progress(&text);
        Self {
            step,
            stream,
            text,
            progress,
        }
    }
}

/// Fires a [`CancelToken`].
#[derive(Debug)]
pub struct Canceller {
    tx: watch::Sender<bool>,
}

impl Canceller {
    /// Cancel every run holding this canceller's token. Idempotent.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }
}

/// Tells a run to stop: see [`ComposeRunner::run`].
///
/// Dropping the [`Canceller`] without calling it never cancels. A
/// `watch::Receiver<bool>`, such as the daemon's shutdown signal, converts
/// into a token that fires when it turns `true`.
#[derive(Debug, Clone)]
pub struct CancelToken {
    rx: watch::Receiver<bool>,
}

/// A connected [`Canceller`] and [`CancelToken`].
pub fn cancellation() -> (Canceller, CancelToken) {
    let (tx, rx) = watch::channel(false);
    (Canceller { tx }, CancelToken { rx })
}

impl CancelToken {
    /// A token that never fires.
    pub fn never() -> Self {
        cancellation().1
    }

    /// Whether the token has fired.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Wait until the token fires, or forever if it never can.
    pub async fn cancelled(&mut self) {
        if self.rx.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl From<watch::Receiver<bool>> for CancelToken {
    fn from(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }
}

/// Why a Compose action did not complete.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeError {
    /// The `docker` binary path was not absolute.
    #[error("docker binary {} is not absolute", binary.display())]
    RelativeBinary {
        /// The rejected path.
        binary: PathBuf,
    },
    /// The binary could not be started.
    #[error("failed to run {}", binary.display())]
    Spawn {
        /// The binary that was run.
        binary: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The action did not finish in time and was stopped.
    #[error("docker compose {action} did not finish within {after:?}")]
    Timeout {
        /// The action that was run.
        action: ComposeAction,
        /// How long it was given.
        after: Duration,
    },
    /// The action was cancelled and stopped.
    #[error("docker compose {action} was cancelled")]
    Cancelled {
        /// The action that was run.
        action: ComposeAction,
    },
    /// A step exited unsuccessfully, and the steps after it did not run.
    #[error("docker compose {command} exited with {}: {message}", code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}")))]
    Failed {
        /// The action that was run.
        action: ComposeAction,
        /// The step that failed.
        command: ComposeCommand,
        /// The exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// Why: Compose's own error message when it printed one, else the
        /// end of its plain standard error.
        message: String,
    },
}

const MAX_CAPTURED: usize = 4096;

/// What a failed step said about why, gathered from its standard error.
#[derive(Debug, Default)]
pub(super) struct FailureNotes {
    error: Option<String>,
    plain: String,
}

impl FailureNotes {
    /// Note one standard error line.
    pub(super) fn note(&mut self, line: &OutputLine) {
        match &line.progress {
            Some(ProgressEvent::Message {
                error: true,
                message,
            }) => self.error = Some(message.clone()),
            Some(_) => {}
            None => {
                self.plain.push_str(&line.text);
                self.plain.push('\n');
                if self.plain.len() > 2 * MAX_CAPTURED {
                    self.plain = tail(&self.plain);
                    self.plain.push('\n');
                }
            }
        }
    }

    /// The reason to report.
    pub(super) fn message(self) -> String {
        self.error.unwrap_or_else(|| tail(&self.plain))
    }
}

/// The last [`MAX_CAPTURED`] bytes of `text`, trimmed, on a character
/// boundary. Compose prints the reason for a failure last.
pub(super) fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| text.len() - i <= MAX_CAPTURED)
        .unwrap_or(text.len());
    text.get(start..).unwrap_or_default().to_owned()
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
    fn keeps_the_tail_of_long_output() {
        assert_eq!(tail("  boom\n"), "boom");
        assert_eq!(tail(""), "");
        let long = format!("{}end", "x".repeat(MAX_CAPTURED));
        let kept = tail(&long);
        assert_eq!(kept.len(), MAX_CAPTURED);
        assert!(kept.ends_with("end"));
        let wide = "é".repeat(MAX_CAPTURED);
        let kept = tail(&wide);
        assert!(kept.len() <= MAX_CAPTURED);
        assert!(kept.chars().all(|c| c == 'é'));
    }

    #[test]
    fn reports_the_compose_error_over_plain_stderr() {
        let cases: [(&[&str], &str); 5] = [
            (&[], ""),
            (
                &["pulling...", "no such service: web"],
                "pulling...\nno such service: web",
            ),
            (
                &[
                    r#"{"id":"Image x","status":"Working","text":"Pulling"}"#,
                    "WARN something",
                    r#"{"error":true,"message":"pull access denied"}"#,
                ],
                "pull access denied",
            ),
            (
                &[
                    r#"{"id":"Image x","status":"Error","text":"Error"}"#,
                    "plain",
                ],
                "plain",
            ),
            (&[r#"{"message":"not an error"}"#], ""),
        ];
        for (lines, want) in cases {
            let mut notes = FailureNotes::default();
            for line in lines {
                notes.note(&OutputLine::new(0, Stream::Stderr, *line));
            }
            assert_eq!(notes.message(), want, "{lines:?}");
        }
    }

    #[test]
    fn bounds_plain_stderr() {
        let mut notes = FailureNotes::default();
        for i in 0..10_000 {
            notes.note(&OutputLine::new(0, Stream::Stderr, format!("line {i}")));
            assert!(notes.plain.len() <= 2 * MAX_CAPTURED + 16);
        }
        let message = notes.message();
        assert!(message.len() <= MAX_CAPTURED);
        assert!(message.ends_with("line 9999"));
    }

    #[test]
    fn parses_output_lines() {
        let line = OutputLine::new(
            2,
            Stream::Stderr,
            r#"{"id":"Container a","status":"Done","text":"Started"}"#,
        );
        assert_eq!(line.step, 2);
        assert!(matches!(line.progress, Some(ProgressEvent::Resource(_))));
        assert_eq!(OutputLine::new(0, Stream::Stdout, "hello").progress, None);
    }

    #[tokio::test]
    async fn cancels_through_the_token() {
        let (canceller, mut token) = cancellation();
        assert!(!token.is_cancelled());
        let waiting = token.clone();
        canceller.cancel();
        canceller.cancel();
        token.cancelled().await;
        assert!(token.is_cancelled());
        assert!(waiting.is_cancelled());

        let (tx, rx) = watch::channel(false);
        let mut token = CancelToken::from(rx);
        tx.send_replace(true);
        token.cancelled().await;
    }

    #[tokio::test]
    async fn never_fires_when_the_canceller_is_dropped() {
        let (canceller, mut token) = cancellation();
        drop(canceller);
        assert!(!token.is_cancelled());
        let waited = tokio::time::timeout(Duration::from_millis(20), token.cancelled()).await;
        assert!(waited.is_err());
        let mut never = CancelToken::never();
        let waited = tokio::time::timeout(Duration::from_millis(20), never.cancelled()).await;
        assert!(waited.is_err());
    }
}
