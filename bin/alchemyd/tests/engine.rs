//! Engine edge wiring: the real `HostProbe`, `DockerCli`, and `DockerApi`
//! implementations against a fake engine socket and a fake `docker` binary,
//! and the `EngineMonitor` against a scripted host and engine.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::future::{Future, ready};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alchemy_core::engine::{
    ContainerAction, ContainerEvent, ContainerState, ContainerStatus, Containers, EngineFlavor,
    EngineState, EngineStatus, Health, InspectedExit, StopReason, reconnect_delay,
};
use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::{Protocol, PublishedPort};
use alchemyd::engine::{
    AddressPool, ApiError, BollardApi, CliError, Connector, DockerApi, DockerCli, Endpoint,
    EngineInfo, EngineKind, EngineMonitor, EngineSource, EngineVersion, HostEnv, HostProbe,
    NetworkSpec, Os, POLL_INTERVAL, ProcessDockerCli, SocketConnect, SystemProbe, engine_kind,
};
use alchemyd::{Deps, State};
use futures_util::{Stream, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};

const VERSION_JSON: &str = r#"{"Platform":{"Name":"Docker Desktop 4.34.2 (167172)"},"Version":"27.3.1","ApiVersion":"1.47","MinAPIVersion":"1.24","Os":"linux","Arch":"arm64"}"#;

const CONTAINERS_JSON: &str = r#"[
  {"Id":"abc","Names":["/blog-db-1"],"Image":"postgres:16","State":"running","Status":"Up 2 minutes (healthy)",
   "Ports":[{"IP":"0.0.0.0","PrivatePort":5432,"PublicPort":5432,"Type":"tcp"},
            {"IP":"::","PrivatePort":5432,"PublicPort":5432,"Type":"tcp"},{"PrivatePort":8008,"Type":"tcp"}],
   "Labels":{"com.docker.compose.project":"blog","com.docker.compose.service":"db"},
   "Health":{"Status":"healthy","FailingStreak":0},
   "NetworkSettings":{"Networks":{"blog_default":{"IPAddress":"172.18.0.2","NetworkID":"n1"},
                                  "alchemy":{"IPAddress":"10.200.0.3","NetworkID":"n9"}}}},
  {"Id":"def","Names":["/scratch"],"Image":"alpine","State":"exited","Status":"Exited (0)","Labels":{},
   "NetworkSettings":{"Networks":{"bridge":{"IPAddress":"","NetworkID":"n0"}}}}
]"#;

const INSPECT_JSON: &str = r#"{"Id":"def","Name":"/scratch","RestartCount":2,
  "State":{"Status":"exited","Running":false,"OOMKilled":true,"ExitCode":137,
   "StartedAt":"2024-05-01T12:00:00Z","FinishedAt":"2024-05-01T12:00:01.5Z"}}"#;

const NETWORKS_JSON: &str = r#"[
  {"Name":"bridge","Id":"n0","Driver":"bridge","IPAM":{"Driver":"default","Options":null,
   "Config":[{"Subnet":"172.17.0.0/16","Gateway":"172.17.0.1"}]},"Labels":{}},
  {"Name":"blog_default","Id":"n1","Driver":"bridge","IPAM":{"Driver":"default","Options":null,
   "Config":[{"Subnet":"172.18.0.0/16","Gateway":"172.18.0.1"},{"Subnet":"fd00:1::/64"}]},
   "Labels":{"com.docker.compose.network":"default","com.docker.compose.project":"blog"}},
  {"Name":"host","Id":"n2","Driver":"host","IPAM":{"Driver":"default","Options":null,"Config":null},"Labels":{}}
]"#;

const BRIDGE_JSON: &str = r#"{"Name":"bridge","Id":"n0","Driver":"bridge","EnableIPv4":true,
  "IPAM":{"Driver":"default","Options":null,"Config":[{"Subnet":"172.17.0.0/16","Gateway":"172.17.0.1"}]},
  "Options":{"com.docker.network.bridge.default_bridge":"true"},"Labels":{},"Containers":{}}"#;

const INFO_JSON: &str = r#"{"ID":"x","Containers":2,"DefaultAddressPools":[{"Base":"10.10.0.0/16","Size":24},{"Base":"fd00::/56","Size":64}],
  "OperatingSystem":"Docker Desktop","KernelVersion":"6.10.14-linuxkit","SecurityOptions":["name=seccomp,profile=builtin","name=cgroupns"]}"#;

const ALREADY_CONNECTED_JSON: &str =
    r#"{"message":"endpoint with name blog-db-1 already exists in network taken"}"#;

const NETWORK_CREATED_JSON: &str = r#"{"Id":"n9","Warning":""}"#;

const EVENTS_JSON: &str = concat!(
    r#"{"Type":"network","Action":"connect","Actor":{"ID":"net1","Attributes":{"name":"bridge"}},"time":1}"#,
    "\n",
    r#"{"Type":"container","Action":"start","Actor":{"ID":"abc","Attributes":{"name":"blog-db-1","image":"postgres:16","com.docker.compose.project":"blog","com.docker.compose.service":"db"}},"time":2}"#,
    "\n",
    r#"{"Type":"container","Action":"exec_start: sh","Actor":{"ID":"abc","Attributes":{}},"time":3}"#,
    "\n",
    r#"{"Type":"container","Action":"health_status: unhealthy","Actor":{"ID":"abc","Attributes":{"name":"blog-db-1"}},"time":4}"#,
    "\n",
    r#"{"Type":"network","Action":"connect","Actor":{"ID":"n9","Attributes":{"container":"abc","name":"alchemy","type":"bridge"}},"time":5}"#,
    "\n",
);

