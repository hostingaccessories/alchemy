//! `alchemy status`: whether alchemyd is reachable, which version it runs,
//! whether Docker is ready, whether Docker's networks overlap the home
//! network, and whether the web address proxy could open its ports.

use std::io::Write;
use std::path::Path;

use alchemy_api::ApiEvent;
use alchemy_api::network::{ListenerState, NetworkReport, NetworkWarning, ProxyReport};
use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};

use crate::context::Context;
use crate::daemon::{Daemon, DaemonInfo};
use crate::error::ClientError;

/// The command's name, for errors.
pub(crate) const NAME: &str = "status";

/// Arguments to `alchemy status`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// Keep running and print the Docker status again each time it
    /// changes, until interrupted.
    #[arg(long)]
    pub watch: bool,
}

/// Run `alchemy status`.
///
/// With `--watch` this only returns on an error, such as the daemon
/// stopping; the caller ends it by dropping the future.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    let daemon = ctx.daemon_info().await?.clone();
    if args.watch {
        return watch(ctx, &daemon).await;
    }
    let engine = ctx.get::<EngineStatus>("/api/engine").await?;
    let network = ctx.get::<NetworkReport>("/api/network").await.ok();
    for line in status_lines(&daemon, ctx.socket(), &engine, network.as_ref(), ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// Follow the engine status, printing the engine line again only when it
/// changes, or every update with `--json`. Network warnings are left to
/// `alchemy status` without `--watch`.
async fn watch<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    daemon: &DaemonInfo,
) -> Result<(), ClientError> {
    let mut events = ctx.events().await?;
    let mut last_line = None;
    loop {
        let Some(event) = events.next().await? else {
            return Err(ClientError::Disconnected);
        };
        let ApiEvent::Engine(engine) = event else {
            continue;
        };
        let line = engine_line(&engine);
        if last_line.is_none() {
            for line in status_lines(daemon, ctx.socket(), &engine, None, ctx.json()) {
                ctx.out().line(line)?;
            }
        } else if ctx.json() {
            let json = status_json(daemon, ctx.socket(), &engine, None);
            ctx.out().line(json)?;
        } else if last_line.as_ref() != Some(&line) {
            ctx.out().line(&line)?;
        }
        last_line = Some(line);
    }
}

/// What `alchemy status` prints: the daemon and engine lines, then any
/// network and proxy warnings, or one JSON object.
pub(crate) fn status_lines(
    daemon: &DaemonInfo,
    socket: &Path,
    engine: &EngineStatus,
    network: Option<&NetworkReport>,
    json: bool,
) -> Vec<String> {
    if json {
        return vec![status_json(daemon, socket, engine, network)];
    }
    let mut lines = vec![daemon_line(daemon, socket), engine_line(engine)];
    if let Some(network) = network {
        lines.extend(warning_lines(&network.warnings));
        lines.extend(proxy_lines(&network.proxy));
    }
    lines
}

/// A warning for each proxy port that couldn't be opened: port 80 first,
/// with its fix, then each app's own port.
pub(crate) fn proxy_lines(proxy: &ProxyReport) -> Vec<String> {
    let mut lines = Vec::new();
    if proxy.host.state == ListenerState::Unavailable {
        lines.push(format!(
            "Warning: {}",
            proxy.host.message.as_deref().unwrap_or(
                "Alchemy can't use port 80. Your apps still work at their IP:port address."
            )
        ));
        if let Some(fix) = proxy.host.fix.as_deref() {
            lines.push(format!("  Fix: {fix}"));
        }
    }
    for port in &proxy.ports {
        if port.state != ListenerState::Unavailable {
            continue;
        }
        let whose = port
            .app
            .as_deref()
            .map_or_else(String::new, |app| format!("{app}'s address on "));
        let why = port.message.as_deref().unwrap_or("it can't be opened.");
        lines.push(format!(
            "Warning: {whose}port {} doesn't work: {why}",
            port.port
        ));
    }
    lines
}

