//! Daemon to client responses.

use serde::{Deserialize, Serialize};

/// A response sent from `alchemyd` to a local client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Response {
    /// Reply to [`crate::Request::Ping`].
    Pong,
    /// Reply to [`crate::Request::Version`].
    Version {
        /// The daemon's release version.
        daemon: String,
        /// The daemon's [`crate::PROTOCOL_VERSION`].
        protocol: u32,
    },
    /// The request failed.
    Error {
        /// Machine-readable failure category.
        code: ErrorCode,
        /// Human-readable detail, safe to show to the user.
        message: String,
    },
}

/// Machine-readable failure category carried by [`Response::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The request could not be decoded, or is not supported by this daemon.
    BadRequest,
    /// The daemon failed while handling a valid request.
    Internal,
}