/// Strip an API version prefix such as `/v1.44`, which the engine treats
/// as optional.
fn unversioned(target: &str) -> &str {
    match target
        .strip_prefix("/v")
        .and_then(|rest| rest.split_once('/'))
    {
        Some((version, rest)) if version.starts_with(|c: char| c.is_ascii_digit()) => {
            &target[target.len() - rest.len() - 1..]
        }
        _ => target,
    }
}

/// A minimal Engine API over a Unix socket: one request per connection,
/// canned responses, and a log of request targets.
struct FakeEngine {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

impl FakeEngine {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let body_log = Arc::clone(&bodies);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let log = Arc::clone(&log);
                let body_log = Arc::clone(&body_log);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < end + length {
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    if length > 0 {
                        body_log
                            .lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&buf[end..]).into_owned());
                    }
                    let target = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    log.lock().unwrap().push(target.clone());
                    let path = unversioned(target.split('?').next().unwrap_or_default());
                    let (status, body) = match path {
                        "/_ping" => ("200 OK", "OK"),
                        "/version" => ("200 OK", VERSION_JSON),
                        "/containers/json" => ("200 OK", CONTAINERS_JSON),
                        "/containers/def/json" => ("200 OK", INSPECT_JSON),
                        "/events" => ("200 OK", EVENTS_JSON),
                        "/networks" => ("200 OK", NETWORKS_JSON),
                        "/networks/bridge" => ("200 OK", BRIDGE_JSON),
                        "/networks/create" => ("201 Created", NETWORK_CREATED_JSON),
                        "/info" => ("200 OK", INFO_JSON),
                        "/networks/alchemy/connect" => ("200 OK", ""),
                        "/networks/taken/connect" => ("403 Forbidden", ALREADY_CONNECTED_JSON),
                        _ => ("404 Not Found", r#"{"message":"page not found"}"#),
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                });
            }
        });
        Self {
            socket,
            requests,
            bodies,
            _dir: dir,
        }
    }

    fn api(&self) -> BollardApi {
        let endpoint = Endpoint::unix(&self.socket).unwrap();
        BollardApi::connect(&endpoint, Duration::from_secs(5)).unwrap()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn bollard_api_pings_and_reports_version() {
    let engine = FakeEngine::start();
    let api = engine.api();
    api.ping().await.unwrap();
    let version = api.version().await.unwrap();
    let mut status = EngineStatus::new(EngineState::Ready);
    version.apply_to(&mut status);
    assert_eq!(status.engine_version.as_deref(), Some("27.3.1"));
    assert_eq!(status.api_version.as_deref(), Some("1.47"));
    assert_eq!(status.flavor, Some(EngineFlavor::DockerDesktop));
    assert_eq!(status.desktop_version.as_deref(), Some("4.34.2"));
    let requests: Vec<_> = engine
        .requests()
        .iter()
        .map(|r| unversioned(r).to_owned())
        .collect();
    assert_eq!(requests, ["/_ping", "/version"]);
}

#[tokio::test]
async fn bollard_api_lists_all_containers() {
    let engine = FakeEngine::start();
    let containers = engine.api().list_containers().await.unwrap();
    assert_eq!(containers.len(), 2);
    let db = &containers[0];
    assert_eq!(
        (db.id.as_str(), db.name.as_str(), db.state, db.health),
        (
            "abc",
            "blog-db-1",
            ContainerState::Running,
            Some(Health::Healthy)
        )
    );
    assert_eq!(db.project.as_deref(), Some("blog"));
    assert_eq!(db.service.as_deref(), Some("db"));
    assert_eq!(
        db.ports,
        [
            PublishedPort::new(Protocol::Tcp, Some("5432".parse().unwrap()))
                .with_target(Some("5432".parse().unwrap()))
        ]
    );
    assert_eq!(
        db.networks,
        BTreeMap::from([
            ("alchemy".to_owned(), "10.200.0.3".parse().unwrap()),
            ("blog_default".to_owned(), "172.18.0.2".parse().unwrap()),
        ])
    );
    assert_eq!(containers[1].state, ContainerState::Exited);
    assert_eq!(containers[1].ports, [] as [PublishedPort; 0]);
    assert!(
        containers[1].networks.is_empty(),
        "no address while stopped"
    );
    assert_eq!(containers[1].health, None);
    let requests = engine.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        unversioned(&requests[0]).starts_with("/containers/json?"),
        "{requests:?}"
    );
    assert!(requests[0].contains("all=true"), "{requests:?}");
}

#[tokio::test]
async fn bollard_api_inspects_how_a_container_exited() {
    let engine = FakeEngine::start();
    let exit = engine.api().inspect_exit("def".into()).await.unwrap();
    assert_eq!(
        exit,
        Some(InspectedExit {
            exit_code: Some(137),
            oom_killed: true,
            started_at: Some(1_714_564_800_000),
            finished_at: Some(1_714_564_801_500),
            restart_count: 2,
        })
    );
    let missing = engine.api().inspect_exit("nope".into()).await.unwrap_err();
    assert!(
        matches!(missing, ApiError::Server { status: 404, .. }),
        "{missing:?}"
    );
    let requests = engine.requests();
    assert_eq!(unversioned(&requests[0]), "/containers/def/json");
}

fn net(cidr: &str) -> Ipv4Net {
    cidr.parse().unwrap()
}

