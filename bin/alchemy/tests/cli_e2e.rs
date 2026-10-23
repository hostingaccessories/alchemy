//! End-to-end: a real alchemyd on a tempdir socket, with a fake engine,
//! driven through the CLI's public API.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use alchemy::commands::status;
use alchemy::{Cli, ClientError, Command, DAEMON_ENV};
use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};
use alchemy_ipc::{
    DEFAULT_MAX_FRAME, ErrorCode, FrameDecoder, PROTOCOL_VERSION, Request, Response, read_frame,
    write_message,
};
use alchemyd::engine::EngineSource;
use alchemyd::{Config, Deps, State};
use clap::Parser;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const DEADLINE: Duration = Duration::from_secs(10);

struct FixedEngine(EngineStatus);

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(self.0);
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

struct Daemon {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    state: State,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), String>>,
}

impl Daemon {
    async fn start(engine: EngineStatus) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("alchemyd.sock");
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: socket.clone(),
        };
        let deps = Deps::new(FixedEngine(engine.clone()));
        let state = deps.state.clone();
        let (stop, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            alchemyd::run_with(config, deps, async {
                let _ = stop_rx.await;
            })
            .await
            .map_err(|e| format!("{e:#}"))
        });
        let mut engines = state.watch_engine();
        tokio::time::timeout(DEADLINE, engines.wait_for(|s| *s == engine))
            .await
            .unwrap()
            .unwrap();
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self {
            _dir: dir,
            socket,
            state,
            stop,
            task,
        }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn cli(socket: &Path, json: bool, watch: bool) -> Cli {
    Cli {
        json,
        daemon: None,
        socket: Some(socket.to_owned()),
        command: Command::Status(status::Args { watch }),
    }
}

async fn run(cli: Cli) -> Result<String, ClientError> {
    let mut out = Vec::new();
    alchemy::run(cli, &mut out).await?;
    Ok(String::from_utf8(out).unwrap())
}

/// Forwards each complete output line to a channel, so a test can follow
/// `--watch` while it runs.
struct LineSink {
    buf: Vec<u8>,
    lines: mpsc::UnboundedSender<String>,
}

impl Write for LineSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            let line = String::from_utf8(line).unwrap();
            let _ = self.lines.send(line.trim_end().to_owned());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Watch {
    lines: mpsc::UnboundedReceiver<String>,
    task: JoinHandle<Result<(), ClientError>>,
}

