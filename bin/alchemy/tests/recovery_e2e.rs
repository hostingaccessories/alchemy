//! End-to-end: `alchemy machine autostart`, `alchemy machine docker
//! start`, `alchemy status`'s recovery lines, and the unreachable error,
//! against a real alchemyd on a tempdir socket with a fake per-user
//! service manager, a fake elevator, a fake Docker Desktop launcher, a
//! fake engine driven through the shared state, and a fake Compose
//! runner.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alchemy::prompt::FakePrompt;
use alchemy::sudo::FakeSudo;
use alchemy::{Cli, ClientError, Color, InstalledService, Lookup};
use alchemy_api::autostart::{AutostartMode, LogHint};
use alchemy_core::engine::{
    ContainerState, ContainerStatus, EngineFlavor, EngineState, EngineStatus,
};
use alchemy_core::lifecycle::{DesktopCommand, Supervisor};
use alchemy_core::privilege::{FileKind, FileMeta, FirewallFacts, HelperPath};
use alchemy_core::route::HostOs;
use alchemyd::apps::{AppsDeps, SystemFs};
use alchemyd::autostart::{AutostartDeps, FakeServiceManager};
use alchemyd::compose::{
    ComposeAction, FakeComposeConfig, FakeComposeRunner, FakeOutcome, FakeScript,
};
use alchemyd::desktop::{DesktopLauncher, LaunchError};
use alchemyd::engine::EngineSource;
use alchemyd::privileges::{
    FakeElevator, FakePrivilegeSystem, HelperReading, PrivilegeDeps, SystemFacts,
};
use alchemyd::reconcile::ReconcileDeps;
use alchemyd::{Config, Deps, State};
use clap::Parser;
use futures_util::future::BoxFuture;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

const DEADLINE: Duration = Duration::from_secs(10);
const HELPER: &str = "/usr/libexec/alchemy/alchemy-helper";
const APPS: [&str; 3] = ["blog", "shop", "wiki"];

/// An engine the test drives through the state.
struct DrivenEngine;

impl EngineSource for DrivenEngine {
    async fn run(self, _state: State, mut shutdown: watch::Receiver<bool>) {
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

fn engine(state: EngineState, flavor: EngineFlavor) -> EngineStatus {
    let mut status = EngineStatus::new(state);
    status.flavor = Some(flavor);
    if state == EngineState::Ready {
        status.desktop_version = Some("4.34.2".into());
        status.engine_version = Some("27.3.1".into());
        status.compose_version = Some("2.29.7".into());
    }
    status
}

/// Docker Desktop that becomes ready a moment after it is launched, as
/// the engine monitor would report it.
#[derive(Debug)]
struct Desktop {
    state: State,
    launched: std::sync::Mutex<usize>,
}

impl DesktopLauncher for Desktop {
    fn launch(&self, _command: DesktopCommand) -> BoxFuture<'_, Result<(), LaunchError>> {
        Box::pin(async move {
            *self.launched.lock().unwrap() += 1;
            let state = self.state.clone();
            tokio::spawn(async move {
                state.set_engine(engine(EngineState::Starting, EngineFlavor::DockerDesktop));
                tokio::time::sleep(Duration::from_millis(100)).await;
                state.set_engine(engine(EngineState::Ready, EngineFlavor::DockerDesktop));
            });
            Ok(())
        })
    }
}

/// The helper and each of its ancestors, root-owned, with the helper at
/// `mode`.
fn meta(helper: &HelperPath, mode: u32) -> BTreeMap<PathBuf, FileMeta> {
    helper
        .trust_paths()
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let (kind, mode) = if i == 0 {
                (FileKind::File, mode)
            } else {
                (FileKind::Dir, 0o755)
            };
            (path, FileMeta { kind, uid: 0, mode })
        })
        .collect()
}

/// An installed alchemyd and a helper on Linux with a polkit agent, as
/// UID 1000, with the helper trusted unless `helper_mode` lets others
/// write it.
fn machine(helper_mode: u32) -> SystemFacts {
    let path = HelperPath::new(HELPER).unwrap();
    SystemFacts {
        alchemyd: PathBuf::from("/usr/libexec/alchemy/alchemyd"),
        uid: 1000,
        helper: Some(HelperReading {
            meta: meta(&path, helper_mode),
            path,
        }),
        pkexec: true,
        capable: false,
        nosuid: false,
        no_new_privs: false,
        file_capable: false,
        firewall: FirewallFacts {
            active: None,
            opened: false,
        },
        service_installed: false,
    }
}

fn recreate(root: &Path, app: &str) -> PathBuf {
    let dir = root.join("apps").join(app);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        "services:\n  web:\n    image: nginx\n",
    )
    .unwrap();
    dir
}

