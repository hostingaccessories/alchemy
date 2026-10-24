//! Operations wiring: `run_with` starting lifecycle actions on apps found
//! in a real folder, streaming their progress over the API socket,
//! refusing a second action on a busy app, recording what shutdown
//! interrupts, and showing a failing build's step output, all against a
//! fake Compose runner.

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

use alchemy_core::engine::{EngineState, EngineStatus};
use alchemyd::apps::{AppsDeps, SystemFs};
use alchemyd::compose::{
    ComposeAction, FakeComposeConfig, FakeComposeRunner, FakeOutcome, FakeScript, Stream,
};
use alchemyd::engine::EngineSource;
use alchemyd::store::{SharedStore, SqliteStore};
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

const TIMEOUT: Duration = Duration::from_secs(10);

struct FixedEngine;

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(EngineStatus::new(EngineState::Ready));
        let _ = shutdown.wait_for(|stop| *stop).await;
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

/// Send one request and return the status and JSON body.
async fn call(socket: &Path, method: &str, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let mut stream = connect(socket).await;
    stream
        .write_all(request(method, uri, body).as_bytes())
        .await
        .unwrap();
    let mut seen = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut seen))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

/// Follow an operation's events until the stream ends, returning the raw
/// response.
async fn follow(socket: &Path, id: &str) -> String {
    let mut stream = connect(socket).await;
    let uri = format!("/api/operations/{id}/events");
    stream
        .write_all(request("GET", &uri, "").as_bytes())
        .await
        .unwrap();
    let mut seen = Vec::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut seen))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(seen).unwrap()
}

async fn wait_for_app(socket: &Path, name: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (_, body) = call(socket, "GET", &format!("/api/apps/{name}"), "").await;
        if body["project"].is_string() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{name} never read: {body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Daemon {
    socket: PathBuf,
    state_dir: PathBuf,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<alchemyd::Stopped>>,
}

async fn start(root: &Path, runner: FakeComposeRunner) -> Daemon {
    let apps = root.join("apps");
    for name in ["blog", "wiki"] {
        std::fs::create_dir_all(apps.join(name)).unwrap();
        std::fs::write(
            apps.join(name).join("compose.yaml"),
            "services:\n  web:\n    image: nginx\n",
        )
        .unwrap();
    }
    let config = FakeComposeConfig::new();
    config.project(apps.join("blog"), "blog", &["web"]);
    config.project(apps.join("wiki"), "wiki", &["web"]);
    let socket = root.join("alchemyd.sock");
    let state_dir = root.join("state");
    let deps = Deps::new(FixedEngine)
        .with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(apps))
                .with_scan_interval(Duration::from_millis(50)),
        )
        .with_runner(Arc::new(runner));
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen: free_port(),
            socket: socket.clone(),
            state_dir: state_dir.clone(),
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));
    wait_for_app(&socket, "blog").await;
    wait_for_app(&socket, "wiki").await;
    Daemon {
        socket,
        state_dir,
        stop,
        task,
    }
}

impl Daemon {
    async fn stop(self) -> PathBuf {
        self.stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.state_dir
    }
}

