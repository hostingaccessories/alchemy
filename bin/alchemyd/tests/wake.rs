//! Wake wiring: `run_with` noticing a sleep on a fake clock, then sending
//! one `woke` event on `/api/events`, reading the LAN address again at
//! once instead of on the next routing tick, and subscribing again to an
//! engine whose event stream hung, all within seconds.
//!
//! This is the sleep test CI runs on both Linux and macOS: nothing in it
//! depends on the host's clocks, network, or Docker.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::future::{Future, ready};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alchemy_api::Wake;
use alchemy_core::engine::{ContainerEvent, ContainerState, ContainerStatus, EngineState};
use alchemyd::Deps;
use alchemyd::engine::{
    ApiError, CliError, Connector, DockerApi, DockerCli, Endpoint, EngineMonitor, EngineVersion,
    HostEnv, HostProbe, Os, SocketConnect,
};
use alchemyd::network::{FakeNetwork, LanAddress};
use alchemyd::wake::{FakeClock, WakeDeps};
use futures_util::{Stream, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

const TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET: &str = "/run/fake/docker.sock";
const DOCKER: &str = "/usr/bin/docker";
const TICK: Duration = Duration::from_millis(20);

/// An engine at [`SOCKET`] that answers everything at once, but whose
/// event streams never send or end, as one can after sleep. Counts each
/// subscription.
#[derive(Debug, Clone, Default)]
struct HangingEngine {
    subscriptions: Arc<AtomicUsize>,
}

impl HangingEngine {
    fn subscriptions(&self) -> usize {
        self.subscriptions.load(Ordering::SeqCst)
    }
}

impl HostProbe for HangingEngine {
    fn exists(&self, path: &Path) -> bool {
        path == Path::new(SOCKET)
    }

    fn is_executable(&self, path: &Path) -> bool {
        path == Path::new(DOCKER)
    }

    fn connect(&self, _socket: &Path) -> impl Future<Output = SocketConnect> + Send {
        ready(SocketConnect::Connected)
    }
}

impl DockerCli for HangingEngine {
    fn context_host(&self) -> impl Future<Output = Result<String, CliError>> + Send {
        ready(Err(CliError::Failed {
            code: Some(1),
            stderr: "no context".into(),
        }))
    }

    fn compose_version(&self) -> impl Future<Output = Result<String, CliError>> + Send {
        ready(Ok("2.40.0".into()))
    }
}

impl DockerApi for HangingEngine {
    fn ping(&self) -> impl Future<Output = Result<(), ApiError>> + Send {
        ready(Ok(()))
    }

    fn version(&self) -> impl Future<Output = Result<EngineVersion, ApiError>> + Send {
        ready(Ok(EngineVersion {
            version: Some("29.8.1".into()),
            api_version: Some("1.52".into()),
            platform: Some("Docker Engine - Community".into()),
        }))
    }

    fn list_containers(
        &self,
    ) -> impl Future<Output = Result<Vec<ContainerStatus>, ApiError>> + Send {
        ready(Ok(vec![ContainerStatus::new(
            "abc",
            "web",
            ContainerState::Running,
        )]))
    }

    fn events(
        &self,
    ) -> impl Stream<Item = Result<ContainerEvent, ApiError>> + Send + Unpin + 'static {
        self.subscriptions.fetch_add(1, Ordering::SeqCst);
        futures_util::stream::pending().boxed()
    }

    fn inspect_exit(
        &self,
        _id: String,
    ) -> impl Future<Output = Result<Option<alchemy_core::engine::InspectedExit>, ApiError>> + Send
    {
        ready(Ok(None))
    }
}

impl Connector for HangingEngine {
    type Cli = Self;
    type Api = Self;

    fn cli(&self, _binary: PathBuf) -> Self {
        self.clone()
    }

    fn api(&self, _endpoint: &Endpoint) -> Result<Self, ApiError> {
        Ok(self.clone())
    }
}

fn lan(addr: [u8; 4]) -> LanAddress {
    LanAddress {
        interface: "en0".into(),
        addr: Ipv4Addr::from(addr),
        subnet: format!("{}.{}.{}.0/24", addr[0], addr[1], addr[2])
            .parse()
            .unwrap(),
    }
}

