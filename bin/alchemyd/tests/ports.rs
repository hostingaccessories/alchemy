//! Port registry wiring: `run_with` serving `GET /api/ports` on both
//! listeners from the apps task's Compose configurations, the engine's
//! containers, the state database's reservations, and the host's
//! listeners, and refusing to start an app whose ports are taken.

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

use alchemy_api::operations::{ActionPlan, OperationResponse};
use alchemy_api::ports::PortList;
use alchemy_api::{ErrorCode, ErrorResponse};
use alchemy_core::engine::{ContainerState, ContainerStatus, EngineState, EngineStatus};
use alchemy_core::ports::{Listener, PortRange, Protocol, PublishedPort};
use alchemyd::apps::{AppsDeps, SystemFs};
use alchemyd::compose::{ComposeConfig, FakeComposeConfig, FakeConfig, ServiceConfig};
use alchemyd::engine::EngineSource;
use alchemyd::network::FakeNetwork;
use alchemyd::store::{PortReservation, SharedStore, SqliteStore};
use alchemyd::{Deps, State};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_secs(10);

fn tcp(port: u16) -> PublishedPort {
    PublishedPort::new(Protocol::Tcp, PortRange::single(port).ok())
}

/// A ready engine running `blog`'s web container on port 8080.
struct BlogRunning;

impl EngineSource for BlogRunning {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        let mut web = ContainerStatus::new("abc", "blog-web-1", ContainerState::Running);
        web.project = Some("blog".into());
        web.service = Some("web".into());
        web.ports = vec![tcp(8080).with_target(PortRange::single(80).ok())];
        state.update_containers(|c| c.replace_all([web]));
        state.set_engine(EngineStatus::new(EngineState::Ready));
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

fn project(name: &str, service: &str, port: u16) -> FakeConfig {
    FakeConfig::Config(ComposeConfig {
        name: name.into(),
        services: vec![ServiceConfig {
            name: service.into(),
            ports: vec![tcp(port)],
            ..ServiceConfig::default()
        }],
        ..ComposeConfig::default()
    })
}

async fn send(mut stream: impl AsyncRead + AsyncWrite + Unpin, request: &str) -> (String, String) {
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut seen))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    (head.to_ascii_lowercase(), body.to_owned())
}

async fn over_socket(socket: &Path, request: &str) -> (String, String) {
    send(
        tokio::net::UnixStream::connect(socket).await.unwrap(),
        request,
    )
    .await
}

async fn over_tcp(addr: SocketAddr, request: &str) -> (String, String) {
    send(tokio::net::TcpStream::connect(addr).await.unwrap(), request).await
}

fn rows(list: &PortList) -> Vec<String> {
    list.ports
        .iter()
        .map(|c| format!("{} {:?} {}", c.port, c.source, c.holder))
        .collect()
}

/// An apps home with `blog` declaring 8080 and `wiki` declaring 3000.
fn apps_home(dir: &Path) -> (PathBuf, FakeComposeConfig) {
    let home = dir.join("apps");
    for app in ["blog", "wiki"] {
        std::fs::create_dir_all(home.join(app)).unwrap();
        std::fs::write(home.join(app).join("compose.yaml"), "services: {}\n").unwrap();
    }
    let home = home.canonicalize().unwrap();
    let config = FakeComposeConfig::new();
    config.set(home.join("blog"), project("blog", "web", 8080));
    config.set(home.join("wiki"), project("wiki", "app", 3000));
    (home, config)
}

/// Reserve 8443 for `proxy` in the database the daemon will open.
async fn reserve_proxy(state_dir: &Path) {
    let store: SharedStore = Arc::new(SqliteStore::open(state_dir).await.unwrap());
    let added = store
        .add_port_reservation(PortReservation {
            owner: "proxy".into(),
            port: 8443,
            protocol: Protocol::Tcp,
            host_ip: None,
            created: 1,
        })
        .await
        .unwrap();
    assert!(added);
}

