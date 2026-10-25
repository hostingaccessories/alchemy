//! End-to-end: `alchemy machine privileges` against a real alchemyd on a
//! tempdir socket, with a fake machine, a fake elevator in place of the
//! system prompt, and a fake sudo in place of the terminal.

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
use alchemy::{Cli, ClientError, Color};
use alchemy_api::privileges::{PrivilegeAction, PrivilegeList, PrivilegeState};
use alchemy_core::engine::{EngineState, EngineStatus};
use alchemy_core::privilege::{FileKind, FileMeta, Firewall, FirewallFacts, HelperPath};
use alchemy_core::route::HostOs;
use alchemyd::engine::EngineSource;
use alchemyd::privileges::{
    FakeElevator, FakePrivilegeSystem, HelperReading, PrivilegeDeps, SystemFacts,
};
use alchemyd::{Config, Deps, State};
use clap::Parser;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

const DEADLINE: Duration = Duration::from_secs(10);
const HELPER: &str = "/usr/libexec/alchemy/alchemy-helper";

struct Ready;

impl EngineSource for Ready {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(EngineStatus::new(EngineState::Ready));
        let _ = shutdown.wait_for(|stop| *stop).await;
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

/// An installed alchemyd and helper on Linux with a polkit agent, ufw
/// on, and Alchemy's ports not open yet.
fn machine() -> SystemFacts {
    let path = HelperPath::new(HELPER).unwrap();
    SystemFacts {
        alchemyd: PathBuf::from("/usr/libexec/alchemy/alchemyd"),
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
            active: Some(Firewall::Ufw),
            opened: false,
        },
        service_installed: false,
    }
}

/// Make the helper writable by everyone, so alchemyd hands over a sudo
/// command instead of prompting.
fn untrust(facts: &mut SystemFacts) {
    if let Some(helper) = facts.helper.as_mut() {
        helper.meta = meta(&helper.path, 0o777);
    }
}

