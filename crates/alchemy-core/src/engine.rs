//! The Docker engine: whether it is usable, what it is, and what it runs.
//!
//! Everything here is a pure decision over plain values. Probing sockets,
//! calling the Engine API, and reading the event stream happen in the
//! daemon, which feeds the results in as [`EngineProbe`], platform strings,
//! and [`ContainerEvent`]s.

mod container;

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use container::{
    ContainerAction, ContainerEvent, ContainerState, ContainerStatus, Containers, Health,
    parse_action,
};

/// How usable the Docker engine is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EngineState {
    /// Neither the Docker CLI nor Docker Desktop was found.
    NotInstalled,
    /// Docker is installed but its socket is missing or refuses connections.
    NotRunning,
    /// The socket accepts connections but the engine does not answer yet.
    Starting,
    /// The engine answered a ping.
    Ready,
}

/// Which Docker distribution serves the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EngineFlavor {
    /// Docker Desktop on macOS or Linux.
    DockerDesktop,
    /// Docker Engine (Community or Enterprise), usually on Linux.
    DockerEngine,
    /// Anything else that speaks the Engine API, such as Colima or `OrbStack`.
    Other,
}

/// A snapshot of the engine as reported to clients.
///
/// Fields other than `state` are `None` until the engine has answered
/// enough requests to know them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EngineStatus {
    /// How usable the engine is.
    pub state: EngineState,
    /// Which distribution serves the engine.
    #[serde(default)]
    pub flavor: Option<EngineFlavor>,
    /// Engine version from `/version`, such as `27.3.1`.
    #[serde(default)]
    pub engine_version: Option<String>,
    /// Engine API version from `/version`, such as `1.47`.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Docker Compose plugin version, such as `2.29.7`.
    #[serde(default)]
    pub compose_version: Option<String>,
    /// Docker Desktop version, when the flavor is Docker Desktop.
    #[serde(default)]
    pub desktop_version: Option<String>,
    /// The endpoint the daemon talks to, such as `unix:///var/run/docker.sock`.
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl EngineStatus {
    /// A status with the given state and nothing else known.
    pub fn new(state: EngineState) -> Self {
        Self {
            state,
            flavor: None,
            engine_version: None,
            api_version: None,
            compose_version: None,
            desktop_version: None,
            endpoint: None,
        }
    }
}

/// What happened when the daemon tried the engine's socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SocketProbe {
    /// No socket exists at the endpoint.
    Missing,
    /// The socket exists but refused the connection.
    Refused,
    /// The socket accepted the connection but the ping failed.
    Unresponsive,
    /// The engine answered a ping.
    Responding,
}

/// The raw observations [`classify`] turns into an [`EngineState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EngineProbe {
    /// Result of connecting to and pinging the engine socket.
    pub socket: SocketProbe,
    /// Whether a `docker` CLI was found.
    pub cli_installed: bool,
    /// Whether a Docker Desktop installation was found.
    pub desktop_installed: bool,
}

/// Decide the engine's state from a probe.
pub fn classify(probe: &EngineProbe) -> EngineState {
    match probe.socket {
        SocketProbe::Responding => EngineState::Ready,
        SocketProbe::Unresponsive => EngineState::Starting,
        SocketProbe::Missing | SocketProbe::Refused => {
            if probe.cli_installed || probe.desktop_installed {
                EngineState::NotRunning
            } else {
                EngineState::NotInstalled
            }
        }
    }
}

const DESKTOP_PREFIX: &str = "Docker Desktop";
const ENGINE_PREFIX: &str = "Docker Engine";

/// Parse the Engine API's `/version` `Platform.Name` into a flavor and,
/// for Docker Desktop, its version.
///
/// Docker Desktop reports `Docker Desktop 4.34.2 (167172)`, Docker Engine
/// reports `Docker Engine - Community`, and other engines report their own
/// name or nothing at all.
pub fn flavor_from_platform(name: &str) -> (EngineFlavor, Option<String>) {
    let name = name.trim();
    if let Some(rest) = strip_word_prefix(name, DESKTOP_PREFIX) {
        let version = rest
            .split_whitespace()
            .next()
            .filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))
            .map(str::to_owned);
        return (EngineFlavor::DockerDesktop, version);
    }
    if strip_word_prefix(name, ENGINE_PREFIX).is_some() {
        return (EngineFlavor::DockerEngine, None);
    }
    (EngineFlavor::Other, None)
}

fn strip_word_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(prefix)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
}

const RECONNECT_BASE_SECS: u64 = 1;
const RECONNECT_MAX_SECS: u64 = 10;

