//! Bringing apps back after alchemyd starts, the computer wakes, or
//! Docker returns.
//!
//! `GET /api/reconcile` returns a [`ReconcileStatus`], and `/api/events`
//! sends the same as the `reconcile` event on connect and on every
//! change.
//!
//! A reconcile starts every app that has no stop intent, was running when
//! Alchemy last saw it before the disruption, and is stopped now. Each
//! start is an ordinary `up` operation whose
//! [`reason`](crate::operations::Operation::reason) says why. It runs once
//! per trigger and is never retried; an app that fails stays down, and
//! its operation says why.
//!
//! Every `message` here is written for the people using Alchemy and safe
//! to show as is.

pub use alchemy_core::StackName;
pub use alchemy_core::lifecycle::{ReconcileTrigger, SkipReason};
use serde::{Deserialize, Serialize};

/// `GET /api/reconcile`, and the `reconcile` event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileStatus {
    /// The last reconcile this alchemyd ran, or `None` before the first,
    /// which runs once Docker is first ready.
    pub last: Option<Reconcile>,
}

/// One reconcile and what it did with each app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconcile {
    /// What set it off.
    pub trigger: ReconcileTrigger,
    /// When it ran, in Unix seconds.
    pub at: i64,
    /// Why apps were started, such as "was running before the computer
    /// slept", as each operation's reason says.
    pub reason: String,
    /// Every app, by name.
    pub apps: Vec<ReconcileApp>,
}

/// What a reconcile did with one app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileApp {
    /// The app.
    pub app: StackName,
    /// What happened.
    pub outcome: ReconcileOutcome,
    /// The `up` operation started for it, when `outcome` is `starting`,
    /// `started`, or `failed` after it began.
    pub operation: Option<String>,
    /// Why it was left alone, when `outcome` is `skipped`.
    pub reason: Option<SkipReason>,
    /// Why it was skipped, or what went wrong, in a sentence.
    pub message: Option<String>,
}

/// The outcome of a [`ReconcileApp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReconcileOutcome {
    /// Its operation is running.
    Starting,
    /// Its operation succeeded.
    Started,
    /// It could not be started, or its operation failed. It is not
    /// tried again until the next trigger.
    Failed,
    /// It was left alone; `reason` says why.
    Skipped,
    /// An outcome this build does not know.
    #[serde(other)]
    Other,
}
