//! Metrics wiring: `run_with` running the collector against a fake source,
//! storing what it samples, and serving it on `/api/metrics`, the history
//! route, and the event stream, then stopping with the daemon.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alchemy_core::engine::{ContainerState, ContainerStatus, EngineState, EngineStatus};
use alchemy_core::metrics::{ContainerSample, HostSample, Traffic};
use alchemyd::engine::EngineSource;
use alchemyd::metrics::{MetricsSource, SourceError};
use alchemyd::store::{SharedStore, SqliteStore};
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_secs(10);
const ENDPOINT: &str = "unix:///var/run/docker.sock";

struct FixedEngine(EngineStatus);

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(self.0);
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

/// A machine with fixed figures on the real clock, and one container.
struct FakeSource;

impl MetricsSource for FakeSource {
    fn now_ms(&self) -> i64 {
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        i64::try_from(elapsed.as_millis()).unwrap()
    }

    fn host(&self) -> impl Future<Output = Result<HostSample, SourceError>> + Send {
        std::future::ready(Ok(HostSample {
            cpus: 6,
            memory_used: 3_000,
            memory_total: 12_000,
            net: vec![("eth0".into(), Traffic { rx: 1, tx: 2 })],
            ..HostSample::default()
        }))
    }

    fn container(
        &self,
        endpoint: &str,
        container: &ContainerStatus,
    ) -> impl Future<Output = Result<ContainerSample, SourceError>> + Send {
        assert_eq!(endpoint, ENDPOINT);
        std::future::ready(Ok(ContainerSample {
            id: container.id.clone(),
            name: container.name.clone(),
            project: container.project.clone(),
            service: container.service.clone(),
            memory: 700,
            ..ContainerSample::default()
        }))
    }
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

/// `GET uri` over the socket, returning the JSON body.
async fn get(socket: &Path, uri: &str) -> serde_json::Value {
    let mut stream = connect(socket).await;
    let req = format!("GET {uri} HTTP/1.1\r\nHost: alchemyd\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut raw))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{uri}: {head}");
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn daemon_samples_stores_and_serves_metrics() {
    let dir = tempfile::Builder::new()
        .prefix("am")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("alchemyd.sock");
    let state_dir = dir.path().join("state");
    let config = alchemyd::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        socket: socket.clone(),
        state_dir: state_dir.clone(),
    };
    let mut ready = EngineStatus::new(EngineState::Ready);
    ready.endpoint = Some(ENDPOINT.into());
    let deps = Deps::new(FixedEngine(ready.clone())).with_metrics(FakeSource);
    deps.state.set_engine(ready);
    deps.state.update_containers(|c| {
        let mut web = ContainerStatus::new("abc", "web-app-1", ContainerState::Running);
        web.project = Some("web".into());
        c.replace_all([web])
    });
    let state = deps.state.clone();
    let mut metrics = state.watch_metrics();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(config, deps, async {
        let _ = stop_rx.await;
    }));
    tokio::time::timeout(TIMEOUT, metrics.wait_for(|m| m.at.is_some()))
        .await
        .unwrap()
        .unwrap();

    let current = get(&socket, "/api/metrics").await;
    assert_eq!(current["machine"]["memory_total"], 12_000);
    assert_eq!(current["machine"]["cpus"], 6);
    assert_eq!(current["projects"][0]["name"], "web");
    assert_eq!(current["projects"][0]["memory"], 700);
    assert_eq!(current["containers"][0]["name"], "web-app-1");

    let web = get(&socket, "/api/metrics?project=web").await;
    assert_eq!(web["machine"], serde_json::Value::Null);
    assert_eq!(web["containers"].as_array().unwrap().len(), 1);

    let history = get(&socket, "/api/metrics/history?range=1h").await;
    assert_eq!(history["step"], 60);
    assert_eq!(history["points"].as_array().unwrap().len(), 1);
    assert_eq!(history["points"][0]["t"], current["at"]);
    let app = get(&socket, "/api/metrics/history?range=1h&project=web").await;
    assert_eq!(app["points"][0]["projects"][0]["name"], "web");

    let mut events = connect(&socket).await;
    events
        .write_all(b"GET /api/events HTTP/1.1\r\nHost: alchemyd\r\n\r\n")
        .await
        .unwrap();
    let mut seen = String::new();
    while !seen.contains("event: metrics\ndata: {\"at\":") {
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(TIMEOUT, events.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "event stream closed: {seen}");
        seen.push_str(std::str::from_utf8(&chunk[..n]).unwrap());
    }

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .expect("daemon did not stop with the collector running")
        .unwrap()
        .unwrap();

    let store: SharedStore = std::sync::Arc::new(SqliteStore::open(&state_dir).await.unwrap());
    let at = current["at"].as_i64().unwrap();
    assert_eq!(store.metric_times(60, 0, i64::MAX).await.unwrap(), [at]);
}