#[tokio::test]
async fn bollard_api_lists_networks() {
    let engine = FakeEngine::start();
    let networks = engine.api().list_networks().await.unwrap();
    let got: Vec<_> = networks
        .iter()
        .map(|n| (n.name.as_str(), n.subnets.clone(), n.project.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("bridge", vec![net("172.17.0.0/16")], None),
            ("blog_default", vec![net("172.18.0.0/16")], Some("blog")),
            ("host", vec![], None),
        ]
    );
    assert_eq!(networks[1].driver.as_deref(), Some("bridge"));
    assert_eq!(
        networks[1]
            .labels
            .get("com.docker.compose.network")
            .map(String::as_str),
        Some("default")
    );
    let requests = engine.requests();
    assert_eq!(unversioned(&requests[0]), "/networks", "{requests:?}");
}

#[tokio::test]
async fn bollard_api_creates_a_bridge_network() {
    let engine = FakeEngine::start();
    let id = engine
        .api()
        .create_network(NetworkSpec {
            name: "alchemy".into(),
            subnet: net("10.200.0.0/24"),
            labels: BTreeMap::from([("com.alchemy.managed".to_owned(), "true".to_owned())]),
        })
        .await
        .unwrap();
    assert_eq!(id, "n9");
    let requests = engine.requests();
    assert_eq!(unversioned(&requests[0]), "/networks/create");
    let body: serde_json::Value = serde_json::from_str(&engine.bodies()[0]).unwrap();
    assert_eq!(body["Name"], "alchemy");
    assert_eq!(body["Driver"], "bridge");
    assert_eq!(body["IPAM"]["Config"][0]["Subnet"], "10.200.0.0/24");
    assert_eq!(body["Labels"]["com.alchemy.managed"], "true");
}

#[tokio::test]
async fn bollard_api_reads_the_bridge_subnet_and_address_pools() {
    let engine = FakeEngine::start();
    let api = engine.api();
    assert_eq!(api.bridge_subnets().await.unwrap(), [net("172.17.0.0/16")]);
    assert_eq!(
        api.address_pools().await.unwrap(),
        [AddressPool {
            base: net("10.10.0.0/16"),
            size: 24
        }]
    );
    let requests: Vec<_> = engine
        .requests()
        .iter()
        .map(|r| unversioned(r).to_owned())
        .collect();
    assert_eq!(requests, ["/networks/bridge", "/info"]);
}

#[tokio::test]
async fn bollard_api_reads_where_the_engine_runs() {
    let engine = FakeEngine::start();
    let api = engine.api();
    let info = api.info().await.unwrap();
    assert_eq!(
        info,
        EngineInfo {
            operating_system: Some("Docker Desktop".into()),
            kernel_version: Some("6.10.14-linuxkit".into()),
            security_options: vec![
                "name=seccomp,profile=builtin".into(),
                "name=cgroupns".into()
            ],
        }
    );
    let version = api.version().await.unwrap();
    assert_eq!(engine_kind(&version, &info, Os::Linux), EngineKind::Desktop);
}

#[tokio::test]
async fn bollard_api_connects_a_container_to_a_network() {
    let engine = FakeEngine::start();
    let api = engine.api();
    api.connect_network("alchemy".into(), "abc".into())
        .await
        .unwrap();
    api.connect_network("taken".into(), "abc".into())
        .await
        .unwrap();
    let missing = api
        .connect_network("missing".into(), "abc".into())
        .await
        .unwrap_err();
    assert!(
        matches!(missing, ApiError::Server { status: 404, .. }),
        "{missing:?}"
    );
    let requests: Vec<_> = engine
        .requests()
        .iter()
        .map(|r| unversioned(r).to_owned())
        .collect();
    assert_eq!(
        requests,
        [
            "/networks/alchemy/connect",
            "/networks/taken/connect",
            "/networks/missing/connect"
        ]
    );
    let body: serde_json::Value = serde_json::from_str(&engine.bodies()[0]).unwrap();
    assert_eq!(body, serde_json::json!({"Container": "abc"}));
}

#[tokio::test]
async fn default_network_calls_report_nothing() {
    let api = ScriptedApi(SharedHost::default());
    assert_eq!(api.list_networks().await.unwrap(), []);
    assert_eq!(api.bridge_subnets().await.unwrap(), []);
    assert_eq!(api.address_pools().await.unwrap(), []);
    let refused = api
        .create_network(NetworkSpec {
            name: "alchemy".into(),
            subnet: net("10.200.0.0/24"),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(refused, ApiError::Server { status: 501, .. }),
        "{refused:?}"
    );
    let refused = api
        .connect_network("alchemy".into(), "abc".into())
        .await
        .unwrap_err();
    assert!(
        matches!(refused, ApiError::Server { status: 501, .. }),
        "{refused:?}"
    );
    assert_eq!(api.info().await.unwrap(), EngineInfo::default());
}

#[tokio::test]
async fn bollard_api_streams_container_events_until_the_connection_ends() {
    let engine = FakeEngine::start();
    let events: Vec<_> = engine.api().events().collect().await;
    let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0].id, "abc");
    assert_eq!(events[0].action, ContainerAction::Start);
    assert_eq!(events[0].project.as_deref(), Some("blog"));
    assert_eq!(events[1].action, ContainerAction::Health(Health::Unhealthy));
    assert_eq!(
        (
            events[2].id.as_str(),
            events[2].action,
            events[2].name.as_deref()
        ),
        ("abc", ContainerAction::NetworkConnect, None)
    );
    let requests = engine.requests();
    assert!(
        unversioned(&requests[0]).starts_with("/events?filters="),
        "{requests:?}"
    );
    assert!(requests[0].contains("container"), "{requests:?}");
    assert!(requests[0].contains("network"), "{requests:?}");
}

