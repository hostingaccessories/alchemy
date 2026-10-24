//! The escape hatch, against the real Docker engine: an app Alchemy
//! started is the same Compose project a hand-run `docker compose` in its
//! folder sees, stops, and starts again once the daemon is gone. One app
//! is written by hand and named after its folder; the other is created
//! through `POST /api/apps` with a top-level `name:`.
//!
//! Ignored by default because it needs Docker with Compose v2 and
//! `busybox:1.37` (pulled if missing). Run it with
//! `cargo test -p alchemyd --test escape -- --ignored`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alchemyd::Deps;
use alchemyd::apps::{AppEditor, AppsDeps, SystemFs};
use alchemyd::compose::CliComposeConfig;
use alchemyd::engine::EngineMonitor;
use alchemyd::operations::SystemRunner;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Long enough to pull a small image on a slow connection.
const SLOW: Duration = Duration::from_secs(180);

const PROJECT_LABEL: &str = "com.docker.compose.project";

/// Two services, so a project's containers are a set rather than one ID.
/// The shell exits on `SIGTERM` at once instead of waiting out the stop
/// timeout.
fn compose_file(name: Option<&str>) -> String {
    let service = "    image: busybox:1.37\n    command: [\"sh\", \"-c\", \"trap 'exit 0' TERM; while :; do sleep 1; done\"]\n";
    let top = name.map(|n| format!("name: {n}\n")).unwrap_or_default();
    format!("{top}services:\n  web:\n{service}  worker:\n{service}")
}

/// Runs `docker compose down` in every app folder when dropped, so a
/// failing assertion still removes the containers and networks.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = compose(dir)
                .args(["down", "--volumes", "--remove-orphans", "--timeout", "0"])
                .output();
        }
    }
}

/// `docker compose` in `dir`, as a person would type it there: no `-p`,
/// no `-f`, and none of this shell's `COMPOSE_*` overrides.
fn compose(dir: &Path) -> Command {
    let mut command = Command::new("docker");
    command.arg("compose").current_dir(dir);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("COMPOSE_") {
            command.env_remove(name);
        }
    }
    command
}

