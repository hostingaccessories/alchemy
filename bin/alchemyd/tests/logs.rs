//! Logs wiring: `run_with` serving an app's logs over the API socket from
//! a fake log source, matched to the app by its containers, following new
//! lines, ending cleanly when the client goes away or the daemon stops,
//! and refusing streams past the limit shared by both listeners.

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

use alchemy_api::apps::AppList;
use alchemy_api::logs::LogStream;
use alchemy_core::engine::{ContainerState, ContainerStatus, EngineState, EngineStatus};
use alchemyd::apps::{AppsDeps, SystemFs};
use alchemyd::compose::FakeComposeConfig;
use alchemyd::engine::EngineSource;
use alchemyd::logs::FakeLogSource;
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::{oneshot, watch};

const TIMEOUT: Duration = Duration::from_secs(10);

struct FixedEngine(EngineStatus);

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(self.0);
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

/// One open `GET` on the socket, read as it arrives.
struct Open {
    stream: UnixStream,
    seen: String,
}

impl Open {
    async fn get(socket: &Path, uri: &str) -> Self {
        let mut stream = connect(socket).await;
        let request = format!("GET {uri} HTTP/1.1\r\nHost: alchemyd\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        Self {
            stream,
            seen: String::new(),
        }
    }

    /// Read until `needle` appears in what has arrived since the last
    /// match, and return that text.
    async fn until(&mut self, needle: &str) -> String {
        loop {
            if let Some(at) = self.seen.find(needle) {
                return self.seen.drain(..at + needle.len()).collect();
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(TIMEOUT, self.stream.read(&mut chunk))
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}: {:?}", self.seen))
                .unwrap();
            assert!(n > 0, "closed before {needle:?}: {:?}", self.seen);
            self.seen
                .push_str(std::str::from_utf8(&chunk[..n]).unwrap());
        }
    }

    /// Whether the daemon ends the response, chunked as it is, within the
    /// timeout.
    async fn ends(&mut self) -> bool {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if self.seen.ends_with("0\r\n\r\n") {
                    return true;
                }
                let mut chunk = [0u8; 4096];
                match self.stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return true,
                    Ok(n) => self
                        .seen
                        .push_str(std::str::from_utf8(&chunk[..n]).unwrap()),
                }
            }
        })
        .await
        .unwrap_or(false)
    }
}

fn container(id: &str, service: &str, dir: &Path, state: ContainerState) -> ContainerStatus {
    let mut c = ContainerStatus::new(id, format!("blog-{service}-1"), state);
    c.project = Some("blog".into());
    c.service = Some(service.into());
    c.working_dir = Some(dir.to_str().unwrap().into());
    c
}

async fn wait_for_apps(state: &State, done: impl Fn(&AppList) -> bool) {
    let mut apps = state.watch_apps();
    tokio::time::timeout(TIMEOUT, apps.wait_for(|list| done(list)))
        .await
        .expect("timed out waiting for the app list")
        .unwrap();
}

