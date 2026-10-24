//! Privileges: what needs root on this machine, whether it is done, and
//! how to get it.
//!
//! Everything here is a pure decision over plain values. The daemon reads
//! the OS, the helper's metadata, its listeners' bind errors, the active
//! firewall, and whether a polkit agent answers, then passes them in as
//! [`PrivilegeFacts`]. Out come each [`Action`]'s [`PrivilegeState`], the
//! [`Elevation`] method, and the exact argv to run.
//!
//! `alchemy-helper` depends on no internal crate, so the argv built by
//! [`HelperOp::args`] is the contract between the two: a contract test in
//! the daemon checks that every one of them parses in the helper.

mod elevate;
mod helper;
mod op;
mod state;

pub use elevate::{
    CommandLine, Elevation, OSASCRIPT, OSASCRIPT_SCRIPT, PKEXEC, PKEXEC_FLAGS, SUDO,
    TerminalReason, elevated_command, elevation, sudo_command,
};
pub use helper::{
    ALCHEMYD, Distrust, FileKind, FileMeta, HELPER, HelperPath, HelperPathError, HelperTrust,
    helper_trust,
};
pub use op::{
    Action, ActionError, HelperOp, MIN_PROXY_PORT, ProxyPorts, ProxyPortsError, ServiceUid,
    ServiceUidError,
};
pub use state::{
    Firewall, FirewallFacts, HelperFacts, HostListener, LowPortFacts, PrivilegeFacts,
    PrivilegeState, ServiceFacts, Step, UnsupportedReason, state, states, step,
};