#[tokio::test]
async fn runs_actions_streams_progress_and_refuses_a_busy_app() {
    let dir = tempfile::Builder::new()
        .prefix("ao")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let runner = FakeComposeRunner::new()
        .with_script(
            ComposeAction::Update,
            FakeScript::new()
                .line(
                    Stream::Stderr,
                    r#"{"id":"Container blog-web-1","status":"Done","text":"Started"}"#,
                )
                .line(Stream::Stdout, "all set"),
        )
        .with_script(
            ComposeAction::Stop,
            FakeScript::new().outcome(FakeOutcome::UntilCancelled),
        );
    let daemon = start(&root, runner.clone()).await;
    let socket = daemon.socket.clone();

    let (status, body) = call(
        &socket,
        "POST",
        "/api/apps/blog/actions",
        r#"{"action":"update"}"#,
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_owned();

    let events = follow(&socket, &id).await;
    let first = events.find("event: operation").unwrap();
    let output = events.find("event: output").unwrap();
    let done = events.find("event: done").unwrap();
    assert!(first < output && output < done, "{events}");
    assert!(events.contains(r#""text":"Container blog-web-1 Started""#));
    assert!(events.contains(r#""text":"all set""#));
    assert!(events.contains(r#""state":"succeeded""#), "{events}");

    let (status, body) = call(&socket, "GET", &format!("/api/operations/{id}"), "").await;
    assert_eq!(status, 200);
    assert_eq!(body["state"], "succeeded");
    assert_eq!(
        runner.runs()[0].steps().len(),
        2,
        "update on a registry app pulls, then ups"
    );

    let (status, body) = call(
        &socket,
        "POST",
        "/api/apps/wiki/actions",
        r#"{"action":"stop"}"#,
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let stopping = body["operation"]["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &socket,
        "POST",
        "/api/apps/wiki/actions",
        r#"{"action":"up"}"#,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "conflict");
    assert!(
        body["error"].as_str().unwrap().contains(&stopping),
        "{body}"
    );

    let (status, body) = call(&socket, "GET", "/api/operations", "").await;
    assert_eq!(status, 200);
    assert_eq!(body["operations"][0]["id"], stopping.as_str());
    assert_eq!(body["operations"][0]["state"], "running");
    assert_eq!(body["operations"][1]["id"], id.as_str());

    let state_dir = daemon.stop().await;
    let store: SharedStore = Arc::new(SqliteStore::open(&state_dir).await.unwrap());
    let stored = store.operation(&stopping).await.unwrap().unwrap();
    assert_eq!(stored.state.as_str(), "interrupted");
    let stored = store.operation(&id).await.unwrap().unwrap();
    assert_eq!(stored.state.as_str(), "succeeded");
    assert_eq!(
        store
            .intent("wiki")
            .await
            .unwrap()
            .map(|i| i.action.as_str()),
        Some("stop")
    );
}

/// `BuildKit` status lines as `docker compose --progress json build`
/// writes them for a `RUN` step that fails, its output in base64.
const FAILING_BUILD: [&str; 5] = [
    r#"{"vertexes":[{"digest":"sha256:58489c9b","name":"[1/2] FROM docker.io/library/alpine:3.20","started":"2026-10-24T00:33:39.1Z","completed":"2026-10-24T00:33:39.2Z","cached":true}]}"#,
    r#"{"vertexes":[{"digest":"sha256:9c2f41aa","inputs":["sha256:58489c9b"],"name":"[2/2] RUN ./build.sh","started":"2026-10-24T00:33:39.5Z"}]}"#,
    r#"{"vertexes":[],"statuses":[],"logs":[{"vertex":"sha256:9c2f41aa","stream":1,"data":"Y29tcGlsaW5nIGFwcAo=","timestamp":"2026-10-24T00:33:39.6Z"},{"vertex":"sha256:9c2f41aa","stream":2,"data":"ZXJyb3I6IG1pc3NpbmcgZGVwZW5kZW5jeSBsaWJmb28K","timestamp":"2026-10-24T00:33:39.7Z"}]}"#,
    r#"{"logs":[{"vertex":"sha256:9c2f41aa","stream":2,"data":"bWFrZTogKioqIFthbGxdIEVycm9yIDE=","timestamp":"2026-10-24T00:33:39.8Z"}]}"#,
    r#"{"vertexes":[{"digest":"sha256:9c2f41aa","inputs":["sha256:58489c9b"],"name":"[2/2] RUN ./build.sh","started":"2026-10-24T00:33:39.5Z","completed":"2026-10-24T00:33:39.9Z","error":"process did not complete successfully: exit code: 1"}]}"#,
];

#[tokio::test]
async fn a_failing_build_shows_the_output_of_its_run_step() {
    let dir = tempfile::Builder::new()
        .prefix("ab")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let script = FAILING_BUILD
        .iter()
        .fold(FakeScript::new(), |script, line| {
            script.line(Stream::Stdout, *line)
        })
        .outcome(FakeOutcome::Fail {
            code: 1,
            message: "failed to solve: process did not complete successfully: exit code: 1"
                .to_owned(),
        });
    let runner = FakeComposeRunner::new().with_script(ComposeAction::Build, script);
    let daemon = start(&root, runner).await;
    let socket = daemon.socket.clone();

    let (status, body) = call(
        &socket,
        "POST",
        "/api/apps/blog/actions",
        r#"{"action":"build"}"#,
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let id = body["operation"]["id"].as_str().unwrap().to_owned();

    let events = follow(&socket, &id).await;
    let texts: Vec<String> = events
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|data| data["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        texts,
        [
            "[1/2] FROM docker.io/library/alpine:3.20: cached",
            "[2/2] RUN ./build.sh",
            "[2/2] RUN ./build.sh | compiling app",
            "[2/2] RUN ./build.sh | error: missing dependency libfoo",
            "[2/2] RUN ./build.sh | make: *** [all] Error 1",
            "[2/2] RUN ./build.sh: error: process did not complete successfully: exit code: 1",
        ],
        "{events}"
    );
    assert!(events.contains(r#""state":"failed""#), "{events}");
    daemon.stop().await;
}