async fn eventually(what: &str, ok: impl Fn() -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// The `woke` events in the SSE text `seen`.
fn wakes(seen: &str) -> Vec<Wake> {
    seen.split("event: woke\ndata: ")
        .skip(1)
        .map(|frame| serde_json::from_str(frame.split('\n').next().unwrap()).unwrap())
        .collect()
}

/// Read the event stream into `seen` until `done` holds for it.
async fn read_until(stream: &mut UnixStream, seen: &mut String, done: impl Fn(&str) -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        while !done(seen) {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the event stream closed: {seen}");
            seen.push_str(std::str::from_utf8(&chunk[..n]).unwrap());
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the event stream never got there: {seen}"));
}

/// Read whatever the event stream sends into `seen` for `wait`.
async fn drain_for(stream: &mut UnixStream, seen: &mut String, wait: Duration) {
    let until = tokio::time::Instant::now() + wait;
    let mut chunk = [0u8; 4096];
    while let Ok(read) = tokio::time::timeout_at(until, stream.read(&mut chunk)).await {
        let n = read.unwrap();
        assert!(n > 0, "the event stream closed: {seen}");
        seen.push_str(std::str::from_utf8(&chunk[..n]).unwrap());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn after_a_sleep_it_rereads_the_lan_and_resubscribes_to_docker_within_seconds() {
    let dir = tempfile::Builder::new()
        .prefix("wk")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("alchemyd.sock");
    let engine = HangingEngine::default();
    let host = FakeNetwork::new();
    host.set_lan_addresses(vec![lan([192, 168, 1, 20])]);
    let clock = FakeClock::new(1_700_000_000_000);
    let monitor = EngineMonitor::new(
        HostEnv {
            docker_host: Some(format!("unix://{SOCKET}")),
            path: Some("/usr/bin".into()),
            ..HostEnv::default()
        },
        Os::Linux,
        engine.clone(),
        engine.clone(),
    );
    let deps = Deps::new(monitor)
        .with_network(Arc::new(host.clone()))
        .with_wake(WakeDeps::new(Arc::new(clock.clone()), TICK));
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
    eventually("the daemon to listen", || socket.exists()).await;
    eventually("Docker to be followed", || engine.subscriptions() == 1).await;
    eventually("the LAN address to be read", || host.lan_reads() == 1).await;

    let mut events = UnixStream::connect(&socket).await.unwrap();
    events
        .write_all(
            b"GET /api/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
        )
        .await
        .unwrap();
    let mut seen = String::new();
    read_until(&mut events, &mut seen, |s| s.contains("event: engine\n")).await;
    drain_for(&mut events, &mut seen, TICK * 5).await;
    assert_eq!(wakes(&seen), [], "no wake on connect or while awake");
    assert_eq!(engine.subscriptions(), 1);

    host.set_lan_addresses(vec![lan([10, 0, 0, 7])]);
    clock.sleep(Duration::from_secs(3_600));
    let slept = tokio::time::Instant::now();
    read_until(&mut events, &mut seen, |s| !wakes(s).is_empty()).await;
    eventually("the LAN address to be read again", || host.lan_reads() == 2).await;
    eventually("Docker to be followed again", || {
        engine.subscriptions() == 2
    })
    .await;
    assert!(
        slept.elapsed() < Duration::from_secs(5),
        "well within the 10s routing tick: {:?}",
        slept.elapsed()
    );
    assert_eq!(state.engine().state, EngineState::Ready);

    drain_for(&mut events, &mut seen, TICK * 10).await;
    let woke = state.last_wake().unwrap();
    assert_eq!(state.wakes(), 1, "counted once for reconcile");
    assert_eq!(woke.slept_for, Duration::from_secs(3_600));
    assert_eq!(
        wakes(&seen),
        [alchemyd::wake::api_wake(woke)],
        "one woke event per sleep: {seen}"
    );
    assert_eq!(
        wakes(&seen).first().map(|w| w.slept_for),
        Some(3_600),
        "in whole seconds"
    );
    assert_eq!(host.lan_reads(), 2, "once per wake");
    assert_eq!(engine.subscriptions(), 2, "once per wake");

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
