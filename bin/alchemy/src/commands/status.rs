//! `alchemy status`: whether alchemyd is reachable, which version it runs,
//! whether Docker is ready, whether alchemyd starts on its own and where
//! it logs, whether apps' `.local` names are announced, what came back
//! after the last restart, wake, or Docker returning, whether Docker's
//! networks overlap the home network, and whether the web address proxy's
//! ports work.

use std::io::Write;
use std::path::Path;

use alchemy_api::ApiEvent;
use alchemy_api::autostart::Autostart;
use alchemy_api::network::{
    ListenerState, MdnsBackend, MdnsReport, MdnsState, NetworkReport, NetworkWarning, ProxyReport,
};
use alchemy_api::privileges::PrivilegeAction;
use alchemy_api::reconcile::{
    Reconcile, ReconcileApp, ReconcileOutcome, ReconcileStatus, ReconcileTrigger, SkipReason,
};
use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};
use alchemy_core::lifecycle::{DockerStart, plan_docker_start};

use crate::commands::machine::autostart::{self, logs_text, mode_text};
use crate::commands::machine::docker;
use crate::context::Context;
use crate::daemon::{Daemon, DaemonInfo};
use crate::error::ClientError;
use crate::service::host_os;

/// The route the last reconcile comes from.
pub(crate) const RECONCILE_PATH: &str = "/api/reconcile";

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
    let autostart = ctx.get::<Autostart>(autostart::PATH).await.ok();
    let reconcile = ctx.get::<ReconcileStatus>(RECONCILE_PATH).await.ok();
    let report = Report {
        engine: &engine,
        network: network.as_ref(),
        autostart: autostart.as_ref(),
        reconcile: reconcile.as_ref(),
        now: unix_now(),
    };
    for line in status_lines(&daemon, ctx.socket(), &report, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// The wall clock in Unix seconds, for how long ago the last reconcile
/// ran. A clock before 1970 reads as 0.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Everything `alchemy status` reports beyond the daemon itself. Each
/// part but the engine is missing from an older alchemyd, and from
/// `--watch`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report<'a> {
    pub(crate) engine: &'a EngineStatus,
    pub(crate) network: Option<&'a NetworkReport>,
    pub(crate) autostart: Option<&'a Autostart>,
    pub(crate) reconcile: Option<&'a ReconcileStatus>,
    /// Now, in Unix seconds.
    pub(crate) now: i64,
}

impl<'a> Report<'a> {
    /// The engine alone, as `--watch` follows it.
    pub(crate) const fn engine(engine: &'a EngineStatus) -> Self {
        Self {
            engine,
            network: None,
            autostart: None,
            reconcile: None,
            now: 0,
        }
    }
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
            for line in status_lines(daemon, ctx.socket(), &Report::engine(&engine), ctx.json()) {
                ctx.out().line(line)?;
            }
        } else if ctx.json() {
            let json = status_json(daemon, ctx.socket(), &Report::engine(&engine));
            ctx.out().line(json)?;
        } else if last_line.as_ref() != Some(&line) {
            ctx.out().line(&line)?;
        }
        last_line = Some(line);
    }
}

/// What `alchemy status` prints: the daemon and engine lines, with how
/// to start Docker when it is down, autostart and the logs, local names,
/// the last reconcile, then any network, proxy, and local names warnings;
/// or one JSON object.
pub(crate) fn status_lines(
    daemon: &DaemonInfo,
    socket: &Path,
    report: &Report<'_>,
    json: bool,
) -> Vec<String> {
    if json {
        return vec![status_json(daemon, socket, report)];
    }
    let mut lines = vec![daemon_line(daemon, socket), engine_line(report.engine)];
    lines.extend(docker_fix(report.engine));
    if let Some(autostart) = report.autostart {
        lines.extend(autostart_lines(autostart));
    }
    if let Some(network) = report.network {
        lines.extend(mdns_line(&network.mdns));
    }
    if let Some(last) = report.reconcile.and_then(|r| r.last.as_ref()) {
        lines.extend(recovery_lines(last, report.now));
    }
    if let Some(network) = report.network {
        lines.extend(warning_lines(&network.warnings));
        lines.extend(proxy_lines(&network.proxy));
        lines.extend(mdns_warning_lines(&network.mdns));
    }
    lines
}