/// A daemon serving the app `blog`, with services `web` (container
/// `w1`) and `db` (`d1`), whose logs come from `logs`.
struct Daemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    listen: SocketAddr,
    blog: PathBuf,
    state: State,
    logs: FakeLogSource,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<alchemyd::Stopped>>,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("al")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("apps");
        let blog = home.join("blog");
        std::fs::create_dir_all(&blog).unwrap();
        std::fs::write(
            blog.join("compose.yaml"),
            "services:\n  web:\n    image: nginx\n  db:\n    image: postgres\n",
        )
        .unwrap();
        let config = FakeComposeConfig::new();
        config.project(blog.clone(), "blog", &["web", "db"]);
        let socket = root.join("alchemyd.sock");
        let listen = free_port();
        let mut engine = EngineStatus::new(EngineState::Ready);
        engine.endpoint = Some("unix:///fake/docker.sock".into());
        let logs = FakeLogSource::new();
        let deps = Deps::new(FixedEngine(engine))
            .with_apps(
                AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home))
                    .with_scan_interval(Duration::from_millis(50)),
            )
            .with_logs(Arc::new(logs.clone()));
        let state = deps.state.clone();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(alchemyd::run_with(
            alchemyd::Config {
                listen,
                socket: socket.clone(),
                state_dir: root.join("state"),
            },
            deps,
            async {
                let _ = stop_rx.await;
            },
        ));
        let daemon = Self {
            _dir: dir,
            socket,
            listen,
            blog,
            state,
            logs,
            stop,
            task,
        };
        daemon.containers(ContainerState::Running);
        wait_for_apps(&daemon.state, |list| {
            list.apps
                .first()
                .is_some_and(|app| app.services.iter().all(|s| !s.containers.is_empty()))
        })
        .await;
        daemon
    }

    /// Report `w1` in `web_state`, and `d1` running.
    fn containers(&self, web_state: ContainerState) {
        self.state.update_containers(|c| {
            c.replace_all([
                container("w1", "web", &self.blog, web_state),
                container("d1", "db", &self.blog, ContainerState::Running),
            ])
        });
    }

    async fn get(&self, uri: &str) -> Open {
        Open::get(&self.socket, uri).await
    }

    async fn shut_down(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .expect("daemon did not shut down")
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn serves_the_tail_in_time_order_then_done() {
    let daemon = Daemon::start().await;
    let at = |secs| format!("2024-05-06T07:08:{secs:02}.000000000Z");
    daemon
        .logs
        .write("w1", LogStream::Stdout, &at(3), "web started");
    daemon
        .logs
        .write("d1", LogStream::Stderr, &at(1), "db ready");
    daemon
        .logs
        .write("d1", LogStream::Stdout, &at(5), "db checkpoint");

    let mut tail = daemon.get("/api/apps/blog/logs?tail=2").await;
    let head = tail.until("\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    let body = tail.until("event: done\ndata: {\"error\":null}").await;
    let web = body.find("\"line\":\"web started\"").expect(&body);
    let checkpoint = body.find("\"line\":\"db checkpoint\"").expect(&body);
    assert!(web < checkpoint, "{body}");
    assert!(
        !body.contains("db ready"),
        "tail=2 sent the oldest line: {body}"
    );
    assert!(tail.ends().await, "the stream did not end after done");

    let mut missing = daemon.get("/api/apps/blog/logs?service=cache").await;
    assert!(missing.until("\r\n\r\n").await.starts_with("HTTP/1.1 400"));
    let mut missing = daemon.get("/api/apps/nope/logs").await;
    assert!(missing.until("\r\n\r\n").await.starts_with("HTTP/1.1 404"));

    daemon.shut_down().await;
}

#[tokio::test]
async fn follows_new_lines_across_a_restart_until_the_client_leaves() {
    let daemon = Daemon::start().await;
    let logs = daemon.logs.clone();
    let at = |secs| format!("2024-05-06T07:08:{secs:02}.000000000Z");
    logs.write("w1", LogStream::Stdout, &at(3), "web started");

    let mut follow = daemon
        .get("/api/apps/blog/logs?service=web&follow=true")
        .await;
    follow.until("\"line\":\"web started\"").await;
    follow.until("event: following\ndata: {}\n\n").await;
    logs.write("w1", LogStream::Stderr, &at(6), "GET /");
    let line = follow.until("}\n\n").await;
    assert!(
        line.contains(r#""service":"web","container":"blog-web-1","stream":"stderr","time":"2024-05-06T07:08:06.000000000Z","line":"GET /""#),
        "{line}"
    );
    logs.write("d1", LogStream::Stdout, &at(7), "not web");

    logs.stop("w1");
    daemon.containers(ContainerState::Exited);
    logs.write("w1", LogStream::Stdout, &at(8), "on the way back");
    logs.start("w1");
    daemon.containers(ContainerState::Running);
    let resumed = follow.until("\"line\":\"on the way back\"").await;
    assert!(!resumed.contains("not web"), "{resumed}");
    assert!(
        !resumed.contains("GET /"),
        "a resumed stream repeated a line: {resumed}"
    );

    drop(follow);
    tokio::time::timeout(TIMEOUT, async {
        while logs.followers("w1") > 0 {
            logs.write("w1", LogStream::Stdout, &at(9), "ping");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the daemon kept following after the client left");

    let mut open = daemon.get("/api/apps/blog/logs?follow=1&tail=0").await;
    open.until("event: following").await;
    daemon.stop.send(()).unwrap();
    assert!(open.ends().await, "shutdown did not end a followed stream");
    assert!(!open.seen.contains("event: done"), "{}", open.seen);
    tokio::time::timeout(TIMEOUT, daemon.task)
        .await
        .expect("daemon did not shut down")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn log_streams_are_capped_across_both_listeners() {
    let daemon = Daemon::start().await;
    let uri = "/api/apps/blog/logs?service=web&follow=1&tail=0";

    let mut tcp = TcpStream::connect(daemon.listen).await.unwrap();
    let token = std::fs::read_to_string(alchemy_api::tcp_token_path(
        &daemon.socket.with_file_name("state"),
    ))
    .unwrap();
    let request = format!(
        "GET {uri} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\n\r\n",
        daemon.listen
    );
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut seen = String::new();
    while !seen.contains("event: following") {
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(TIMEOUT, tcp.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "closed: {seen}");
        seen.push_str(std::str::from_utf8(&chunk[..n]).unwrap());
    }

    let mut open = Vec::new();
    for _ in 1..alchemyd::logs::MAX_STREAMS {
        let mut follow = daemon.get(uri).await;
        follow.until("event: following").await;
        open.push(follow);
    }
    let mut refused = daemon.get(uri).await;
    let head = refused.until("\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    refused.until("\"code\":\"busy\"").await;

    drop(open.pop());
    let logs = daemon.logs.clone();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            logs.write(
                "w1",
                LogStream::Stdout,
                "2024-05-06T07:08:09.000000000Z",
                "ping",
            );
            let mut again = daemon.get(uri).await;
            if again.until("\r\n\r\n").await.starts_with("HTTP/1.1 200") {
                again.until("event: following").await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("closing a stream never freed its place");

    drop(tcp);
    drop(open);
    daemon.shut_down().await;
}