#[tokio::test]
async fn bollard_api_maps_server_errors() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let body = r#"{"message":"client version 1.44 is too new"}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let api =
        BollardApi::connect(&Endpoint::unix(&socket).unwrap(), Duration::from_secs(5)).unwrap();
    match api.ping().await {
        Err(ApiError::Server { status, message }) => {
            assert_eq!(status, 400);
            assert!(message.contains("too new"), "{message}");
        }
        other => panic!("expected a server error, got {other:?}"),
    }
}

/// Leave a socket file at `path` that nothing listens on.
///
/// Other tests in this binary spawn processes, and a child forked while the
/// listener is open keeps its descriptor until it execs, so the socket can
/// briefly still accept connections after the drop. Wait until it refuses.
async fn stale_socket(path: &Path) {
    drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::os::unix::net::UnixStream::connect(path).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stale socket kept accepting connections");
}

#[tokio::test]
async fn bollard_api_reports_a_dead_socket_as_transport_error() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker.sock");
    stale_socket(&socket).await;
    let api =
        BollardApi::connect(&Endpoint::unix(&socket).unwrap(), Duration::from_secs(5)).unwrap();
    assert!(matches!(api.ping().await, Err(ApiError::Transport(_))));
    let mut events = api.events();
    assert!(matches!(
        events.next().await,
        Some(Err(ApiError::Transport(_)))
    ));
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn system_probe_classifies_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("live.sock");
    let _listener = UnixListener::bind(&live).unwrap();
    let stale = dir.path().join("stale.sock");
    stale_socket(&stale).await;
    let file = dir.path().join("file");
    std::fs::write(&file, b"").unwrap();
    let missing = dir.path().join("missing.sock");

    let probe = SystemProbe;
    let cases: [(&Path, SocketConnect, bool); 4] = [
        (&live, SocketConnect::Connected, true),
        (&stale, SocketConnect::Refused, true),
        (&file, SocketConnect::Refused, true),
        (&missing, SocketConnect::Missing, false),
    ];
    for (path, connect, exists) in cases {
        assert_eq!(probe.connect(path).await, connect, "{}", path.display());
        assert_eq!(probe.exists(path), exists, "{}", path.display());
    }
}

