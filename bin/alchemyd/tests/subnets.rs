//! Network check wiring: `subnets::run` creating and remembering the
//! `alchemy` network on a free subnet, leaving an overlapping one alone,
//! and `run_with` serving the report on `GET /api/network`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alchemy_api::network::{AlchemyNetworkState, NetworkReport, NetworkWarningKind, PoolsSource};
use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};
use alchemy_core::network::{Ipv4Net, MANAGED_LABEL, default_address_pools};
use alchemyd::engine::{AddressPool, DockerNetwork, EngineSource};
use alchemyd::network::{FakeNetwork, Route, SharedNetwork};
use alchemyd::store::{MemoryStore, SharedStore};
use alchemyd::subnets::{self, FakeDaemonConfig, FakeEngineNetworks, SETTINGS_KEY, SubnetsDeps};
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

const TIMEOUT: Duration = Duration::from_secs(10);
const ENDPOINT: &str = "unix:///run/fake/docker.sock";

fn n(s: &str) -> Ipv4Net {
    s.parse().unwrap()
}

fn ready() -> EngineStatus {
    let mut status = EngineStatus::new(EngineState::Ready);
    status.endpoint = Some(ENDPOINT.to_owned());
    status.flavor = Some(EngineFlavor::DockerEngine);
    status
}

fn bridge() -> DockerNetwork {
    DockerNetwork {
        id: "b".into(),
        name: "bridge".into(),
        driver: Some("bridge".into()),
        subnets: vec![n("172.17.0.0/16")],
        labels: BTreeMap::new(),
        project: None,
    }
}

/// A host on `lan` with Docker's default bridge.
fn home(lan: &str) -> (FakeNetwork, FakeEngineNetworks) {
    let host = FakeNetwork::new();
    host.set_lan_subnets(vec![n(lan)]);
    host.set_routes(vec![
        Route {
            destination: n("0.0.0.0/0"),
            gateway: Some("10.0.0.1".parse().unwrap()),
            interface: "eth0".into(),
        },
        Route {
            destination: n(lan),
            gateway: None,
            interface: "eth0".into(),
        },
    ]);
    let engine = FakeEngineNetworks::new();
    engine.set_networks(vec![bridge()]);
    engine.set_bridge(vec![n("172.17.0.0/16")]);
    (host, engine)
}

struct Running {
    state: State,
    store: SharedStore,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn report_where(&self, done: impl Fn(&NetworkReport) -> bool) -> NetworkReport {
        let mut network = self.state.watch_network();
        tokio::time::timeout(TIMEOUT, network.wait_for(|r| done(r)))
            .await
            .expect("no matching network report")
            .unwrap()
            .clone()
    }

