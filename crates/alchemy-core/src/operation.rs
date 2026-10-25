//! Lifecycle operations: an action run on one app, its steps, and its
//! output.
//!
//! The daemon plans an [`Action`] as one or more Compose subcommands, each
//! a [`StepCommand`], and runs them in order. A [`Tracker`] turns what the
//! run reports, as [`Progress`], into the [`Operation`] clients see: each
//! step's state and a one-line message in plain words, and the raw output
//! as text lines kept in an [`OutputBuffer`]. Everything here is a pure
//! decision over plain values; the daemon supplies ids and timestamps.

mod output;
mod track;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::StackName;
use crate::engine::IntentAction;

pub use output::{OutputBuffer, OutputLine};
pub use track::{Applied, BuildLog, BuildStep, Outcome, Progress, ResourceState, Tracker};

/// How many output lines an operation keeps by default. Older lines are
/// dropped first.
pub const OUTPUT_LINES: usize = 5000;

/// The most bytes kept of one output line. The daemon cuts Compose's own
/// lines here, and a build step's log lines, tag included, stay within it.
pub const MAX_OUTPUT_LINE: usize = 64 * 1024;

/// A lifecycle action on an app.
///
/// Exhaustive, so whoever plans actions must handle a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Create and start every service, detached.
    Up,
    /// Stop and remove containers and networks. Volumes are kept.
    Down,
    /// Start existing containers.
    Start,
    /// Stop running containers without removing them.
    Stop,
    /// Restart every service.
    Restart,
    /// Pull every service's image.
    Pull,
    /// Build every service that has a `build:` section.
    Build,
    /// Fetch or rebuild images, then recreate what changed. There is no
    /// image rollback.
    Update,
}

impl Action {
    /// Every action, in the order the UI lists them.
    pub const ALL: [Self; 8] = [
        Self::Up,
        Self::Down,
        Self::Start,
        Self::Stop,
        Self::Restart,
        Self::Pull,
        Self::Build,
        Self::Update,
    ];

    /// The action's name, such as `up`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Pull => "pull",
            Self::Build => "build",
            Self::Update => "update",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A name that is not an [`Action`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown compose action {name:?}")]
pub struct UnknownAction {
    /// The rejected name.
    pub name: String,
}

impl FromStr for Action {
    type Err = UnknownAction;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == name)
            .ok_or_else(|| UnknownAction {
                name: name.to_owned(),
            })
    }
}

/// The `docker compose` subcommand one step of an operation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepCommand {
    /// `up`
    Up,
    /// `down`
    Down,
    /// `start`
    Start,
    /// `stop`
    Stop,
    /// `restart`
    Restart,
    /// `pull`
    Pull,
    /// `build`
    Build,
}

impl StepCommand {
    /// The subcommand, such as `up`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Pull => "pull",
            Self::Build => "build",
        }
    }
}

impl fmt::Display for StepCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What `command` does as part of `action`, in plain words for someone
/// new to Docker, such as "Downloading the app".
pub fn step_label(action: Action, command: StepCommand) -> &'static str {
    match (action, command) {
        (Action::Update, StepCommand::Pull) => "Downloading updates",
        (Action::Update, StepCommand::Build) => "Rebuilding the app",
        (Action::Update, StepCommand::Up) => "Starting the new version",
        (_, StepCommand::Pull) => "Downloading the app",
        (_, StepCommand::Build) => "Building the app",
        (_, StepCommand::Up | StepCommand::Start) => "Starting the app",
        (_, StepCommand::Stop) => "Stopping the app",
        (_, StepCommand::Restart) => "Restarting the app",
        (_, StepCommand::Down) => "Removing the app's containers",
    }
}

/// The intent to record before running `action`, so the containers it
/// stops are known to have been stopped on purpose. Pull and build leave
/// containers alone; an update recreates them as `up` does.
pub fn intent_action(action: Action) -> Option<IntentAction> {
    match action {
        Action::Up | Action::Update => Some(IntentAction::Up),
        Action::Start => Some(IntentAction::Start),
        Action::Restart => Some(IntentAction::Restart),
        Action::Stop => Some(IntentAction::Stop),
        Action::Down => Some(IntentAction::Down),
        Action::Pull | Action::Build => None,
    }
}

/// Where an operation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OperationState {
    /// Still running.
    Running,
    /// Every step finished.
    Succeeded,
    /// A step failed, and the steps after it did not run.
    Failed,
    /// Someone cancelled it before it finished.
    Cancelled,
    /// alchemyd stopped before it finished.
    Interrupted,
    /// A state this build does not know.
    #[serde(other)]
    Unknown,
}