#[test]
fn system_probe_checks_executables() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("exe");
    let plain = dir.path().join("plain");
    for (path, mode) in [(&exe, 0o755), (&plain, 0o644)] {
        std::fs::write(path, b"").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let probe = SystemProbe;
    assert!(probe.is_executable(&exe));
    assert!(!probe.is_executable(&plain));
    assert!(!probe.is_executable(dir.path()));
    assert!(!probe.is_executable(&dir.path().join("missing")));
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// One test, so every fixture is written before any child is spawned:
/// exec'ing a file another thread still holds open for writing fails with
/// `ETXTBSY` on Linux.
#[tokio::test]
async fn process_docker_cli_runs_the_binary_with_fixed_argv() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let docker = write_script(
        dir.path(),
        "docker",
        &format!(
            r#"echo "$@" >> '{}'
case "$1 $2" in
  "context inspect") echo 'unix:///Users/me/.colima/default/docker.sock' ;;
  "compose version") echo 'v2.29.7' ;;
esac"#,
            log.display()
        ),
    );
    let failing = write_script(dir.path(), "failing", "echo 'boom' >&2\nexit 3");
    let garbage = write_script(dir.path(), "garbage", "echo 'Docker Compose version dev'");
    let slow = write_script(dir.path(), "slow", "exec sleep 10");

    let cli = ProcessDockerCli::new(&docker);
    assert_eq!(cli.binary(), docker);
    assert_eq!(
        cli.context_host().await.unwrap(),
        "unix:///Users/me/.colima/default/docker.sock"
    );
    assert_eq!(cli.compose_version().await.unwrap(), "2.29.7");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "context inspect --format {{.Endpoints.docker.Host}}\ncompose version --short\n"
    );

    match ProcessDockerCli::new(&failing).compose_version().await {
        Err(CliError::Failed { code, stderr }) => {
            assert_eq!(code, Some(3));
            assert_eq!(stderr, "boom");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(matches!(
        ProcessDockerCli::new(&garbage).compose_version().await,
        Err(CliError::UnexpectedOutput { .. })
    ));
    assert!(matches!(
        ProcessDockerCli::new(dir.path().join("missing"))
            .context_host()
            .await,
        Err(CliError::Spawn { .. })
    ));
    let started = std::time::Instant::now();
    let timeout = Duration::from_millis(200);
    assert!(matches!(
        ProcessDockerCli::new(&slow).with_timeout(timeout).context_host().await,
        Err(CliError::Timeout { after }) if after == timeout
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Talks to whatever engine this machine runs. Run with
/// `cargo test -p alchemyd --test engine -- --ignored`.
#[tokio::test]
#[ignore = "needs a running Docker engine"]
async fn real_engine_smoke() {
    use alchemyd::engine::{
        HostEnv, Os, discover_endpoint, docker_binary_candidates, find_docker_binary, probe_socket,
    };
    let env = HostEnv::current();
    let os = Os::current();
    let probe = SystemProbe;
    let candidates = docker_binary_candidates(env.path.as_deref(), env.home.as_deref(), os);
    let cli = find_docker_binary(&probe, &candidates).map(ProcessDockerCli::new);
    let endpoint = discover_endpoint(&env, cli.as_ref(), &probe, os)
        .await
        .unwrap();
    let api = BollardApi::connect(&endpoint, BollardApi::DEFAULT_TIMEOUT).unwrap();
    let socket = probe_socket(&probe, &endpoint, &api).await;
    let version = api.version().await.unwrap();
    let containers = api.list_containers().await.unwrap();
    let compose = match &cli {
        Some(cli) => cli.compose_version().await.ok(),
        None => None,
    };
    eprintln!(
        "{endpoint} {socket:?} {version:?} compose={compose:?} containers={}",
        containers.len()
    );
    assert_eq!(socket, alchemy_core::engine::SocketProbe::Responding);
}

/// What the scripted host and engine look like right now. Tests change it
/// while an [`EngineMonitor`] runs against it.
#[derive(Debug, Default)]
struct Host {
    existing: Vec<PathBuf>,
    executable: Vec<PathBuf>,
    connect: Option<SocketConnect>,
    ping_ok: bool,
    containers: Vec<ContainerStatus>,
    inspected: Vec<(String, InspectedExit)>,
    streams: Vec<mpsc::UnboundedSender<Result<ContainerEvent, ApiError>>>,
    calls: Vec<&'static str>,
}

type SharedHost = Arc<Mutex<Host>>;

const SOCKET: &str = "/run/fake/docker.sock";
const DOCKER: &str = "/usr/bin/docker";

#[derive(Clone)]
struct ScriptedProbe(SharedHost);

impl HostProbe for ScriptedProbe {
    fn exists(&self, path: &Path) -> bool {
        self.0.lock().unwrap().existing.iter().any(|p| p == path)
    }

    fn is_executable(&self, path: &Path) -> bool {
        self.0.lock().unwrap().executable.iter().any(|p| p == path)
    }

    fn connect(&self, _socket: &Path) -> impl Future<Output = SocketConnect> + Send {
        ready(
            self.0
                .lock()
                .unwrap()
                .connect
                .unwrap_or(SocketConnect::Missing),
        )
    }
}

struct ScriptedCli;

impl DockerCli for ScriptedCli {
    fn context_host(&self) -> impl Future<Output = Result<String, CliError>> + Send {
        ready(Err(CliError::Failed {
            code: Some(1),
            stderr: "no context".into(),
        }))
    }

    fn compose_version(&self) -> impl Future<Output = Result<String, CliError>> + Send {
        ready(Ok("2.29.7".into()))
    }
}

struct ScriptedApi(SharedHost);

impl ScriptedApi {
    fn call(&self, name: &'static str) -> std::sync::MutexGuard<'_, Host> {
        let mut host = self.0.lock().unwrap();
        host.calls.push(name);
        host
    }
}

impl DockerApi for ScriptedApi {
    fn ping(&self) -> impl Future<Output = Result<(), ApiError>> + Send {
        let ok = self.call("ping").ping_ok;
        ready(if ok { Ok(()) } else { Err(ApiError::Timeout) })
    }

    fn version(&self) -> impl Future<Output = Result<EngineVersion, ApiError>> + Send {
        drop(self.call("version"));
        ready(Ok(EngineVersion {
            version: Some("27.3.1".into()),
            api_version: Some("1.47".into()),
            platform: Some("Docker Engine - Community".into()),
        }))
    }

    fn list_containers(
        &self,
    ) -> impl Future<Output = Result<Vec<ContainerStatus>, ApiError>> + Send {
        ready(Ok(self.call("list").containers.clone()))
    }

    fn events(
        &self,
    ) -> impl Stream<Item = Result<ContainerEvent, ApiError>> + Send + Unpin + 'static {
        let (tx, rx) = mpsc::unbounded_channel();
        self.call("events").streams.push(tx);
        futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed()
    }

    fn inspect_exit(
        &self,
        id: String,
    ) -> impl Future<Output = Result<Option<InspectedExit>, ApiError>> + Send {
        let host = self.call("inspect");
        let found = host
            .inspected
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, exit)| *exit);
        ready(Ok(found))
    }
}

#[derive(Clone)]
struct ScriptedConnector(SharedHost);

impl Connector for ScriptedConnector {
    type Cli = ScriptedCli;
    type Api = ScriptedApi;

    fn cli(&self, _binary: PathBuf) -> ScriptedCli {
        ScriptedCli
    }

    fn api(&self, endpoint: &Endpoint) -> Result<ScriptedApi, ApiError> {
        let exists = self
            .0
            .lock()
            .unwrap()
            .existing
            .iter()
            .any(|p| p == endpoint.socket());
        if exists {
            Ok(ScriptedApi(Arc::clone(&self.0)))
        } else {
            Err(ApiError::SocketNotFound(endpoint.socket().to_owned()))
        }
    }
}

type ScriptedMonitor = EngineMonitor<ScriptedProbe, ScriptedConnector>;

/// A monitor over a scripted Linux host with `docker` installed and
/// `DOCKER_HOST` set to `docker_host`.
fn scripted_monitor(docker_host: &str) -> (SharedHost, ScriptedMonitor) {
    let host = Arc::new(Mutex::new(Host {
        executable: vec![PathBuf::from(DOCKER)],
        ..Host::default()
    }));
    let env = HostEnv {
        docker_host: Some(docker_host.to_owned()),
        home: Some(PathBuf::from("/home/me")),
        path: Some("/usr/bin".into()),
        ..HostEnv::default()
    };
    let monitor = EngineMonitor::new(
        env,
        Os::Linux,
        ScriptedProbe(Arc::clone(&host)),
        ScriptedConnector(Arc::clone(&host)),
    );
    (host, monitor)
}

/// A host whose engine at [`SOCKET`] is up and answering.
fn ready_host(host: &SharedHost) {
    let mut h = host.lock().unwrap();
    h.existing.push(PathBuf::from(SOCKET));
    h.connect = Some(SocketConnect::Connected);
    h.ping_ok = true;
}

struct Running {
    state: State,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn stop(self) {
        self.stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("monitor did not stop")
            .unwrap();
    }
}

fn spawn_monitor(monitor: impl EngineSource) -> Running {
    let state = State::new();
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(monitor.run(state.clone(), stop_rx));
    Running { state, stop, task }
}

/// Wait for the engine status to satisfy `pred`. Under paused time the
/// timeout is virtual, so it only fires if the monitor stops making
/// progress.
async fn engine_until(state: &State, pred: impl FnMut(&EngineStatus) -> bool) -> EngineStatus {
    let mut rx = state.watch_engine();
    tokio::time::timeout(Duration::from_secs(120), rx.wait_for(pred))
        .await
        .expect("engine status never matched")
        .unwrap()
        .clone()
}

async fn containers_until(state: &State, pred: impl FnMut(&Containers) -> bool) -> Containers {
    let mut rx = state.watch_containers();
    tokio::time::timeout(Duration::from_secs(120), rx.wait_for(pred))
        .await
        .expect("containers never matched")
        .unwrap()
        .clone()
}

fn states(containers: &Containers) -> Vec<(String, ContainerState)> {
    containers.iter().map(|c| (c.id.clone(), c.state)).collect()
}

fn has_state(containers: &Containers, id: &str, state: ContainerState) -> bool {
    containers.get(id).is_some_and(|c| c.state == state)
}

fn send(host: &SharedHost, event: ContainerEvent) {
    let host = host.lock().unwrap();
    host.streams.last().unwrap().send(Ok(event)).unwrap();
}

fn subscriptions(host: &SharedHost) -> usize {
    let host = host.lock().unwrap();
    host.calls.iter().filter(|c| **c == "events").count()
}

fn unix(path: &str) -> String {
    format!("unix://{path}")
}

#[tokio::test(start_paused = true)]
async fn monitor_moves_from_not_running_through_starting_to_ready() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    {
        let mut h = host.lock().unwrap();
        h.existing.push(PathBuf::from(SOCKET));
        h.connect = Some(SocketConnect::Refused);
        h.containers = vec![ContainerStatus::new("abc", "web", ContainerState::Running)];
    }
    let run = spawn_monitor(monitor);

    let status = engine_until(&run.state, |s| s.endpoint.is_some()).await;
    let mut want = EngineStatus::new(EngineState::NotRunning);
    want.endpoint = Some(unix(SOCKET));
    assert_eq!(status, want);

    host.lock().unwrap().connect = Some(SocketConnect::Connected);
    let status = engine_until(&run.state, |s| s.state != EngineState::NotRunning).await;
    assert_eq!(status.state, EngineState::Starting);
    assert!(run.state.containers().is_empty());

    host.lock().unwrap().ping_ok = true;
    let status = engine_until(&run.state, |s| s.state == EngineState::Ready).await;
    assert_eq!(status.endpoint, Some(unix(SOCKET)));
    assert_eq!(status.engine_version.as_deref(), Some("27.3.1"));
    assert_eq!(status.api_version.as_deref(), Some("1.47"));
    assert_eq!(status.compose_version.as_deref(), Some("2.29.7"));
    assert_eq!(status.flavor, Some(EngineFlavor::DockerEngine));

    let containers = containers_until(&run.state, |c| !c.is_empty()).await;
    assert_eq!(
        states(&containers),
        [("abc".to_owned(), ContainerState::Running)]
    );
    let calls = host.lock().unwrap().calls.clone();
    let tail = calls.get(calls.len() - 2..).unwrap();
    assert_eq!(
        tail,
        ["events", "list"],
        "subscribe before listing: {calls:?}"
    );

    send(&host, ContainerEvent::new("abc", ContainerAction::Stop));
    let containers =
        containers_until(&run.state, |c| has_state(c, "abc", ContainerState::Exited)).await;
    assert_eq!(containers.len(), 1);

    run.stop().await;
}

#[tokio::test(start_paused = true)]
async fn monitor_learns_ports_of_containers_started_after_the_listing() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    ready_host(&host);
    host.lock().unwrap().containers =
        vec![ContainerStatus::new("db", "db", ContainerState::Running)];
    let run = spawn_monitor(monitor);
    containers_until(&run.state, |c| !c.is_empty()).await;

    let mut web = ContainerStatus::new("web", "web", ContainerState::Running);
    web.ports = vec![PublishedPort::new(
        Protocol::Tcp,
        Some("8080".parse().unwrap()),
    )];
    host.lock().unwrap().containers.push(web.clone());
    send(&host, ContainerEvent::new("web", ContainerAction::Start));
    let containers = containers_until(&run.state, |c| {
        c.get("web").is_some_and(|w| !w.ports.is_empty())
    })
    .await;
    assert_eq!(containers.get("web").unwrap().ports, web.ports);
    assert_eq!(
        containers.get("db").unwrap().ports,
        [] as [PublishedPort; 0]
    );

    send(&host, ContainerEvent::new("web", ContainerAction::Die));
    let containers =
        containers_until(&run.state, |c| has_state(c, "web", ContainerState::Exited)).await;
    assert_eq!(
        containers.get("web").unwrap().ports,
        [] as [PublishedPort; 0]
    );

    run.stop().await;
}

