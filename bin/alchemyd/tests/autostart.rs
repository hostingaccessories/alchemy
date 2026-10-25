//! Autostart wiring: `run_with` serving `/api/machine/autostart` on the
//! socket, switching between off, login, and boot through a fake
//! per-user service manager and the fake elevator, with only one service
//! installed at a time, and handing over from inside the per-user service.

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

use alchemy_api::autostart::{
    Autostart, AutostartChange, AutostartMode, AutostartOutcome, LogHint, Supervisor,
};
use alchemy_api::privileges::{AttemptResult, PrivilegeAction, PrivilegeList, PrivilegeState};
use alchemy_api::{ErrorCode, ErrorResponse};
use alchemy_core::engine::{EngineState, EngineStatus};
use alchemy_core::lifecycle::systemd_user_unit;
use alchemy_core::privilege::{FileKind, FileMeta, FirewallFacts, HelperPath};
use alchemy_core::route::HostOs;
use alchemyd::autostart::{AutostartDeps, FakeCall, FakeServiceManager};
use alchemyd::engine::EngineSource;
use alchemyd::privileges::{
    FakeElevator, FakePrivilegeSystem, HelperReading, PrivilegeDeps, SystemFacts,
};
use alchemyd::{Deps, State, Stopped};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(10);
const HELPER: &str = "/usr/libexec/alchemy/alchemy-helper";
const ALCHEMYD: &str = "/usr/libexec/alchemy/alchemyd";

struct Ready;

impl EngineSource for Ready {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(EngineStatus::new(EngineState::Ready));
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

fn meta(helper: &HelperPath, mode: u32) -> BTreeMap<PathBuf, FileMeta> {
    helper
        .trust_paths()
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let kind = if i == 0 {
                FileKind::File
            } else {
                FileKind::Dir
            };
            let mode = if i == 0 { mode } else { 0o755 };
            (path, FileMeta { kind, uid: 0, mode })
        })
        .collect()
}

