//! Text and JSON rendering for `alchemy status`. Pure: no I/O.

use std::path::Path;

use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};

/// What the daemon said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Daemon {
    pub(crate) version: String,
    pub(crate) protocol: u32,
}

/// The first line of `alchemy status`.
pub(crate) fn daemon_line(daemon: &Daemon, socket: &Path) -> String {
    format!(
        "alchemyd {} (protocol {}) at {}",
        daemon.version,
        daemon.protocol,
        socket.display()
    )
}

/// One line describing the Docker engine, leading with what is wrong
/// when something is.
pub(crate) fn engine_line(engine: &EngineStatus) -> String {
    let endpoint = engine.endpoint.as_deref();
    match engine.state {
        EngineState::NotInstalled => {
            "Docker isn't installed: install Docker Desktop or Docker Engine".to_owned()
        }
        EngineState::NotRunning => match endpoint {
            Some(endpoint) => format!("{} isn't running at {endpoint}", product(engine)),
            None => format!("{} isn't running", product(engine)),
        },
        EngineState::Starting => format!("{} is starting…", product(engine)),
        EngineState::Ready => ready_line(engine),
        EngineState::Unsupported => match endpoint {
            Some(endpoint) => {
                format!(
                    "Docker endpoint {endpoint} isn't supported: Alchemy needs a unix:// socket"
                )
            }
            None => "Docker endpoint isn't supported: Alchemy needs a unix:// socket".to_owned(),
        },
        _ => "Docker is in a state this alchemy doesn't know; upgrade alchemy".to_owned(),
    }
}

/// The `--json` object: the daemon, its socket, and the engine status.
pub(crate) fn status_json(daemon: &Daemon, socket: &Path, engine: &EngineStatus) -> String {
    serde_json::json!({
        "daemon": daemon.version,
        "protocol": daemon.protocol,
        "socket": socket,
        "engine": engine,
    })
    .to_string()
}

fn product(engine: &EngineStatus) -> &'static str {
    match engine.flavor {
        Some(EngineFlavor::DockerDesktop) => "Docker Desktop",
        _ => "Docker",
    }
}

fn ready_line(engine: &EngineStatus) -> String {
    let versions = [
        engine
            .desktop_version
            .as_deref()
            .map(|v| format!("Desktop {v}")),
        engine
            .engine_version
            .as_deref()
            .map(|v| format!("Engine {v}")),
        engine
            .compose_version
            .as_deref()
            .map(|v| format!("Compose v{}", v.strip_prefix('v').unwrap_or(v))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    if engine.compose_version.is_some() {
        format!("Docker ready: {versions}")
    } else if versions.is_empty() {
        "Docker Compose wasn't found (Docker ready)".to_owned()
    } else {
        format!("Docker Compose wasn't found (Docker ready: {versions})")
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

    fn status(
        state: EngineState,
        flavor: Option<EngineFlavor>,
        versions: [Option<&str>; 3],
        endpoint: Option<&str>,
    ) -> EngineStatus {
        let [desktop, engine, compose] = versions.map(|v| v.map(str::to_owned));
        let mut status = EngineStatus::new(state);
        status.flavor = flavor;
        status.desktop_version = desktop;
        status.engine_version = engine;
        status.compose_version = compose;
        status.endpoint = endpoint.map(str::to_owned);
        status
    }

    #[test]
    fn engine_line_per_state() {
        use EngineFlavor::{DockerDesktop, DockerEngine, Other};
        use EngineState::{NotInstalled, NotRunning, Ready, Starting, Unsupported};
        const SOCK: Option<&str> = Some("unix:///var/run/docker.sock");
        const TCP: Option<&str> = Some("tcp://10.0.0.5:2375");
        let none = [None, None, None];
        let cases = [
            (
                status(NotInstalled, None, none, None),
                "Docker isn't installed: install Docker Desktop or Docker Engine",
            ),
            (status(NotRunning, None, none, None), "Docker isn't running"),
            (
                status(NotRunning, None, none, SOCK),
                "Docker isn't running at unix:///var/run/docker.sock",
            ),
            (
                status(NotRunning, Some(DockerDesktop), none, None),
                "Docker Desktop isn't running",
            ),
            (status(Starting, None, none, SOCK), "Docker is starting…"),
            (
                status(Starting, Some(DockerDesktop), none, None),
                "Docker Desktop is starting…",
            ),
            (
                status(
                    Ready,
                    Some(DockerDesktop),
                    [Some("4.34.2"), Some("27.3.1"), Some("2.29.7")],
                    SOCK,
                ),
                "Docker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7",
            ),
            (
                status(
                    Ready,
                    Some(DockerEngine),
                    [None, Some("28.0.1"), Some("v2.33.0")],
                    SOCK,
                ),
                "Docker ready: Engine 28.0.1, Compose v2.33.0",
            ),
            (
                status(Ready, Some(Other), [None, Some("28.0.1"), None], SOCK),
                "Docker Compose wasn't found (Docker ready: Engine 28.0.1)",
            ),
            (
                status(Ready, None, none, None),
                "Docker Compose wasn't found (Docker ready)",
            ),
            (
                status(Ready, None, [None, None, Some("2.29.7")], None),
                "Docker ready: Compose v2.29.7",
            ),
            (
                status(Unsupported, None, none, TCP),
                "Docker endpoint tcp://10.0.0.5:2375 isn't supported: Alchemy needs a unix:// socket",
            ),
            (
                status(Unsupported, None, none, None),
                "Docker endpoint isn't supported: Alchemy needs a unix:// socket",
            ),
        ];
        for (engine, want) in cases {
            assert_eq!(engine_line(&engine), want, "{engine:?}");
        }
    }

    #[test]
    fn daemon_line_names_version_protocol_and_socket() {
        let daemon = Daemon {
            version: "1.2.3".into(),
            protocol: 1,
        };
        assert_eq!(
            daemon_line(&daemon, Path::new("/run/alchemyd.sock")),
            "alchemyd 1.2.3 (protocol 1) at /run/alchemyd.sock"
        );
    }

    #[test]
    fn status_json_nests_the_engine() {
        let daemon = Daemon {
            version: "1.2.3".into(),
            protocol: 1,
        };
        let engine = EngineStatus::new(EngineState::Starting);
        let json: serde_json::Value = serde_json::from_str(&status_json(
            &daemon,
            Path::new("/run/alchemyd.sock"),
            &engine,
        ))
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "daemon": "1.2.3",
                "protocol": 1,
                "socket": "/run/alchemyd.sock",
                "engine": serde_json::to_value(&engine).unwrap(),
            })
        );
    }
}
