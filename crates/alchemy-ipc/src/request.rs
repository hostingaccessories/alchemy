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
}