struct Daemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    state: State,
    system: FakePrivilegeSystem,
    elevator: FakeElevator,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<alchemyd::Stopped, String>>,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ap")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("alchemyd.sock");
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: socket.clone(),
            state_dir: dir.path().join("state"),
        };
        let system = FakePrivilegeSystem::new(machine());
        let elevator = FakeElevator::new();
        let deps = Deps::new(Ready).with_privileges(
            PrivilegeDeps::new(Arc::new(system.clone()), Arc::new(elevator.clone()))
                .with_os(HostOs::Linux)
                .with_interval(Duration::from_millis(50)),
        );
        let state = deps.state.clone();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            alchemyd::run_with(config, deps, async {
                let _ = stop_rx.await;
            })
            .await
            .map_err(|e| format!("{e:#}"))
        });
        tokio::time::timeout(DEADLINE, async {
            while !socket.exists() {
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
            stop,
            task,
        }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        let stopped = tokio::time::timeout(DEADLINE, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(stopped, alchemyd::Stopped::Shutdown);
    }

    /// Resolve once alchemyd publishes the firewall as `state`.
    async fn firewall_becomes(&self, state: PrivilegeState) {
        let mut published = self.state.watch_privileges();
        tokio::time::timeout(
            DEADLINE,
            published.wait_for(|list| {
                list.privileges
                    .iter()
                    .any(|p| p.action == PrivilegeAction::Firewall && p.state == state)
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("firewall never {state:?}: {:?}", self.state.privileges()))
        .unwrap();
    }
}

/// Run `alchemy` with `args` against `socket` and `sudo`, returning the
/// result and everything written to stdout.
async fn alchemy(
    socket: &Path,
    args: &[&str],
    sudo: &mut FakeSudo,
) -> (Result<(), ClientError>, String) {
    let mut command_line = vec!["alchemy", "--socket", socket.to_str().unwrap()];
    command_line.extend_from_slice(args);
    let cli = Cli::try_parse_from(command_line).unwrap();
    let mut out = Vec::new();
    let result =
        alchemy::run_with_sudo(cli, &mut out, Color::Off, &mut FakePrompt::no_tty(), sudo).await;
    (result, String::from_utf8(out).unwrap())
}

fn firewall_state(json: &str) -> PrivilegeState {
    let list: PrivilegeList = serde_json::from_str(json.trim_end()).unwrap();
    list.privileges
        .iter()
        .find(|p| p.action == PrivilegeAction::Firewall)
        .unwrap()
        .state
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_through_the_prompt_and_revokes_through_sudo() {
    let daemon = Daemon::start().await;
    let socket = daemon.socket.clone();

    let (result, out) = alchemy(&socket, &["machine", "privileges"], &mut FakeSudo::no_tty()).await;
    result.unwrap();
    assert_eq!(
        out,
        "ACTION          STATE\n\
         low-ports       not needed\n\
         firewall        needed\n\
         system-service  not needed\n\
         \n\
         firewall: not allowed yet; `alchemy machine privileges grant firewall` allows it\n"
    );

    daemon.elevator.hold();
    let mut sudo = FakeSudo::no_tty();
    let grant = alchemy(
        &socket,
        &["machine", "privileges", "grant", "firewall"],
        &mut sudo,
    );
    let answer = async {
        daemon.firewall_becomes(PrivilegeState::Pending).await;
        daemon.system.update(|f| f.firewall.opened = true);
        daemon.elevator.release();
    };
    let ((result, out), ()) = tokio::join!(grant, answer);
    result.unwrap();
    assert_eq!(out, "Granted firewall\n");
    assert_eq!(
        daemon.elevator.commands()[0].argv(),
        [
            "/usr/bin/pkexec",
            "--disable-internal-agent",
            HELPER,
            "open-firewall",
            "--proxy-ports",
            "8100-8999"
        ]
    );

    let (result, out) = alchemy(
        &socket,
        &["--json", "machine", "privileges"],
        &mut FakeSudo::no_tty(),
    )
    .await;
    result.unwrap();
    assert_eq!(firewall_state(&out), PrivilegeState::Granted);

    daemon.system.update(untrust);
    let system = daemon.system.clone();
    let mut sudo = FakeSudo::tty(move |_| {
        system.update(|f| f.firewall.opened = false);
        Ok(())
    });
    let (result, out) = alchemy(
        &socket,
        &["--json", "machine", "privileges", "revoke", "firewall"],
        &mut sudo,
    )
    .await;
    result.unwrap();
    let report: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(report["result"], "revoked", "{report}");
    assert_eq!(report["action"], "firewall", "{report}");
    assert_eq!(
        report["ran"]["line"],
        format!("sudo {HELPER} close-firewall --proxy-ports 8100-8999"),
        "{report}"
    );
    assert_eq!(
        sudo.ran(),
        [vec![
            HELPER.to_owned(),
            "close-firewall".to_owned(),
            "--proxy-ports".to_owned(),
            "8100-8999".to_owned(),
        ]]
    );
    assert_eq!(daemon.elevator.commands().len(), 1, "no prompt for sudo");

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_terminal_the_sudo_command_is_printed() {
    let daemon = Daemon::start().await;
    daemon.system.update(untrust);

    let mut sudo = FakeSudo::no_tty();
    let (result, out) = alchemy(
        &daemon.socket,
        &["machine", "privileges", "grant", "firewall"],
        &mut sudo,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");
    assert_eq!(err.kind(), "needs_terminal");
    assert_eq!(
        err.to_string(),
        format!(
            "`machine privileges grant firewall` needs sudo in a terminal: alchemy-helper isn't installed safely: {HELPER} is writable by everyone."
        )
    );
    assert_eq!(
        out,
        format!("sudo {HELPER} open-firewall --proxy-ports 8100-8999\n")
    );
    assert_eq!(sudo.ran(), Vec::<Vec<String>>::new());

    let (result, out) = alchemy(
        &daemon.socket,
        &["machine", "privileges", "grant", "system-service"],
        &mut sudo,
    )
    .await;
    result.unwrap();
    assert_eq!(out, "system-service isn't needed on this machine\n");

    daemon.system.update(|f| f.helper = None);
    let (result, out) = alchemy(
        &daemon.socket,
        &["machine", "privileges", "grant", "firewall"],
        &mut sudo,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.exit_code(), 1, "{err}");
    assert_eq!(
        err.to_string(),
        "alchemyd: Alchemy can't find alchemy-helper next to alchemyd."
    );
    assert_eq!(out, "");

    daemon.stop().await;
}