/// Docker Desktop listening for blog's container, and `nc -l 9000`.
fn listening() -> FakeNetwork {
    let network = FakeNetwork::new();
    network.set_listeners(vec![
        Listener {
            protocol: Protocol::Tcp,
            addr: "::".parse().unwrap(),
            port: 8080,
            process: Some("com.docker.backe".into()),
            pid: Some(7),
        },
        Listener {
            protocol: Protocol::Tcp,
            addr: "0.0.0.0".parse().unwrap(),
            port: 9000,
            process: Some("nc".into()),
            pid: Some(42),
        },
    ]);
    network
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_declared_live_reserved_and_other_programs_ports() {
    let dir = tempfile::Builder::new()
        .prefix("ap")
        .tempdir_in("/tmp")
        .unwrap();
    let (home, config) = apps_home(dir.path());
    let state_dir = dir.path().join("state");
    reserve_proxy(&state_dir).await;
    let network = listening();

    let listen = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let socket = dir.path().join("alchemyd.sock");
    let deps = Deps::new(BlogRunning)
        .with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home))
                .with_scan_interval(Duration::from_millis(20)),
        )
        .with_network(Arc::new(network.clone()));
    let state = deps.state.clone();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen,
            socket: socket.clone(),
            state_dir: state_dir.clone(),
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));
    let mut apps = state.watch_apps();
    tokio::time::timeout(
        TIMEOUT,
        apps.wait_for(|list| list.apps.len() == 2 && list.apps.iter().all(|a| a.project.is_some())),
    )
    .await
    .unwrap()
    .unwrap();
    let token_path = alchemy_api::tcp_token_path(&state_dir);
    tokio::time::timeout(TIMEOUT, async {
        while !socket.exists() || !token_path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let get = "GET /api/ports HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let (head, body) = over_socket(&socket, get).await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    let list: PortList = serde_json::from_str(&body).unwrap();
    assert_eq!(
        rows(&list),
        [
            "3000/tcp Declared service app of app wiki",
            "8080/tcp Live service web of app blog",
            "8443/tcp Reserved a reservation for proxy",
            "0.0.0.0:9000/tcp Live another program (nc, pid 42)",
        ]
    );
    assert_eq!(list.warnings, Vec::<String>::new());

    let (head, _) = over_tcp(
        listen,
        &format!("GET /api/ports HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(head.starts_with("http/1.1 401"), "signing in: {head}");
    let token = std::fs::read_to_string(&token_path).unwrap();
    let (head, body) = over_tcp(
        listen,
        &format!(
            "GET /api/ports HTTP/1.1\r\nHost: {listen}\r\nAuthorization: Bearer {token}\r\n\
             Connection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(serde_json::from_str::<PortList>(&body).unwrap(), list);

    network.set_failing(true);
    let (_, body) = over_socket(&socket, get).await;
    let list: PortList = serde_json::from_str(&body).unwrap();
    assert_eq!(list.ports.len(), 3, "{list:?}");
    assert_eq!(list.warnings, [alchemyd::ports::LISTENERS_UNREAD]);

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

fn post(uri: &str, body: &str) -> String {
    format!(
        "POST {uri} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn get(uri: &str) -> String {
    format!("GET {uri} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_to_start_an_app_whose_ports_are_taken_unless_forced() {
    let dir = tempfile::Builder::new()
        .prefix("ap")
        .tempdir_in("/tmp")
        .unwrap();
    let (home, config) = apps_home(dir.path());
    config.set(home.join("wiki"), project("wiki", "app", 8080));
    let state_dir = dir.path().join("state");
    let socket = dir.path().join("alchemyd.sock");
    let listen = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let deps = Deps::new(BlogRunning)
        .with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home))
                .with_scan_interval(Duration::from_millis(20)),
        )
        .with_network(Arc::new(listening()));
    let state = deps.state.clone();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen,
            socket: socket.clone(),
            state_dir,
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));
    let mut apps = state.watch_apps();
    tokio::time::timeout(
        TIMEOUT,
        apps.wait_for(|list| list.apps.len() == 2 && list.apps.iter().all(|a| a.project.is_some())),
    )
    .await
    .unwrap()
    .unwrap();
    let mut engine = state.watch_engine();
    tokio::time::timeout(TIMEOUT, engine.wait_for(|e| e.state == EngineState::Ready))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(TIMEOUT, async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let taken = "Port 8080/tcp for service app is in use by service web of app blog.";

    let (head, body) = over_socket(&socket, &get("/api/apps/wiki/plan?action=up")).await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    let plan: ActionPlan = serde_json::from_str(&body).unwrap();
    assert_eq!(plan.conflicts.len(), 1, "{body}");
    assert_eq!(plan.conflicts[0].message, taken);
    assert_eq!(plan.conflicts[0].service, "app");

    let up = r#"{"action":"up"}"#;
    let (head, body) = over_socket(&socket, &post("/api/apps/wiki/actions", up)).await;
    assert!(head.starts_with("http/1.1 409"), "{head}");
    let refusal: ErrorResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(refusal.code, ErrorCode::Conflict);
    assert_eq!(
        refusal.error,
        format!("{taken} Free that port, or force up to start wiki anyway.")
    );

    let forced = r#"{"action":"up","force":true}"#;
    let (head, body) = over_socket(&socket, &post("/api/apps/wiki/actions", forced)).await;
    assert!(head.starts_with("http/1.1 202"), "{head} {body}");
    let started: OperationResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(started.operation.app.as_str(), "wiki");

    let restart = r#"{"action":"restart"}"#;
    let (head, body) = over_socket(&socket, &post("/api/apps/blog/actions", restart)).await;
    assert!(
        head.starts_with("http/1.1 202"),
        "an app's own ports never stop it: {head} {body}"
    );

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
