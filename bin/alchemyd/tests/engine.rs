//! Engine edge wiring: the real `HostProbe`, `DockerCli`, and `DockerApi`
//! implementations against a fake engine socket and a fake `docker` binary.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alchemy_core::engine::{
    ContainerAction, ContainerState, EngineFlavor, EngineState, EngineStatus, Health,
};
use alchemyd::engine::{
    ApiError, BollardApi, CliError, DockerApi, DockerCli, Endpoint, HostProbe, ProcessDockerCli,
    SocketConnect, SystemProbe,
};
use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixListener;

const VERSION_JSON: &str = r#"{"Platform":{"Name":"Docker Desktop 4.34.2 (167172)"},"Version":"27.3.1","ApiVersion":"1.47","MinAPIVersion":"1.24","Os":"linux","Arch":"arm64"}"#;

const CONTAINERS_JSON: &str = r#"[
  {"Id":"abc","Names":["/blog-db-1"],"Image":"postgres:16","State":"running","Status":"Up 2 minutes (healthy)",
   "Labels":{"com.docker.compose.project":"blog","com.docker.compose.service":"db"},
   "Health":{"Status":"healthy","FailingStreak":0}},
  {"Id":"def","Names":["/scratch"],"Image":"alpine","State":"exited","Status":"Exited (0)","Labels":{}}
]"#;

const EVENTS_JSON: &str = concat!(
    r#"{"Type":"network","Action":"connect","Actor":{"ID":"net1","Attributes":{"name":"bridge"}},"time":1}"#,
    "\n",
    r#"{"Type":"container","Action":"start","Actor":{"ID":"abc","Attributes":{"name":"blog-db-1","image":"postgres:16","com.docker.compose.project":"blog","com.docker.compose.service":"db"}},"time":2}"#,
    "\n",
    r#"{"Type":"container","Action":"exec_start: sh","Actor":{"ID":"abc","Attributes":{}},"time":3}"#,
    "\n",
    r#"{"Type":"container","Action":"health_status: unhealthy","Actor":{"ID":"abc","Attributes":{"name":"blog-db-1"}},"time":4}"#,
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
    _dir: tempfile::TempDir,
}

impl FakeEngine {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let log = Arc::clone(&log);
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
                    let head = String::from_utf8_lossy(&buf);
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
                        "/events" => ("200 OK", EVENTS_JSON),
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
    assert_eq!(containers[1].state, ContainerState::Exited);
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
async fn bollard_api_streams_container_events_until_the_connection_ends() {
    let engine = FakeEngine::start();
    let events: Vec<_> = engine.api().events().collect().await;
    let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].id, "abc");
    assert_eq!(events[0].action, ContainerAction::Start);
    assert_eq!(events[0].project.as_deref(), Some("blog"));
    assert_eq!(events[1].action, ContainerAction::Health(Health::Unhealthy));
    let requests = engine.requests();
    assert!(
        unversioned(&requests[0]).starts_with("/events?filters="),
        "{requests:?}"
    );
    assert!(requests[0].contains("container"), "{requests:?}");
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

#[tokio::test]
async fn bollard_api_reports_a_dead_socket_as_transport_error() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker.sock");
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
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
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
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