/// How long to wait before reconnect attempt `attempt`, counting from 0.
///
/// Starts at 1s and doubles per attempt, capped at 10s.
pub fn reconnect_delay(attempt: u32) -> Duration {
    let secs = RECONNECT_BASE_SECS
        .checked_shl(attempt)
        .unwrap_or(u64::MAX)
        .min(RECONNECT_MAX_SECS);
    Duration::from_secs(secs)
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
    fn classify_maps_probes_to_states() {
        use EngineState::{NotInstalled, NotRunning, Ready, Starting};
        use SocketProbe::{Missing, Refused, Responding, Unresponsive};
        let cases = [
            (Responding, false, false, Ready),
            (Responding, true, true, Ready),
            (Unresponsive, false, false, Starting),
            (Unresponsive, true, false, Starting),
            (Missing, false, false, NotInstalled),
            (Missing, true, false, NotRunning),
            (Missing, false, true, NotRunning),
            (Missing, true, true, NotRunning),
            (Refused, false, false, NotInstalled),
            (Refused, true, false, NotRunning),
            (Refused, false, true, NotRunning),
        ];
        for (socket, cli_installed, desktop_installed, want) in cases {
            let probe = EngineProbe {
                socket,
                cli_installed,
                desktop_installed,
            };
            assert_eq!(classify(&probe), want, "{probe:?}");
        }
    }

    #[test]
    fn flavor_from_platform_parses_known_names() {
        let cases = [
            (
                "Docker Desktop 4.34.2 (167172)",
                EngineFlavor::DockerDesktop,
                Some("4.34.2"),
            ),
            (
                "  Docker Desktop 4.40.0 (187762)  ",
                EngineFlavor::DockerDesktop,
                Some("4.40.0"),
            ),
            (
                "Docker Desktop 4.5.0",
                EngineFlavor::DockerDesktop,
                Some("4.5.0"),
            ),
            ("Docker Desktop", EngineFlavor::DockerDesktop, None),
            ("Docker Desktop (beta)", EngineFlavor::DockerDesktop, None),
            (
                "Docker Engine - Community",
                EngineFlavor::DockerEngine,
                None,
            ),
            (
                "Docker Engine - Enterprise",
                EngineFlavor::DockerEngine,
                None,
            ),
            ("Docker Engine", EngineFlavor::DockerEngine, None),
            ("Docker DesktopX 1.0", EngineFlavor::Other, None),
            ("OrbStack", EngineFlavor::Other, None),
            ("", EngineFlavor::Other, None),
            ("docker desktop 4.34.2", EngineFlavor::Other, None),
        ];
        for (name, flavor, version) in cases {
            assert_eq!(
                flavor_from_platform(name),
                (flavor, version.map(str::to_owned)),
                "{name:?}"
            );
        }
    }

    #[test]
    fn reconnect_delay_doubles_then_caps() {
        let cases = [
            (0, 1),
            (1, 2),
            (2, 4),
            (3, 8),
            (4, 10),
            (5, 10),
            (63, 10),
            (64, 10),
            (u32::MAX, 10),
        ];
        for (attempt, secs) in cases {
            assert_eq!(
                reconnect_delay(attempt),
                Duration::from_secs(secs),
                "{attempt}"
            );
        }
    }

    #[test]
    fn engine_state_serializes_snake_case() {
        let cases = [
            (EngineState::NotInstalled, "\"not_installed\""),
            (EngineState::NotRunning, "\"not_running\""),
            (EngineState::Starting, "\"starting\""),
            (EngineState::Ready, "\"ready\""),
        ];
        for (state, want) in cases {
            assert_eq!(serde_json::to_string(&state).unwrap(), want);
            assert_eq!(serde_json::from_str::<EngineState>(want).unwrap(), state);
        }
    }

    #[test]
    fn engine_flavor_serializes_snake_case() {
        let cases = [
            (EngineFlavor::DockerDesktop, "\"docker_desktop\""),
            (EngineFlavor::DockerEngine, "\"docker_engine\""),
            (EngineFlavor::Other, "\"other\""),
        ];
        for (flavor, want) in cases {
            assert_eq!(serde_json::to_string(&flavor).unwrap(), want);
            assert_eq!(serde_json::from_str::<EngineFlavor>(want).unwrap(), flavor);
        }
    }

    #[test]
    fn engine_status_wire_shape() {
        let mut status = EngineStatus::new(EngineState::Ready);
        status.flavor = Some(EngineFlavor::DockerDesktop);
        status.engine_version = Some("27.3.1".into());
        status.api_version = Some("1.47".into());
        status.compose_version = Some("2.29.7".into());
        status.desktop_version = Some("4.34.2".into());
        status.endpoint = Some("unix:///var/run/docker.sock".into());
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "state": "ready",
                "flavor": "docker_desktop",
                "engine_version": "27.3.1",
                "api_version": "1.47",
                "compose_version": "2.29.7",
                "desktop_version": "4.34.2",
                "endpoint": "unix:///var/run/docker.sock",
            })
        );
        assert_eq!(
            serde_json::from_value::<EngineStatus>(json).unwrap(),
            status
        );
    }

    #[test]
    fn engine_status_unknown_fields_are_null() {
        let status = EngineStatus::new(EngineState::NotRunning);
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "state": "not_running",
                "flavor": null,
                "engine_version": null,
                "api_version": null,
                "compose_version": null,
                "desktop_version": null,
                "endpoint": null,
            })
        );
        let sparse: EngineStatus = serde_json::from_str(r#"{"state":"not_running"}"#).unwrap();
        assert_eq!(sparse, status);
    }
}
