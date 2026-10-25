//! Starting Docker Desktop.
//!
//! `POST /api/engine/start` takes an empty JSON object, `{}`, and answers
//! at once with an [`EngineStart`]; follow `GET /api/engine` or the
//! `engine` event to see Docker come up. Alchemy never starts Docker
//! unless asked.
//!
//! - `starting` (202): Docker Desktop is launching, or was already on its
//!   way.
//! - `already_running` (200): Docker is ready.
//! - `unsupported` (200): Alchemy doesn't start this Docker, such as
//!   Docker Engine on Linux, which needs root. [`EngineStart::command`]
//!   is the command to run in a terminal, when there is one.
//!
//! A launch command that fails is a 500 `internal`, with what it said as
//! the error.

use serde::{Deserialize, Serialize};

/// What `POST /api/engine/start` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStart {
    /// What happened.
    pub result: EngineStartResult,
    /// The same, in a sentence safe to show.
    pub message: String,
    /// What to run in a terminal instead, such as
    /// `sudo systemctl start docker`, when `result` is `unsupported`.
    pub command: Option<String>,
}

/// The result of an [`EngineStart`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EngineStartResult {
    /// Docker Desktop is starting.
    Starting,
    /// Docker is ready already.
    AlreadyRunning,
    /// Alchemy doesn't start this Docker.
    Unsupported,
    /// A result this build does not know.
    #[serde(other)]
    Other,
}
