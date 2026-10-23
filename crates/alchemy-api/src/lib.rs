//! HTTP API types shared by `alchemyd` and its web and desktop clients.
//!
//! Every route lives under [`API_PREFIX`]. Like the IPC protocol, these
//! shapes only grow additively; the JSON is pinned in
//! `tests/wire_stability.rs`. Bump [`API_VERSION`] on any breaking change.
//!
//! `GET /api/engine` returns an [`EngineStatus`], re-exported from
//! `alchemy-core` so the wire type and the domain type cannot drift.
//! `GET /api/events` streams [`ApiEvent`]s.
//!
//! The default build is pure. Enable the `socket` feature for the daemon
//! socket path shared by `alchemyd` and its local clients.

pub use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};
use serde::{Deserialize, Serialize, Serializer};

#[cfg(feature = "socket")]
mod socket;

#[cfg(feature = "socket")]
pub use socket::{SOCKET_ENV, SOCKET_FILE, default_socket_path, runtime_dir};

/// Path prefix for every API route.
pub const API_PREFIX: &str = "/api";

/// The current API version, sent as [`VersionResponse::api`].
///
/// Bumped only on a breaking change to a route or a wire shape. Additive
/// changes keep it as is.
pub const API_VERSION: u32 = 1;

/// `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Always `"ok"` when the daemon can answer.
    pub status: HealthStatus,
}

/// Coarse daemon health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HealthStatus {
    /// The daemon is serving requests.
    Ok,
}

/// `GET /api/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResponse {
    /// The daemon's release version.
    pub version: String,
    /// The daemon's [`API_VERSION`].
    pub api: u32,
}

/// Body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable detail, safe to show to the user.
    pub error: String,
    /// Machine-readable failure category. Decodes as
    /// [`ErrorCode::Unknown`] when absent.
    #[serde(default)]
    pub code: ErrorCode,
}

impl ErrorResponse {
    /// An error response with `code` and a human-readable `error`.
    pub fn new(code: ErrorCode, error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code,
        }
    }
}

/// Machine-readable failure category carried by [`ErrorResponse::code`].
///
/// Clients branch on this rather than on the HTTP status or the message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// No route matches the request path.
    NotFound,
    /// The route exists but does not accept the request method.
    MethodNotAllowed,
    /// The request was malformed.
    BadRequest,
    /// The daemon failed while handling a valid request.
    Internal,
    /// A code this build does not know, sent by a newer daemon, or no code
    /// at all.
    #[default]
    #[serde(other)]
    Unknown,
}

/// One message on the `GET /api/events` server-sent event stream.
///
/// Each variant is sent as an SSE frame whose `event:` field is
/// [`ApiEvent::name`] and whose `data:` field is the variant's payload as
/// JSON. Serialized with serde, the same event is
/// `{"event": <name>, "data": <payload>}`.
///
/// Not `#[non_exhaustive]`, so the daemon's SSE mapping must handle every
/// variant. Adding one is still additive on the wire: clients ignore event
/// names they do not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ApiEvent {
    /// The engine status, sent once on connect and again on every change.
    Engine(EngineStatus),
}

impl ApiEvent {
    /// The SSE `event:` name for this variant.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Engine(_) => "engine",
        }
    }

    /// The payload alone, which serializes as the SSE `data:` field.
    pub fn data(&self) -> EventData<'_> {
        EventData(self)
    }
}

/// An [`ApiEvent`]'s payload without its name, from [`ApiEvent::data`].
#[derive(Debug, Clone, Copy)]
pub struct EventData<'a>(&'a ApiEvent);

impl Serialize for EventData<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            ApiEvent::Engine(status) => status.serialize(serializer),
        }
    }
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
    fn api_event_data_is_the_payload() {
        let status = EngineStatus::new(EngineState::Ready);
        let event = ApiEvent::Engine(status.clone());
        assert_eq!(
            serde_json::to_string(&event.data()).unwrap(),
            serde_json::to_string(&status).unwrap()
        );
    }
}
