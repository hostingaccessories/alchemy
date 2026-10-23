//! Asking the person at the terminal to confirm a destructive command.

use std::future::Future;
use std::io::{BufRead, IsTerminal, Write};

use tokio::sync::oneshot;

/// Where a destructive command asks for confirmation: the terminal in
/// the binary, a script in tests.
pub trait Prompt {
    /// Whether there is someone to ask. Without one, a command needing
    /// confirmation refuses rather than waiting on stdin.
    fn is_interactive(&self) -> bool;

    /// Ask `question` and resolve to whether the answer was yes. Anything
    /// but a yes, including a read failure or end of input, is a no.
    fn confirm(&mut self, question: &str) -> impl Future<Output = bool>;
}

/// Asks on stderr and reads the answer from stdin.
#[derive(Debug, Clone, Copy, Default)]
pub struct TtyPrompt;

impl Prompt for TtyPrompt {
    /// Both stdin and stderr must be terminals: stdin to answer on, and
    /// stderr so the question is seen.
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    /// Reads on its own thread, so the binary's ctrl-c handling still
    /// runs while the question waits. That thread is left blocked on
    /// stdin if the future is dropped; it ends with the process.
    fn confirm(&mut self, question: &str) -> impl Future<Output = bool> {
        let question = format!("{question} [y/N] ");
        async move {
            let (tx, rx) = oneshot::channel();
            let reader = std::thread::Builder::new()
                .name("prompt".into())
                .spawn(move || tx.send(ask_terminal(&question)));
            match reader {
                Ok(_) => rx.await.unwrap_or(false),
                Err(_) => false,
            }
        }
    }
}

fn ask_terminal(question: &str) -> bool {
    let mut stderr = std::io::stderr().lock();
    if write!(stderr, "{question}")
        .and_then(|()| stderr.flush())
        .is_err()
    {
        return false;
    }
    drop(stderr);
    let mut answer = String::new();
    match std::io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => is_yes(&answer),
    }
}

/// Whether `answer` to a `[y/N]` question means yes.
pub(crate) fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// A scripted [`Prompt`]: answers from a list, in order, and records
/// each question.
#[cfg(any(test, feature = "test-command"))]
#[derive(Debug, Clone, Default)]
pub struct FakePrompt {
    interactive: bool,
    answers: std::collections::VecDeque<bool>,
    asked: Vec<String>,
}

#[cfg(any(test, feature = "test-command"))]
impl FakePrompt {
    /// A terminal that answers with `answers`, then with no once they run
    /// out.
    pub fn tty(answers: impl IntoIterator<Item = bool>) -> Self {
        Self {
            interactive: true,
            answers: answers.into_iter().collect(),
            asked: Vec::new(),
        }
    }

    /// No terminal: nothing can be asked.
    pub fn no_tty() -> Self {
        Self::default()
    }

    /// Every question asked so far, in order.
    pub fn asked(&self) -> &[String] {
        &self.asked
    }
}

#[cfg(any(test, feature = "test-command"))]
impl Prompt for FakePrompt {
    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn confirm(&mut self, question: &str) -> impl Future<Output = bool> {
        self.asked.push(question.to_owned());
        std::future::ready(self.interactive && self.answers.pop_front().unwrap_or(false))
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
    fn only_y_or_yes_is_yes() {
        let cases = [
            ("y\n", true),
            ("Y\n", true),
            ("yes\n", true),
            ("  YES  \r\n", true),
            ("\n", false),
            ("n\n", false),
            ("no\n", false),
            ("yep\n", false),
            ("y y\n", false),
            ("", false),
        ];
        for (answer, want) in cases {
            assert_eq!(is_yes(answer), want, "{answer:?}");
        }
    }

    #[tokio::test]
    async fn fake_answers_in_order_then_no() {
        let mut prompt = FakePrompt::tty([true, false]);
        assert!(prompt.is_interactive());
        assert!(prompt.confirm("one?").await);
        assert!(!prompt.confirm("two?").await);
        assert!(!prompt.confirm("three?").await);
        assert_eq!(prompt.asked(), ["one?", "two?", "three?"]);

        let mut prompt = FakePrompt::no_tty();
        assert!(!prompt.is_interactive());
        assert!(!prompt.confirm("four?").await);
    }
}
