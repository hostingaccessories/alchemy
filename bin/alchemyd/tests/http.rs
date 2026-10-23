//! HTTP front door wiring: `run_with` serving `/api/events` over a real TCP
//! listener to a client on the Tauri origin.

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

    let first = raw.until("\n\n").await;
    assert!(first.contains("event: engine\n"), "{first}");
    if !first.contains(r#""state":"ready""#) {
        let next = raw.until(r#""state":"ready""#).await;
        assert!(next.contains("event: engine\n"), "{next}");
    }

    state.set_engine(EngineStatus::new(EngineState::NotRunning));
    let change = raw.until(r#""state":"not_running""#).await;
    assert!(change.contains("event: engine\n"), "{change}");

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
