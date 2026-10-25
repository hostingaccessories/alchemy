//! Routing wiring: `run_with` planning routes from the apps task's
//! Compose configurations and the engine's containers, reserving each
//! app's proxy port, attaching containers to the `alchemy` network, and
//! serving the app through the proxy, with its addresses on
//! `GET /api/apps` and the proxy's listeners on `GET /api/network`, with
//! a listener another program answers made unavailable, and `<app>.local`
//! routed only while the `network.mdns` setting is on, announced through
//! a fake announcer that follows the LAN address, the setting, conflicts,
//! a wake, and shutdown.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alchemy_api::apps::{AddressKind, AddressState, App, AppList, AppProblemKind};
use alchemy_api::network::{ListenerState, NetworkReport, Reachability};
use alchemy_api::ports::PortList;
use alchemy_core::engine::{
    ContainerState, ContainerStatus, EngineFlavor, EngineState, EngineStatus,
};
use alchemy_core::ports::{PortHolder, PortRange, Protocol, PublishedPort};
use alchemy_core::route::{EngineFacts, ExposedPort, HostOs};
use alchemyd::apps::{AppsDeps, SystemFs};
use alchemyd::compose::{ComposeConfig, FakeComposeConfig, FakeConfig, ServiceConfig};
use alchemyd::engine::{DockerNetwork, EngineSource};
use alchemyd::network::{FakeNetwork, LanAddress};
use alchemyd::proxy::{FakeAnswer, FakeConnector, FakeProbe};
use alchemyd::routing::ProxyDeps;
use alchemyd::subnets::{FakeDaemonConfig, FakeEngineNetworks, SubnetsDeps};
use alchemyd::{Deps, State};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_secs(10);
const ENDPOINT: &str = "unix:///run/fake/docker.sock";

/// A ready engine with `facts`, running `containers`.
struct Engine {
    facts: EngineFacts,
    containers: Vec<ContainerStatus>,
}

impl EngineSource for Engine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.update_containers(|c| c.replace_all(self.containers));
        let mut status = EngineStatus::new(EngineState::Ready);
        status.endpoint = Some(ENDPOINT.to_owned());
        status.flavor = self.facts.flavor;
        state.set_engine(status);
        state.set_engine_facts(Some(self.facts));
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

fn running(name: &str, project: &str, service: &str) -> ContainerStatus {
    let mut c = ContainerStatus::new(name, name, ContainerState::Running);
    c.project = Some(project.into());
    c.service = Some(service.into());
    c
}

fn tcp(published: u16, target: u16) -> PublishedPort {
    PublishedPort::new(Protocol::Tcp, PortRange::single(published).ok())
        .with_target(PortRange::single(target).ok())
}

fn service(name: &str, expose: &[&str], ports: Vec<PublishedPort>) -> ServiceConfig {
    ServiceConfig {
        name: name.into(),
        expose: expose
            .iter()
            .map(|e| e.parse::<ExposedPort>().unwrap())
            .collect(),
        ports,
        ..ServiceConfig::default()
    }
}

/// An apps home holding `apps`, each with its Compose project and
/// services.
fn apps_home(dir: &Path, apps: Vec<(&str, Vec<ServiceConfig>)>) -> (PathBuf, FakeComposeConfig) {
    let home = dir.join("apps");
    for (app, _) in &apps {
        std::fs::create_dir_all(home.join(app)).unwrap();
        std::fs::write(home.join(app).join("compose.yaml"), "services: {}\n").unwrap();
    }
    let home = home.canonicalize().unwrap();
    let config = FakeComposeConfig::new();
    for (app, services) in apps {
        config.set(
            home.join(app),
            FakeConfig::Config(ComposeConfig {
                name: app.into(),
                services,
                ..ComposeConfig::default()
            }),
        );
    }
    (home, config)
}

