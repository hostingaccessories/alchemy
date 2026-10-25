//! Privileges: what needs root on this machine, whether it is done, and
//! how to get it.
//!
//! `GET /api/machine/privileges` returns a [`PrivilegeList`], one
//! [`Privilege`] per action, and `/api/events` sends the same list as the
//! `privileges` event on connect and on every change.
//!
//! `POST /api/machine/privileges/{action}` takes a [`PrivilegeRequest`]:
//! `{"grant": true}` to grant, `{"grant": false}` to undo. `{action}` is
//! `low-ports`, `firewall`, or `system-service` (the snake-case names work
//! too). It answers with a [`PrivilegeChange`] and never waits for a
//! password prompt:
//!
//! - `pending` (202): a system prompt is up. The action's state is
//!   `pending` until it ends; then [`Privilege::last`] says how.
//! - `restarting` (202): the change is on disk, and alchemyd shuts down
//!   and re-executes itself to put it in effect, keeping its PID. Its
//!   listeners close for a moment and the TCP token changes; reconnect and
//!   sign in again.
//! - `needs_terminal` (200): no prompt is safe or available. Run
//!   [`PrivilegeChange::command`] in a terminal, then send the same
//!   request again, which restarts alchemyd if that is all that is left.
//! - `unchanged` (200): there was nothing to do.
//!
//! An unknown action is a 404 `not_found`, a malformed body a 400
//! `bad_request`, an action whose grant or undo is already in flight a
//! 409 `conflict`, and one Alchemy can't change on this machine a 422
//! `unsupported`, with the reason as the error.
//!
//! Every `message` here is written for the people using Alchemy and safe
//! to show as is.

pub use alchemy_core::privilege::Action;
use serde::{Deserialize, Serialize};

/// `GET /api/machine/privileges`, and the `privileges` event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegeList {
    /// Each action, in display order.
    pub privileges: Vec<Privilege>,
}

/// One action and where it stands on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Privilege {
    /// The action.
    pub action: PrivilegeAction,
    /// Where it stands.
    pub state: PrivilegeState,
    /// Why it needs a terminal or isn't supported, for clients that
    /// branch on it.
    pub reason: Option<PrivilegeReason>,
    /// The same, in a sentence, when `state` is `needs_terminal` or
    /// `unsupported`.
    pub message: Option<String>,
    /// The `sudo alchemy-helper ...` command that grants it, when `state`
    /// is `needs_terminal`.
    pub command: Option<HelperCommand>,
    /// Whether granting or undoing it restarts alchemyd, as low ports do.
    pub restarts: bool,
    /// How the last grant or undo asked of this alchemyd ended, or `None`
    /// before any has ended. A restart forgets it.
    pub last: Option<PrivilegeAttempt>,
}

/// Something on this machine that needs root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PrivilegeAction {
    /// Let alchemyd listen on ports below 1024, such as 80, on Linux.
    LowPorts,
    /// Open Alchemy's ports in the host firewall.
    Firewall,
    /// Run alchemyd at boot as a system service, as the user.
    SystemService,
    /// An action this build does not know.
    #[serde(other)]
    Other,
}

impl PrivilegeAction {
    /// The domain action, or `None` for [`Self::Other`].
    pub fn action(self) -> Option<Action> {
        match self {
            Self::LowPorts => Some(Action::LowPorts),
            Self::Firewall => Some(Action::Firewall),
            Self::SystemService => Some(Action::SystemService),
            Self::Other => None,
        }
    }
}

impl From<Action> for PrivilegeAction {
    fn from(action: Action) -> Self {
        match action {
            Action::LowPorts => Self::LowPorts,
            Action::Firewall => Self::Firewall,
            Action::SystemService => Self::SystemService,
        }
    }
}

/// Where a [`Privilege`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PrivilegeState {
    /// Nothing to do here, such as low ports on macOS or no firewall.
    NotNeeded,
    /// Not done yet, and a system prompt can do it.
    Needed,
    /// Done, and can be undone.
    Granted,
    /// Done on disk but not in effect until alchemyd restarts, as after
    /// the command was run in a terminal. Granting again restarts
    /// alchemyd without a prompt.
    NeedsRestart,
    /// A grant or undo is in flight, such as while the prompt is up.
    Pending,
    /// Not done yet, and no prompt is safe or available: the user runs
    /// [`Privilege::command`] in a terminal.
    NeedsTerminal,
    /// Alchemy can't do it here; [`Privilege::message`] says why.
    Unsupported,
    /// A state this build does not know.
    #[serde(other)]
    Other,
}

/// Why a privilege needs a terminal or isn't supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PrivilegeReason {
    /// The helper could be swapped by a user, as with a dev build, so no
    /// password prompt is put in front of it.
    UntrustedHelper,
    /// Linux with no polkit agent to show a prompt, as over SSH.
    NoPolkitAgent,
    /// There is no `alchemy-helper` next to alchemyd.
    NoHelper,
    /// The helper acts only on the alchemyd next to it, which isn't the
    /// one running.
    NotHelperSibling,
    /// The firewall is raw nftables or iptables, which Alchemy never
    /// edits.
    UnmanagedFirewall,
    /// alchemyd runs as root.
    RunsAsRoot,
    /// alchemyd's binary is on a `nosuid` mount.
    NosuidMount,
    /// alchemyd runs with `no_new_privs`.
    NoNewPrivileges,
    /// A reason this build does not know.
    #[serde(other)]
    Other,
}

/// A command that runs `alchemy-helper` as root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperCommand {
    /// What to run, the program first, such as
    /// `["sudo", "/usr/libexec/alchemy/alchemy-helper", "grant-low-ports"]`.
    pub argv: Vec<String>,
    /// The same as one POSIX shell line, quoted where needed, to show and
    /// copy.
    pub line: String,
}

/// How a grant or undo ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegeAttempt {
    /// Whether it was a grant, rather than an undo.
    pub grant: bool,
    /// How it ended.
    pub result: AttemptResult,
    /// What went wrong, when it didn't succeed.
    pub message: Option<String>,
    /// When it ended, in Unix seconds.
    pub at: i64,
}

/// The result of a [`PrivilegeAttempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttemptResult {
    /// The helper ran and succeeded.
    Done,
    /// The user dismissed the prompt.
    Cancelled,
    /// The prompt refused, such as after a wrong password.
    Denied,
    /// No prompt could be shown after all, so the action now needs a
    /// terminal.
    NeedsTerminal,
    /// The prompt or the helper failed.
    Failed,
    /// A result this build does not know.
    #[serde(other)]
    Other,
}

/// The body of `POST /api/machine/privileges/{action}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegeRequest {
    /// `true` to grant the action, `false` to undo it.
    pub grant: bool,
}

/// What `POST /api/machine/privileges/{action}` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegeChange {
    /// What happened.
    pub outcome: ChangeOutcome,
    /// The command to run in a terminal, when `outcome` is
    /// `needs_terminal`. Undoing a granted action can need one too.
    pub command: Option<HelperCommand>,
    /// Why there is no prompt, when `outcome` is `needs_terminal`.
    pub message: Option<String>,
    /// The action as it stands now.
    pub privilege: Privilege,
}

/// The outcome of a [`PrivilegeChange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChangeOutcome {
    /// A system prompt is up; follow the action's state.
    Pending,
    /// alchemyd is restarting to put the change in effect.
    Restarting,
    /// Run [`PrivilegeChange::command`] in a terminal, then ask again.
    NeedsTerminal,
    /// It was already that way, or isn't needed.
    Unchanged,
    /// An outcome this build does not know.
    #[serde(other)]
    Other,
}
