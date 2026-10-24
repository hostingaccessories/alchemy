//! [`FakeComposeRunner`]: a [`ComposeRunner`] for tests that runs nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;

use super::{
    CancelToken, ComposeAction, ComposeError, ComposeEvent, ComposePlan, ComposeRunner, OutputLine,
    Stream,
};

/// How a [`FakeScript`] ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FakeOutcome {
    /// Every step succeeds.
    #[default]
    Succeed,
    /// The last step fails with [`ComposeError::Failed`].
    Fail {
        /// The exit code to report.
        code: i32,
        /// The message to report.
        message: String,
    },
    /// The last step runs until the run is cancelled, then ends with
    /// [`ComposeError::Cancelled`].
    UntilCancelled,
}

/// What [`FakeComposeRunner`] does for one action.
///
/// Every step but the last starts and finishes silently; the last writes
/// the script's lines, then ends as its [`FakeOutcome`] says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeScript {
    lines: Vec<(Stream, String)>,
    outcome: FakeOutcome,
}

impl FakeScript {
    /// A script that writes nothing and succeeds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Also write `text` to `stream`. Progress lines are parsed as the
    /// real runner parses them.
    #[must_use]
    pub fn line(mut self, stream: Stream, text: impl Into<String>) -> Self {
        self.lines.push((stream, text.into()));
        self
    }

    /// End with `outcome`.
    #[must_use]
    pub fn outcome(mut self, outcome: FakeOutcome) -> Self {
        self.outcome = outcome;
        self
    }
}

#[derive(Debug, Default)]
struct Inner {
    scripts: HashMap<ComposeAction, FakeScript>,
    runs: Vec<ComposePlan>,
}

/// [`ComposeRunner`] that records each plan and plays back a
/// [`FakeScript`] instead of running anything.
///
/// Actions without a script succeed silently. Clones share their scripts
/// and records, so a test can keep one and hand another to the code under
/// test.
#[derive(Debug, Clone, Default)]
pub struct FakeComposeRunner {
    inner: Arc<Mutex<Inner>>,
}

impl FakeComposeRunner {
    /// A runner with no scripts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Play `script` whenever `action` runs.
    #[must_use]
    pub fn with_script(self, action: ComposeAction, script: FakeScript) -> Self {
        self.lock().scripts.insert(action, script);
        self
    }

    /// Every plan run so far, oldest first, including those still running.
    pub fn runs(&self) -> Vec<ComposePlan> {
        self.lock().runs.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ComposeRunner for FakeComposeRunner {
    async fn run(
        &self,
        plan: &ComposePlan,
        events: mpsc::Sender<ComposeEvent>,
        mut cancel: CancelToken,
    ) -> Result<(), ComposeError> {
        let action = plan.action();
        let script = {
            let mut inner = self.lock();
            inner.runs.push(plan.clone());
            inner.scripts.get(&action).cloned().unwrap_or_default()
        };
        let last = plan.steps().len().saturating_sub(1);
        for (index, step) in plan.steps().iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(ComposeError::Cancelled { action });
            }
            let command = step.command();
            let _ = events
                .send(ComposeEvent::StepStarted {
                    step: index,
                    command,
                })
                .await;
            if index == last {
                for (stream, text) in &script.lines {
                    let line = OutputLine::new(index, *stream, text.clone());
                    let _ = events.send(ComposeEvent::Output(line)).await;
                }
                match &script.outcome {
                    FakeOutcome::Succeed => {}
                    FakeOutcome::Fail { code, message } => {
                        return Err(ComposeError::Failed {
                            action,
                            command,
                            code: Some(*code),
                            message: message.clone(),
                        });
                    }
                    FakeOutcome::UntilCancelled => {
                        cancel.cancelled().await;
                        return Err(ComposeError::Cancelled { action });
                    }
                }
            }
            let _ = events
                .send(ComposeEvent::StepFinished {
                    step: index,
                    command,
                })
                .await;
        }
        Ok(())
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
    use crate::compose::{ComposeCommand, ComposeProject, ImageSource, cancellation};

    fn plan(action: ComposeAction) -> ComposePlan {
        let project = ComposeProject::new("/srv/blog").unwrap();
        ComposePlan::new(&project, action, ImageSource::Registry)
    }

    async fn run(
        runner: &FakeComposeRunner,
        action: ComposeAction,
        cancel: CancelToken,
    ) -> (Vec<ComposeEvent>, Result<(), ComposeError>) {
        let (tx, mut rx) = mpsc::channel(64);
        let result = runner.run(&plan(action), tx, cancel).await;
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        (events, result)
    }

    #[tokio::test]
    async fn plays_back_scripts_and_records_plans() {
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Update,
            FakeScript::new()
                .line(
                    Stream::Stderr,
                    r#"{"id":"Container a","status":"Done","text":"Started"}"#,
                )
                .line(Stream::Stdout, "plain"),
        );
        let (events, result) = run(&runner, ComposeAction::Update, CancelToken::never()).await;
        result.unwrap();
        assert_eq!(
            events,
            [
                ComposeEvent::StepStarted {
                    step: 0,
                    command: ComposeCommand::Pull
                },
                ComposeEvent::StepFinished {
                    step: 0,
                    command: ComposeCommand::Pull
                },
                ComposeEvent::StepStarted {
                    step: 1,
                    command: ComposeCommand::Up
                },
                ComposeEvent::Output(OutputLine::new(
                    1,
                    Stream::Stderr,
                    r#"{"id":"Container a","status":"Done","text":"Started"}"#
                )),
                ComposeEvent::Output(OutputLine::new(1, Stream::Stdout, "plain")),
                ComposeEvent::StepFinished {
                    step: 1,
                    command: ComposeCommand::Up
                },
            ]
        );

        let (events, result) = run(&runner, ComposeAction::Stop, CancelToken::never()).await;
        result.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            runner.runs(),
            [plan(ComposeAction::Update), plan(ComposeAction::Stop)]
        );
    }

    #[tokio::test]
    async fn fails_as_scripted() {
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Pull,
            FakeScript::new().outcome(FakeOutcome::Fail {
                code: 1,
                message: "pull access denied".to_owned(),
            }),
        );
        let (events, result) = run(&runner, ComposeAction::Pull, CancelToken::never()).await;
        assert_eq!(events.len(), 1);
        match result {
            Err(ComposeError::Failed {
                action: ComposeAction::Pull,
                command: ComposeCommand::Pull,
                code: Some(1),
                message,
            }) => assert_eq!(message, "pull access denied"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn waits_for_cancellation_when_scripted() {
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Up,
            FakeScript::new()
                .line(Stream::Stderr, "starting")
                .outcome(FakeOutcome::UntilCancelled),
        );
        let (canceller, token) = cancellation();
        let (tx, mut rx) = mpsc::channel(64);
        let task = tokio::spawn({
            let runner = runner.clone();
            async move { runner.run(&plan(ComposeAction::Up), tx, token).await }
        });
        assert!(matches!(
            rx.recv().await,
            Some(ComposeEvent::StepStarted { .. })
        ));
        assert!(matches!(rx.recv().await, Some(ComposeEvent::Output(_))));
        canceller.cancel();
        assert!(matches!(
            task.await.unwrap(),
            Err(ComposeError::Cancelled {
                action: ComposeAction::Up
            })
        ));
        assert!(rx.recv().await.is_none());

        let (canceller, token) = cancellation();
        canceller.cancel();
        let (events, result) = run(&runner, ComposeAction::Up, token).await;
        assert_eq!(events, []);
        assert!(matches!(result, Err(ComposeError::Cancelled { .. })));
    }
}
