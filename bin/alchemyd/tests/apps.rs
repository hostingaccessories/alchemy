//! Apps wiring: `run_with` finding stacks in real folders, matching them to
//! containers, and serving them and their settings over the API socket,
//! without changing any file in the folders but staging folders a stopped
//! daemon left behind.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alchemy_api::apps::{AppDirState, AppList, SkipReason, StackStatus};
use alchemy_core::engine::{ContainerState, ContainerStatus, EngineState, EngineStatus};
use alchemyd::apps::{AppsDeps, SystemDisk, SystemFs};
use alchemyd::compose::FakeComposeConfig;
use alchemyd::engine::EngineSource;
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

const TIMEOUT: Duration = Duration::from_secs(10);

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

/// Send one request on the socket and return the status and JSON body.
async fn call(socket: &Path, method: &str, uri: &str, body: &str) -> (u16, serde_json::Value) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut stream = loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => break stream,
            Err(err) if tokio::time::Instant::now() > deadline => panic!("connect: {err}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let content_type = if method == "GET" {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let request = format!(
        "{method} {uri} HTTP/1.1\r\nHost: alchemyd\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut seen))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

async fn apps_until(socket: &Path, what: &str, done: impl Fn(&AppList) -> bool) -> AppList {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (status, body) = call(socket, "GET", "/api/apps", "").await;
        assert_eq!(status, 200, "{body}");
        let apps: AppList = serde_json::from_value(body).unwrap();
        if done(&apps) {
            return apps;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {apps:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Every path under `dir` with its size and modification time.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut seen = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            if meta.is_dir() {
                pending.push(entry.path());
            }
            seen.push((entry.path(), meta.len(), meta.modified().unwrap()));
        }
    }
    seen.sort();
    seen
}

fn container(id: &str, project: &str, service: &str, working_dir: &str) -> ContainerStatus {
    let mut c = ContainerStatus::new(
        id,
        format!("{project}-{service}-1"),
        ContainerState::Running,
    );
    c.project = Some(project.into());
    c.service = Some(service.into());
    c.working_dir = Some(working_dir.into());
    c
}

#[tokio::test]
async fn finds_stacks_in_the_configured_folders_and_changes_nothing() {
    let dir = tempfile::Builder::new()
        .prefix("aa")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home/Alchemy/apps");
    let stacks = root.join("stacks");
    for (path, contents) in [
        (
            "stacks/blog/compose.yaml",
            "services:\n  web:\n    image: nginx\n",
        ),
        ("stacks/blog/.env", "SECRET=1\n"),
        ("stacks/My App/compose.yaml", "services: {}\n"),
        ("stacks/notes/README.md", "hi\n"),
    ] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    let before = snapshot(&stacks);
    test_stacks(&root, &home, &stacks).await;
    assert_eq!(
        snapshot(&stacks),
        before,
        "discovery changed the app folder"
    );
    assert!(!home.exists(), "discovery created the home");
}

/// Run a daemon over `stacks` with `home` as the default home, check what
/// it serves, and shut it down.
async fn test_stacks(root: &Path, home: &Path, stacks: &Path) {
    let config = FakeComposeConfig::new();
    config.project(stacks.join("blog"), "blog", &["web", "db"]);
    let socket = root.join("alchemyd.sock");
    let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready))).with_apps(
        AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home.to_owned()))
            .with_scan_interval(Duration::from_millis(50)),
    );
    let state = deps.state.clone();
    let (stop, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen: free_port(),
            socket: socket.clone(),
            state_dir: root.join("state"),
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));

    let apps = apps_until(&socket, "the home listed", |a| !a.dirs.is_empty()).await;
    assert_eq!(apps.dirs.len(), 1);
    assert!(apps.dirs[0].home);
    assert_eq!(apps.dirs[0].state, AppDirState::Missing);
    assert!(!home.exists(), "listing created the home");

    let (status, settings) = call(&socket, "GET", "/api/settings/apps", "").await;
    assert_eq!(status, 200);
    assert_eq!(settings["home"], home.to_str().unwrap());
    assert_eq!(settings["default_home"], home.to_str().unwrap());

    let update = serde_json::json!({ "extra": [stacks] }).to_string();
    let (status, settings) = call(&socket, "PUT", "/api/settings/apps", &update).await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings["extra"], serde_json::json!([stacks]));

    state.update_containers(|c| {
        c.replace_all([
            container("1", "blog", "web", stacks.join("blog").to_str().unwrap()),
            container("2", "legacy", "app", "/home/me/legacy"),
        ])
    });
    let apps = apps_until(&socket, "blog matched", |a| {
        a.apps
            .first()
            .is_some_and(|a| a.status == StackStatus::Partial)
    })
    .await;
    assert_eq!(apps.apps.len(), 1);
    let blog = &apps.apps[0];
    assert_eq!(blog.name.as_str(), "blog");
    assert_eq!(blog.dir, stacks.join("blog").to_str().unwrap());
    assert_eq!(blog.project.as_deref(), Some("blog"));
    let services: Vec<_> = blog.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(services, ["web", "db"]);
    assert_eq!(apps.unmanaged.len(), 1);
    assert_eq!(apps.unmanaged[0].project, "legacy");
    assert_eq!(apps.unmanaged[0].dir.as_deref(), Some("/home/me/legacy"));
    assert_eq!(apps.dirs.len(), 2);
    let skipped: Vec<_> = apps.dirs[1]
        .skipped
        .iter()
        .map(|s| (s.name.as_str(), s.reason))
        .collect();
    assert_eq!(skipped, [("My App", SkipReason::InvalidName)]);

    let (status, app) = call(&socket, "GET", "/api/apps/blog", "").await;
    assert_eq!(status, 200);
    assert_eq!(app["status"], "partial");
    let (status, err) = call(&socket, "GET", "/api/apps/nope", "").await;
    assert_eq!(status, 404);
    assert_eq!(err["code"], "app_not_found");

    let (status, err) = call(&socket, "PUT", "/api/settings/apps", r#"{"home":"rel"}"#).await;
    assert_eq!(status, 422);
    assert_eq!(err["code"], "invalid");

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .expect("daemon did not shut down")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn settings_survive_a_restart() {
    let dir = tempfile::Builder::new()
        .prefix("ab")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let socket = root.join("alchemyd.sock");
    let config = || alchemyd::Config {
        listen: free_port(),
        socket: socket.clone(),
        state_dir: root.join("state"),
    };
    for (round, body) in [
        (0, Some(r#"{"home":"/srv/apps","extra":["/opt/stacks"]}"#)),
        (1, None),
    ] {
        let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready)));
        let (stop, stop_rx) = oneshot::channel::<()>();
        let daemon = tokio::spawn(alchemyd::run_with(config(), deps, async {
            let _ = stop_rx.await;
        }));
        if let Some(body) = body {
            let (status, got) = call(&socket, "PUT", "/api/settings/apps", body).await;
            assert_eq!(status, 200, "{got}");
        }
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let (_, got) = call(&socket, "GET", "/api/settings/apps", "").await;
            if got["home"] == "/srv/apps" {
                assert_eq!(
                    got["extra"],
                    serde_json::json!(["/opt/stacks"]),
                    "round {round}"
                );
                assert_eq!(got["default_home"], serde_json::Value::Null);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "round {round}: {got}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, daemon)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn removes_staging_folders_left_by_a_crash_and_nothing_else() {
    let dir = tempfile::Builder::new()
        .prefix("ac")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("apps");
    let two_hours_ago = SystemTime::now() - Duration::from_hours(2);
    std::fs::create_dir_all(home.join("blog")).unwrap();
    std::fs::write(home.join("blog/compose.yaml"), "services: {}\n").unwrap();
    let create = home.join(".alchemy-tmp-shop-a1B2c3");
    let edit = home.join("blog/.alchemy-tmp-blog-d4E5f6");
    for left in [&create.join("shop"), &edit] {
        std::fs::create_dir_all(left).unwrap();
        std::fs::write(left.join("compose.yaml"), "services: {}\n").unwrap();
    }
    for left in [&create, &edit] {
        std::fs::File::open(left)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();
    }
    std::os::unix::fs::symlink(home.join("blog"), home.join(".alchemy-tmp-blog-g7H8i9")).unwrap();
    std::fs::create_dir(home.join(".alchemy-tmp-blog-j1K2l3")).unwrap();
    let kept: Vec<_> = snapshot(&home)
        .into_iter()
        .filter(|(path, _, _)| !path.starts_with(&create) && !path.starts_with(&edit))
        .map(|(path, _, _)| path)
        .collect();

    let socket = root.join("alchemyd.sock");
    let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready))).with_apps(
        AppsDeps::new(
            Arc::new(SystemFs),
            Arc::new(FakeComposeConfig::new()),
            Some(home.clone()),
        )
        .with_scan_interval(Duration::from_millis(50))
        .with_disk(Arc::new(SystemDisk)),
    );
    let (stop, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(
        alchemyd::Config {
            listen: free_port(),
            socket: socket.clone(),
            state_dir: root.join("state"),
        },
        deps,
        async {
            let _ = stop_rx.await;
        },
    ));

    let apps = apps_until(&socket, "blog found", |a| a.apps.len() == 1).await;
    assert_eq!(apps.apps[0].name.as_str(), "blog");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while create.exists() || edit.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "staging folders kept"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let now: Vec<_> = snapshot(&home)
        .into_iter()
        .map(|(path, _, _)| path)
        .collect();
    assert_eq!(now, kept);
    assert_eq!(
        std::fs::read(home.join("blog/compose.yaml")).unwrap(),
        b"services: {}\n"
    );

    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .expect("daemon did not shut down")
        .unwrap()
        .unwrap();
}