/// Each warning as `Warning: ...`, followed by its fix, indented. A fix
/// already shown for an earlier warning is not repeated.
pub(crate) fn warning_lines(warnings: &[NetworkWarning]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut shown: Vec<&str> = Vec::new();
    for warning in warnings {
        lines.push(format!("Warning: {}", warning.message));
        let Some(fix) = warning.fix.as_deref() else {
            continue;
        };
        if shown.contains(&fix) {
            lines.push("  Fix: the same as above.".to_owned());
            continue;
        }
        shown.push(fix);
        let mut fix_lines = fix.lines();
        if let Some(first) = fix_lines.next() {
            lines.push(format!("  Fix: {first}"));
        }
        lines.extend(fix_lines.map(|line| format!("  {line}")));
    }
    lines
}

/// The first line of `alchemy status`.
pub(crate) fn daemon_line(daemon: &DaemonInfo, socket: &Path) -> String {
    format!(
        "alchemyd {} (protocol {}) at {}",
        daemon.version,
        daemon.api,
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

/// The `--json` object: the daemon, its socket, the engine status, and
/// the network report, when there is one.
pub(crate) fn status_json(
    daemon: &DaemonInfo,
    socket: &Path,
    engine: &EngineStatus,
    network: Option<&NetworkReport>,
) -> String {
    let mut json = serde_json::json!({
        "daemon": daemon.version,
        "protocol": daemon.api,
        "socket": socket,
        "engine": engine,
    });
    if let (Some(network), Some(object)) = (network, json.as_object_mut()) {
        object.insert(
            "network".to_owned(),
            serde_json::to_value(network).unwrap_or_default(),
        );
    }
    json.to_string()
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
    use alchemy_api::network::NetworkWarningKind;

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::{FAKE_SOCKET, FakeDaemon, sse};
    use crate::sse::SseEvent;

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
        let daemon = DaemonInfo {
            version: "1.2.3".into(),
            api: 1,
        };
        assert_eq!(
            daemon_line(&daemon, Path::new("/run/alchemyd.sock")),
            "alchemyd 1.2.3 (protocol 1) at /run/alchemyd.sock"
        );
    }

    #[test]
    fn status_json_nests_the_engine() {
        let daemon = DaemonInfo {
            version: "1.2.3".into(),
            api: 1,
        };
        let engine = EngineStatus::new(EngineState::Starting);
        let json: serde_json::Value = serde_json::from_str(&status_json(
            &daemon,
            Path::new("/run/alchemyd.sock"),
            &engine,
            None,
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

    const DAEMON_LINE: &str = "alchemyd 1.2.3 (protocol 1) at /run/alchemyd.sock";
    const READY_LINE: &str = "Docker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7";

    fn ready() -> EngineStatus {
        status(
            EngineState::Ready,
            Some(EngineFlavor::DockerDesktop),
            [Some("4.34.2"), Some("27.3.1"), Some("2.29.7")],
            Some("unix:///var/run/docker.sock"),
        )
    }

    fn engine(state: EngineState) -> Result<SseEvent, ClientError> {
        engine_event(&EngineStatus::new(state))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn engine_event(engine: &EngineStatus) -> Result<SseEvent, ClientError> {
        Ok(sse("engine", &serde_json::to_string(engine).unwrap()))
    }

    const ENGINE: &str = "/api/engine";
    const NETWORK: &str = "/api/network";

    struct Run {
        result: Result<(), ClientError>,
        out: String,
        requests: Vec<String>,
    }

    async fn run_with(fake: FakeDaemon, json: bool, watch: bool) -> Run {
        let mut ctx = Context::buffered(fake, json, NAME);
        let result = run(&mut ctx, Args { watch }).await;
        Run {
            result,
            out: ctx.written(),
            requests: ctx.daemon().requests().to_vec(),
        }
    }

    fn json_lines(out: &str) -> Vec<serde_json::Value> {
        out.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn warning(kind: NetworkWarningKind, message: &str, fix: Option<&str>) -> NetworkWarning {
        NetworkWarning {
            kind,
            message: message.into(),
            fix: fix.map(str::to_owned),
            subnet: None,
            host: None,
            host_subnet: None,
            network: None,
        }
    }

    fn overlapping() -> NetworkReport {
        NetworkReport {
            engine_ready: true,
            warnings: vec![
                warning(
                    NetworkWarningKind::BridgeOverlap,
                    "Docker's default bridge network (172.17.0.0/16) overlaps your local network 172.17.0.0/16.",
                    Some("Add these settings:\n{\n  \"bip\": \"172.18.0.1/24\"\n}"),
                ),
                warning(
                    NetworkWarningKind::PoolOverlap,
                    "Docker's address pool 172.17.0.0/16 overlaps your local network 172.17.0.0/16.",
                    Some("Add these settings:\n{\n  \"bip\": \"172.18.0.1/24\"\n}"),
                ),
                warning(
                    NetworkWarningKind::CreateFailed,
                    "Alchemy couldn't create its network alchemy.",
                    None,
                ),
            ],
            ..NetworkReport::default()
        }
    }

    #[test]
    fn warnings_print_each_fix_once() {
        assert_eq!(
            warning_lines(&overlapping().warnings),
            [
                "Warning: Docker's default bridge network (172.17.0.0/16) overlaps your local network 172.17.0.0/16.",
                "  Fix: Add these settings:",
                "  {",
                "    \"bip\": \"172.18.0.1/24\"",
                "  }",
                "Warning: Docker's address pool 172.17.0.0/16 overlaps your local network 172.17.0.0/16.",
                "  Fix: the same as above.",
                "Warning: Alchemy couldn't create its network alchemy.",
            ]
        );
        assert_eq!(warning_lines(&[]), Vec::<String>::new());
    }

    #[tokio::test]
    async fn status_prints_the_daemon_and_the_engine() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .ok(Method::GET, NETWORK, &NetworkReport::default());
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        assert_eq!(run.out, format!("{DAEMON_LINE}\n{READY_LINE}\n"));
        assert_eq!(
            run.requests,
            ["GET /api/version", "GET /api/engine", "GET /api/network"]
        );
    }

    #[tokio::test]
    async fn status_prints_network_warnings() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .ok(Method::GET, NETWORK, &overlapping());
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        let lines: Vec<&str> = run.out.lines().collect();
        assert_eq!(lines[..2], [DAEMON_LINE, READY_LINE]);
        assert_eq!(lines[2..], warning_lines(&overlapping().warnings));
    }

    #[test]
    fn proxy_warnings_cover_port_80_and_app_ports() {
        use alchemy_api::network::ProxyListener;
        let listener =
            |port, app: Option<&str>, state, message: Option<&str>, fix: Option<&str>| {
                ProxyListener {
                    port,
                    app: app.map(Into::into),
                    state,
                    message: message.map(Into::into),
                    fix: fix.map(Into::into),
                }
            };
        let proxy = ProxyReport {
            reachability: None,
            host: listener(
                80,
                None,
                ListenerState::Unavailable,
                Some(
                    "Alchemy doesn't have permission to listen on port 80, so addresses like http://<app>.localhost don't work. Each app's own port still does.",
                ),
                Some("Let alchemyd use low ports."),
            ),
            ports: vec![
                listener(8100, Some("blog"), ListenerState::Listening, None, None),
                listener(
                    8101,
                    Some("wiki"),
                    ListenerState::Unavailable,
                    Some("Another program is using port 8101."),
                    None,
                ),
            ],
        };
        assert_eq!(
            proxy_lines(&proxy),
            [
                "Warning: Alchemy doesn't have permission to listen on port 80, so addresses like http://<app>.localhost don't work. Each app's own port still does.",
                "  Fix: Let alchemyd use low ports.",
                "Warning: wiki's address on port 8101 doesn't work: Another program is using port 8101.",
            ]
        );
        assert_eq!(proxy_lines(&ProxyReport::default()), Vec::<String>::new());
    }

    #[tokio::test]
    async fn status_skips_network_warnings_an_older_daemon_lacks() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .error(Method::GET, NETWORK, 404, "not_found", "not found");
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        assert_eq!(run.out, format!("{DAEMON_LINE}\n{READY_LINE}\n"));
    }

    #[tokio::test]
    async fn status_json_prints_one_object() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .ok(Method::GET, NETWORK, &overlapping());
        let run = run_with(fake, true, false).await;
        run.result.unwrap();
        assert_eq!(
            json_lines(&run.out),
            [serde_json::json!({
                "daemon": "1.2.3",
                "protocol": 1,
                "socket": FAKE_SOCKET,
                "engine": serde_json::to_value(ready()).unwrap(),
                "network": serde_json::to_value(overlapping()).unwrap(),
            })]
        );
    }

    #[tokio::test]
    async fn watch_prints_the_engine_line_only_when_it_changes() {
        let mut api_only = EngineStatus::new(EngineState::Starting);
        api_only.api_version = Some("1.47".into());
        let fake = FakeDaemon::new().version("1.2.3", 1).events([
            engine(EngineState::NotRunning),
            engine(EngineState::Starting),
            Ok(sse("some_future_topic", "{}")),
            Ok(sse(
                "metrics",
                r#"{"at":null,"interval":60,"containers_in_vm":false,"machine":null,"projects":[],"containers":[]}"#,
            )),
            Ok(sse("apps", r#"{"apps":[],"unmanaged":[],"dirs":[]}"#)),
            engine_event(&api_only),
            engine_event(&ready()),
        ]);
        let run = run_with(fake, false, true).await;
        assert!(
            matches!(run.result, Err(ClientError::Disconnected)),
            "{:?}",
            run.result
        );
        assert_eq!(
            run.out.lines().collect::<Vec<_>>(),
            [
                DAEMON_LINE,
                "Docker isn't running",
                "Docker is starting…",
                READY_LINE
            ]
        );
        assert_eq!(run.requests, ["GET /api/version", "GET /api/events"]);
    }

    #[tokio::test]
    async fn watch_json_prints_every_update() {
        let fake = FakeDaemon::new().version("1.2.3", 1).events([
            engine(EngineState::Starting),
            engine(EngineState::Starting),
            engine_event(&ready()),
        ]);
        let run = run_with(fake, true, true).await;
        assert!(matches!(run.result, Err(ClientError::Disconnected)));
        let states: Vec<_> = json_lines(&run.out)
            .iter()
            .map(|json| {
                assert_eq!(json["daemon"], "1.2.3");
                assert_eq!(json["socket"], FAKE_SOCKET);
                json["engine"]["state"].clone()
            })
            .collect();
        assert_eq!(states, ["starting", "starting", "ready"]);
    }

    #[tokio::test]
    async fn failures_are_reported_as_errors() {
        let unreachable = || ClientError::DaemonUnreachable(format!("{FAKE_SOCKET}: refused"));
        let version = || FakeDaemon::new().version("0.0.1", 1);
        let too_old = "alchemyd 0.0.1 is too old for `status`";
        let cases: Vec<(FakeDaemon, bool, u8, &str)> = vec![
            (
                version().error(Method::GET, ENGINE, 404, "not_found", "not found"),
                false,
                76,
                too_old,
            ),
            (
                version()
                    .refuse_events(404, br#"{"error":"not found","code":"not_found"}"#.to_vec()),
                true,
                76,
                too_old,
            ),
            (
                version().error(
                    Method::GET,
                    ENGINE,
                    500,
                    "internal",
                    "engine monitor stopped",
                ),
                false,
                1,
                "alchemyd: engine monitor stopped",
            ),
            (
                version().reply(Method::GET, ENGINE, 200, b"pong".to_vec()),
                false,
                76,
                "cannot talk to alchemyd: decode: ",
            ),
            (
                version().events([Ok(sse("engine", "pong"))]),
                true,
                76,
                "cannot talk to alchemyd: decode: ",
            ),
            (
                version().fail_events(unreachable()),
                true,
                69,
                "cannot reach alchemyd: /run/alchemyd.sock: refused",
            ),
            (
                FakeDaemon::new().fail(Method::GET, "/api/version", unreachable()),
                false,
                69,
                "cannot reach alchemyd: /run/alchemyd.sock: refused",
            ),
            (
                FakeDaemon::new().fail(Method::GET, "/api/version", unreachable()),
                true,
                69,
                "cannot reach alchemyd: /run/alchemyd.sock: refused",
            ),
        ];
        for (fake, watch, code, message) in cases {
            let run = run_with(fake, false, watch).await;
            let err = run.result.unwrap_err();
            assert!(err.to_string().starts_with(message), "{err}");
            assert_eq!(err.exit_code(), code, "{err}");
            assert_eq!(run.out, "", "{err}");
        }
    }
}
