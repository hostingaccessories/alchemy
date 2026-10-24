//! Privileges wiring: `run_with` serving `/api/machine/privileges` on the
//! socket from a fake machine, running the helper through a fake
//! elevator in the background, and publishing each change on
//! `/api/events`.

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

use alchemy_api::privileges::{
    AttemptResult, ChangeOutcome, PrivilegeAction, PrivilegeChange, PrivilegeList, PrivilegeReason,
    PrivilegeState,
};
use alchemy_api::{ErrorCode, ErrorResponse};
use alchemy_core::engine::{EngineState, EngineStatus};
use alchemy_core::privilege::{FileKind, FileMeta, Firewall, FirewallFacts, HelperPath};
use alchemy_core::route::HostOs;
use alchemyd::engine::EngineSource;
use alchemyd::privileges::{
    FakeElevator, FakePrivilegeSystem, HelperReading, PrivilegeDeps, SystemFacts,
};
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_secs(10);
const HELPER: &str = "/usr/libexec/alchemy/alchemy-helper";

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

/// An installed alchemyd and helper on Linux, with ufw on and Alchemy's
/// ports not open yet.
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

async fn request(socket: &Path, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
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

async fn list(socket: &Path) -> PrivilegeList {
    let (status, body) = request(socket, "GET", "/api/machine/privileges", None).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn firewall(list: &PrivilegeList) -> &alchemy_api::privileges::Privilege {
    list.privileges
        .iter()
        .find(|p| p.action == PrivilegeAction::Firewall)
        .unwrap()
}

async fn change(socket: &Path, action: &str, body: &str) -> (u16, String) {
    request(
        socket,
        "POST",
        &format!("/api/machine/privileges/{action}"),
        Some(body),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn grants_through_one_prompt_and_reports_every_step() {
    let dir = tempfile::Builder::new()
        .prefix("ap")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("alchemyd.sock");
    let system = FakePrivilegeSystem::new(machine());
    let elevator = FakeElevator::new();
    let deps = Deps::new(Ready).with_privileges(
        PrivilegeDeps::new(Arc::new(system.clone()), Arc::new(elevator.clone()))
            .with_os(HostOs::Linux)
            .with_interval(Duration::from_millis(50)),
    );
    let state = deps.state.clone();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(
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
    let mut published = state.watch_privileges();
    tokio::time::timeout(TIMEOUT, published.wait_for(|l| !l.privileges.is_empty()))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(TIMEOUT, async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let now = list(&socket).await;
    let each: Vec<_> = now.privileges.iter().map(|p| (p.action, p.state)).collect();
    assert_eq!(
        each,
        [
            (PrivilegeAction::LowPorts, PrivilegeState::NotNeeded),
            (PrivilegeAction::Firewall, PrivilegeState::Needed),
            (PrivilegeAction::SystemService, PrivilegeState::NotNeeded),
        ]
    );

    elevator.hold();
    let (status, body) = change(&socket, "firewall", r#"{"grant":true}"#).await;
    assert_eq!(status, 202, "{body}");
    let pending: PrivilegeChange = serde_json::from_str(&body).unwrap();
    assert_eq!(pending.outcome, ChangeOutcome::Pending);
    assert_eq!(pending.privilege.state, PrivilegeState::Pending);
    assert_eq!(
        firewall(&list(&socket).await).state,
        PrivilegeState::Pending
    );

    let (status, body) = change(&socket, "firewall", r#"{"grant":true}"#).await;
    assert_eq!(status, 409, "{body}");
    let busy: ErrorResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(busy.code, ErrorCode::Conflict);

    system.update(|f| f.firewall.opened = true);
    elevator.release();
    tokio::time::timeout(
        TIMEOUT,
        published.wait_for(|l| {
            l.privileges.iter().any(|p| {
                p.action == PrivilegeAction::Firewall && p.state == PrivilegeState::Granted
            })
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let granted = list(&socket).await;
    let last = firewall(&granted).last.clone().unwrap();
    assert!(last.grant);
    assert_eq!(last.result, AttemptResult::Done);
    assert_eq!(
        elevator.commands()[0].argv(),
        [
            "/usr/bin/pkexec",
            "--disable-internal-agent",
            HELPER,
            "open-firewall",
            "--proxy-ports",
            "8100-8999"
        ]
    );

    let (status, body) = change(&socket, "system-service", r#"{"grant":true}"#).await;
    assert_eq!(status, 200, "{body}");
    let unchanged: PrivilegeChange = serde_json::from_str(&body).unwrap();
    assert_eq!(unchanged.outcome, ChangeOutcome::Unchanged);

    system.update(|f| {
        if let Some(helper) = f.helper.as_mut() {
            helper.meta = meta(&helper.path, 0o777);
        }
    });
    let (status, body) = change(&socket, "firewall", r#"{"grant":false}"#).await;
    assert_eq!(status, 200, "{body}");
    let terminal: PrivilegeChange = serde_json::from_str(&body).unwrap();
    assert_eq!(terminal.outcome, ChangeOutcome::NeedsTerminal);
    let command = terminal.command.unwrap();
    assert_eq!(
        command.line,
        format!("sudo {HELPER} close-firewall --proxy-ports 8100-8999")
    );
    assert_eq!(
        terminal.message.as_deref(),
        Some(&*format!(
            "alchemy-helper isn't installed safely: {HELPER} is writable by everyone."
        ))
    );
    assert_eq!(
        elevator.commands().len(),
        1,
        "no prompt for an untrusted helper"
    );

    system.update(|f| f.helper = None);
    let (status, body) = change(&socket, "firewall", r#"{"grant":false}"#).await;
    assert_eq!(status, 422, "{body}");
    let unsupported: ErrorResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(unsupported.code, ErrorCode::Unsupported);
    assert_eq!(
        unsupported.error,
        "Alchemy can't find alchemy-helper next to alchemyd."
    );
    system.update(|f| f.firewall.opened = false);
    tokio::time::timeout(
        TIMEOUT,
        published.wait_for(|l| {
            l.privileges.iter().any(|p| {
                p.action == PrivilegeAction::Firewall && p.reason == Some(PrivilegeReason::NoHelper)
            })
        }),
    )
    .await
    .unwrap_or_else(|_| panic!("no-helper never published: {:?}", state.privileges()))
    .unwrap();

    let (status, body) = change(&socket, "docker-group", r#"{"grant":true}"#).await;
    assert_eq!(status, 404, "{body}");
    let unknown: ErrorResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(unknown.code, ErrorCode::NotFound);
    let (status, body) = change(&socket, "low_ports", r#"{"grant":"yes"}"#).await;
    assert_eq!(status, 400, "{body}");
    let (status, _) = request(&socket, "GET", "/api/machine/privileges/firewall", None).await;
    assert_eq!(status, 405);

    stop.send(()).unwrap();
    let stopped = tokio::time::timeout(TIMEOUT, daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(stopped, alchemyd::Stopped::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_carries_the_privileges() {
    let dir = tempfile::Builder::new()
        .prefix("ap")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("alchemyd.sock");
    let deps = Deps::new(Ready).with_privileges(
        PrivilegeDeps::new(
            Arc::new(FakePrivilegeSystem::new(machine())),
            Arc::new(FakeElevator::new()),
        )
        .with_os(HostOs::Linux),
    );
    let state = deps.state.clone();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(
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
    let mut published = state.watch_privileges();
    tokio::time::timeout(TIMEOUT, published.wait_for(|l| !l.privileges.is_empty()))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(TIMEOUT, async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stream
        .write_all(
            b"GET /api/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
        )
        .await
        .unwrap();
    let mut seen = String::new();
    tokio::time::timeout(TIMEOUT, async {
        while !seen.contains(r#""action":"system_service""#) {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "closed: {seen}");
            seen.push_str(std::str::from_utf8(&chunk[..n]).unwrap());
        }
    })
    .await
    .unwrap();
    let frame = seen
        .split("event: privileges\ndata: ")
        .nth(1)
        .unwrap_or_else(|| panic!("no privileges event: {seen}"));
    let data = frame.split('\n').next().unwrap();
    let list: PrivilegeList = serde_json::from_str(data).unwrap();
    assert_eq!(firewall(&list).state, PrivilegeState::Needed);

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
