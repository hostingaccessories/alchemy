//! App file wiring: `run_with` creating an app in a real home and editing
//! its files over the API socket, with a stub validator in place of
//! Compose, and starting it in the same request with a fake runner.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alchemy_api::apps::{AppFile, AppList};
use alchemy_core::engine::{EngineState, EngineStatus};
use alchemyd::apps::{AppEditor, AppsDeps, SystemDisk, SystemFs, content_hash};
use alchemyd::compose::{
    ComposeAction, ComposeConfig, FakeComposeConfig, FakeComposeRunner, FakeComposeValidator,
    FakeConfig, FakeOutcome, FakeScript,
};
use alchemyd::engine::EngineSource;
use alchemyd::{Deps, State};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

const TIMEOUT: Duration = Duration::from_secs(10);
const COMPOSE: &str = "services:\n  web:\n    image: nginx\n";

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

struct Reply {
    status: u16,
    head: String,
    body: serde_json::Value,
}

/// Send one request on the socket and return the status, headers, and
/// JSON body.
async fn call(socket: &Path, method: &str, uri: &str, body: &str) -> Reply {
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
    let _ = stream.write_all(request.as_bytes()).await;
    let mut seen = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_string(&mut seen))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = seen.split_once("\r\n\r\n").unwrap();
    Reply {
        status: head.split(' ').nth(1).unwrap().parse().unwrap(),
        head: head.to_ascii_lowercase(),
        body: serde_json::from_str(body).unwrap(),
    }
}

async fn apps_until(socket: &Path, what: &str, done: impl Fn(&AppList) -> bool) -> AppList {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let reply = call(socket, "GET", "/api/apps", "").await;
        let apps: AppList = serde_json::from_value(reply.body).unwrap();
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

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn put(content: &str, base_hash: Option<&str>) -> String {
    serde_json::json!({ "content": content, "base_hash": base_hash }).to_string()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one daemon walked through the whole flow"
)]
async fn creates_an_app_and_edits_its_files_without_overwriting_a_hand_edit() {
    let dir = tempfile::Builder::new()
        .prefix("af")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home/Alchemy/apps");
    let blog = home.join("blog");
    let socket = root.join("alchemyd.sock");
    let config = FakeComposeConfig::new();
    config.project(&blog, "blog", &["web"]);
    let validator = FakeComposeValidator::new();
    let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready)))
        .with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config), Some(home.clone()))
                .with_scan_interval(Duration::from_secs(60)),
        )
        .with_editor(AppEditor::new(
            Arc::new(SystemDisk),
            Arc::new(validator.clone()),
        ));
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
    apps_until(&socket, "the home listed", |a| !a.dirs.is_empty()).await;
    assert!(!home.exists());

    let new = serde_json::json!({ "name": "blog", "compose": COMPOSE, "env": "SECRET=1\n" });
    let reply = call(&socket, "POST", "/api/apps", &new.to_string()).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.body["name"], "blog");
    assert_eq!(reply.body["dir"], blog.to_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(blog.join("compose.yaml")).unwrap(),
        COMPOSE
    );
    assert_eq!(mode(&blog.join(".env")), 0o600);
    let apps = apps_until(&socket, "blog listed", |a| {
        a.apps
            .iter()
            .any(|app| app.project.as_deref() == Some("blog"))
    })
    .await;
    assert_eq!(apps.apps.len(), 1);

    let reply = call(&socket, "POST", "/api/apps", &new.to_string()).await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(reply.body["code"], "conflict");

    let reply = call(&socket, "GET", "/api/apps/blog/files/compose", "").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let file: AppFile = serde_json::from_value(reply.body).unwrap();
    let hash = file.hash.unwrap();
    assert_eq!(hash, content_hash(COMPOSE.as_bytes()));
    assert!(
        reply.head.contains(&format!("etag: \"{hash}\"")),
        "{}",
        reply.head
    );

    validator.answer(FakeConfig::Invalid(
        "services.web Additional property imagee is not allowed".into(),
    ));
    let reply = call(
        &socket,
        "PUT",
        "/api/apps/blog/files/compose",
        &put("services:\n  web:\n    imagee: nginx\n", Some(&hash)),
    )
    .await;
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(reply.body["code"], "invalid");
    assert_eq!(
        reply.body["error"],
        "services.web Additional property imagee is not allowed"
    );
    assert_eq!(
        std::fs::read_to_string(blog.join("compose.yaml")).unwrap(),
        COMPOSE
    );

    validator.answer(FakeConfig::Config(ComposeConfig {
        name: "blog".into(),
        ..ComposeConfig::default()
    }));
    let by_hand = format!("{COMPOSE}# edited by hand\n");
    std::fs::write(blog.join("compose.yaml"), &by_hand).unwrap();
    let edited = format!("{COMPOSE}    restart: always\n");
    let reply = call(
        &socket,
        "PUT",
        "/api/apps/blog/files/compose",
        &put(&edited, Some(&hash)),
    )
    .await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(reply.body["code"], "conflict");
    assert_eq!(
        std::fs::read_to_string(blog.join("compose.yaml")).unwrap(),
        by_hand
    );

    let reply = call(
        &socket,
        "PUT",
        "/api/apps/blog/files/compose",
        &put(&edited, Some(&content_hash(by_hand.as_bytes()))),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        std::fs::read_to_string(blog.join("compose.yaml")).unwrap(),
        edited
    );

    let reply = call(&socket, "GET", "/api/apps/blog/files/env", "").await;
    assert_eq!(reply.body["content"], "SECRET=1\n");
    let reply = call(&socket, "GET", "/api/apps/blog/files/override", "").await;
    assert_eq!(reply.status, 404);
    let reply = call(&socket, "GET", "/api/apps/nope/files/compose", "").await;
    assert_eq!(reply.body["code"], "app_not_found");

    let huge = put(&"a".repeat(4 * 1024 * 1024), None);
    let reply = call(&socket, "PUT", "/api/apps/blog/files/env", &huge).await;
    assert_eq!(reply.status, 413, "{}", reply.body);
    assert_eq!(
        std::fs::read_to_string(blog.join(".env")).unwrap(),
        "SECRET=1\n"
    );

    let names: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["blog"], "a staging folder was left behind");

    stop.send(()).unwrap();
    daemon.await.unwrap().unwrap();
}