/// How to start Docker when it isn't running: `alchemy machine docker
/// start` for Docker Desktop, or the command alchemyd would give for an
/// engine it doesn't start, such as Docker Engine on Linux.
pub(crate) fn docker_fix(engine: &EngineStatus) -> Option<String> {
    if engine.state != EngineState::NotRunning {
        return None;
    }
    match plan_docker_start(host_os(), engine.state, engine.flavor) {
        DockerStart::Launch(_) => Some(format!("  Fix: run `alchemy {}`", docker::START_NAME)),
        DockerStart::Refuse(refusal) => refusal
            .command()
            .map(|command| format!("  Fix: run `{command}`")),
        DockerStart::AlreadyRunning | DockerStart::AlreadyStarting => None,
    }
}

/// Whether alchemyd starts on its own, and where the one answering logs.
pub(crate) fn autostart_lines(autostart: &Autostart) -> Vec<String> {
    let pending = autostart
        .pending
        .map_or_else(String::new, |mode| format!("; switching to {mode}"));
    vec![
        format!("Autostart: {}{pending}", mode_text(autostart.mode)),
        format!(
            "Logs: {}",
            logs_text(&autostart.log_hint, autostart.supervisor)
        ),
    ]
}

/// What the last reconcile did: a heading with what set it off and when,
/// then each app it started, is starting, couldn't start, or left down
/// for a reason worth knowing. Apps that weren't running before, or
/// already were, are left out.
pub(crate) fn recovery_lines(last: &Reconcile, now: i64) -> Vec<String> {
    let heading = format!(
        "Recovery {}, {}",
        trigger_text(last.trigger),
        ago(last.at, now)
    );
    let apps: Vec<String> = last.apps.iter().filter_map(recovery_app).collect();
    if apps.is_empty() {
        return vec![format!("{heading}: nothing needed starting")];
    }
    std::iter::once(format!("{heading}:"))
        .chain(apps.into_iter().map(|line| format!("  {line}")))
        .collect()
}

fn recovery_app(app: &ReconcileApp) -> Option<String> {
    let name = &app.app;
    let message = app.message.as_deref();
    Some(match app.outcome {
        ReconcileOutcome::Started => format!("{name}: started"),
        ReconcileOutcome::Starting => format!("{name}: starting…"),
        ReconcileOutcome::Failed => format!(
            "{name}: didn't come back: {}",
            message.unwrap_or("its start failed")
        ),
        ReconcileOutcome::Skipped => match app.reason {
            Some(SkipReason::NotRunningBefore | SkipReason::AlreadyRunning) => return None,
            reason => format!(
                "{name}: not started: {}",
                message
                    .or(reason.map(SkipReason::message))
                    .unwrap_or("Alchemy left it alone")
            ),
        },
        _ => format!("{name}: in a state this alchemy doesn't know; upgrade alchemy"),
    })
}

fn trigger_text(trigger: ReconcileTrigger) -> &'static str {
    match trigger {
        ReconcileTrigger::DaemonStart => "after alchemyd started",
        ReconcileTrigger::Wake => "after the computer woke",
        ReconcileTrigger::EngineReturned => "after Docker came back",
    }
}

/// How long before `now` the moment `at` was, both in Unix seconds, such
/// as `3 minutes ago`. A moment in the future reads as just now.
pub(crate) fn ago(at: i64, now: i64) -> String {
    let secs = now.saturating_sub(at);
    let (count, unit) = match secs {
        ..60 => return "just now".to_owned(),
        60..3_600 => (secs / 60, "minute"),
        3_600..86_400 => (secs / 3_600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    if count == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{count} {unit}s ago")
    }
}

/// Whether apps' `.local` names are announced, and through what, or `None`
/// when they can't be, which [`mdns_warning_lines`] reports instead.
pub(crate) fn mdns_line(mdns: &MdnsReport) -> Option<String> {
    let state = match mdns.state {
        MdnsState::Off => "off".to_owned(),
        MdnsState::Starting => "starting…".to_owned(),
        MdnsState::Announcing => match mdns.backend {
            Some(MdnsBackend::Avahi) => "announced through avahi".to_owned(),
            Some(MdnsBackend::DnsSd) => "announced through dns-sd (mDNSResponder)".to_owned(),
            Some(MdnsBackend::BuiltIn) => "announced by the built-in responder".to_owned(),
            _ => "announced".to_owned(),
        },
        MdnsState::Unavailable => return None,
        _ => "in a state this alchemy doesn't know; upgrade alchemy".to_owned(),
    };
    Some(format!("Local names (mDNS): {state}"))
}

/// A warning, with its fix, when apps' `.local` names can't be announced.
pub(crate) fn mdns_warning_lines(mdns: &MdnsReport) -> Vec<String> {
    if mdns.state != MdnsState::Unavailable {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "Warning: {}",
        mdns.message.as_deref().unwrap_or(
            "Alchemy can't announce apps' .local names. Your apps still work at their IP:port address."
        )
    )];
    if let Some(fix) = mdns.fix.as_deref() {
        lines.push(format!("  Fix: {fix}"));
    }
    lines
}

