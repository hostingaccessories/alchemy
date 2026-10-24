//! API socket wiring: `run_with` serving the API, and only the API, over a
//! real Unix socket, then removing the socket on shutdown.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alchemy_core::engine::{EngineState, EngineStatus};
use alchemyd::engine::EngineSource;
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Publishes a fixed status, the way front-door tests can fake the engine.
struct FixedEngine(EngineStatus);

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(self.0);
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

/// A daemon running on a throwaway socket and port.
struct Daemon {
    socket: PathBuf,
    state: State,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// Start `run_with` on a socket under `/tmp`, which keeps the path
    /// short of macOS's 104-byte `sun_path` limit.
    fn start(engine: EngineStatus) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ad")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("alchemyd.sock");
        let config = alchemyd::Config {
            listen: free_port(),
            socket: socket.clone(),
        };
        let deps = Deps::new(FixedEngine(engine));
        let state = deps.state.clone();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(alchemyd::run_with(config, deps, async {
            let _ = stop_rx.await;
        }));
        Self {
            socket,
            state,
            stop,
            task,
            _dir: dir,
        }
    }

    async fn shutdown(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .expect("daemon did not shut down")
            .unwrap()
            .unwrap();
    }
}

fn free_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

async fn connect(path: &Path) -> UnixStream {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return stream,
            Err(err) if tokio::time::Instant::now() > deadline => panic!("connect: {err}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// A raw HTTP/1.1 client reading one response.
struct Raw {
    stream: UnixStream,
    seen: String,
}

impl Raw {
    async fn get(path: &Path, uri: &str, extra: &str) -> Self {
        let mut stream = connect(path).await;
        let req = format!("GET {uri} HTTP/1.1\r\nHost: localhost\r\n{extra}\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
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

    async fn rest(mut self) -> String {
        tokio::time::timeout(TIMEOUT, self.stream.read_to_string(&mut self.seen))
            .await
            .unwrap()
            .unwrap();
        self.seen
    }

    async fn closed(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        loop {
            match tokio::time::timeout(TIMEOUT, self.stream.read(&mut chunk)).await {
                Ok(Ok(0) | Err(_)) => return true,
                Ok(Ok(_)) => {}
                Err(_) => return false,
            }
        }
    }
}

/// `GET uri` with `Connection: close`, returning the lowercased head and
/// the body.
async fn get(path: &Path, uri: &str) -> (String, String) {
    let mut raw = Raw::get(
        path,
        uri,
        "Origin: tauri://localhost\r\nConnection: close\r\n",
    )
    .await;
    let head = raw.until("\r\n\r\n").await.to_ascii_lowercase();
    assert!(
        !head.contains("access-control-"),
        "CORS header on the socket for {uri}: {head}"
    );
    (head, raw.rest().await)
}

#[tokio::test]
async fn socket_serves_the_api_and_nothing_else() {
    let mut ready = EngineStatus::new(EngineState::Ready);
    ready.engine_version = Some("29.8.1".into());
    let daemon = Daemon::start(ready.clone());

    let (head, body) = get(&daemon.socket, "/api/health").await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(body, r#"{"status":"ok"}"#);

    let (head, body) = get(&daemon.socket, "/api/version").await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "version": alchemy_core::VERSION, "api": alchemy_api::API_VERSION })
    );

    let (head, body) = get(&daemon.socket, "/api/engine").await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::to_value(&ready).unwrap()
    );

    for uri in ["/", "/machines", "/api/nope"] {
        let (head, body) = get(&daemon.socket, uri).await;
        assert!(head.starts_with("http/1.1 404"), "{uri}: {head}");
        assert!(
            head.contains("content-type: application/json"),
            "{uri}: {head}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({ "error": "not found", "code": "not_found" }),
            "{uri}"
        );
    }

    let mode = std::fs::metadata(&daemon.socket)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    let socket = daemon.socket.clone();
    daemon.shutdown().await;
    assert!(!socket.exists(), "socket left behind after shutdown");
}

#[tokio::test]
async fn events_stream_over_the_socket_and_end_on_shutdown() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Ready));
    let mut raw = Raw::get(
        &daemon.socket,
        "/api/events",
        "Origin: tauri://localhost\r\nAccept: text/event-stream\r\n",
    )
    .await;

    let head = raw.until("\r\n\r\n").await.to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    assert!(!head.contains("access-control-"), "{head}");

    raw.until("event: engine\ndata: {\"state\":\"ready\"").await;
    daemon
        .state
        .set_engine(EngineStatus::new(EngineState::NotRunning));
    raw.until("event: engine\ndata: {\"state\":\"not_running\"")
        .await;

    let socket = daemon.socket.clone();
    daemon.shutdown().await;
    assert!(
        raw.closed().await,
        "event stream stayed open after shutdown"
    );
    assert!(!socket.exists(), "socket left behind after shutdown");
}

#[tokio::test]
async fn a_second_daemon_refuses_a_live_socket() {
    let first = Daemon::start(EngineStatus::new(EngineState::Ready));
    get(&first.socket, "/api/health").await;

    let config = alchemyd::Config {
        listen: free_port(),
        socket: first.socket.clone(),
    };
    let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready)));
    let err = alchemyd::run_with(config, deps, std::future::pending())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("already listening"), "{err:#}");

    let (head, _) = get(&first.socket, "/api/health").await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    first.shutdown().await;
}

/// Send `request` as written, then read the whole response, lowercasing
/// the head.
async fn exchange(path: &Path, request: &str) -> (String, String) {
    let mut stream = connect(path).await;
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Raw {
        stream,
        seen: String::new(),
    };
    let head = raw.until("\r\n\r\n").await.to_ascii_lowercase();
    (head, raw.rest().await)
}

fn code(body: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(body).unwrap()["code"].clone()
}

#[tokio::test]
async fn socket_wants_json_on_mutations_but_ignores_host_and_origin() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Ready));

    let (head, _) = exchange(
        &daemon.socket,
        "GET /api/health HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(head.starts_with("http/1.1 200"), "{head}");

    let (head, body) = exchange(
        &daemon.socket,
        "POST /api/health HTTP/1.1\r\nHost: alchemyd\r\nOrigin: https://evil.example\r\n\
         Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    )
    .await;
    assert!(head.starts_with("http/1.1 405"), "{head}");
    assert_eq!(code(&body), "method_not_allowed");

    let (head, body) = exchange(
        &daemon.socket,
        "POST /api/health HTTP/1.1\r\nHost: alchemyd\r\nContent-Type: text/plain\r\n\
         Content-Length: 2\r\nConnection: close\r\n\r\n{}",
    )
    .await;
    assert!(head.starts_with("http/1.1 415"), "{head}");
    assert_eq!(code(&body), "unsupported_media_type");

    daemon.shutdown().await;
}