fn run_ok(mut command: Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The full IDs of `dir`'s project's containers, as `docker compose ps`
/// in that folder lists them, stopped ones too.
fn hand_ps(dir: &Path) -> BTreeSet<String> {
    let mut command = compose(dir);
    command.args(["ps", "--all", "--quiet", "--no-trunc"]);
    run_ok(command).lines().map(str::to_owned).collect()
}

/// `id`'s Compose project label and whether it is running.
fn inspect(id: &str) -> (String, bool) {
    let mut command = Command::new("docker");
    command.args([
        "inspect",
        "--format",
        &format!("{{{{index .Config.Labels \"{PROJECT_LABEL}\"}}}} {{{{.State.Running}}}}"),
        id,
    ]);
    let out = run_ok(command);
    let (project, running) = out.trim().split_once(' ').unwrap();
    (project.to_owned(), running == "true")
}

fn assert_project(ids: &BTreeSet<String>, project: &str, running: bool) {
    for id in ids {
        assert_eq!(inspect(id), (project.to_owned(), running), "container {id}");
    }
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

fn request(method: &str, uri: &str, body: &str) -> String {
    let content_type = if method == "GET" {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    format!(
        "{method} {uri} HTTP/1.1\r\nHost: alchemyd\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Send one request and return the status and the raw body.
async fn send(socket: &Path, method: &str, uri: &str, body: &str, wait: Duration) -> (u16, String) {
    let mut stream = connect(socket).await;
    stream
        .write_all(request(method, uri, body).as_bytes())
        .await
        .unwrap();
    let mut seen = Vec::new();
    tokio::time::timeout(wait, stream.read_to_end(&mut seen))
        .await
        .unwrap_or_else(|_| panic!("{method} {uri} timed out"))
        .unwrap();
    let seen = String::from_utf8(seen).unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_owned())
}

async fn call(socket: &Path, method: &str, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let (status, body) = send(socket, method, uri, body, TIMEOUT).await;
    (status, serde_json::from_str(&body).unwrap())
}

/// Poll `/api/apps/NAME` until `done` holds for it, returning it.
async fn wait_for_app(
    socket: &Path,
    name: &str,
    what: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + SLOW;
    loop {
        let (_, app) = call(socket, "GET", &format!("/api/apps/{name}"), "").await;
        if done(&app) {
            return app;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{name} never {what}: {app}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The IDs of the containers alchemyd lists for `app`.
fn listed_ids(app: &serde_json::Value) -> BTreeSet<String> {
    app["services"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|service| service["containers"].as_array().unwrap())
        .map(|container| container["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Run `action` on `name` through the operations route and follow it to
/// the end.
async fn act(socket: &Path, name: &str, action: &str) {
    let (status, body) = call(
        socket,
        "POST",
        &format!("/api/apps/{name}/actions"),
        &format!(r#"{{"action":"{action}"}}"#),
    )
    .await;
    assert_eq!(status, 202, "{action} {name}: {body}");
    let id = body["operation"]["id"].as_str().unwrap();
    let (_, events) = send(
        socket,
        "GET",
        &format!("/api/operations/{id}/events"),
        "",
        SLOW,
    )
    .await;
    let (_, done) = events
        .rsplit_once("event: done")
        .unwrap_or_else(|| panic!("{action} {name} never finished: {events}"));
    assert!(
        done.contains(r#""state":"succeeded""#),
        "{action} {name} failed: {events}"
    );
}

struct Daemon {
    socket: PathBuf,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<alchemyd::Stopped>>,
}

/// alchemyd with the real engine monitor, `docker compose config`, app
/// editor, and `docker compose` runner, with `apps` as its app home.
fn start(root: &Path, apps: &Path) -> Daemon {
    let socket = root.join("alchemyd.sock");
    let deps = Deps::new(EngineMonitor::system())
        .with_apps(
            AppsDeps::new(
                Arc::new(SystemFs),
                Arc::new(CliComposeConfig::system()),
                Some(apps.to_owned()),
            )
            .with_scan_interval(Duration::from_millis(200)),
        )
        .with_runner(Arc::new(SystemRunner::new()))
        .with_editor(AppEditor::system());
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
    Daemon { socket, stop, task }
}

impl Daemon {
    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!self.socket.exists(), "the socket outlived the daemon");
    }
}

/// A suffix that keeps this run's projects apart from any others on the
/// engine, including an earlier run that was killed before cleaning up.
fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{}-{nanos:x}", std::process::id())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a running Docker engine with Compose v2"]
async fn docker_compose_by_hand_targets_the_project_alchemy_runs() {
    let dir = tempfile::Builder::new()
        .prefix("aesc")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let apps = root.join("apps");
    let suffix = unique();
    let plain = format!("escape-plain-{suffix}");
    let named = format!("escape-named-{suffix}");
    let renamed = format!("escape-renamed-{suffix}");
    let cases = [
        (plain.clone(), plain.clone(), None),
        (named.clone(), renamed.clone(), Some(renamed.as_str())),
    ];
    std::fs::create_dir_all(apps.join(&plain)).unwrap();
    std::fs::write(apps.join(&plain).join("compose.yaml"), compose_file(None)).unwrap();
    let _cleanup = Cleanup(cases.iter().map(|(app, _, _)| apps.join(app)).collect());

    let daemon = start(&root, &apps);
    let create = serde_json::json!({"name": named, "compose": compose_file(Some(&renamed))});
    let (status, body) = call(&daemon.socket, "POST", "/api/apps", &create.to_string()).await;
    assert_eq!(status, 201, "create {named}: {body}");
    let mut started = Vec::new();
    for (app, project, _) in &cases {
        let listed = wait_for_app(&daemon.socket, app, "read by Compose", |a| {
            a["project"].is_string()
        })
        .await;
        assert_eq!(listed["project"], project.as_str(), "{listed}");
        act(&daemon.socket, app, "up").await;
        let listed = wait_for_app(&daemon.socket, app, "running", |a| {
            a["status"] == "running" && listed_ids(a).len() == 2
        })
        .await;
        started.push(listed_ids(&listed));
    }
    daemon.stop().await;

    for ((app, project, _), ids) in cases.iter().zip(&started) {
        let folder = apps.join(app);
        assert_eq!(&hand_ps(&folder), ids, "{app}: ps by hand");
        assert_project(ids, project, true);

        run_ok({
            let mut c = compose(&folder);
            c.args(["stop"]);
            c
        });
        assert_eq!(&hand_ps(&folder), ids, "{app}: ps after stop");
        assert_project(ids, project, false);

        run_ok({
            let mut c = compose(&folder);
            c.args(["up", "--detach"]);
            c
        });
        assert_eq!(&hand_ps(&folder), ids, "{app}: up reused the containers");
        assert_project(ids, project, true);
    }

    let daemon = start(&root, &apps);
    for ((app, _, _), ids) in cases.iter().zip(&started) {
        wait_for_app(&daemon.socket, app, "seen running again", |a| {
            a["status"] == "running" && &listed_ids(a) == ids
        })
        .await;
    }
    for (app, _, _) in &cases {
        act(&daemon.socket, app, "down").await;
    }
    daemon.stop().await;
    for (app, _, _) in &cases {
        assert!(
            hand_ps(&apps.join(app)).is_empty(),
            "{app}: down by Alchemy"
        );
    }
}
