//! Client to daemon requests.

use serde::{Deserialize, Serialize};

/// A request sent from a local client to `alchemyd`.
///
/// Serialised as an internally tagged JSON object: `{"type": "ping"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Request {
    /// Liveness probe. Answered with [`crate::Response::Pong`].
    Ping,
    /// Ask for the daemon's version. Answered with
    /// [`crate::Response::Version`].
    Version,
    /// Ask for the Docker engine's status. Answered with
    /// [`crate::Response::Engine`].
    Engine,
    /// Subscribe to the Docker engine's status. Answered with a
    /// [`crate::Response::Engine`] for the current status, then another on
    /// every change, until the client disconnects or the daemon exits.
    ///
    /// The daemon reads nothing else from the connection after this.
    WatchEngine,
}