/// An installed alchemyd and a trusted helper on Linux, as UID 1000.
fn machine() -> SystemFacts {
    let path = HelperPath::new(HELPER).unwrap();
    SystemFacts {
        alchemyd: PathBuf::from(ALCHEMYD),
        uid: 1000,
        helper: Some(HelperReading {
            meta: meta(&path, 0o755),
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

fn unit() -> String {
    systemd_user_unit(Path::new(ALCHEMYD)).unwrap()
}

struct Daemon {
    dir: tempfile::TempDir,
    socket: PathBuf,
    state: State,
    system: FakePrivilegeSystem,
    elevator: FakeElevator,
    manager: FakeServiceManager,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<Stopped>>,
}

async fn start(supervisor: Supervisor, facts: SystemFacts, manager: FakeServiceManager) -> Daemon {
    let dir = tempfile::Builder::new()
        .prefix("aa")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("alchemyd.sock");
    let system = FakePrivilegeSystem::new(facts);
    let elevator = FakeElevator::new();
    let deps = Deps::new(Ready)
        .with_privileges(
            PrivilegeDeps::new(Arc::new(system.clone()), Arc::new(elevator.clone()))
                .with_os(HostOs::Linux)
                .with_supervisor(supervisor)
                .with_interval(Duration::from_millis(50)),
        )
        .with_autostart(
            AutostartDeps::new(Arc::new(manager.clone()))
                .with_os(HostOs::Linux)
                .with_home("/home/u")
                .with_supervisor(supervisor)
                .with_handover_delay(Duration::from_millis(10))
                .with_interval(Duration::from_millis(50)),
        );
    let state = deps.state.clone();
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: socket.clone(),
            state_dir: dir.path().join("state"),
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));
    tokio::time::timeout(TIMEOUT, async {
        while tokio::net::UnixStream::connect(&socket).await.is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    Daemon {
        dir,
        socket,
        state,
        system,
        elevator,
        manager,
        stop: Some(stop),
        task,
    }
}

impl Daemon {
    async fn request(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let mut stream = match tokio::net::UnixStream::connect(&self.socket).await {
            Ok(s) => s,
            Err(e) => panic!("connect: {e}; daemon finished: {}", self.task.is_finished()),
        };
        let body_head = body.map_or_else(String::new, |b| {
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            )
        });
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{body_head}\r\n{}",
            body.unwrap_or("")
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut seen = String::new();
        tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut seen))
            .await
            .unwrap()
            .unwrap();
        let (head, body) = seen.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, body.to_owned())
    }

    async fn get(&self) -> Autostart {
        let (status, body) = self.request("GET", "/api/machine/autostart", None).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn put(&self, mode: &str) -> (u16, String) {
        self.request(
            "PUT",
            "/api/machine/autostart",
            Some(&format!(r#"{{"mode":"{mode}"}}"#)),
        )
        .await
    }

    async fn switch(&self, mode: &str, status: u16) -> AutostartChange {
        let (got, body) = self.put(mode).await;
        assert_eq!(got, status, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Wait for the published autostart to satisfy `done`.
    async fn settled(&self, done: impl Fn(&Autostart) -> bool) -> Autostart {
        let mut watch = self.state.watch_autostart();
        let found =
            tokio::time::timeout(TIMEOUT, watch.wait_for(|a| a.as_ref().is_some_and(&done)))
                .await
                .unwrap_or_else(|_| panic!("never settled: {:?}", self.state.autostart()))
                .unwrap()
                .clone();
        found.unwrap()
    }

    async fn privileges(&self) -> PrivilegeList {
        let (status, body) = self.request("GET", "/api/machine/privileges", None).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn system_service(&self) -> PrivilegeState {
        self.privileges()
            .await
            .privileges
            .into_iter()
            .find(|p| p.action == PrivilegeAction::SystemService)
            .unwrap()
            .state
    }

    async fn stop(self) -> Stopped {
        self.stop_keeping_dir().await.0
    }

    async fn stop_keeping_dir(mut self) -> (Stopped, tempfile::TempDir) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let stopped = tokio::time::timeout(TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        (stopped, self.dir)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn switches_between_off_login_and_boot_with_one_service_at_a_time() {
    let d = start(Supervisor::Manual, machine(), FakeServiceManager::new()).await;

    let now = d.get().await;
    assert_eq!(now.mode, AutostartMode::Off);
    assert_eq!(
        now.available_modes,
        [
            AutostartMode::Off,
            AutostartMode::Login,
            AutostartMode::Boot
        ]
    );
    assert_eq!(now.log_hint, LogHint::Terminal);
    assert_eq!(now.supervisor, Supervisor::Manual);
    assert_eq!((now.pending, now.last), (None, None));
    assert_eq!(d.system_service().await, PrivilegeState::NotNeeded);

    let login = d.switch("login", 200).await;
    assert_eq!(login.outcome, AutostartOutcome::Done);
    assert_eq!(login.autostart.mode, AutostartMode::Login);
    assert_eq!(
        login.autostart.log_hint,
        LogHint::Journal {
            command: "journalctl --user -u alchemyd.service".into()
        }
    );
    assert_eq!(login.autostart.last.unwrap().result, AttemptResult::Done);
    assert_eq!(
        d.manager.calls(),
        [FakeCall::Install {
            file: unit(),
            start: true
        }]
    );
    let again = d.switch("login", 200).await;
    assert_eq!(again.outcome, AutostartOutcome::Unchanged);
    assert_eq!(d.manager.calls().len(), 1);

    d.elevator.hold();
    let boot = d.switch("boot", 202).await;
    assert_eq!(boot.outcome, AutostartOutcome::Pending);
    assert_eq!(boot.autostart.pending, Some(AutostartMode::Boot));
    assert_eq!(
        boot.autostart.mode,
        AutostartMode::Login,
        "until the prompt ends"
    );
    assert_eq!(d.system_service().await, PrivilegeState::Pending);
    let (status, body) = d.put("off").await;
    assert_eq!(status, 409, "{body}");
    let busy: ErrorResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(busy.code, ErrorCode::Conflict);
    assert!(
        d.manager.file().is_some(),
        "the user service stays until boot is in"
    );

    d.system.update(|f| f.service_installed = true);
    d.elevator.release();
    let booted = d
        .settled(|a| a.pending.is_none() && a.mode == AutostartMode::Boot)
        .await;
    assert_eq!(booted.last.unwrap().result, AttemptResult::Done);
    assert_eq!(
        booted.log_hint,
        LogHint::Journal {
            command: "journalctl -u alchemyd-1000.service".into()
        }
    );
    assert_eq!(d.manager.file(), None);
    assert_eq!(
        d.manager.calls().last(),
        Some(&FakeCall::Remove { stop: true })
    );
    assert_eq!(d.system_service().await, PrivilegeState::Granted);
    assert_eq!(
        d.elevator.commands()[0].argv(),
        [
            "/usr/bin/pkexec",
            "--disable-internal-agent",
            HELPER,
            "install-service",
            "--uid",
            "1000"
        ]
    );

    d.elevator.hold();
    let back = d.switch("login", 202).await;
    assert_eq!(back.outcome, AutostartOutcome::Pending);
    assert_eq!(
        d.manager.file(),
        Some(unit()),
        "the user service is installed before the prompt"
    );
    d.system.update(|f| f.service_installed = false);
    d.elevator.release();
    let login = d
        .settled(|a| a.pending.is_none() && a.mode == AutostartMode::Login)
        .await;
    assert_eq!(login.last.unwrap().result, AttemptResult::Done);
    assert_eq!(d.elevator.commands()[1].args()[2], "remove-service");
    assert_eq!(d.system_service().await, PrivilegeState::NotNeeded);

    let off = d.switch("off", 200).await;
    assert_eq!(off.outcome, AutostartOutcome::Done);
    assert_eq!(off.autostart.mode, AutostartMode::Off);
    assert_eq!(d.manager.file(), None);
    assert_eq!(
        d.manager.calls().last(),
        Some(&FakeCall::Remove { stop: true })
    );

    let (status, body) = d.put("sometimes").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(d.stop().await, Stopped::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dismissed_prompt_keeps_the_old_mode() {
    let manager = FakeServiceManager::new();
    manager.set_file(Some(&unit()));
    let d = start(Supervisor::Manual, machine(), manager).await;
    assert_eq!(d.get().await.mode, AutostartMode::Login);

    d.elevator.answer(Ok(alchemyd::privileges::ElevatorOutput {
        code: Some(126),
        ..Default::default()
    }));
    let boot = d.switch("boot", 202).await;
    assert_eq!(boot.outcome, AutostartOutcome::Pending);
    let after = d.settled(|a| a.pending.is_none()).await;
    assert_eq!(after.mode, AutostartMode::Login);
    let last = after.last.unwrap();
    assert_eq!(
        (last.mode, last.result),
        (AutostartMode::Boot, AttemptResult::Cancelled)
    );
    assert_eq!(d.manager.file(), Some(unit()));
    assert_eq!(d.manager.calls(), []);
    assert_eq!(
        d.system_service().await,
        PrivilegeState::Needed,
        "boot is still wanted"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_needs_a_terminal_for_an_untrusted_helper() {
    let mut facts = machine();
    if let Some(helper) = facts.helper.as_mut() {
        helper.meta = meta(&helper.path, 0o775);
    }
    let d = start(Supervisor::Manual, facts, FakeServiceManager::new()).await;
    let boot = d.switch("boot", 200).await;
    assert_eq!(boot.outcome, AutostartOutcome::NeedsTerminal);
    assert_eq!(
        boot.command.unwrap().line,
        format!("sudo {HELPER} install-service --uid 1000")
    );
    assert!(boot.message.unwrap().contains("isn't installed safely"));
    assert_eq!(boot.autostart.mode, AutostartMode::Off);
    assert_eq!(boot.autostart.pending, None);
    assert_eq!(d.elevator.commands(), []);

    d.system.update(|f| f.service_installed = true);
    let done = d.switch("boot", 200).await;
    assert_eq!(done.outcome, AutostartOutcome::Unchanged);
    assert_eq!(done.autostart.mode, AutostartMode::Boot);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn modes_that_cannot_work_here_are_refused() {
    let mut facts = machine();
    facts.helper = None;
    let manager = FakeServiceManager::new();
    manager.set_unavailable();
    let d = start(Supervisor::Manual, facts, manager).await;
    let now = d.get().await;
    assert_eq!(now.available_modes, [AutostartMode::Off]);
    assert_eq!(
        now.unavailable
            .iter()
            .map(|u| (u.mode, u.message.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                AutostartMode::Login,
                "No systemd --user manager is running for this user."
            ),
            (
                AutostartMode::Boot,
                "Alchemy can't find alchemy-helper next to alchemyd."
            ),
        ]
    );
    for (mode, why) in [
        (
            "boot",
            "Alchemy can't find alchemy-helper next to alchemyd.",
        ),
        (
            "login",
            "No systemd --user manager is running for this user.",
        ),
    ] {
        let (status, body) = d.put(mode).await;
        assert_eq!(status, 422, "{body}");
        let refused: ErrorResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(refused.code, ErrorCode::Unsupported);
        assert_eq!(refused.error, why);
    }
    assert_eq!(d.get().await.last, None);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_user_service_hands_over_to_the_system_service() {
    let manager = FakeServiceManager::new();
    manager.set_file(Some(&unit()));
    let d = start(Supervisor::UserService, machine(), manager.clone()).await;
    let now = d.get().await;
    assert_eq!(
        (now.mode, now.supervisor),
        (AutostartMode::Login, Supervisor::UserService)
    );

    d.elevator.hold();
    assert_eq!(
        d.switch("boot", 202).await.outcome,
        AutostartOutcome::Pending
    );
    d.system.update(|f| f.service_installed = true);
    d.elevator.release();
    let stopped = tokio::time::timeout(TIMEOUT, d.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(stopped, Stopped::HandedOver);
    assert_eq!(
        manager.calls(),
        [FakeCall::Remove { stop: false }],
        "unregistered, not stopped: it exits by itself"
    );
    assert_eq!(manager.file(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn turning_off_from_inside_the_user_service_keeps_it_running() {
    let manager = FakeServiceManager::new();
    manager.set_file(Some("an older unit"));
    let d = start(Supervisor::UserService, machine(), manager).await;
    let refreshed = d.switch("login", 200).await;
    assert_eq!(refreshed.outcome, AutostartOutcome::Done);
    assert_eq!(
        d.manager.calls(),
        [FakeCall::Install {
            file: unit(),
            start: false
        }],
        "the outdated file is rewritten without restarting itself"
    );
    let off = d.switch("off", 200).await;
    assert_eq!(off.outcome, AutostartOutcome::Done);
    assert_eq!(
        d.manager.calls().last(),
        Some(&FakeCall::Remove { stop: false })
    );
    let (status, _) = d.request("GET", "/api/health", None).await;
    assert_eq!(status, 200, "still serving");
    assert_eq!(d.stop().await, Stopped::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_carries_autostart() {
    let d = start(Supervisor::Manual, machine(), FakeServiceManager::new()).await;
    let mut stream = tokio::net::UnixStream::connect(&d.socket).await.unwrap();
    stream
        .write_all(b"GET /api/events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    tokio::time::timeout(TIMEOUT, async {
        while !String::from_utf8_lossy(&seen).contains("event: autostart") {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "stream ended");
            seen.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no autostart event: {}", String::from_utf8_lossy(&seen)));
    assert!(String::from_utf8_lossy(&seen).contains(r#""mode":"off""#));
    drop(stream);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_waits_for_the_state_directory() {
    let first = start(Supervisor::Manual, machine(), FakeServiceManager::new()).await;
    let state_dir = first.dir.path().join("state");
    let second_dir = tempfile::Builder::new()
        .prefix("ab")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = second_dir.path().join("alchemyd.sock");
    let deps = Deps::new(Ready).with_autostart(
        AutostartDeps::fake()
            .with_os(HostOs::Linux)
            .with_supervisor(Supervisor::SystemService),
    );
    let (stop, stop_rx) = oneshot::channel::<()>();
    let second = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: socket.clone(),
            state_dir,
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!socket.exists(), "waiting, not serving");
    assert!(!second.is_finished());

    let (_, _first_dir) = first.stop_keeping_dir().await;
    tokio::time::timeout(TIMEOUT, async {
        while tokio::net::UnixStream::connect(&socket).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    let stopped = tokio::time::timeout(TIMEOUT, second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(stopped, Stopped::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_service_stops_when_asked() {
    let first = start(Supervisor::Manual, machine(), FakeServiceManager::new()).await;
    let deps = Deps::new(Ready).with_autostart(
        AutostartDeps::fake()
            .with_os(HostOs::Linux)
            .with_supervisor(Supervisor::UserService),
    );
    let second = alchemyd::run_with(
        alchemyd::Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: first.dir.path().join("second.sock"),
            state_dir: first.dir.path().join("state"),
        },
        deps,
        tokio::time::sleep(Duration::from_millis(300)),
    );
    let stopped = tokio::time::timeout(TIMEOUT, second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped, Stopped::Shutdown);

    let manual = alchemyd::run_with(
        alchemyd::Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: first.dir.path().join("third.sock"),
            state_dir: first.dir.path().join("state"),
        },
        Deps::new(Ready),
        std::future::pending(),
    );
    let error = tokio::time::timeout(TIMEOUT, manual)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("already using the state directory"),
        "{error:#}"
    );
    first.stop().await;
}

#[tokio::test]
async fn alchemyd_service_install_and_remove() {
    let manager = FakeServiceManager::new();
    let deps = AutostartDeps::new(Arc::new(manager.clone())).with_os(HostOs::Linux);
    let alchemyd = Path::new(ALCHEMYD);
    assert!(
        alchemyd::autostart::install_user_service(&deps, alchemyd, false, true)
            .await
            .unwrap()
    );
    assert!(
        !alchemyd::autostart::install_user_service(&deps, alchemyd, false, true)
            .await
            .unwrap(),
        "idempotent"
    );
    assert_eq!(manager.file(), Some(unit()));
    let refused = alchemyd::autostart::install_user_service(&deps, alchemyd, true, true)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("system service"), "{refused}");
    let appimage = alchemyd::autostart::install_user_service(
        &deps,
        Path::new("/tmp/.mount_Alch/usr/bin/alchemyd"),
        false,
        true,
    )
    .await
    .unwrap_err();
    assert!(appimage.to_string().contains("AppImage"), "{appimage}");
    let unsafe_path = alchemyd::autostart::install_user_service(
        &deps,
        Path::new("/home/u/My Apps/alchemyd"),
        false,
        false,
    )
    .await
    .unwrap_err();
    assert!(
        unsafe_path.to_string().contains("systemd unit"),
        "{unsafe_path}"
    );

    assert!(
        alchemyd::autostart::remove_user_service(&deps)
            .await
            .unwrap()
    );
    assert!(
        !alchemyd::autostart::remove_user_service(&deps)
            .await
            .unwrap()
    );
    assert_eq!(manager.file(), None);

    let macos = AutostartDeps::new(Arc::new(FakeServiceManager::new())).with_os(HostOs::MacOs);
    let no_home = alchemyd::autostart::install_user_service(&macos, alchemyd, false, true)
        .await
        .unwrap_err();
    assert!(no_home.to_string().contains("HOME"), "{no_home}");
}
