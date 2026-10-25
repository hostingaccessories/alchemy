//! Autostart: whether alchemyd starts on its own, at login or at boot.
//!
//! `GET /api/machine/autostart` returns an [`Autostart`], and
//! `/api/events` sends the same value as the `autostart` event on connect
//! and on every change.
//!
//! `PUT /api/machine/autostart` takes an [`AutostartRequest`],
//! `{"mode": "off" | "login" | "boot"}`, and answers with an
//! [`AutostartChange`]. Only one service is ever installed: the new mode's
//! service is installed before the old one is removed.
//!
//! - `done` (200): the switch is complete.
//! - `unchanged` (200): alchemyd was already in that mode.
//! - `pending` (202): `boot`, or leaving it, needs a system prompt, which
//!   is up now. [`Autostart::pending`] names the mode until it ends; then
//!   [`Autostart::last`] says how, and the `system_service` privilege
//!   carries the details.
//! - `needs_terminal` (200): no prompt is safe or available. Run
//!   [`AutostartChange::command`] in a terminal, then send the same
//!   request again to finish.
//! - `restarting` (202): this alchemyd runs as the per-user service and
//!   is handing over to the system service: it stops in a moment, and the
//!   system service's alchemyd takes over. Reconnect and sign in again.
//!
//! Removing the service that runs the alchemyd answering also stops it
//! once the step is done, and the other service's alchemyd, which waits
//! for the state directory, takes over. Turning autostart off from inside
//! the per-user service leaves it running until it exits.
//!
//! A malformed body or unknown mode is a 400 `bad_request`, a mode not in
//! [`Autostart::available_modes`] a 422 `unsupported`, and a switch while
//! another is in flight a 409 `conflict`.

pub use alchemy_core::lifecycle::{AutostartMode, LogHint, Supervisor};
use serde::{Deserialize, Serialize};

use crate::privileges::{AttemptResult, HelperCommand};

/// `GET /api/machine/autostart`, and the `autostart` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Autostart {
    /// The mode the installed services put alchemyd in.
    pub mode: AutostartMode,
    /// The modes it can be switched to on this machine, `mode` included,
    /// in display order.
    pub available_modes: Vec<AutostartMode>,
    /// Where alchemyd logs in `mode`.
    pub log_hint: LogHint,
    /// What runs the alchemyd answering.
    pub supervisor: Supervisor,
    /// The mode a switch in flight is going to, such as while a prompt is
    /// up.
    pub pending: Option<AutostartMode>,
    /// How the last switch asked of this alchemyd ended, or `None` before
    /// any has ended. A restart forgets it.
    pub last: Option<AutostartAttempt>,
    /// The modes left out of `available_modes`, each with why, in display
    /// order. Omitted when every mode is available, and empty from an
    /// alchemyd older than it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<UnavailableMode>,
}

/// A mode that can't be switched to on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnavailableMode {
    /// The mode.
    pub mode: AutostartMode,
    /// Why, in a sentence safe to show, such as that no `systemd --user`
    /// manager runs.
    pub message: String,
}

/// How a switch ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartAttempt {
    /// The mode it was going to.
    pub mode: AutostartMode,
    /// How it ended.
    pub result: AttemptResult,
    /// What went wrong, when it didn't succeed.
    pub message: Option<String>,
    /// When it ended, in Unix seconds.
    pub at: i64,
}

/// The body of `PUT /api/machine/autostart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartRequest {
    /// The mode to switch to.
    pub mode: AutostartMode,
}

/// What `PUT /api/machine/autostart` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartChange {
    /// What happened.
    pub outcome: AutostartOutcome,
    /// The command to run in a terminal, when `outcome` is
    /// `needs_terminal`.
    pub command: Option<HelperCommand>,
    /// Why there is no prompt, when `outcome` is `needs_terminal`.
    pub message: Option<String>,
    /// Autostart as it stands now.
    pub autostart: Autostart,
}

/// The outcome of an [`AutostartChange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AutostartOutcome {
    /// The switch is complete.
    Done,
    /// It was already that way.
    Unchanged,
    /// A system prompt is up; follow [`Autostart::pending`].
    Pending,
    /// Run [`AutostartChange::command`] in a terminal, then ask again.
    NeedsTerminal,
    /// This alchemyd stops in a moment and another takes over.
    Restarting,
    /// An outcome this build does not know.
    #[serde(other)]
    Other,
}