fn container(project: &str, state: ContainerState) -> ContainerStatus {
    let mut c = ContainerStatus::new(format!("{project}-web"), format!("{project}-web-1"), state);
    c.project = Some(project.into());
    c.service = Some("web".into());
    c
}

/// Docker Desktop is ready with the apps in `running` up and the rest
/// exited, and a new connection has listed them.
fn connect_engine(state: &State, running: &[&str]) {
    state.set_engine(engine(EngineState::Ready, EngineFlavor::DockerDesktop));
    state.update_containers(|containers| {
        containers.replace_all(APPS.iter().map(|app| {
            let up = if running.contains(app) {
                ContainerState::Running
            } else {
                ContainerState::Exited
            };
            container(app, up)
        }))
    });
    state.engine_listed();
}

/// Docker Desktop quit.
fn lose_engine(state: &State) {
    state.engine_lost();
    state.update_containers(|containers| containers.replace_all([]));
    state.set_engine(engine(EngineState::NotRunning, EngineFlavor::DockerDesktop));
}

struct Daemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    state: State,
    system: FakePrivilegeSystem,
    elevator: FakeElevator,
    manager: FakeServiceManager,
    desktop: Arc<Desktop>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<alchemyd::Stopped, String>>,
}

impl Daemon {
    async fn start(facts: SystemFacts, runner: FakeComposeRunner) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ar")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let socket = root.join("alchemyd.sock");
        let config = FakeComposeConfig::new();
        for app in APPS {
            config.project(recreate(&root, app), app, &["web"]);
        }
        let system = FakePrivilegeSystem::new(facts);
        let elevator = FakeElevator::new();
        let manager = FakeServiceManager::new();
        let state = State::new();
        let desktop = Arc::new(Desktop {
            state: state.clone(),
            launched: std::sync::Mutex::new(0),
        });
        let mut deps = Deps::new(DrivenEngine)
            .with_apps(
                AppsDeps::new(
                    Arc::new(SystemFs),
                    Arc::new(config),
                    Some(root.join("apps")),
                )
                .with_scan_interval(Duration::from_millis(50)),
            )
            .with_runner(Arc::new(runner))
            .with_desktop(desktop.clone())
            .with_reconcile(ReconcileDeps::default().with_grace(Duration::from_millis(200)))
            .with_privileges(
                PrivilegeDeps::new(Arc::new(system.clone()), Arc::new(elevator.clone()))
                    .with_os(HostOs::Linux)
                    .with_interval(Duration::from_millis(50)),
            )
            .with_autostart(
                AutostartDeps::new(Arc::new(manager.clone()))
                    .with_os(HostOs::Linux)
                    .with_home("/home/u")
                    .with_supervisor(Supervisor::Manual)
                    .with_interval(Duration::from_millis(50)),
            );
        deps.state = state.clone();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: socket.clone(),
            state_dir: root.join("state"),
        };
        let task = tokio::spawn(async move {
            alchemyd::run_with(config, deps, async {
                let _ = stop_rx.await;
            })
            .await
            .map_err(|e| format!("{e:#}"))
        });
        tokio::time::timeout(DEADLINE, async {
            while tokio::net::UnixStream::connect(&socket).await.is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        Self {
            _dir: dir,
            socket,
            state,
            system,
            elevator,
            manager,
            desktop,
            stop,
            task,
        }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// Run `alchemy` with `args` against this daemon and `sudo`, returning
    /// the result and everything written to stdout.
    async fn alchemy(
        &self,
        args: &[&str],
        sudo: &mut FakeSudo,
    ) -> (Result<(), ClientError>, String) {
        let mut command_line = vec!["alchemy", "--socket", self.socket.to_str().unwrap()];
        command_line.extend_from_slice(args);
        let cli = Cli::try_parse_from(command_line).unwrap();
        let mut out = Vec::new();
        let result = tokio::time::timeout(
            DEADLINE,
            alchemy::run_with_sudo(cli, &mut out, Color::Off, &mut FakePrompt::no_tty(), sudo),
        )
        .await
        .unwrap();
        (result, String::from_utf8(out).unwrap())
    }

    /// Poll `GET /api/reconcile` through the CLI's `--json status` until
    /// `done` holds for it.
    async fn reconciled(&self, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let (result, out) = self
                .alchemy(&["--json", "status"], &mut FakeSudo::no_tty())
                .await;
            result.unwrap();
            let json: serde_json::Value = serde_json::from_str(&out).unwrap();
            if done(&json["reconcile"]) {
                return json;
            }
            assert!(tokio::time::Instant::now() < deadline, "never: {json}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn autostart_switches_to_login_then_boot_through_the_prompt() {
    let d = Daemon::start(machine(0o755), FakeComposeRunner::new()).await;
    let mut no_tty = FakeSudo::no_tty();

    let (result, out) = d.alchemy(&["machine", "autostart"], &mut no_tty).await;
    result.unwrap();
    assert_eq!(
        out.lines().next(),
        Some("Autostart: off (alchemyd starts only when you run it)"),
        "{out}"
    );

    let (result, out) = d
        .alchemy(&["machine", "autostart", "login"], &mut no_tty)
        .await;
    result.unwrap();
    assert_eq!(
        out,
        "Autostart is now login: alchemyd starts when you log in, as a per-user service\n"
    );
    assert!(d.manager.file().is_some());
    let (result, out) = d
        .alchemy(&["machine", "autostart", "login"], &mut no_tty)
        .await;
    result.unwrap();
    assert_eq!(out, "Autostart is already login\n");

    d.elevator.hold();
    let prompt = async {
        tokio::time::timeout(DEADLINE, async {
            while d.elevator.commands().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        d.system.update(|f| f.service_installed = true);
        d.elevator.release();
    };
    let ((result, out), ()) = tokio::join!(
        d.alchemy(&["--json", "machine", "autostart", "boot"], &mut no_tty),
        prompt
    );
    result.unwrap();
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["result"], "changed", "{out}");
    assert_eq!(json["autostart"]["mode"], "boot", "{out}");
    assert_eq!(
        json["autostart"]["log_hint"]["command"],
        "journalctl -u alchemyd-1000.service"
    );
    assert_eq!(d.manager.file(), None, "only one service is installed");

    let (result, out) = d.alchemy(&["status"], &mut no_tty).await;
    result.unwrap();
    assert!(
        out.contains(
            "\nAutostart: boot (alchemyd starts when this computer starts, as a system service; needs your password)\n"
        ),
        "{out}"
    );
    assert!(
        out.contains("\nLogs: the terminal alchemyd was started from\n"),
        "a hand-started alchemyd logs to its terminal: {out}"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_runs_the_sudo_command_or_prints_it_without_a_terminal() {
    let d = Daemon::start(machine(0o777), FakeComposeRunner::new()).await;
    let line = format!("sudo {HELPER} install-service --uid 1000");

    let (result, out) = d
        .alchemy(&["machine", "autostart", "boot"], &mut FakeSudo::no_tty())
        .await;
    let err = result.unwrap_err();
    assert_eq!((err.kind(), err.exit_code()), ("needs_terminal", 3));
    assert_eq!(out, format!("{line}\n"));
    assert_eq!(
        err.hint().unwrap(),
        format!("run `{line}` in a terminal, then run `alchemy machine autostart boot` again")
    );

    let system = d.system.clone();
    let mut sudo = FakeSudo::tty(move |_| {
        system.update(|f| f.service_installed = true);
        Ok(())
    });
    let (result, out) = d
        .alchemy(&["machine", "autostart", "boot"], &mut sudo)
        .await;
    result.unwrap();
    assert!(out.starts_with("Autostart is now boot: "), "{out}");
    assert_eq!(
        sudo.ran(),
        [vec![
            HELPER.to_owned(),
            "install-service".into(),
            "--uid".into(),
            "1000".into()
        ]]
    );
    assert!(d.elevator.commands().is_empty(), "no prompt was shown");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn docker_start_waits_until_docker_desktop_is_ready() {
    let d = Daemon::start(machine(0o755), FakeComposeRunner::new()).await;
    let mut no_tty = FakeSudo::no_tty();
    d.state
        .set_engine(engine(EngineState::NotRunning, EngineFlavor::DockerDesktop));

    let (result, out) = d.alchemy(&["status"], &mut no_tty).await;
    result.unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[1..3],
        [
            "Docker Desktop isn't running",
            "  Fix: run `alchemy machine docker start`"
        ],
        "{out}"
    );

    let (result, out) = d
        .alchemy(&["machine", "docker", "start"], &mut no_tty)
        .await;
    result.unwrap();
    assert_eq!(
        out,
        "Docker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7\n"
    );
    assert_eq!(*d.desktop.launched.lock().unwrap(), 1);

    let (result, out) = d
        .alchemy(&["--json", "machine", "docker", "start"], &mut no_tty)
        .await;
    result.unwrap();
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["result"], "already_running", "{out}");
    assert_eq!(*d.desktop.launched.lock().unwrap(), 1);

    d.state
        .set_engine(engine(EngineState::NotRunning, EngineFlavor::DockerEngine));
    let (result, out) = d
        .alchemy(&["machine", "docker", "start"], &mut no_tty)
        .await;
    let err = result.unwrap_err();
    assert_eq!((err.kind(), err.exit_code()), ("docker_not_started", 1));
    assert_eq!(
        err.hint().as_deref(),
        Some("run `sudo systemctl start docker` in a terminal")
    );
    assert_eq!(out, "");
    assert_eq!(
        *d.desktop.launched.lock().unwrap(),
        1,
        "never Docker Engine"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_names_the_apps_that_did_not_come_back_and_why() {
    let runner = FakeComposeRunner::new().with_script(
        ComposeAction::Up,
        FakeScript::new().outcome(FakeOutcome::Fail {
            code: 1,
            message: "port 8080 is taken".into(),
        }),
    );
    let d = Daemon::start(machine(0o755), runner).await;
    let mut no_tty = FakeSudo::no_tty();
    connect_engine(&d.state, &APPS);
    d.reconciled(|r| r["last"]["trigger"] == "daemon_start")
        .await;

    lose_engine(&d.state);
    connect_engine(&d.state, &["blog"]);
    d.reconciled(|r| {
        r["last"]["trigger"] == "engine_returned"
            && r["last"]["apps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| a["outcome"] == "failed")
                .count()
                == 2
    })
    .await;

    let (result, out) = d.alchemy(&["status"], &mut no_tty).await;
    result.unwrap();
    let recovery: Vec<&str> = out
        .lines()
        .skip_while(|line| !line.starts_with("Recovery "))
        .take(3)
        .collect();
    assert_eq!(
        recovery[0], "Recovery after Docker came back, just now:",
        "{out}"
    );
    for (line, app) in recovery[1..].iter().zip(["shop", "wiki"]) {
        assert!(
            line.starts_with(&format!("  {app}: didn't come back: "))
                && line.contains("port 8080 is taken"),
            "{out}"
        );
    }
    assert!(!out.contains("blog:"), "already running is left out: {out}");
    d.stop().await;
}

/// Run `alchemy status` with nothing in the environment naming alchemyd,
/// looking at `sockets` and finding `installed` as the service on disk.
async fn status_without_a_daemon(
    sockets: Vec<PathBuf>,
    installed: Option<InstalledService>,
    json: bool,
) -> ClientError {
    let mut args = vec!["alchemy", "status"];
    if json {
        args.insert(1, "--json");
    }
    let cli = Cli::try_parse_from(args).unwrap();
    let lookup = Lookup {
        daemon_env: None,
        socket_env: None,
        sockets,
        installed: Box::new(move || installed),
    };
    let mut out = Vec::new();
    let err = alchemy::run_with_lookup(
        cli,
        lookup,
        &mut out,
        Color::Off,
        &mut FakePrompt::no_tty(),
        &mut FakeSudo::no_tty(),
    )
    .await
    .unwrap_err();
    assert_eq!(out, b"");
    err
}

#[tokio::test]
async fn the_unreachable_error_suggests_autostart_or_the_service_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("alchemyd.sock");
    let login = InstalledService {
        mode: AutostartMode::Login,
        logs: LogHint::Journal {
            command: "journalctl --user -u alchemyd.service".into(),
        },
    };
    let boot = InstalledService {
        mode: AutostartMode::Boot,
        logs: LogHint::File {
            path: "/Library/Logs/Alchemy/alchemyd.501.log".into(),
        },
    };
    let cases = [
        (
            None,
            "start alchemyd by running `alchemyd`, then `alchemy machine autostart login` starts it whenever you log in; or point --socket at the socket it listens on",
        ),
        (
            Some(login),
            "alchemyd is set to start at login but isn't answering; to find out why, see `journalctl --user -u alchemyd.service`",
        ),
        (
            Some(boot),
            "alchemyd is set to start at boot but isn't answering; to find out why, see /Library/Logs/Alchemy/alchemyd.501.log",
        ),
    ];
    for (installed, hint) in cases {
        for json in [false, true] {
            let err = status_without_a_daemon(vec![missing.clone()], installed.clone(), json).await;
            assert_eq!((err.kind(), err.exit_code()), ("daemon_unreachable", 69));
            assert!(
                err.to_string()
                    .starts_with(&format!("cannot reach alchemyd: {}: ", missing.display())),
                "{err}"
            );
            assert_eq!(err.hint().as_deref(), Some(hint), "{installed:?}");
            let report = if json { err.to_json() } else { err.to_text() };
            assert!(report.contains(hint), "{report}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_answers_never_looks_for_a_service() {
    let d = Daemon::start(machine(0o755), FakeComposeRunner::new()).await;
    let cli = Cli::try_parse_from(["alchemy", "status"]).unwrap();
    let lookup = Lookup {
        daemon_env: None,
        socket_env: None,
        sockets: vec![d.socket.clone()],
        installed: Box::new(|| panic!("looked for a service with alchemyd answering")),
    };
    let mut out = Vec::new();
    alchemy::run_with_lookup(
        cli,
        lookup,
        &mut out,
        Color::Off,
        &mut FakePrompt::no_tty(),
        &mut FakeSudo::no_tty(),
    )
    .await
    .unwrap();
    assert!(String::from_utf8(out).unwrap().starts_with("alchemyd "));
    d.stop().await;
}