/// A warning for each proxy port that couldn't be opened: port 80 first,
/// with its fix, then each app's own port. When a privilege would open
/// port 80, the fix is the command that grants it.
pub(crate) fn proxy_lines(proxy: &ProxyReport) -> Vec<String> {
    let mut lines = Vec::new();
    if proxy.host.state == ListenerState::Unavailable {
        lines.push(format!(
            "Warning: {}",
            proxy.host.message.as_deref().unwrap_or(
                "Alchemy can't use port 80. Your apps still work at their IP:port address."
            )
        ));
        let grant = proxy
            .host
            .privilege
            .and_then(PrivilegeAction::action)
            .map(|action| {
                format!(
                    "run `alchemy machine privileges grant {}`",
                    action.cli_name()
                )
            });
        if let Some(fix) = grant.as_deref().or(proxy.host.fix.as_deref()) {
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
/// the network report, autostart, and last reconcile, when there are
/// ones.
pub(crate) fn status_json(daemon: &DaemonInfo, socket: &Path, report: &Report<'_>) -> String {
    let mut json = serde_json::json!({
        "daemon": daemon.version,
        "protocol": daemon.api,
        "socket": socket,
        "engine": report.engine,
    });
    if let Some(object) = json.as_object_mut() {
        let parts = [
            ("network", report.network.map(serde_json::to_value)),
            ("autostart", report.autostart.map(serde_json::to_value)),
            ("reconcile", report.reconcile.map(serde_json::to_value)),
        ];
        for (key, value) in parts {
            if let Some(value) = value {
                object.insert(key.to_owned(), value.unwrap_or_default());
            }
        }
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
    use alchemy_api::sse::SseEvent;

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::{FAKE_SOCKET, FakeDaemon, sse};

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
            &Report::engine(&engine),
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
    const AUTOSTART: &str = "/api/machine/autostart";

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
            .ok(Method::GET, NETWORK, &NetworkReport::default())
            .error(Method::GET, AUTOSTART, 404, "not_found", "not found")
            .error(Method::GET, RECONCILE_PATH, 404, "not_found", "not found");
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        assert_eq!(
            run.out,
            format!("{DAEMON_LINE}\n{READY_LINE}\nLocal names (mDNS): off\n")
        );
        assert_eq!(
            run.requests,
            [
                "GET /api/version",
                "GET /api/engine",
                "GET /api/network",
                "GET /api/machine/autostart",
                "GET /api/reconcile"
            ]
        );
    }

    #[tokio::test]
    async fn status_prints_network_warnings() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .ok(Method::GET, NETWORK, &overlapping())
            .error(Method::GET, AUTOSTART, 404, "not_found", "not found")
            .error(Method::GET, RECONCILE_PATH, 404, "not_found", "not found");
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        let lines: Vec<&str> = run.out.lines().collect();
        assert_eq!(
            lines[..3],
            [DAEMON_LINE, READY_LINE, "Local names (mDNS): off"]
        );
        assert_eq!(lines[3..], warning_lines(&overlapping().warnings));
    }

    fn mdns(state: MdnsState, backend: Option<MdnsBackend>) -> MdnsReport {
        MdnsReport {
            state,
            backend,
            message: None,
            fix: None,
        }
    }

    #[test]
    fn mdns_line_per_state_and_backend() {
        use MdnsBackend::{Avahi, BuiltIn, DnsSd, Other};
        use MdnsState::{Announcing, Off, Starting, Unavailable};
        let cases = [
            (mdns(Off, None), Some("Local names (mDNS): off")),
            (mdns(Starting, None), Some("Local names (mDNS): starting…")),
            (
                mdns(Announcing, Some(Avahi)),
                Some("Local names (mDNS): announced through avahi"),
            ),
            (
                mdns(Announcing, Some(DnsSd)),
                Some("Local names (mDNS): announced through dns-sd (mDNSResponder)"),
            ),
            (
                mdns(Announcing, Some(BuiltIn)),
                Some("Local names (mDNS): announced by the built-in responder"),
            ),
            (
                mdns(Announcing, Some(Other)),
                Some("Local names (mDNS): announced"),
            ),
            (
                mdns(Announcing, None),
                Some("Local names (mDNS): announced"),
            ),
            (
                mdns(MdnsState::Other, None),
                Some("Local names (mDNS): in a state this alchemy doesn't know; upgrade alchemy"),
            ),
            (mdns(Unavailable, Some(DnsSd)), None),
        ];
        for (report, want) in cases {
            assert_eq!(mdns_line(&report).as_deref(), want, "{report:?}");
        }
    }

    #[test]
    fn mdns_warnings_say_what_still_works_and_the_fix() {
        let denied = MdnsReport {
            message: Some(
                "macOS doesn't let Alchemy use your local network. Your apps still work at their IP:port address.".into(),
            ),
            fix: Some("Allow alchemyd in System Settings > Privacy & Security > Local Network.".into()),
            ..mdns(MdnsState::Unavailable, Some(MdnsBackend::DnsSd))
        };
        assert_eq!(
            mdns_warning_lines(&denied),
            [
                "Warning: macOS doesn't let Alchemy use your local network. Your apps still work at their IP:port address.",
                "  Fix: Allow alchemyd in System Settings > Privacy & Security > Local Network.",
            ]
        );
        assert_eq!(
            mdns_warning_lines(&mdns(MdnsState::Unavailable, None)),
            [
                "Warning: Alchemy can't announce apps' .local names. Your apps still work at their IP:port address."
            ]
        );
        assert_eq!(
            mdns_warning_lines(&mdns(MdnsState::Announcing, None)),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn status_prints_local_names_and_port_80_warnings_last() {
        use alchemy_api::network::ProxyListener;
        let taken = "Another program on this computer takes port 80. Your apps still work at their IP:port address.";
        let report = NetworkReport {
            engine_ready: true,
            proxy: ProxyReport {
                reachability: None,
                host: ProxyListener {
                    port: 80,
                    app: None,
                    state: ListenerState::Unavailable,
                    message: Some(taken.into()),
                    fix: Some("Stop the program or firewall rule that takes port 80.".into()),
                    privilege: None,
                },
                ports: vec![],
            },
            mdns: MdnsReport {
                message: Some("No announcer works on this computer.".into()),
                ..mdns(MdnsState::Unavailable, None)
            },
            ..NetworkReport::default()
        };
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .ok(Method::GET, NETWORK, &report)
            .error(Method::GET, AUTOSTART, 404, "not_found", "not found")
            .error(Method::GET, RECONCILE_PATH, 404, "not_found", "not found");
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        assert_eq!(
            run.out.lines().collect::<Vec<_>>(),
            [
                DAEMON_LINE,
                READY_LINE,
                &format!("Warning: {taken}"),
                "  Fix: Stop the program or firewall rule that takes port 80.",
                "Warning: No announcer works on this computer.",
            ]
        );
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
                    privilege: None,
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

        let cases = [
            (
                Some(PrivilegeAction::LowPorts),
                "  Fix: run `alchemy machine privileges grant low-ports`",
            ),
            (
                Some(PrivilegeAction::Other),
                "  Fix: Let alchemyd use low ports.",
            ),
            (None, "  Fix: Let alchemyd use low ports."),
        ];
        for (privilege, want) in cases {
            let mut proxy = proxy.clone();
            proxy.host.privilege = privilege;
            assert_eq!(proxy_lines(&proxy)[1], want, "{privilege:?}");
        }
    }

    #[tokio::test]
    async fn status_skips_network_warnings_an_older_daemon_lacks() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .error(Method::GET, NETWORK, 404, "not_found", "not found")
            .error(Method::GET, AUTOSTART, 404, "not_found", "not found")
            .error(Method::GET, RECONCILE_PATH, 404, "not_found", "not found");
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        assert_eq!(run.out, format!("{DAEMON_LINE}\n{READY_LINE}\n"));
    }

    #[tokio::test]
    async fn status_json_prints_one_object() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &ready())
            .ok(Method::GET, NETWORK, &overlapping())
            .error(Method::GET, AUTOSTART, 404, "not_found", "not found")
            .error(Method::GET, RECONCILE_PATH, 404, "not_found", "not found");
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
        let fix = cfg!(target_os = "linux").then_some("  Fix: run `sudo systemctl start docker`");
        let want: Vec<&str> = [DAEMON_LINE, "Docker isn't running"]
            .into_iter()
            .chain(fix)
            .chain(["Docker is starting…", READY_LINE])
            .collect();
        assert_eq!(run.out.lines().collect::<Vec<_>>(), want);
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

    #[test]
    fn ago_reads_naturally() {
        let now = 1_700_000_000;
        let cases = [
            (now + 5, "just now"),
            (now, "just now"),
            (now - 59, "just now"),
            (now - 60, "1 minute ago"),
            (now - 150, "2 minutes ago"),
            (now - 3_600, "1 hour ago"),
            (now - 7_300, "2 hours ago"),
            (now - 86_400 * 3, "3 days ago"),
        ];
        for (at, want) in cases {
            assert_eq!(ago(at, now), want, "{}", now - at);
        }
    }

    #[test]
    fn docker_fix_points_at_the_way_to_start_it() {
        use EngineFlavor::{DockerDesktop, DockerEngine, Other};
        let desktop = "  Fix: run `alchemy machine docker start`";
        let engine = "  Fix: run `sudo systemctl start docker`";
        let linux = cfg!(target_os = "linux");
        let cases = [
            (EngineState::NotRunning, Some(DockerDesktop), Some(desktop)),
            (EngineState::NotRunning, Some(DockerEngine), Some(engine)),
            (EngineState::NotRunning, None, linux.then_some(engine)),
            (EngineState::NotRunning, Some(Other), None),
            (EngineState::Ready, Some(DockerDesktop), None),
            (EngineState::Starting, Some(DockerDesktop), None),
            (EngineState::NotInstalled, Some(DockerDesktop), None),
        ];
        for (state, flavor, want) in cases {
            let engine = status(state, flavor, [None, None, None], None);
            assert_eq!(docker_fix(&engine).as_deref(), want, "{state:?} {flavor:?}");
        }
    }

    fn reconcile_app(
        app: &str,
        outcome: ReconcileOutcome,
        reason: Option<SkipReason>,
        message: Option<&str>,
    ) -> ReconcileApp {
        ReconcileApp {
            app: alchemy_api::reconcile::StackName::new(app).unwrap(),
            outcome,
            operation: None,
            reason,
            message: message.map(str::to_owned),
        }
    }

    fn reconcile(trigger: ReconcileTrigger, apps: Vec<ReconcileApp>) -> Reconcile {
        Reconcile {
            trigger,
            at: 1_700_000_000,
            reason: trigger.start_reason().to_owned(),
            apps,
        }
    }

    #[test]
    fn recovery_names_what_came_back_and_what_didnt() {
        use ReconcileOutcome::{Failed, Skipped, Started, Starting};
        let now = 1_700_000_180;
        let last = reconcile(
            ReconcileTrigger::Wake,
            vec![
                reconcile_app("blog", Started, None, None),
                reconcile_app("shop", Starting, None, None),
                reconcile_app("wiki", Failed, None, Some("port 8080 is already in use")),
                reconcile_app("docs", Failed, None, None),
                reconcile_app(
                    "mail",
                    Skipped,
                    Some(SkipReason::StopIntent),
                    Some("you stopped it in Alchemy"),
                ),
                reconcile_app("git", Skipped, Some(SkipReason::PartlyRunning), None),
                reconcile_app("old", Skipped, Some(SkipReason::NotRunningBefore), None),
                reconcile_app("up", Skipped, Some(SkipReason::AlreadyRunning), None),
            ],
        );
        assert_eq!(
            recovery_lines(&last, now),
            [
                "Recovery after the computer woke, 3 minutes ago:",
                "  blog: started",
                "  shop: starting…",
                "  wiki: didn't come back: port 8080 is already in use",
                "  docs: didn't come back: its start failed",
                "  mail: not started: you stopped it in Alchemy",
                "  git: not started: some of its services are already running",
            ]
        );
        let quiet = reconcile(
            ReconcileTrigger::DaemonStart,
            vec![reconcile_app(
                "up",
                Skipped,
                Some(SkipReason::AlreadyRunning),
                None,
            )],
        );
        assert_eq!(
            recovery_lines(&quiet, 1_700_000_000),
            ["Recovery after alchemyd started, just now: nothing needed starting"]
        );
        let returned = reconcile(ReconcileTrigger::EngineReturned, Vec::new());
        assert!(recovery_lines(&returned, now)[0].starts_with("Recovery after Docker came back"));
    }

    fn autostart(
        mode: alchemy_api::autostart::AutostartMode,
        supervisor: alchemy_api::autostart::Supervisor,
    ) -> Autostart {
        Autostart {
            mode,
            available_modes: alchemy_api::autostart::AutostartMode::ALL.to_vec(),
            log_hint: alchemy_api::autostart::LogHint::Journal {
                command: "journalctl --user -u alchemyd.service".into(),
            },
            supervisor,
            pending: None,
            last: None,
            unavailable: Vec::new(),
        }
    }

    #[test]
    fn autostart_lines_say_the_mode_and_where_logs_go() {
        use alchemy_api::autostart::{AutostartMode, Supervisor};
        let login = autostart(AutostartMode::Login, Supervisor::UserService);
        assert_eq!(
            autostart_lines(&login),
            [
                "Autostart: login (alchemyd starts when you log in, as a per-user service)",
                "Logs: journalctl --user -u alchemyd.service",
            ]
        );
        let mut manual = autostart(AutostartMode::Off, Supervisor::Manual);
        manual.pending = Some(AutostartMode::Boot);
        assert_eq!(
            autostart_lines(&manual),
            [
                "Autostart: off (alchemyd starts only when you run it); switching to boot",
                "Logs: the terminal alchemyd was started from",
            ]
        );
    }

    #[tokio::test]
    async fn status_adds_autostart_and_the_last_recovery() {
        use alchemy_api::autostart::{AutostartMode, Supervisor};
        let mut down = ready();
        down.state = EngineState::NotRunning;
        let now = autostart(AutostartMode::Login, Supervisor::UserService);
        let last = ReconcileStatus {
            last: Some(reconcile(
                ReconcileTrigger::DaemonStart,
                vec![reconcile_app(
                    "wiki",
                    ReconcileOutcome::Failed,
                    None,
                    Some("Docker isn't running"),
                )],
            )),
        };
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &down)
            .error(Method::GET, NETWORK, 404, "not_found", "not found")
            .ok(Method::GET, AUTOSTART, &now)
            .ok(Method::GET, RECONCILE_PATH, &last);
        let run = run_with(fake, false, false).await;
        run.result.unwrap();
        let lines: Vec<&str> = run.out.lines().collect();
        assert_eq!(lines[0], DAEMON_LINE);
        assert_eq!(
            lines[1..3],
            [
                "Docker Desktop isn't running at unix:///var/run/docker.sock",
                "  Fix: run `alchemy machine docker start`",
            ]
        );
        assert_eq!(
            lines[3..5],
            [
                "Autostart: login (alchemyd starts when you log in, as a per-user service)",
                "Logs: journalctl --user -u alchemyd.service",
            ]
        );
        assert!(
            lines[5].starts_with("Recovery after alchemyd started, "),
            "{}",
            run.out
        );
        assert_eq!(lines[6], "  wiki: didn't come back: Docker isn't running");
        assert_eq!(lines.len(), 7, "{}", run.out);

        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, ENGINE, &down)
            .error(Method::GET, NETWORK, 404, "not_found", "not found")
            .ok(Method::GET, AUTOSTART, &now)
            .ok(Method::GET, RECONCILE_PATH, &last);
        let run = run_with(fake, true, false).await;
        let json = &json_lines(&run.out)[0];
        assert_eq!(json["autostart"], serde_json::to_value(&now).unwrap());
        assert_eq!(json["reconcile"], serde_json::to_value(&last).unwrap());
        assert!(json.get("network").is_none(), "{json}");
    }
}