#[tokio::test(start_paused = true)]
async fn monitor_learns_addresses_of_containers_connected_after_the_listing() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    ready_host(&host);
    host.lock().unwrap().containers =
        vec![ContainerStatus::new("web", "web", ContainerState::Running)];
    let run = spawn_monitor(monitor);
    containers_until(&run.state, |c| !c.is_empty()).await;
    assert_eq!(run.state.engine_kind(), Some(EngineKind::Native));

    let mut web = ContainerStatus::new("web", "web", ContainerState::Running);
    web.networks
        .insert("alchemy".to_owned(), "10.200.0.3".parse().unwrap());
    host.lock().unwrap().containers = vec![web.clone()];
    send(
        &host,
        ContainerEvent::new("web", ContainerAction::NetworkConnect),
    );
    let containers = containers_until(&run.state, |c| {
        c.get("web").is_some_and(|w| !w.networks.is_empty())
    })
    .await;
    assert_eq!(containers.get("web").unwrap().networks, web.networks);

    host.lock().unwrap().containers =
        vec![ContainerStatus::new("web", "web", ContainerState::Running)];
    send(
        &host,
        ContainerEvent::new("web", ContainerAction::NetworkDisconnect),
    );
    containers_until(&run.state, |c| {
        c.get("web").is_some_and(|w| w.networks.is_empty())
    })
    .await;

    {
        let mut h = host.lock().unwrap();
        h.ping_ok = false;
        h.streams.clear();
    }
    engine_until(&run.state, |s| s.state != EngineState::Ready).await;
    assert_eq!(run.state.engine_kind(), None, "unknown while not ready");

    run.stop().await;
}

