//! Lifecycle operations: actions run on apps, with streamed progress.
//!
//! - `POST /api/apps/{name}/actions` takes an [`ActionRequest`] and
//!   answers 202 with an [`OperationResponse`] for the operation it
//!   started. It is 404 [`ErrorCode::AppNotFound`] for an unknown app, and
//!   409 [`ErrorCode::Conflict`] while another operation runs on the app,
//!   before Compose has read it, while Docker is not running, or, unless
//!   `force` is set, when another app shares its Compose project or when
//!   `up`, `start`, `restart`, or `update` would publish a host port that
//!   something else holds. The port refusal names each port and its
//!   holder.
//! - `GET /api/apps/{name}/plan?action=ACTION` returns the [`ActionPlan`]
//!   that action would run, changing nothing, with the port conflicts
//!   that would refuse it.
//! - `GET /api/operations` returns an [`OperationList`] of recent
//!   operations, newest first; `?app=NAME` keeps one app's.
//! - `GET /api/operations/{id}` returns one [`Operation`], or 404
//!   [`ErrorCode::OperationNotFound`].
//! - `POST /api/operations/{id}/cancel` stops a running operation and
//!   answers 202 with an [`OperationResponse`]; 409 when it has finished.
//! - `GET /api/operations/{id}/events` streams [`OperationEvent`]s: the
//!   operation, every kept output line, then new lines and changes as they
//!   happen, and a final `done`.
//!
//! The `operations` event on `/api/events` carries an [`OperationList`] of
//! every operation the daemon holds in memory, on every change.
//!
//! [`ErrorCode::AppNotFound`]: crate::ErrorCode::AppNotFound
//! [`ErrorCode::Conflict`]: crate::ErrorCode::Conflict
//! [`ErrorCode::OperationNotFound`]: crate::ErrorCode::OperationNotFound

pub use alchemy_core::StackName;
pub use alchemy_core::operation::{
    Action, Operation, OperationState, OutputLine, Step, StepCommand, StepState,
};
use serde::{Deserialize, Serialize, Serializer};

use crate::ports::PortConflict;

/// The body of `POST /api/apps/{name}/actions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRequest {
    /// What to do.
    pub action: Action,
    /// Go ahead even though another app shares this app's Compose project,
    /// so the action reaches that app's containers too, or a host port the
    /// app publishes is taken.
    #[serde(default)]
    pub force: bool,
}

/// The answer to starting or cancelling an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationResponse {
    /// The operation as it stands.
    pub operation: Operation,
}

/// Recent operations, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationList {
    /// The operations.
    pub operations: Vec<Operation>,
}

/// What an action would run on an app, from
/// `GET /api/apps/{name}/plan?action=ACTION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPlan {
    /// The app.
    pub app: StackName,
    /// The action.
    pub action: Action,
    /// The steps, in the order they would run.
    pub steps: Vec<PlannedStep>,
    /// The host ports the app publishes that something else holds now,
    /// which refuse the action unless it is forced. Only `up`, `start`,
    /// `restart`, and `update` check ports. Left out when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<PortConflict>,
}

/// One step of an [`ActionPlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedStep {
    /// The subcommand it runs.
    pub command: StepCommand,
    /// What it does, in plain words.
    pub label: String,
    /// The whole command line, starting with `docker`.
    pub argv: Vec<String>,
}

/// One message on `GET /api/operations/{id}/events`.
///
/// Sent as an SSE frame whose `event:` field is [`OperationEvent::name`]
/// and whose `data:` field is the payload as JSON. Serialized with serde,
/// the same event is `{"event": <name>, "data": <payload>}`, which is also
/// what `alchemy app <action> --json` prints per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum OperationEvent {
    /// The operation, sent first and again whenever a step changes.
    Operation(Operation),
    /// One line of raw output.
    Output(OutputLine),
    /// The operation once it has ended. Always the last event.
    Done(Operation),
}

impl OperationEvent {
    /// The SSE `event:` name for this variant.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Operation(_) => "operation",
            Self::Output(_) => "output",
            Self::Done(_) => "done",
        }
    }

    /// The payload alone, which serializes as the SSE `data:` field.
    pub fn data(&self) -> OperationEventData<'_> {
        OperationEventData(self)
    }
}

/// An [`OperationEvent`]'s payload without its name.
#[derive(Debug, Clone, Copy)]
pub struct OperationEventData<'a>(&'a OperationEvent);

impl Serialize for OperationEventData<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            OperationEvent::Operation(op) | OperationEvent::Done(op) => op.serialize(serializer),
            OperationEvent::Output(line) => line.serialize(serializer),
        }
    }
}
