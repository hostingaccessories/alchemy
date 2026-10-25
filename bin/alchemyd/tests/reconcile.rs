//! Reconcile wiring: `run_with` bringing back the apps that were running
//! after a restart, Docker returning, and a wake, but not after the engine
//! monitor subscribes again for a wake, skipping stop intents
//! and stops made by hand, reporting a failing app without retrying it,
//! and starting Docker Desktop on request, all against a fake engine
//! driven through the shared state, a fake Compose runner, and a fake
//! launcher.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alchemy_core::engine::{
    ContainerState, ContainerStatus, EngineFlavor, EngineState, EngineStatus,
};
use alchemyd::apps::{AppsDeps, SystemFs};
use alchemyd::compose::{
    ComposeAction, FakeComposeConfig, FakeComposeRunner, FakeOutcome, FakeScript,
};
use alchemyd::desktop::FakeLauncher;
use alchemyd::engine::EngineSource;
use alchemyd::reconcile::ReconcileDeps;
use alchemyd::{Deps, State};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

const TIMEOUT: Duration = Duration::from_secs(10);
const GRACE: Duration = Duration::from_millis(200);
const APPS: [&str; 3] = ["blog", "shop", "wiki"];

/// An engine the test drives through the state.
struct DrivenEngine;

impl EngineSource for DrivenEngine {
    async fn run(self, _state: State, mut shutdown: watch::Receiver<bool>) {
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

fn container(project: &str, state: ContainerState) -> ContainerStatus {
    let mut c = ContainerStatus::new(format!("{project}-web"), format!("{project}-web-1"), state);
    c.project = Some(project.into());
    c.service = Some("web".into());
    c
}

/// Docker is ready, with each app's one container running or exited, and
/// a new connection has listed them.
fn connect_engine(state: &State, running: &[&str]) {
    state.set_engine(EngineStatus::new(EngineState::Ready));
    state.update_containers(|containers| {
        containers.replace_all(APPS.iter().map(|app| {
            let up = running.contains(app);
            container(
                app,
                if up {
                    ContainerState::Running
                } else {
                    ContainerState::Exited
                },
            )
        }))
    });
    state.engine_listed();
}

/// Docker went away.
fn lose_engine(state: &State) {
    state.engine_lost();
    state.update_containers(|containers| containers.replace_all([]));
    state.set_engine(EngineStatus::new(EngineState::NotRunning));
}

/// `app`'s container stopped while Docker stays up.
fn stop_container(state: &State, app: &str) {
    state.update_containers(|containers| {
        let mut all: Vec<ContainerStatus> = containers.iter().cloned().collect();
        for c in &mut all {
            if c.project.as_deref() == Some(app) {
                c.state = ContainerState::Exited;
            }
        }
        containers.replace_all(all)
    });
}

fn free_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

async fn connect(socket: &Path) -> UnixStream {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => return stream,
            Err(err) if tokio::time::Instant::now() > deadline => panic!("connect: {err}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

async fn call(socket: &Path, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let content_type = if method == "GET" {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let request = format!(
        "{method} {uri} HTTP/1.1\r\nHost: alchemyd\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = connect(socket).await;
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut seen))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

/// Poll `uri` until `done` holds for its body, and return it.
async fn until(socket: &Path, uri: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (_, body) = call(socket, "GET", uri, "").await;
        if done(&body) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} never happened: {body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Each app's outcome in the last reconcile, by name.
fn outcomes(body: &Value) -> Vec<(String, String, Option<String>)> {
    body["last"]["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| {
            (
                app["app"].as_str().unwrap().to_owned(),
                app["outcome"].as_str().unwrap().to_owned(),
                app["reason"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn outcome<'a>(body: &'a Value, app: &str) -> &'a Value {
    body["last"]["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["app"] == app)
        .unwrap()
}

/// The apps the runner ran `up` for, by folder name, oldest first.
fn ups(runner: &FakeComposeRunner) -> Vec<String> {
    runner
        .runs()
        .iter()
        .filter(|plan| plan.action() == ComposeAction::Up)
        .map(|plan| {
            plan.steps()[0]
                .dir()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

struct Daemon {
    socket: PathBuf,
    state: State,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<alchemyd::Stopped>>,
}

/// Write the folders of `present`, and tell Compose about every app.
fn write_apps(root: &Path, present: &[&str]) -> FakeComposeConfig {
    let apps = root.join("apps");
    let config = FakeComposeConfig::new();
    for name in APPS {
        if present.contains(&name) {
            recreate(root, name);
        }
        config.project(apps.join(name), name, &["web"]);
    }
    config
}

async fn start(root: &Path, runner: &FakeComposeRunner, launcher: Arc<FakeLauncher>) -> Daemon {
    start_with(root, runner, launcher, &APPS).await
}

/// A daemon over the app folders in `present` alone.
async fn start_with(
    root: &Path,
    runner: &FakeComposeRunner,
    launcher: Arc<FakeLauncher>,
    present: &[&str],
) -> Daemon {
    let config = write_apps(root, present);
    let socket = root.join("alchemyd.sock");
    let mut deps = Deps::new(DrivenEngine)
        .with_apps(
            AppsDeps::new(
                Arc::new(SystemFs),
                Arc::new(config),
                Some(root.join("apps")),
            )
            .with_scan_interval(Duration::from_millis(50)),
        )
        .with_runner(Arc::new(runner.clone()))
        .with_desktop(launcher)
        .with_reconcile(ReconcileDeps::default().with_grace(GRACE));
    let state = State::new();
    deps.state = state.clone();
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen: free_port(),
            socket: socket.clone(),
            state_dir: root.join("state"),
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));
    for app in present {
        until(
            &socket,
            &format!("/api/apps/{app}"),
            "reading the app",
            |b| b["project"].is_string(),
        )
        .await;
    }
    Daemon {
        socket,
        state,
        stop,
        task,
    }
}

impl Daemon {
    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ar")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

#[tokio::test]
async fn a_restart_brings_back_what_was_running_and_leaves_stopped_apps_down() {
    let (_dir, root) = tempdir();
    let runner = FakeComposeRunner::new();
    let first = start(&root, &runner, Arc::new(FakeLauncher::new())).await;
    let (_, before) = call(&first.socket, "GET", "/api/reconcile", "").await;
    assert_eq!(before, serde_json::json!({ "last": null }));

    connect_engine(&first.state, &APPS);
    let body = until(
        &first.socket,
        "/api/reconcile",
        "the first reconcile",
        |b| !b["last"].is_null(),
    )
    .await;
    assert_eq!(body["last"]["trigger"], "daemon_start");
    assert!(
        outcomes(&body)
            .iter()
            .all(|(_, outcome, reason)| outcome == "skipped"
                && reason.as_deref() == Some("already_running")),
        "{body}"
    );

    let (status, _) = call(
        &first.socket,
        "POST",
        "/api/apps/shop/actions",
        r#"{"action":"stop"}"#,
    )
    .await;
    assert_eq!(status, 202);
    stop_container(&first.state, "shop");
    stop_container(&first.state, "wiki");
    tokio::time::sleep(GRACE * 5).await;
    first.stop().await;

    let second = start(&root, &runner, Arc::new(FakeLauncher::new())).await;
    connect_engine(&second.state, &[]);
    let body = until(&second.socket, "/api/reconcile", "blog started", |b| {
        !b["last"].is_null() && outcome(b, "blog")["outcome"] == "started"
    })
    .await;
    assert_eq!(body["last"]["trigger"], "daemon_start");
    assert_eq!(
        body["last"]["reason"],
        "was running before Alchemy restarted"
    );
    assert_eq!(
        outcomes(&body),
        [
            ("blog".into(), "started".into(), None),
            ("shop".into(), "skipped".into(), Some("stop_intent".into())),
            (
                "wiki".into(),
                "skipped".into(),
                Some("not_running_before".into())
            ),
        ]
    );
    assert_eq!(
        outcome(&body, "wiki")["message"],
        "it wasn't running before"
    );
    let id = outcome(&body, "blog")["operation"].as_str().unwrap();
    let (_, op) = call(&second.socket, "GET", &format!("/api/operations/{id}"), "").await;
    assert_eq!(op["action"], "up");
    assert_eq!(op["reason"], "was running before Alchemy restarted");
    assert_eq!(ups(&runner), ["blog"]);
    second.stop().await;
}

#[tokio::test]
async fn docker_returning_and_a_wake_each_try_a_failing_app_once() {
    let (_dir, root) = tempdir();
    let runner = FakeComposeRunner::new().with_script(
        ComposeAction::Up,
        FakeScript::new().outcome(FakeOutcome::Fail {
            code: 1,
            message: "port 80 is taken".into(),
        }),
    );
    let daemon = start(&root, &runner, Arc::new(FakeLauncher::new())).await;
    connect_engine(&daemon.state, &["blog"]);
    until(
        &daemon.socket,
        "/api/reconcile",
        "the first reconcile",
        |b| b["last"]["trigger"] == "daemon_start",
    )
    .await;

    lose_engine(&daemon.state);
    connect_engine(&daemon.state, &[]);
    let body = until(&daemon.socket, "/api/reconcile", "blog failing", |b| {
        b["last"]["trigger"] == "engine_returned" && outcome(b, "blog")["outcome"] == "failed"
    })
    .await;
    assert_eq!(body["last"]["reason"], "was running before Docker stopped");
    assert!(outcome(&body, "blog")["operation"].is_string(), "{body}");
    assert!(
        outcome(&body, "blog")["message"]
            .as_str()
            .unwrap()
            .contains("port 80 is taken"),
        "{body}"
    );
    tokio::time::sleep(GRACE * 3).await;
    assert_eq!(ups(&runner), ["blog"], "no retry loop");

    daemon.state.woke();
    let body = until(
        &daemon.socket,
        "/api/reconcile",
        "the wake reconcile",
        |b| b["last"]["trigger"] == "wake" && outcome(b, "blog")["outcome"] == "failed",
    )
    .await;
    assert_eq!(
        body["last"]["reason"],
        "was running before the computer slept"
    );
    tokio::time::sleep(GRACE * 3).await;
    assert_eq!(ups(&runner), ["blog", "blog"], "once per trigger");
    daemon.stop().await;
}

#[tokio::test]
async fn subscribing_again_after_a_wake_is_not_docker_returning() {
    let (_dir, root) = tempdir();
    let runner = FakeComposeRunner::new();
    let daemon = start(&root, &runner, Arc::new(FakeLauncher::new())).await;
    connect_engine(&daemon.state, &APPS);
    until(
        &daemon.socket,
        "/api/reconcile",
        "the first reconcile",
        |b| b["last"]["trigger"] == "daemon_start",
    )
    .await;

    stop_container(&daemon.state, "blog");
    daemon.state.woke();
    let woke = until(
        &daemon.socket,
        "/api/reconcile",
        "the wake reconcile",
        |b| b["last"]["trigger"] == "wake" && outcome(b, "blog")["outcome"] == "started",
    )
    .await;

    daemon.state.engine_resubscribing();
    connect_engine(&daemon.state, &APPS);
    tokio::time::sleep(GRACE * 5).await;
    let (_, after) = call(&daemon.socket, "GET", "/api/reconcile", "").await;
    assert_eq!(after, woke, "one reconcile per wake, the wake's");
    assert_eq!(ups(&runner), ["blog"]);

    lose_engine(&daemon.state);
    connect_engine(&daemon.state, &APPS);
    until(&daemon.socket, "/api/reconcile", "Docker returning", |b| {
        b["last"]["trigger"] == "engine_returned"
    })
    .await;
    daemon.stop().await;
}

#[tokio::test]
async fn starts_docker_desktop_only_when_it_is_not_running() {
    let (_dir, root) = tempdir();
    let launcher = Arc::new(FakeLauncher::new());
    let daemon = start(&root, &FakeComposeRunner::new(), launcher.clone()).await;
    let set = |state: EngineState, flavor| {
        let mut status = EngineStatus::new(state);
        status.flavor = flavor;
        daemon.state.set_engine(status);
    };

    set(EngineState::NotRunning, Some(EngineFlavor::DockerDesktop));
    let (status, body) = call(&daemon.socket, "POST", "/api/engine/start", "{}").await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["result"], "starting");
    assert_eq!(launcher.launched().len(), 1);
    let program = &launcher.launched()[0][0];
    assert!(
        program == "/usr/bin/open" || program == "/usr/bin/systemctl",
        "{program}"
    );

    set(EngineState::Ready, Some(EngineFlavor::DockerDesktop));
    let (status, body) = call(&daemon.socket, "POST", "/api/engine/start", "{}").await;
    assert_eq!(
        (status, &body["result"]),
        (200, &Value::from("already_running"))
    );

    set(EngineState::NotRunning, Some(EngineFlavor::DockerEngine));
    let (status, body) = call(&daemon.socket, "POST", "/api/engine/start", "{}").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"], "unsupported");
    assert_eq!(body["command"], "sudo systemctl start docker");
    assert_eq!(launcher.launched().len(), 1, "never started Docker Engine");
    daemon.stop().await;
}

#[tokio::test]
async fn a_missing_docker_desktop_is_unsupported() {
    let (_dir, root) = tempdir();
    let daemon = start(
        &root,
        &FakeComposeRunner::new(),
        Arc::new(FakeLauncher::not_installed()),
    )
    .await;
    let mut status = EngineStatus::new(EngineState::NotRunning);
    status.flavor = Some(EngineFlavor::DockerDesktop);
    daemon.state.set_engine(status);
    let (status, body) = call(&daemon.socket, "POST", "/api/engine/start", "{}").await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "unsupported");
    assert_eq!(body["error"], "Docker Desktop isn't installed");
    daemon.stop().await;
}

async fn until_missing(socket: &Path, app: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while call(socket, "GET", &format!("/api/apps/{app}"), "").await.0 != 404 {
        assert!(tokio::time::Instant::now() < deadline, "{app} never went");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn recreate(root: &Path, app: &str) {
    let dir = root.join("apps").join(app);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        "services:\n  web:\n    image: nginx\n",
    )
    .unwrap();
}

#[tokio::test]
async fn an_app_deleted_while_docker_was_away_is_forgotten() {
    let (_dir, root) = tempdir();
    let runner = FakeComposeRunner::new();
    let daemon = start(&root, &runner, Arc::new(FakeLauncher::new())).await;
    connect_engine(&daemon.state, &["blog"]);
    until(
        &daemon.socket,
        "/api/reconcile",
        "the first reconcile",
        |b| b["last"]["trigger"] == "daemon_start",
    )
    .await;

    lose_engine(&daemon.state);
    std::fs::remove_dir_all(root.join("apps/blog")).unwrap();
    until_missing(&daemon.socket, "blog").await;
    connect_engine(&daemon.state, &[]);
    until(&daemon.socket, "/api/reconcile", "Docker returning", |b| {
        b["last"]["trigger"] == "engine_returned"
    })
    .await;

    recreate(&root, "blog");
    until(&daemon.socket, "/api/apps/blog", "the new blog read", |b| {
        b["project"].is_string()
    })
    .await;
    daemon.state.woke();
    let body = until(
        &daemon.socket,
        "/api/reconcile",
        "the wake reconcile",
        |b| b["last"]["trigger"] == "wake",
    )
    .await;
    assert_eq!(
        outcome(&body, "blog")["reason"],
        "not_running_before",
        "{body}"
    );
    assert_eq!(ups(&runner), [] as [String; 0]);
    daemon.stop().await;
}

#[tokio::test]
async fn an_app_deleted_while_alchemyd_was_stopped_is_forgotten() {
    let (_dir, root) = tempdir();
    let runner = FakeComposeRunner::new();
    let first = start(&root, &runner, Arc::new(FakeLauncher::new())).await;
    connect_engine(&first.state, &["blog"]);
    until(
        &first.socket,
        "/api/reconcile",
        "the first reconcile",
        |b| !b["last"].is_null(),
    )
    .await;
    first.stop().await;

    std::fs::remove_dir_all(root.join("apps/blog")).unwrap();
    let second = start_with(
        &root,
        &runner,
        Arc::new(FakeLauncher::new()),
        &["shop", "wiki"],
    )
    .await;
    connect_engine(&second.state, &[]);
    until(
        &second.socket,
        "/api/reconcile",
        "the restart reconcile",
        |b| b["last"]["trigger"] == "daemon_start",
    )
    .await;

    recreate(&root, "blog");
    until(&second.socket, "/api/apps/blog", "the new blog read", |b| {
        b["project"].is_string()
    })
    .await;
    second.state.woke();
    let body = until(
        &second.socket,
        "/api/reconcile",
        "the wake reconcile",
        |b| b["last"]["trigger"] == "wake",
    )
    .await;
    assert_eq!(
        outcome(&body, "blog")["reason"],
        "not_running_before",
        "{body}"
    );
    assert_eq!(ups(&runner), [] as [String; 0]);
    second.stop().await;
}