/// A fake app on loopback that answers every request with `tag` and the
/// `Host` it was sent.
async fn fake_app(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0_u8; 1024];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
                let head = String::from_utf8_lossy(&seen).to_ascii_lowercase();
                let host = head
                    .lines()
                    .find_map(|l| l.strip_prefix("host: "))
                    .unwrap_or("")
                    .to_owned();
                let body = format!("{tag} {host}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

/// A free port for a proxy port range, below the ephemeral range on
/// Linux and macOS, so a listener another test binds to port `0` never
/// takes it first.
fn free_port() -> u16 {
    free_ports(1)
}

/// The first of `count` free ports in a row, as [`free_port`] picks.
fn free_ports(count: u16) -> u16 {
    use std::hash::{BuildHasher as _, Hasher as _};

    let seed = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let start = 20_000 + u16::try_from(seed % 5_000).unwrap() * 2;
    (start..30_000)
        .chain(20_000..start)
        .find(|&port| {
            (port..port + count).all(|p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok())
        })
        .unwrap()
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

async fn get_socket<T: serde::de::DeserializeOwned>(socket: &Path, uri: &str) -> T {
    let request = format!("GET {uri} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let (head, body) = send(
        tokio::net::UnixStream::connect(socket).await.unwrap(),
        &request,
    )
    .await;
    assert!(head.starts_with("http/1.1 200"), "{uri}: {head}");
    serde_json::from_str(&body).unwrap()
}

async fn get_tcp(addr: SocketAddr, host: &str) -> (String, String) {
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    send(
        tokio::net::TcpStream::connect(addr).await.unwrap(),
        &request,
    )
    .await
}

struct Daemon {
    state: State,
    socket: PathBuf,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<alchemyd::Stopped>>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(
        dir: tempfile::TempDir,
        engine: Engine,
        apps: (PathBuf, FakeComposeConfig),
        networks: FakeEngineNetworks,
        proxy: ProxyDeps,
    ) -> Self {
        Self::start_on(dir, engine, apps, networks, proxy, FakeNetwork::new()).await
    }

    /// [`Self::start`] on a host whose LAN `host` describes.
    async fn start_on(
        dir: tempfile::TempDir,
        engine: Engine,
        apps: (PathBuf, FakeComposeConfig),
        networks: FakeEngineNetworks,
        proxy: ProxyDeps,
        host: FakeNetwork,
    ) -> Self {
        let socket = dir.path().join("alchemyd.sock");
        let listen = SocketAddr::from(([127, 0, 0, 1], 0));
        let (home, config) = apps;
        let deps = Deps::new(engine)
            .with_apps(
                AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home))
                    .with_scan_interval(Duration::from_millis(20)),
            )
            .with_subnets(SubnetsDeps::new(
                Arc::new(networks),
                Arc::new(FakeDaemonConfig::new()),
            ))
            .with_network(Arc::new(host))
            .with_proxy(proxy);
        let state = deps.state.clone();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(alchemyd::run_with(
            alchemyd::Config {
                listen,
                socket: socket.clone(),
                state_dir: dir.path().join("state"),
            },
            deps,
            async {
                let _ = stop_rx.await;
            },
        ));
        tokio::time::timeout(TIMEOUT, async {
            while tokio::net::UnixStream::connect(&socket).await.is_err() {
                assert!(!task.is_finished(), "the daemon stopped while starting");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        Self {
            state,
            socket,
            stop,
            task,
            _dir: dir,
        }
    }

    async fn app_where(&self, name: &str, done: impl Fn(&App) -> bool) -> App {
        let mut apps = self.state.watch_apps();
        let find = |list: &AppList| list.apps.iter().find(|a| a.name.as_str() == name).cloned();
        tokio::time::timeout(
            TIMEOUT,
            apps.wait_for(|list| find(list).is_some_and(|app| done(&app))),
        )
        .await
        .unwrap_or_else(|_| panic!("{name} never got there: {:?}", find(&self.state.apps())))
        .unwrap();
        find(&self.state.apps()).unwrap()
    }

    /// `GET /api/network`, once `done` holds for it.
    async fn report_where(&self, done: impl Fn(&NetworkReport) -> bool) -> NetworkReport {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let report: NetworkReport = get_socket(&self.socket, "/api/network").await;
            if done(&report) {
                return report;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the network report never got there: {report:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
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

fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rt")
        .tempdir_in("/tmp")
        .unwrap()
}

fn alchemy_network() -> DockerNetwork {
    DockerNetwork {
        id: "a".into(),
        name: "alchemy".into(),
        driver: Some("bridge".into()),
        subnets: vec!["10.89.0.0/24".parse().unwrap()],
        labels: BTreeMap::new(),
        project: None,
    }
}

/// What the API says about `blog`, routed on `port`, and the `worker`
/// app next to it that has no port to route to.
async fn serves_the_blog_on_the_api(daemon: &Daemon, port: u16) {
    let blog: App = get_socket(&daemon.socket, "/api/apps/blog").await;
    assert_eq!(blog.addresses[0].url, format!("http://127.0.0.1:{port}/"));
    assert_eq!(blog.addresses[0].service, "web");
    assert_eq!(blog.addresses[0].port, 80);
    let worker: App = get_socket(&daemon.socket, "/api/apps/worker").await;
    assert_eq!(worker.addresses, []);
    assert_eq!(worker.problems.len(), 1, "{worker:?}");
    assert_eq!(worker.problems[0].kind, AppProblemKind::Route);

    let report: NetworkReport = get_socket(&daemon.socket, "/api/network").await;
    assert_eq!(report.proxy.reachability, Some(Reachability::ContainerIp));
    assert_eq!(report.proxy.host.state, ListenerState::Off);
    assert_eq!(report.proxy.ports.len(), 1);
    assert_eq!(report.proxy.ports[0].port, port);
    assert_eq!(report.proxy.ports[0].app.as_deref(), Some("blog"));
    assert_eq!(report.proxy.ports[0].state, ListenerState::Listening);

    let ports: PortList = get_socket(&daemon.socket, "/api/ports").await;
    assert!(
        ports
            .ports
            .iter()
            .any(|c| c.port.published == PortRange::single(port).ok()
                && c.holder
                    == PortHolder::Reservation {
                        owner: "proxy:blog".into()
                    }),
        "{ports:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_an_unpublished_app_on_its_reserved_port_once_attached() {
    let upstream = fake_app("blog").await;
    let container_ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 89, 0, 2)), 80);
    let connector = FakeConnector::new();
    connector.redirect(container_ip, upstream);
    let port = free_port();
    let networks = FakeEngineNetworks::new();
    networks.set_networks(vec![alchemy_network()]);

    let dir = tempdir();
    let apps = apps_home(
        dir.path(),
        vec![
            ("blog", vec![service("web", &["80"], vec![])]),
            (
                "worker",
                vec![service("queue", &[], vec![]), service("jobs", &[], vec![])],
            ),
        ],
    );
    let daemon = Daemon::start(
        dir,
        Engine {
            facts: EngineFacts {
                host_os: HostOs::Linux,
                flavor: Some(EngineFlavor::DockerEngine),
                rootless: false,
            },
            containers: vec![running("blog-web-1", "blog", "web")],
        },
        apps,
        networks.clone(),
        ProxyDeps::loopback()
            .with_connector(Arc::new(connector))
            .with_range(PortRange::single(port).unwrap())
            .with_interval(Duration::from_millis(50)),
    )
    .await;

    let url = format!("http://127.0.0.1:{port}/");
    let pending = daemon
        .app_where("blog", |app| {
            app.addresses.iter().any(|a| {
                a.url == url
                    && a.message.as_deref()
                        == Some("Alchemy is connecting service web to the alchemy network.")
            })
        })
        .await;
    assert_eq!(pending.addresses.len(), 1, "{pending:?}");
    assert_eq!(pending.addresses[0].kind, AddressKind::Port);
    assert_eq!(pending.addresses[0].state, AddressState::Pending);
    tokio::time::timeout(TIMEOUT, async {
        while networks.connected().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        networks.connected(),
        [("alchemy".to_owned(), "blog-web-1".to_owned())]
    );

    daemon.state.update_containers(|c| {
        let mut attached = running("blog-web-1", "blog", "web");
        attached
            .networks
            .insert("alchemy".into(), Ipv4Addr::new(10, 89, 0, 2));
        c.replace_all([attached])
    });
    daemon
        .app_where("blog", |app| {
            app.addresses
                .first()
                .is_some_and(|a| a.state == AddressState::Ready)
        })
        .await;
    let (head, body) = get_tcp(SocketAddr::from(([127, 0, 0, 1], port)), "anything").await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(body, "blog anything");

    serves_the_blog_on_the_api(&daemon, port).await;

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_published_ports_by_host_where_container_ips_are_out_of_reach() {
    let upstream = fake_app("wiki").await;
    let connector = FakeConnector::new();
    connector.redirect(upstream, upstream);
    let port = free_port();

    let dir = tempdir();
    let apps = apps_home(
        dir.path(),
        vec![
            (
                "wiki",
                vec![service("app", &[], vec![tcp(upstream.port(), 3000)])],
            ),
            ("blog", vec![service("web", &["80"], vec![])]),
        ],
    );
    let mut wiki = running("wiki-app-1", "wiki", "app");
    wiki.ports = vec![tcp(upstream.port(), 3000)];
    let daemon = Daemon::start(
        dir,
        Engine {
            facts: EngineFacts {
                host_os: HostOs::MacOs,
                flavor: Some(EngineFlavor::DockerDesktop),
                rootless: false,
            },
            containers: vec![wiki],
        },
        apps,
        FakeEngineNetworks::new(),
        ProxyDeps::loopback()
            .with_connector(Arc::new(connector))
            .with_range(PortRange::single(port).unwrap())
            .with_host_listen(Some(SocketAddr::from(([127, 0, 0, 1], 0))))
            .with_interval(Duration::from_millis(50)),
    )
    .await;

    let wiki = daemon
        .app_where("wiki", |app| {
            app.addresses.len() == 2 && app.addresses.iter().all(|a| a.state == AddressState::Ready)
        })
        .await;
    let host = wiki
        .addresses
        .iter()
        .find(|a| a.kind == AddressKind::Host)
        .unwrap();
    let host_port: u16 = host
        .url
        .strip_prefix("http://wiki.localhost:")
        .and_then(|rest| rest.strip_suffix('/'))
        .unwrap()
        .parse()
        .unwrap();
    let (head, body) = get_tcp(
        SocketAddr::from(([127, 0, 0, 1], host_port)),
        &format!("WIKI.localhost:{host_port}"),
    )
    .await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(body, format!("wiki wiki.localhost:{host_port}"));
    let (head, _) = get_tcp(
        SocketAddr::from(([127, 0, 0, 1], host_port)),
        "nobody.localhost",
    )
    .await;
    assert!(head.starts_with("http/1.1 404"), "{head}");

    let blog = daemon
        .app_where("blog", |app| {
            app.problems.iter().any(|p| p.kind == AppProblemKind::Route)
        })
        .await;
    assert_eq!(blog.addresses, []);
    assert_eq!(
        blog.problems[0].message,
        "Publish port 80 of service web to reach this app. This Docker engine only lets the host reach published ports."
    );

    let report: NetworkReport = get_socket(&daemon.socket, "/api/network").await;
    assert_eq!(report.proxy.reachability, Some(Reachability::PublishedOnly));
    assert_eq!(report.proxy.host.state, ListenerState::Listening);
    assert_eq!(report.proxy.host.port, host_port);

    daemon.stop().await;
}

#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn a_listener_another_program_answers_is_unavailable_and_open_uses_the_next_address() {
    let upstream = fake_app("wiki").await;
    let connector = FakeConnector::new();
    connector.redirect(upstream, upstream);
    let port = free_port();
    let probe = FakeProbe::new(FakeAnswer::Foreign(404));
    probe.set(port, FakeAnswer::Ours);

    let dir = tempdir();
    let apps = apps_home(
        dir.path(),
        vec![(
            "wiki",
            vec![service("app", &[], vec![tcp(upstream.port(), 3000)])],
        )],
    );
    let mut wiki = running("wiki-app-1", "wiki", "app");
    wiki.ports = vec![tcp(upstream.port(), 3000)];
    let daemon = Daemon::start(
        dir,
        Engine {
            facts: EngineFacts {
                host_os: HostOs::MacOs,
                flavor: Some(EngineFlavor::DockerDesktop),
                rootless: false,
            },
            containers: vec![wiki],
        },
        apps,
        FakeEngineNetworks::new(),
        ProxyDeps::loopback()
            .with_connector(Arc::new(connector))
            .with_probe(Arc::new(probe.clone()))
            .with_range(PortRange::single(port).unwrap())
            .with_host_listen(Some(SocketAddr::from(([127, 0, 0, 1], 0))))
            .with_interval(Duration::from_millis(50)),
    )
    .await;

    let wiki = daemon
        .app_where("wiki", |app| {
            app.addresses
                .iter()
                .any(|a| a.kind == AddressKind::Host && a.state == AddressState::Unavailable)
        })
        .await;
    assert_eq!(wiki.addresses.len(), 2, "{wiki:?}");
    let (first, host) = (&wiki.addresses[0], &wiki.addresses[1]);
    assert_eq!(first.kind, AddressKind::Port);
    assert_eq!(first.url, format!("http://127.0.0.1:{port}/"));
    assert_eq!(first.state, AddressState::Ready);
    let host_port: u16 = host
        .url
        .strip_prefix("http://wiki.localhost:")
        .and_then(|rest| rest.strip_suffix('/'))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        host.message.as_deref(),
        Some(format!("Another program on this computer takes port {host_port}.").as_str())
    );
    let asked = probe.asked();
    assert!(
        asked.contains(&SocketAddr::from(([127, 0, 0, 1], host_port)))
            && asked.contains(&SocketAddr::from(([127, 0, 0, 1], port))),
        "{asked:?}"
    );

    let report: NetworkReport = get_socket(&daemon.socket, "/api/network").await;
    assert_eq!(report.proxy.host.state, ListenerState::Unavailable);
    assert_eq!(report.proxy.host.port, host_port);
    assert_eq!(
        report.proxy.host.message.as_deref(),
        Some(
            format!(
                "Another program on this computer takes port {host_port}. Your apps still work at their IP:port address."
            )
            .as_str()
        )
    );
    assert!(report.proxy.host.fix.is_some());
    assert_eq!(report.proxy.ports[0].state, ListenerState::Listening);

    probe.set_default(FakeAnswer::Ours);
    daemon
        .app_where("wiki", |app| {
            app.addresses.len() == 2 && app.addresses.iter().all(|a| a.state == AddressState::Ready)
        })
        .await;
    let report: NetworkReport = get_socket(&daemon.socket, "/api/network").await;
    assert_eq!(report.proxy.host.state, ListenerState::Listening);
    assert_eq!(report.proxy.host.message, None);

    probe.set(port, FakeAnswer::Foreign(200));
    let wiki = daemon
        .app_where("wiki", |app| {
            app.addresses
                .iter()
                .any(|a| a.kind == AddressKind::Port && a.state == AddressState::Unavailable)
        })
        .await;
    assert_eq!(wiki.addresses[0].kind, AddressKind::Host);
    assert_eq!(wiki.addresses[0].state, AddressState::Ready);
    let report: NetworkReport = get_socket(&daemon.socket, "/api/network").await;
    assert_eq!(report.proxy.ports[0].state, ListenerState::Unavailable);
    assert_eq!(
        report.proxy.ports[0].message.as_deref(),
        Some(format!("Another program on this computer takes port {port}.").as_str())
    );

    daemon.stop().await;
}

/// Send `PUT uri` with the JSON `body` over the socket, and return the
/// response's JSON.
async fn put_socket<T: serde::de::DeserializeOwned>(socket: &Path, uri: &str, body: &str) -> T {
    let request = format!(
        "PUT {uri} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (head, body) = send(
        tokio::net::UnixStream::connect(socket).await.unwrap(),
        &request,
    )
    .await;
    assert!(head.starts_with("http/1.1 200"), "{uri}: {head}");
    serde_json::from_str(&body).unwrap()
}

/// Ask the Host listener on `port` for `host` until it answers with
/// `status`, and return the body.
async fn host_answers(port: u16, host: &str, status: &str) -> String {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (head, body) = get_tcp(addr, host).await;
        if head.starts_with(&format!("http/1.1 {status}")) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{host} never answered {status}: {head}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn routes_local_names_while_mdns_is_on_except_for_apps_that_opt_out() {
    use alchemy_api::network::{MdnsState, NetworkSettings};
    use alchemy_core::route::{XAlchemy, XAlchemyBlock};

    let wiki_app = fake_app("wiki").await;
    let notes_app = fake_app("notes").await;
    let connector = FakeConnector::new();
    connector.redirect(wiki_app, wiki_app);
    connector.redirect(notes_app, notes_app);
    let port = free_port();

    let dir = tempdir();
    let (home, config) = apps_home(
        dir.path(),
        vec![
            (
                "wiki",
                vec![service("app", &[], vec![tcp(wiki_app.port(), 3000)])],
            ),
            (
                "notes",
                vec![service("app", &[], vec![tcp(notes_app.port(), 3000)])],
            ),
        ],
    );
    config.set(
        home.join("notes"),
        FakeConfig::Config(ComposeConfig {
            name: "notes".into(),
            services: vec![service("app", &[], vec![tcp(notes_app.port(), 3000)])],
            x_alchemy: Some(XAlchemyBlock::Read(XAlchemy {
                mdns: false,
                ..XAlchemy::default()
            })),
            ..ComposeConfig::default()
        }),
    );
    let mut wiki = running("wiki-app-1", "wiki", "app");
    wiki.ports = vec![tcp(wiki_app.port(), 3000)];
    let mut notes = running("notes-app-1", "notes", "app");
    notes.ports = vec![tcp(notes_app.port(), 3000)];
    let daemon = Daemon::start(
        dir,
        Engine {
            facts: EngineFacts {
                host_os: HostOs::MacOs,
                flavor: Some(EngineFlavor::DockerDesktop),
                rootless: false,
            },
            containers: vec![wiki, notes],
        },
        (home, config),
        FakeEngineNetworks::new(),
        ProxyDeps::loopback()
            .with_connector(Arc::new(connector))
            .with_range(PortRange::single(port).unwrap())
            .with_host_listen(Some(SocketAddr::from(([127, 0, 0, 1], 0))))
            .with_interval(Duration::from_millis(50)),
    )
    .await;

    let settings: NetworkSettings = get_socket(&daemon.socket, "/api/settings/network").await;
    assert_eq!(settings, NetworkSettings { mdns: true });
    let wiki = daemon
        .app_where("wiki", |app| {
            app.addresses
                .iter()
                .any(|a| a.kind == AddressKind::Host && a.state == AddressState::Ready)
        })
        .await;
    let host_port: u16 = wiki
        .addresses
        .iter()
        .find_map(|a| a.url.strip_prefix("http://wiki.localhost:"))
        .and_then(|rest| rest.strip_suffix('/'))
        .unwrap()
        .parse()
        .unwrap();

    let body = host_answers(host_port, &format!("Wiki.Local:{host_port}"), "200").await;
    assert_eq!(body, format!("wiki wiki.local:{host_port}"));
    host_answers(host_port, "notes.local", "404").await;
    let body = host_answers(host_port, "notes.localhost", "200").await;
    assert_eq!(body, "notes notes.localhost");
    let report = daemon
        .report_where(|r| r.mdns.state != MdnsState::Starting)
        .await;
    assert_eq!(report.mdns.state, MdnsState::Unavailable, "no LAN address");
    assert!(
        report
            .mdns
            .message
            .as_deref()
            .is_some_and(|m| m.contains("can't find this computer's address")),
        "{:?}",
        report.mdns
    );

    let settings: NetworkSettings =
        put_socket(&daemon.socket, "/api/settings/network", r#"{"mdns":false}"#).await;
    assert_eq!(settings, NetworkSettings { mdns: false });
    host_answers(host_port, "wiki.local", "404").await;
    let body = host_answers(host_port, "wiki.localhost", "200").await;
    assert_eq!(body, "wiki wiki.localhost");
    let report: NetworkReport = get_socket(&daemon.socket, "/api/network").await;
    assert_eq!(report.mdns.state, MdnsState::Off);
    let settings: NetworkSettings = get_socket(&daemon.socket, "/api/settings/network").await;
    assert_eq!(settings, NetworkSettings { mdns: false });

    let settings: NetworkSettings = put_socket(&daemon.socket, "/api/settings/network", "{}").await;
    assert_eq!(
        settings,
        NetworkSettings { mdns: false },
        "{{}} changes nothing"
    );
    put_socket::<NetworkSettings>(&daemon.socket, "/api/settings/network", r#"{"mdns":true}"#)
        .await;
    let body = host_answers(host_port, "wiki.local", "200").await;
    assert_eq!(body, "wiki wiki.local");

    daemon.stop().await;
}

fn lan_address(interface: &str, addr: [u8; 4]) -> LanAddress {
    LanAddress {
        interface: interface.into(),
        addr: Ipv4Addr::from(addr),
        subnet: format!("{}.{}.{}.0/24", addr[0], addr[1], addr[2])
            .parse()
            .unwrap(),
    }
}

async fn eventually(what: &str, ok: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !ok() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `wiki`, and `notes`, which opts out of `.local`, routed on two ports
/// of their own, on a Mac whose LAN address `host` gives, announcing
/// through `proxy`.
async fn local_names_daemon(host: FakeNetwork, proxy: ProxyDeps) -> Daemon {
    use alchemy_core::route::{XAlchemy, XAlchemyBlock};

    let wiki_app = fake_app("wiki").await;
    let notes_app = fake_app("notes").await;
    let connector = FakeConnector::new();
    connector.redirect(wiki_app, wiki_app);
    connector.redirect(notes_app, notes_app);
    let port = free_ports(2);
    let dir = tempdir();
    let (home, config) = apps_home(
        dir.path(),
        vec![
            (
                "wiki",
                vec![service("app", &[], vec![tcp(wiki_app.port(), 3000)])],
            ),
            (
                "notes",
                vec![service("app", &[], vec![tcp(notes_app.port(), 3000)])],
            ),
        ],
    );
    config.set(
        home.join("notes"),
        FakeConfig::Config(ComposeConfig {
            name: "notes".into(),
            services: vec![service("app", &[], vec![tcp(notes_app.port(), 3000)])],
            x_alchemy: Some(XAlchemyBlock::Read(XAlchemy {
                mdns: false,
                ..XAlchemy::default()
            })),
            ..ComposeConfig::default()
        }),
    );
    let mut wiki = running("wiki-app-1", "wiki", "app");
    wiki.ports = vec![tcp(wiki_app.port(), 3000)];
    let mut notes = running("notes-app-1", "notes", "app");
    notes.ports = vec![tcp(notes_app.port(), 3000)];
    Daemon::start_on(
        dir,
        Engine {
            facts: EngineFacts {
                host_os: HostOs::MacOs,
                flavor: Some(EngineFlavor::DockerDesktop),
                rootless: false,
            },
            containers: vec![wiki, notes],
        },
        (home, config),
        FakeEngineNetworks::new(),
        proxy
            .with_connector(Arc::new(connector))
            .with_range(PortRange::new(port, port.saturating_add(1)).unwrap())
            .with_host_listen(Some(SocketAddr::from(([127, 0, 0, 1], 0))))
            .with_interval(Duration::from_millis(50)),
        host,
    )
    .await
}

/// The state of `app`'s address at `url`, if it has one.
fn state_of(app: &App, url: &str) -> Option<AddressState> {
    app.addresses.iter().find(|a| a.url == url).map(|a| a.state)
}

fn local_urls(app: &App) -> Vec<String> {
    app.addresses
        .iter()
        .filter(|a| a.url.contains(".local/") || a.url.contains(".local:"))
        .map(|a| a.url.clone())
        .collect()
}

#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn announces_local_names_and_follows_the_lan_the_setting_and_conflicts() {
    use alchemy_api::network::{MdnsBackend, MdnsState, NetworkSettings};
    use alchemy_core::route::HostLabel;
    use alchemyd::mdns::{Backend, FakeAnnouncer, FakeCall, RecordState};

    let host = FakeNetwork::new();
    host.set_lan_addresses(vec![lan_address("en0", [192, 168, 1, 20])]);
    let announcer = FakeAnnouncer::new(Backend::DnsSd);
    let daemon = local_names_daemon(
        host.clone(),
        ProxyDeps::loopback().with_announcer(Arc::new(announcer.clone())),
    )
    .await;
    let wiki_label = HostLabel::new("wiki").unwrap();

    let wiki = daemon
        .app_where("wiki", |app| {
            app.addresses.len() == 4
                && app.addresses.first().is_some_and(|a| {
                    a.url.starts_with("http://wiki.local:") && a.state == AddressState::Ready
                })
        })
        .await;
    let first = &wiki.addresses[0];
    assert_eq!(first.kind, AddressKind::Host, "{wiki:?}");
    let host_port: u16 = first
        .url
        .strip_prefix("http://wiki.local:")
        .and_then(|rest| rest.strip_suffix('/'))
        .unwrap()
        .parse()
        .unwrap();
    let local = format!("http://wiki.local:{host_port}/");
    let wiki_port: u16 = wiki
        .addresses
        .iter()
        .find_map(|a| {
            a.url
                .strip_prefix("http://127.0.0.1:")
                .and_then(|r| r.strip_suffix('/'))
        })
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        local_urls(&wiki),
        [local.clone(), format!("http://wiki.local:{wiki_port}/")]
    );
    eventually("the service to name the verified Host port", || {
        announcer.published().first().map(|r| r.port) == Some(host_port)
    })
    .await;
    let published = announcer.published();
    assert_eq!(published.len(), 1, "notes opts out: {published:?}");
    assert_eq!(published[0].name, "wiki.local");
    assert_eq!(published[0].interface, "en0");
    assert_eq!(published[0].addr, Ipv4Addr::new(192, 168, 1, 20));
    let notes = daemon
        .app_where("notes", |app| !app.addresses.is_empty())
        .await;
    assert_eq!(local_urls(&notes), Vec::<String>::new());
    let report = daemon
        .report_where(|r| r.mdns.state == MdnsState::Announcing)
        .await;
    assert_eq!(report.mdns.backend, Some(MdnsBackend::DnsSd));
    assert_eq!(report.mdns.message, None);

    announcer.set_state(&wiki_label, RecordState::Registering);
    let wiki = daemon
        .app_where("wiki", |app| {
            state_of(app, &local) == Some(AddressState::Pending)
        })
        .await;
    let pending = wiki.addresses.iter().find(|a| a.url == local).unwrap();
    assert_eq!(
        pending.message.as_deref(),
        Some("Alchemy is announcing wiki.local on your network.")
    );
    assert_eq!(
        wiki.addresses[0].url,
        format!("http://127.0.0.1:{wiki_port}/"),
        "{wiki:#?}"
    );
    announcer.set_state(&wiki_label, RecordState::Established);
    daemon
        .app_where("wiki", |app| {
            state_of(app, &local) == Some(AddressState::Ready)
        })
        .await;

    announcer.take(&wiki_label);
    let wiki = daemon
        .app_where("wiki", |app| !app.problems.is_empty())
        .await;
    assert_eq!(wiki.problems[0].kind, AppProblemKind::Route);
    assert_eq!(
        wiki.problems[0].message,
        "Another device on your network uses wiki.local. Set a hostname in x-alchemy."
    );
    for url in local_urls(&wiki) {
        assert_eq!(
            state_of(&wiki, &url),
            Some(AddressState::Unavailable),
            "{url}"
        );
    }
    assert_eq!(
        wiki.addresses[0].url,
        format!("http://127.0.0.1:{wiki_port}/")
    );
    assert_eq!(wiki.addresses[0].state, AddressState::Ready);
    let report: NetworkReport = get_socket(&daemon.socket, "/api/network").await;
    assert_eq!(
        report.mdns.state,
        MdnsState::Announcing,
        "a conflict is the app's problem"
    );

    announcer.release(&wiki_label);
    host.set_lan_addresses(vec![lan_address("en1", [10, 0, 0, 7])]);
    let wiki = daemon
        .app_where("wiki", |app| {
            app.problems.is_empty() && state_of(app, &local) == Some(AddressState::Ready)
        })
        .await;
    assert_eq!(wiki.addresses[0].url, local, "{wiki:?}");
    assert!(
        !announcer
            .calls()
            .contains(&FakeCall::Withdraw(wiki_label.clone())),
        "a new address replaces the old one in a single announce"
    );
    let published = announcer.published();
    assert_eq!(published.len(), 1);
    assert_eq!(
        (published[0].interface.as_str(), published[0].addr),
        ("en1", Ipv4Addr::new(10, 0, 0, 7))
    );

    let _: NetworkSettings =
        put_socket(&daemon.socket, "/api/settings/network", r#"{"mdns":false}"#).await;
    let wiki = daemon
        .app_where("wiki", |app| {
            !app.addresses.is_empty() && local_urls(app).is_empty()
        })
        .await;
    assert_eq!(wiki.problems, []);
    let report = daemon
        .report_where(|r| r.mdns.state == MdnsState::Off)
        .await;
    assert_eq!(report.mdns.backend, None);
    eventually("wiki.local to be withdrawn", || {
        announcer.published().is_empty()
    })
    .await;
    assert!(
        announcer
            .calls()
            .contains(&FakeCall::Withdraw(wiki_label.clone()))
    );

    let _: NetworkSettings =
        put_socket(&daemon.socket, "/api/settings/network", r#"{"mdns":true}"#).await;
    daemon
        .app_where("wiki", |app| {
            state_of(app, &local) == Some(AddressState::Ready)
        })
        .await;
    assert_eq!(announcer.published().len(), 1);

    daemon.stop().await;
    assert_eq!(announcer.calls().last(), Some(&FakeCall::Shutdown));
    assert_eq!(announcer.published(), []);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_announces_every_local_name_again() {
    use alchemy_core::lifecycle::Woke;
    use alchemy_core::route::HostLabel;
    use alchemyd::mdns::{Backend, FakeAnnouncer, FakeCall};

    let host = FakeNetwork::new();
    host.set_lan_addresses(vec![lan_address("en0", [192, 168, 1, 20])]);
    let announcer = FakeAnnouncer::new(Backend::Builtin);
    let daemon = local_names_daemon(
        host.clone(),
        ProxyDeps::loopback().with_announcer(Arc::new(announcer.clone())),
    )
    .await;
    let wiki = HostLabel::new("wiki").unwrap();
    daemon
        .app_where("wiki", |app| {
            app.addresses
                .iter()
                .any(|a| a.url.starts_with("http://wiki.local:") && a.state == AddressState::Ready)
        })
        .await;
    let before = tokio::time::timeout(TIMEOUT, async {
        loop {
            let calls = announcer.calls().len();
            tokio::time::sleep(Duration::from_millis(200)).await;
            if announcer.published().len() == 1 && announcer.calls().len() == calls {
                return calls;
            }
        }
    })
    .await
    .expect("the announcements never settled");
    let published = announcer.published();

    daemon.state.record_wake(Woke {
        slept_for: Duration::from_secs(3_600),
        at_ms: 1_700_000_000_000,
    });
    eventually("wiki.local to be announced again", || {
        announcer.calls().len() >= before + 2
    })
    .await;
    let calls = announcer.calls();
    assert_eq!(
        calls[before..before + 2],
        [
            FakeCall::Withdraw(wiki.clone()),
            FakeCall::Announce(published[0].clone())
        ],
        "{calls:?}"
    );
    assert_eq!(announcer.published(), published);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(announcer.calls().len(), before + 2, "once per wake");

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_responder_that_cannot_start_leaves_ip_port_addresses_working() {
    use alchemy_api::network::{MdnsBackend, MdnsState};
    use alchemyd::mdns::{AnnounceError, Backend, Responder};

    let host = FakeNetwork::new();
    host.set_lan_addresses(vec![lan_address("eth0", [192, 168, 1, 20])]);
    let daemon = local_names_daemon(
        host,
        ProxyDeps::loopback().with_responder(Responder::Failed(AnnounceError::Responder {
            backend: Backend::Builtin,
            message: "cannot open port 5353".into(),
        })),
    )
    .await;

    let report = daemon
        .report_where(|r| r.mdns.state == MdnsState::Unavailable)
        .await;
    assert_eq!(report.mdns.backend, Some(MdnsBackend::BuiltIn));
    assert_eq!(
        report.mdns.message.as_deref(),
        Some(
            "Alchemy can't start announcing app names on your network (built-in failed: cannot open port 5353). Your apps still work at their IP:port address."
        )
    );
    assert!(report.mdns.fix.is_some());
    let wiki = daemon
        .app_where("wiki", |app| {
            app.addresses.first().is_some_and(|a| {
                a.url.starts_with("http://127.0.0.1:") && a.state == AddressState::Ready
            })
        })
        .await;
    for url in local_urls(&wiki) {
        assert_eq!(state_of(&wiki, &url), Some(AddressState::Pending), "{url}");
    }

    daemon.stop().await;
}