    async fn stop(self) {
        self.shutdown.send_replace(true);
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

fn start(
    host: FakeNetwork,
    engine: FakeEngineNetworks,
    daemon_json: FakeDaemonConfig,
    store: SharedStore,
) -> Running {
    let state = State::new();
    let (shutdown, rx) = watch::channel(false);
    let deps = SubnetsDeps::new(Arc::new(engine), Arc::new(daemon_json))
        .with_settle(Duration::from_millis(10));
    let host: SharedNetwork = Arc::new(host);
    let task = tokio::spawn(subnets::run(deps, host, state.clone(), store.clone(), rx));
    Running {
        state,
        store,
        shutdown,
        task,
    }
}

fn store() -> SharedStore {
    Arc::new(MemoryStore::new())
}

#[derive(serde::Deserialize)]
struct Saved {
    subnet: Ipv4Net,
}

#[tokio::test]
async fn creates_the_network_once_the_engine_is_ready_on_a_disjoint_subnet() {
    for (lan, want) in [
        ("10.0.1.0/24", "172.18.0.0/24"),
        ("172.18.0.0/16", "172.19.0.0/24"),
        ("192.168.0.0/16", "172.18.0.0/24"),
    ] {
        let (host, engine) = home(lan);
        let running = start(host, engine.clone(), FakeDaemonConfig::new(), store());

        let waiting = running.report_where(|r| !r.lan.is_empty()).await;
        assert!(!waiting.engine_ready, "{lan}");
        assert_eq!(waiting.alchemy.state, AlchemyNetworkState::Pending, "{lan}");
        assert_eq!(engine.created(), [], "{lan}");

        running.state.set_engine(ready());
        let report = running.report_where(|r| r.engine_ready).await;
        assert_eq!(report.alchemy.state, AlchemyNetworkState::Ready, "{lan}");
        assert_eq!(report.alchemy.subnet, Some(n(want)), "{lan}");
        assert_eq!(report.pools_source, PoolsSource::Defaults, "{lan}");
        assert_eq!(report.pools, default_address_pools(), "{lan}");
        let created = engine.created();
        assert_eq!(created.len(), 1, "{lan}");
        assert_eq!(created[0].name, "alchemy");
        assert_eq!(created[0].subnet, n(want), "{lan}");
        assert_eq!(
            created[0].labels.get(MANAGED_LABEL).map(String::as_str),
            Some("true")
        );
        assert!(
            report
                .networks
                .iter()
                .any(|net| net.name == "alchemy" && net.managed),
            "{lan}"
        );
        let saved: Saved = running.store.setting(SETTINGS_KEY).await.unwrap().unwrap();
        assert_eq!(saved.subnet, n(want), "{lan}");
        running.stop().await;
    }
}

#[tokio::test]
async fn overlap_with_the_lan_is_explained() {
    let (host, engine) = home("192.168.0.0/16");
    let running = start(host, engine, FakeDaemonConfig::new(), store());
    running.state.set_engine(ready());
    let report = running.report_where(|r| r.engine_ready).await;
    let kinds: Vec<_> = report.warnings.iter().map(|w| w.kind).collect();
    assert_eq!(kinds, [NetworkWarningKind::PoolOverlap]);
    let fix = report.warnings[0].fix.as_deref().unwrap();
    assert!(fix.contains("/etc/docker/daemon.json"), "{fix}");
    assert!(fix.contains("\"default-address-pools\""), "{fix}");
    running.stop().await;
}

#[tokio::test]
async fn an_overlapping_network_is_left_alone() {
    let (host, engine) = home("172.18.0.0/16");
    let mut labels = BTreeMap::new();
    labels.insert(MANAGED_LABEL.to_owned(), "true".to_owned());
    engine.set_networks(vec![
        bridge(),
        DockerNetwork {
            id: "a".into(),
            name: "alchemy".into(),
            driver: Some("bridge".into()),
            subnets: vec![n("172.18.0.0/24")],
            labels,
            project: None,
        },
    ]);
    let running = start(host, engine.clone(), FakeDaemonConfig::new(), store());
    running.state.set_engine(ready());
    let report = running.report_where(|r| r.engine_ready).await;
    assert_eq!(report.alchemy.state, AlchemyNetworkState::Overlapping);
    assert_eq!(report.alchemy.subnet, Some(n("172.18.0.0/24")));
    assert_eq!(
        report.warnings[0].kind,
        NetworkWarningKind::AlchemyNetworkOverlap
    );
    assert_eq!(engine.created(), []);
    assert_eq!(
        running
            .store
            .setting::<serde_json::Value>(SETTINGS_KEY)
            .await
            .unwrap(),
        None
    );
    running.stop().await;
}

#[tokio::test]
async fn a_lost_network_comes_back_on_its_remembered_subnet() {
    let store = store();
    store
        .set_setting(
            SETTINGS_KEY,
            &serde_json::json!({"subnet": "172.25.3.0/24"}),
        )
        .await
        .unwrap();
    let (host, engine) = home("10.0.1.0/24");
    let running = start(host, engine.clone(), FakeDaemonConfig::new(), store);
    running.state.set_engine(ready());
    let report = running.report_where(|r| r.engine_ready).await;
    assert_eq!(report.alchemy.subnet, Some(n("172.25.3.0/24")));

    engine.set_networks(vec![bridge()]);
    let mut status = ready();
    status.engine_version = Some("29.0.0".into());
    running.state.set_engine(status);
    tokio::time::timeout(TIMEOUT, async {
        while engine.created().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(engine.created()[1].subnet, n("172.25.3.0/24"));
    running.stop().await;
}

#[tokio::test]
async fn pools_come_from_daemon_json_when_the_engine_reports_none() {
    let json = r#"{"default-address-pools":[{"base":"10.10.0.0/16","size":24}]}"#;
    let (host, engine) = home("10.10.0.0/16");
    let running = start(
        host,
        engine.clone(),
        FakeDaemonConfig::with_text(json),
        store(),
    );
    running.state.set_engine(ready());
    let report = running.report_where(|r| r.engine_ready).await;
    assert_eq!(report.pools_source, PoolsSource::DaemonJson);
    assert_eq!(report.warnings[0].kind, NetworkWarningKind::PoolOverlap);
    running.stop().await;

    let (host, engine) = home("10.0.1.0/24");
    engine.set_pools(vec![AddressPool {
        base: n("10.20.0.0/16"),
        size: 24,
    }]);
    let running = start(host, engine, FakeDaemonConfig::with_text(json), store());
    running.state.set_engine(ready());
    let report = running.report_where(|r| r.engine_ready).await;
    assert_eq!(report.pools_source, PoolsSource::Engine);
    assert_eq!(report.pools[0].base(), n("10.20.0.0/16"));
    running.stop().await;
}

#[tokio::test]
async fn failures_become_warnings() {
    let (host, engine) = home("10.0.1.0/24");
    engine.set_refuse_create(true);
    let running = start(
        host.clone(),
        engine.clone(),
        FakeDaemonConfig::new(),
        store(),
    );
    running.state.set_engine(ready());
    let report = running.report_where(|r| r.engine_ready).await;
    assert_eq!(report.alchemy.state, AlchemyNetworkState::Unavailable);
    assert_eq!(report.warnings[0].kind, NetworkWarningKind::CreateFailed);

    engine.set_failing(true);
    host.set_failing(true);
    let mut status = ready();
    status.engine_version = Some("29.0.0".into());
    running.state.set_engine(status);
    let report = running.report_where(|r| !r.engine_ready).await;
    let kinds: Vec<_> = report.warnings.iter().map(|w| w.kind).collect();
    assert_eq!(
        kinds,
        [
            NetworkWarningKind::EngineUnreadable,
            NetworkWarningKind::HostUnreadable
        ]
    );
    running.stop().await;
}

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

async fn get(socket: &Path, uri: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut stream = loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => break stream,
            Err(err) if tokio::time::Instant::now() > deadline => panic!("connect: {err}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let request = format!("GET {uri} HTTP/1.1\r\nHost: alchemyd\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn the_daemon_serves_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("alchemyd.sock");
    let config = alchemyd::Config {
        listen: free_port(),
        socket: socket.clone(),
        state_dir: dir.path().join("state"),
    };
    let (host, engine) = home("192.168.0.0/16");
    let deps = Deps::new(FixedEngine(ready()))
        .with_network(Arc::new(host))
        .with_subnets(
            SubnetsDeps::new(Arc::new(engine.clone()), Arc::new(FakeDaemonConfig::new()))
                .with_settle(Duration::from_millis(10)),
        );
    let state = deps.state.clone();
    let (stop, stopped) = oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(config, deps, async {
        let _ = stopped.await;
    }));

    let mut network = state.watch_network();
    tokio::time::timeout(TIMEOUT, network.wait_for(|r| r.engine_ready))
        .await
        .unwrap()
        .unwrap();
    let body = get(&socket, "/api/network").await;
    assert_eq!(body["engine_ready"], true);
    assert_eq!(body["lan"], serde_json::json!(["192.168.0.0/16"]));
    assert_eq!(body["alchemy"]["state"], "ready");
    assert_eq!(body["alchemy"]["subnet"], "172.18.0.0/24");
    assert_eq!(body["warnings"][0]["kind"], "pool_overlap");
    assert_eq!(engine.created().len(), 1);

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