#[tokio::test(start_paused = true)]
async fn monitor_reconnects_and_resyncs_when_the_stream_drops() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    ready_host(&host);
    host.lock().unwrap().containers =
        vec![ContainerStatus::new("abc", "web", ContainerState::Running)];
    let run = spawn_monitor(monitor);
    containers_until(&run.state, |c| !c.is_empty()).await;
    let mut engine_rx = run.state.watch_engine();
    assert_eq!(engine_rx.borrow_and_update().state, EngineState::Ready);

    {
        let mut h = host.lock().unwrap();
        h.containers = vec![ContainerStatus::new("def", "db", ContainerState::Running)];
        h.streams.clear();
    }
    let dropped = tokio::time::Instant::now();
    let containers = containers_until(&run.state, |c| c.get("def").is_some()).await;
    assert_eq!(
        states(&containers),
        [("def".to_owned(), ContainerState::Running)],
        "the listing replaces containers missed while disconnected"
    );
    assert_eq!(dropped.elapsed(), reconnect_delay(0));
    assert_eq!(subscriptions(&host), 2);
    assert!(
        !engine_rx.has_changed().unwrap(),
        "reconnecting to the same engine does not republish its status"
    );

    send(&host, ContainerEvent::new("def", ContainerAction::Pause));
    containers_until(&run.state, |c| has_state(c, "def", ContainerState::Paused)).await;

    {
        let mut h = host.lock().unwrap();
        h.connect = Some(SocketConnect::Refused);
        h.streams.clear();
    }
    let status = engine_until(&run.state, |s| s.state != EngineState::Ready).await;
    assert_eq!(status.state, EngineState::NotRunning);
    assert!(run.state.containers().is_empty());
    assert_eq!(
        status.flavor,
        Some(EngineFlavor::DockerEngine),
        "keeps the flavor the endpoint reported while ready"
    );

    run.stop().await;
}

#[tokio::test(start_paused = true)]
async fn monitor_reports_docker_desktop_while_an_installed_desktop_is_down() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    {
        let mut h = host.lock().unwrap();
        h.existing.push(PathBuf::from("/opt/docker-desktop"));
        h.existing.push(PathBuf::from(SOCKET));
        h.connect = Some(SocketConnect::Refused);
    }
    let run = spawn_monitor(monitor);

    let status = engine_until(&run.state, |s| s.endpoint.is_some()).await;
    let mut want = EngineStatus::new(EngineState::NotRunning);
    want.flavor = Some(EngineFlavor::DockerDesktop);
    want.endpoint = Some(unix(SOCKET));
    assert_eq!(status, want);

    host.lock().unwrap().connect = Some(SocketConnect::Connected);
    let status = engine_until(&run.state, |s| s.state != EngineState::NotRunning).await;
    assert_eq!(status.state, EngineState::Starting);
    assert_eq!(status.flavor, Some(EngineFlavor::DockerDesktop));

    run.stop().await;
}

