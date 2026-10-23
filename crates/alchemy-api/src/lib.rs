//! HTTP API types shared by `alchemyd` and its web and desktop clients.
//!
//! Every route lives under [`API_PREFIX`]. Like the IPC protocol, these
//! shapes only grow additively; the JSON is pinned by the tests below.
//!
//! `GET /api/engine` returns an [`EngineStatus`], re-exported from
//! `alchemy-core` so the wire type and the domain type cannot drift.
//! `GET /api/events` streams [`ApiEvent`]s.

pub use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};
use serde::{Deserialize, Serialize, Serializer};

/// Path prefix for every API route.
pub const API_PREFIX: &str = "/api";

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
}

/// Body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable detail, safe to show to the user.
    pub error: String,
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
    fn wire_shapes() {
        let health = HealthResponse {
            status: HealthStatus::Ok,
        };
        assert_eq!(
            serde_json::to_string(&health).unwrap(),
            r#"{"status":"ok"}"#
        );

        let version = VersionResponse {
            version: "0.1.0".into(),
        };
        assert_eq!(
            serde_json::to_string(&version).unwrap(),
            r#"{"version":"0.1.0"}"#
        );

        let error = ErrorResponse {
            error: "not found".into(),
        };
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"error":"not found"}"#
        );
    }

    #[test]
    fn engine_status_wire_shape() {
        let mut status = EngineStatus::new(EngineState::Ready);
        status.flavor = Some(EngineFlavor::DockerDesktop);
        status.engine_version = Some("29.8.1".into());
        status.api_version = Some("1.52".into());
        status.compose_version = Some("2.40.0".into());
        status.desktop_version = Some("4.93.0".into());
        status.endpoint = Some("unix:///var/run/docker.sock".into());
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            concat!(
                r#"{"state":"ready","flavor":"docker_desktop","engine_version":"29.8.1","#,
                r#""api_version":"1.52","compose_version":"2.40.0","desktop_version":"4.93.0","#,
                r#""endpoint":"unix:///var/run/docker.sock"}"#,
            )
        );

        assert_eq!(
            serde_json::to_string(&EngineStatus::new(EngineState::NotRunning)).unwrap(),
            concat!(
                r#"{"state":"not_running","flavor":null,"engine_version":null,"#,
                r#""api_version":null,"compose_version":null,"desktop_version":null,"#,
                r#""endpoint":null}"#,
            )
        );

        let state_names = [
            (EngineState::NotInstalled, r#""not_installed""#),
            (EngineState::NotRunning, r#""not_running""#),
            (EngineState::Starting, r#""starting""#),
            (EngineState::Ready, r#""ready""#),
            (EngineState::Unsupported, r#""unsupported""#),
        ];
        for (state, want) in state_names {
            assert_eq!(serde_json::to_string(&state).unwrap(), want);
        }

        let flavors = [
            (EngineFlavor::DockerDesktop, r#""docker_desktop""#),
            (EngineFlavor::DockerEngine, r#""docker_engine""#),
            (EngineFlavor::Other, r#""other""#),
        ];
        for (flavor, want) in flavors {
            assert_eq!(serde_json::to_string(&flavor).unwrap(), want);
        }
    }

    #[test]
    fn api_event_wire_shape() {
        let event = ApiEvent::Engine(EngineStatus::new(EngineState::Starting));
        assert_eq!(event.name(), "engine");
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"event":"engine","data":{"state":"starting","flavor":null,"#,
                r#""engine_version":null,"api_version":null,"compose_version":null,"#,
                r#""desktop_version":null,"endpoint":null}}"#,
            )
        );
        assert_eq!(serde_json::from_str::<ApiEvent>(&json).unwrap(), event);
    }

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