async fn operation_until(socket: &Path, id: &str, state: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let reply = call(socket, "GET", &format!("/api/operations/{id}"), "").await;
        if reply.body["state"] == state {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "operation {id} never {state}: {}",
            reply.body
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one daemon walked through the whole flow"
)]
async fn saves_and_starts_an_app_in_one_request() {
    let dir = tempfile::Builder::new()
        .prefix("as")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home/Alchemy/apps");
    let blog = home.join("blog");
    let socket = root.join("alchemyd.sock");
    let config = FakeComposeConfig::new();
    config.project(&blog, "blog", &["web"]);
    let runner = FakeComposeRunner::new().with_script(
        ComposeAction::Stop,
        FakeScript::new().outcome(FakeOutcome::UntilCancelled),
    );
    let deps = Deps::new(FixedEngine(EngineStatus::new(EngineState::Ready)))
        .with_apps(
            AppsDeps::new(Arc::new(SystemFs), Arc::new(config.clone()), Some(home))
                .with_scan_interval(Duration::from_secs(60)),
        )
        .with_editor(AppEditor::new(
            Arc::new(SystemDisk),
            Arc::new(FakeComposeValidator::new()),
        ))
        .with_runner(Arc::new(runner.clone()));
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
    apps_until(&socket, "the home listed", |a| !a.dirs.is_empty()).await;

    let dry =
        serde_json::json!({ "name": "blog", "compose": COMPOSE, "dry_run": true, "start": true });
    let reply = call(&socket, "POST", "/api/apps", &dry.to_string()).await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(reply.body["code"], "bad_request");
    assert!(!blog.exists());

    let new = serde_json::json!({ "name": "blog", "compose": COMPOSE, "start": true });
    let reply = call(&socket, "POST", "/api/apps", &new.to_string()).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.body["name"], "blog");
    assert_eq!(reply.body["operation"]["action"], "up");
    assert_eq!(reply.body["operation"]["app"], "blog");
    assert!(reply.body.get("start_error").is_none(), "{}", reply.body);
    let id = reply.body["operation"]["id"].as_str().unwrap().to_owned();
    operation_until(&socket, &id, "succeeded").await;
    assert_eq!(runner.runs().len(), 1);
    assert_eq!(runner.runs()[0].action(), ComposeAction::Up);

    let reply = call(&socket, "GET", "/api/apps/blog/files/compose", "").await;
    let hash = reply.body["hash"].as_str().unwrap().to_owned();
    let edited = format!("{COMPOSE}    restart: always\n");
    let reply = call(
        &socket,
        "PUT",
        "/api/apps/blog/files/compose",
        &put(&edited, Some(&hash)),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let keys: Vec<_> = reply.body.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["content", "hash", "kind", "name"], "without start");

    config.project(&blog, "renamed", &["web"]);
    let update = serde_json::json!({
        "content": COMPOSE,
        "base_hash": content_hash(edited.as_bytes()),
        "start": true,
    });
    let reply = call(
        &socket,
        "PUT",
        "/api/apps/blog/files/compose",
        &update.to_string(),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body["content"], COMPOSE);
    let id = reply.body["operation"]["id"].as_str().unwrap().to_owned();
    let reply = call(&socket, "GET", "/api/apps/blog", "").await;
    assert_eq!(
        reply.body["project"], "renamed",
        "started once the edit was read"
    );
    operation_until(&socket, &id, "succeeded").await;
    assert_eq!(runner.runs().len(), 2);

    let reply = call(
        &socket,
        "POST",
        "/api/apps/blog/actions",
        r#"{"action":"stop"}"#,
    )
    .await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    let stopping = reply.body["operation"]["id"].as_str().unwrap().to_owned();
    let update = serde_json::json!({
        "content": edited,
        "base_hash": content_hash(COMPOSE.as_bytes()),
        "start": true,
    });
    let reply = call(
        &socket,
        "PUT",
        "/api/apps/blog/files/compose",
        &update.to_string(),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.get("operation").is_none(), "{}", reply.body);
    assert_eq!(reply.body["start_error"]["code"], "conflict");
    let error = reply.body["start_error"]["error"].as_str().unwrap();
    assert!(error.contains(&stopping), "{error}");
    assert_eq!(
        std::fs::read_to_string(blog.join("compose.yaml")).unwrap(),
        edited,
        "a refused start keeps the save"
    );

    let reply = call(
        &socket,
        "POST",
        &format!("/api/operations/{stopping}/cancel"),
        "{}",
    )
    .await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    stop.send(()).unwrap();
    daemon.await.unwrap().unwrap();
}