#[tokio::test(start_paused = true)]
async fn monitor_backs_off_while_streams_keep_failing() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    ready_host(&host);
    let run = spawn_monitor(monitor);
    engine_until(&run.state, |s| s.state == EngineState::Ready).await;

    let mut gaps = Vec::new();
    let mut last = tokio::time::Instant::now();
    for want in 2..=5 {
        while subscriptions(&host) < want {
            for tx in host.lock().unwrap().streams.drain(..) {
                let _ = tx.send(Err(ApiError::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let now = tokio::time::Instant::now();
        gaps.push((now - last).as_secs());
        last = now;
    }
    assert_eq!(gaps, [1, 2, 4, 8]);

    run.stop().await;
}

#[tokio::test(start_paused = true)]
async fn monitor_reports_an_unsupported_endpoint_without_probing_it() {
    let tcp = "tcp://nas:2375";
    let (host, monitor) = scripted_monitor(tcp);
    let run = spawn_monitor(monitor);
    let status = engine_until(&run.state, |s| s.endpoint.is_some()).await;
    let mut want = EngineStatus::new(EngineState::Unsupported);
    want.endpoint = Some(tcp.to_owned());
    assert_eq!(status, want);
    tokio::time::sleep(POLL_INTERVAL * 3).await;
    let calls = host.lock().unwrap().calls.clone();
    assert_eq!(calls, Vec::<&str>::new(), "never probes a socket");
    run.stop().await;
}

#[tokio::test(start_paused = true)]
async fn monitor_reports_not_installed_without_cli_or_desktop() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    host.lock().unwrap().executable.clear();
    let run = spawn_monitor(monitor);
    let status = engine_until(&run.state, |s| s.endpoint.is_some()).await;
    assert_eq!(status.state, EngineState::NotInstalled);

    host.lock().unwrap().executable.push(PathBuf::from(DOCKER));
    let status = engine_until(&run.state, |s| s.state != EngineState::NotInstalled).await;
    assert_eq!(status.state, EngineState::NotRunning);
    run.stop().await;
}

/// Publishes a fixed status, the way front-door tests can fake the engine.
struct FixedEngine(EngineStatus);

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(self.0);
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

async fn run_daemon_until(deps: Deps<impl EngineSource>, pred: impl FnMut(&EngineStatus) -> bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = alchemyd::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        socket: dir.path().join("alchemyd.sock"),
        state_dir: dir.path().join("state"),
    };
    let state = deps.state.clone();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(config.clone(), deps, async {
        let _ = stop_rx.await;
    }));
    engine_until(&state, pred).await;
    assert!(config.socket.exists());
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), daemon)
        .await
        .expect("daemon did not shut down")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn run_with_runs_the_engine_monitor_until_shutdown() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    ready_host(&host);
    run_daemon_until(Deps::new(monitor), |s| s.state == EngineState::Ready).await;
}

#[tokio::test]
async fn run_with_accepts_any_engine_source() {
    let engine = FixedEngine(EngineStatus::new(EngineState::Starting));
    run_daemon_until(Deps::new(engine), |s| s.state == EngineState::Starting).await;
}

#[tokio::test]
async fn run_with_starts_without_docker() {
    let (_host, monitor) = scripted_monitor(&unix(SOCKET));
    run_daemon_until(Deps::new(monitor), |s| {
        s.state == EngineState::NotRunning && s.endpoint.is_some()
    })
    .await;
}

fn in_blog(id: &str, state: ContainerState) -> ContainerStatus {
    let mut status = ContainerStatus::new(id, id, state);
    status.project = Some("blog".into());
    status
}

fn reason_of(containers: &Containers, id: &str) -> Option<StopReason> {
    containers.get(id).map(|c| c.stop_reason)
}

#[tokio::test(start_paused = true)]
async fn monitor_inspects_stopped_containers_and_tells_stops_from_crashes() {
    let (host, monitor) = scripted_monitor(&unix(SOCKET));
    ready_host(&host);
    {
        let mut h = host.lock().unwrap();
        h.containers = vec![
            in_blog("old", ContainerState::Exited),
            in_blog("web", ContainerState::Running),
            in_blog("db", ContainerState::Running),
            ContainerStatus::new("loose", "loose", ContainerState::Exited),
        ];
        let crashed = InspectedExit {
            exit_code: Some(1),
            ..InspectedExit::default()
        };
        h.inspected = vec![("old".into(), crashed), ("loose".into(), crashed)];
    }
    let run = spawn_monitor(monitor);
    let crashed = StopReason::Crashed {
        exit_code: Some(1),
        oom: false,
    };
    containers_until(&run.state, |c| reason_of(c, "old") == Some(crashed)).await;
    let inspects = host
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| **c == "inspect")
        .count();
    assert_eq!(inspects, 1, "only stopped Compose containers are inspected");
    assert_eq!(
        reason_of(&run.state.containers(), "loose"),
        Some(StopReason::Unknown)
    );

    let at = |action, ms| {
        let mut event = ContainerEvent::new("web", action);
        event.at = Some(ms);
        event
    };
    let mut kill = at(ContainerAction::Kill, 1_000);
    kill.signal = Some(15);
    let mut die = at(ContainerAction::Die, 1_100);
    die.exit_code = Some(0);
    for event in [kill, die, at(ContainerAction::Stop, 1_100)] {
        send(&host, event);
    }
    containers_until(&run.state, |c| {
        reason_of(c, "web") == Some(StopReason::StoppedOutside)
    })
    .await;

    let mut die = ContainerEvent::new("db", ContainerAction::Die);
    die.exit_code = Some(137);
    send(&host, ContainerEvent::new("db", ContainerAction::Oom));
    send(&host, die);
    let containers =
        containers_until(&run.state, |c| has_state(c, "db", ContainerState::Exited)).await;
    assert_eq!(
        reason_of(&containers, "db"),
        Some(StopReason::Crashed {
            exit_code: Some(137),
            oom: true
        })
    );

    run.stop().await;
}