impl OperationState {
    /// Whether the operation has ended, one way or another.
    pub fn is_finished(self) -> bool {
        self != Self::Running
    }

    /// The state's name, such as `running`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }
}

/// Where one step of an operation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepState {
    /// Not started yet.
    Pending,
    /// Running now.
    Active,
    /// Finished.
    Done,
    /// Failed, or stopped before it finished.
    Failed,
    /// Never ran, because an earlier step did not finish.
    Skipped,
    /// A state this build does not know.
    #[serde(other)]
    Unknown,
}

/// One step of an operation: one `docker compose` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The subcommand it runs.
    pub command: StepCommand,
    /// What it does, in plain words, from [`step_label`].
    pub label: String,
    /// Where it is.
    pub state: StepState,
    /// One line under the label: progress while it runs, such as
    /// "2 of 3 images downloaded", or the problem when it failed.
    pub message: Option<String>,
}

/// An action run on one app, and how it went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// Identifies the operation, unique across restarts of the daemon.
    pub id: String,
    /// The app it runs on.
    pub app: StackName,
    /// What it does.
    pub action: Action,
    /// Where it is.
    pub state: OperationState,
    /// Its steps, in the order they run. Never empty.
    pub steps: Vec<Step>,
    /// When it started, in Unix seconds.
    pub started_at: i64,
    /// When it ended, in Unix seconds, or `None` while it runs.
    pub finished_at: Option<i64>,
    /// Why it did not succeed, in a sentence safe to show. `None` while
    /// it runs and when it succeeded.
    pub error: Option<String>,
    /// Why Alchemy started it on its own, such as "was running before the
    /// computer slept". `None`, and left off the wire, when someone asked
    /// for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    fn parses_action_names() {
        for action in Action::ALL {
            assert_eq!(action.as_str().parse(), Ok(action));
            assert_eq!(action.to_string(), action.as_str());
            assert_eq!(
                serde_json::to_string(&action).unwrap(),
                format!("\"{action}\"")
            );
        }
        for name in ["", "UP", "kill", "up --detach"] {
            assert_eq!(
                name.parse::<Action>(),
                Err(UnknownAction {
                    name: name.to_owned()
                })
            );
        }
    }

    #[test]
    fn labels_steps_in_plain_words() {
        let cases = [
            (Action::Up, StepCommand::Up, "Starting the app"),
            (Action::Start, StepCommand::Start, "Starting the app"),
            (Action::Stop, StepCommand::Stop, "Stopping the app"),
            (Action::Restart, StepCommand::Restart, "Restarting the app"),
            (
                Action::Down,
                StepCommand::Down,
                "Removing the app's containers",
            ),
            (Action::Pull, StepCommand::Pull, "Downloading the app"),
            (Action::Build, StepCommand::Build, "Building the app"),
            (Action::Update, StepCommand::Pull, "Downloading updates"),
            (Action::Update, StepCommand::Build, "Rebuilding the app"),
            (Action::Update, StepCommand::Up, "Starting the new version"),
        ];
        for (action, command, want) in cases {
            assert_eq!(step_label(action, command), want, "{action} {command}");
        }
    }

    #[test]
    fn maps_actions_to_intents() {
        let cases = [
            (Action::Up, Some(IntentAction::Up)),
            (Action::Update, Some(IntentAction::Up)),
            (Action::Start, Some(IntentAction::Start)),
            (Action::Restart, Some(IntentAction::Restart)),
            (Action::Stop, Some(IntentAction::Stop)),
            (Action::Down, Some(IntentAction::Down)),
            (Action::Pull, None),
            (Action::Build, None),
        ];
        for (action, want) in cases {
            assert_eq!(intent_action(action), want, "{action}");
        }
    }

    #[test]
    fn unknown_states_decode_as_a_fallback() {
        assert_eq!(
            serde_json::from_str::<OperationState>("\"paused\"").unwrap(),
            OperationState::Unknown
        );
        assert_eq!(
            serde_json::from_str::<StepState>("\"waiting\"").unwrap(),
            StepState::Unknown
        );
        assert!(!OperationState::Running.is_finished());
        for state in [
            OperationState::Succeeded,
            OperationState::Failed,
            OperationState::Cancelled,
            OperationState::Interrupted,
        ] {
            assert!(state.is_finished(), "{state:?}");
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.as_str())
            );
        }
    }
}