impl Watch {
    fn start(cli: Cli) -> Self {
        let (tx, lines) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut sink = LineSink {
                buf: Vec::new(),
                lines: tx,
            };
            alchemy::run(cli, &mut sink).await
        });
        Self { lines, task }
    }

    async fn line(&mut self) -> String {
        tokio::time::timeout(DEADLINE, self.lines.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn finish(self) -> Result<(), ClientError> {
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .unwrap()
            .unwrap()
    }
}

fn ready_desktop() -> EngineStatus {
    let mut status = EngineStatus::new(EngineState::Ready);
    status.flavor = Some(EngineFlavor::DockerDesktop);
    status.desktop_version = Some("4.34.2".into());
    status.engine_version = Some("27.3.1".into());
    status.api_version = Some("1.47".into());
    status.compose_version = Some("2.29.7".into());
    status.endpoint = Some("unix:///var/run/docker.sock".into());
    status
}

fn daemon_line(socket: &Path) -> String {
    format!(
        "alchemyd {} (protocol {PROTOCOL_VERSION}) at {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_round_trips_through_a_real_daemon() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Starting)).await;
    assert_eq!(
        alchemy::exchange_at(&daemon.socket, &Request::Ping)
            .await
            .unwrap(),
        Response::Pong
    );
    assert_eq!(
        alchemy::exchange_at(&daemon.socket, &Request::Engine)
            .await
            .unwrap(),
        Response::Engine(EngineStatus::new(EngineState::Starting))
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_the_daemon_and_docker() {
    let daemon = Daemon::start(ready_desktop()).await;
    let out = run(cli(&daemon.socket, false, false)).await.unwrap();
    assert_eq!(
        out,
        format!(
            "{}\nDocker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7\n",
            daemon_line(&daemon.socket)
        )
    );

    daemon
        .state
        .set_engine(EngineStatus::new(EngineState::NotInstalled));
    let out = run(cli(&daemon.socket, false, false)).await.unwrap();
    assert_eq!(
        out.lines().nth(1),
        Some("Docker isn't installed: install Docker Desktop or Docker Engine")
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_json_adds_the_engine() {
    let daemon = Daemon::start(ready_desktop()).await;
    let out = run(cli(&daemon.socket, true, false)).await.unwrap();
    assert_eq!(out.lines().count(), 1, "{out}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["daemon"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["protocol"], PROTOCOL_VERSION);
    assert_eq!(json["socket"], daemon.socket.to_str().unwrap());
    assert_eq!(
        json["engine"],
        serde_json::to_value(ready_desktop()).unwrap()
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_watch_prints_a_line_per_change_until_the_daemon_stops() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::NotRunning)).await;
    let mut watch = Watch::start(cli(&daemon.socket, false, true));
    assert_eq!(watch.line().await, daemon_line(&daemon.socket));
    assert_eq!(watch.line().await, "Docker isn't running");

    daemon
        .state
        .set_engine(EngineStatus::new(EngineState::Starting));
    assert_eq!(watch.line().await, "Docker is starting…");

    let mut api_only = EngineStatus::new(EngineState::Starting);
    api_only.api_version = Some("1.47".into());
    daemon.state.set_engine(api_only);
    daemon.state.set_engine(ready_desktop());
    assert_eq!(
        watch.line().await,
        "Docker ready: Desktop 4.34.2, Engine 27.3.1, Compose v2.29.7"
    );

    let mut unsupported = EngineStatus::new(EngineState::Unsupported);
    unsupported.endpoint = Some("tcp://10.0.0.5:2375".into());
    daemon.state.set_engine(unsupported);
    assert_eq!(
        watch.line().await,
        "Docker endpoint tcp://10.0.0.5:2375 isn't supported: Alchemy needs a unix:// socket"
    );

    daemon.stop().await;
    let err = watch.finish().await.unwrap_err();
    assert!(matches!(err, ClientError::Disconnected), "{err:?}");
    assert_eq!(err.exit_code(), 69);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_watch_json_prints_one_object_per_change() {
    let daemon = Daemon::start(EngineStatus::new(EngineState::Starting)).await;
    let mut watch = Watch::start(cli(&daemon.socket, true, true));
    let first: serde_json::Value = serde_json::from_str(&watch.line().await).unwrap();
    assert_eq!(first["daemon"], env!("CARGO_PKG_VERSION"));
    assert_eq!(first["engine"]["state"], "starting");

    daemon.state.set_engine(ready_desktop());
    let next: serde_json::Value = serde_json::from_str(&watch.line().await).unwrap();
    assert_eq!(next["socket"], daemon.socket.to_str().unwrap());
    assert_eq!(
        next["engine"],
        serde_json::to_value(ready_desktop()).unwrap()
    );

    watch.task.abort();
    daemon.stop().await;
}

/// Answers like an alchemyd from before the engine requests existed: it
/// knows `ping` and `version`, and rejects anything else as a bad request.
fn old_daemon(socket: &Path) -> JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut decoder = FrameDecoder::new();
                while let Ok(Some(frame)) = read_frame(&mut stream, &mut decoder).await {
                    let req: serde_json::Value = serde_json::from_slice(&frame).unwrap();
                    let (resp, keep_open) = match req["type"].as_str() {
                        Some("ping") => (Response::Pong, true),
                        Some("version") => (
                            Response::Version {
                                daemon: "0.0.1".into(),
                                protocol: 1,
                            },
                            true,
                        ),
                        _ => (
                            Response::Error {
                                code: ErrorCode::BadRequest,
                                message: "unsupported or malformed request".into(),
                            },
                            false,
                        ),
                    };
                    write_message(&mut stream, &resp, DEFAULT_MAX_FRAME)
                        .await
                        .unwrap();
                    if !keep_open {
                        return;
                    }
                }
            });
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_daemon_is_reported_as_too_old() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("old.sock");
    let server = old_daemon(&socket);
    for watch in [false, true] {
        let err = run(cli(&socket, false, watch)).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::DaemonTooOld { daemon, command: "status" } if daemon == "0.0.1"),
            "{err:?}"
        );
        assert!(err.to_string().contains("too old"), "{err}");
        assert_eq!(err.exit_code(), 76);
    }
    server.abort();
}

#[tokio::test]
async fn missing_daemon_is_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    for watch in [false, true] {
        let err = run(cli(&tmp.path().join("nope.sock"), false, watch))
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");
        assert_eq!(err.exit_code(), 69);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_unix_daemon_url_reaches_the_same_socket_as_socket() {
    let daemon = Daemon::start(ready_desktop()).await;
    let url = format!("unix://{}", daemon.socket.display());
    let cli = Cli::try_parse_from(["alchemy", "--daemon", &url, "status", "--json"]).unwrap();
    let json: serde_json::Value = serde_json::from_str(&run(cli).await.unwrap()).unwrap();
    assert_eq!(json["socket"], daemon.socket.to_str().unwrap());
    daemon.stop().await;
}

#[test]
fn daemon_and_socket_flags_together_are_a_usage_error() {
    let err = Cli::try_parse_from([
        "alchemy",
        "--daemon",
        "unix:///a.sock",
        "--socket",
        "/b.sock",
        "status",
    ])
    .unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

/// Runs the real `alchemy` binary with `args` and `env`, and without the
/// caller's own daemon variables.
async fn alchemy_bin(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_alchemy"));
    cmd.args(args)
        .env_remove(DAEMON_ENV)
        .env_remove(alchemy_ipc::SOCKET_ENV)
        .envs(env.iter().copied());
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_remote_daemon_is_refused_until_auth_lands() {
    let flag = alchemy_bin(&["--daemon", "https://x", "status"], &[]).await;
    let env = alchemy_bin(&["status"], &[(DAEMON_ENV, "https://x")]).await;
    for (out, origin) in [(flag, "--daemon"), (env, DAEMON_ENV)] {
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
        assert!(
            stderr.starts_with(&format!(
                "alchemy: cannot use a https:// daemon from {origin}: remote daemons need authentication, which arrives in M13"
            )),
            "{stderr}"
        );
        assert!(stderr.contains("issues/16"), "{stderr}");
        assert!(stderr.contains("\nhint: "), "{stderr}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_flag_beats_the_daemon_env() {
    let daemon = Daemon::start(ready_desktop()).await;
    let socket = daemon.socket.to_str().unwrap().to_owned();
    let out = alchemy_bin(
        &["--socket", &socket, "status"],
        &[(DAEMON_ENV, "https://x")],
    )
    .await;
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stdout}");
    assert_eq!(stdout.lines().next().unwrap(), daemon_line(&daemon.socket));
    daemon.stop().await;
}
