//! HTTP front door wiring: `run_with` serving `/api/events` over a real TCP
//! listener to a client on the Tauri origin, and refusing requests other
//! web pages could send.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::time::Duration;

use alchemy_core::engine::{EngineState, EngineStatus};
use alchemyd::engine::EngineSource;
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Publishes a fixed status, the way front-door tests can fake the engine.
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

async fn connect(addr: SocketAddr) -> TcpStream {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => return stream,
            Err(err) if tokio::time::Instant::now() > deadline => panic!("connect: {err}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// A raw HTTP/1.1 client reading one long-lived response.
struct Raw {
    stream: TcpStream,
    seen: String,
}

impl Raw {
    /// Read until `needle` appears in what has arrived since the last
    /// match, and return that text.
    async fn until(&mut self, needle: &str) -> String {
        loop {
            if let Some(at) = self.seen.find(needle) {
                let text: String = self.seen.drain(..at + needle.len()).collect();
                return text;
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

#[tokio::test]
async fn events_stream_reaches_the_tauri_origin_and_ends_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let listen = free_port();
    let config = alchemyd::Config {
        listen,
        socket: dir.path().join("alchemyd.sock"),
        state_dir: dir.path().join("state"),
    };
    let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready)));
    let state = deps.state.clone();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(config, deps, async {
        let _ = stop_rx.await;
    }));

    let mut stream = connect(listen).await;
    stream
        .write_all(
            b"GET /api/events HTTP/1.1\r\n\
              Host: 127.0.0.1\r\n\
              Origin: tauri://localhost\r\n\
              Accept: text/event-stream\r\n\r\n",
        )
        .await
        .unwrap();
    let mut raw = Raw {
        stream,
        seen: String::new(),
    };

    let head = raw.until("\r\n\r\n").await.to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    assert!(
        head.contains("access-control-allow-origin: tauri://localhost"),
        "{head}"
    );

    raw.until("event: engine\ndata: {\"state\":\"ready\"").await;

    state.set_engine(EngineStatus::new(EngineState::NotRunning));
    raw.until("event: engine\ndata: {\"state\":\"not_running\"")
        .await;

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .expect("daemon did not shut down with an event stream open")
        .unwrap()
        .unwrap();
    assert!(
        raw.closed().await,
        "event stream stayed open after shutdown"
    );
}

/// Send `request` as written, then read the whole response, lowercasing
/// the head.
async fn exchange(addr: SocketAddr, request: &str) -> (String, String) {
    let mut stream = connect(addr).await;
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut seen))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    (head.to_ascii_lowercase(), body.to_owned())
}

/// A `POST /api/health` with `headers`, a two-byte body, and
/// `Connection: close`. No mutating route exists yet; one that passes the
/// checks reaches the router and gets its 405.
fn post(headers: &str) -> String {
    format!(
        "POST /api/health HTTP/1.1\r\n{headers}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    )
}

fn code(body: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(body).unwrap()["code"].clone()
}

/// A daemon serving on a free port until [`Self::stop`].
struct Daemon {
    listen: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listen = free_port();
        let config = alchemyd::Config {
            listen,
            socket: dir.path().join("alchemyd.sock"),
            state_dir: dir.path().join("state"),
        };
        let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready)));
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(alchemyd::run_with(config, deps, async {
            let _ = stop_rx.await;
        }));
        Self {
            listen,
            stop,
            task,
            _dir: dir,
        }
    }

    /// Send each request and check its status and error code.
    async fn assert_answers(&self, cases: Vec<(&str, String, &str, &str)>) {
        for (name, request, status, want) in cases {
            let (head, body) = exchange(self.listen, &request).await;
            assert!(
                head.starts_with(&format!("http/1.1 {status}")),
                "{name}: {head}"
            );
            assert!(
                head.contains("content-type: application/json"),
                "{name}: {head}"
            );
            assert_eq!(code(&body), want, "{name}");
        }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn tcp_listener_refuses_rebinding_cross_origin_and_non_json_requests() {
    let daemon = Daemon::start();
    let listen = daemon.listen;
    let host = format!("Host: {listen}\r\n");
    let same_origin = format!("{host}Origin: http://{listen}\r\n");

    daemon
        .assert_answers(vec![
            (
                "rebinding host",
                "GET /api/health HTTP/1.1\r\nHost: evil.example:7240\r\nConnection: close\r\n\r\n"
                    .to_owned(),
                "403",
                "forbidden",
            ),
            (
                "cross-origin form post",
                post(&format!(
                    "{host}Origin: https://evil.example\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\n"
                )),
                "403",
                "forbidden",
            ),
            (
                "text/plain post",
                post(&format!("{same_origin}Content-Type: text/plain\r\n")),
                "415",
                "unsupported_media_type",
            ),
            (
                "same-origin json post",
                post(&format!("{same_origin}Content-Type: application/json\r\n")),
                "405",
                "method_not_allowed",
            ),
            (
                "tauri json post",
                post(&format!(
                    "{host}Origin: tauri://localhost\r\nContent-Type: application/json\r\n"
                )),
                "405",
                "method_not_allowed",
            ),
            (
                "localhost get",
                "GET /api/nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_owned(),
                "404",
                "not_found",
            ),
        ])
        .await;
    daemon.stop().await;
}

#[tokio::test]
async fn tcp_listener_refuses_reads_sent_for_pages_on_other_sites() {
    let daemon = Daemon::start();
    let host = format!("Host: {}\r\n", daemon.listen);

    daemon
        .assert_answers(vec![
            (
                "cross-site no-cors get",
                format!(
                    "GET /api/apps/blog/logs?follow=true HTTP/1.1\r\n{host}\
                 Sec-Fetch-Site: cross-site\r\nConnection: close\r\n\r\n"
                ),
                "403",
                "forbidden",
            ),
            (
                "same-site get from another localhost port",
                format!(
                    "GET /api/nope HTTP/1.1\r\n{host}Origin: http://localhost:8080\r\n\
                 Sec-Fetch-Site: same-site\r\nConnection: close\r\n\r\n"
                ),
                "403",
                "forbidden",
            ),
            (
                "same-origin get",
                format!(
                    "GET /api/nope HTTP/1.1\r\n{host}Sec-Fetch-Site: same-origin\r\n\
                 Connection: close\r\n\r\n"
                ),
                "404",
                "not_found",
            ),
            (
                "tauri cross-site get",
                format!(
                    "GET /api/nope HTTP/1.1\r\n{host}Origin: tauri://localhost\r\n\
                 Sec-Fetch-Site: cross-site\r\nConnection: close\r\n\r\n"
                ),
                "404",
                "not_found",
            ),
        ])
        .await;
    daemon.stop().await;
}
