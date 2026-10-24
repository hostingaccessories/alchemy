//! State store wiring: `run_with` opening, creating, or refusing the state
//! database before it binds either listener.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alchemy_core::engine::{EngineState, EngineStatus};
use alchemyd::engine::EngineSource;
use alchemyd::store::{DB_FILE, SharedStore, SqliteStore};
use alchemyd::{Config, Deps, State};
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Leaves a state directory in some state before the daemon starts.
type Prepare = fn(&Path);

/// Publishes a fixed status, the way front-door tests can fake the engine.
struct FixedEngine;

impl EngineSource for FixedEngine {
    async fn run(self, state: State, mut shutdown: watch::Receiver<bool>) {
        state.set_engine(EngineStatus::new(EngineState::Ready));
        let _ = shutdown.wait_for(|stop| *stop).await;
    }
}

/// A config under `/tmp`, which keeps the socket path short of macOS's
/// 104-byte `sun_path` limit.
fn config() -> (tempfile::TempDir, Config) {
    let dir = tempfile::Builder::new()
        .prefix("as")
        .tempdir_in("/tmp")
        .unwrap();
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        socket: dir.path().join("alchemyd.sock"),
        state_dir: dir.path().join("state"),
    };
    (dir, config)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Run the daemon until its socket appears, then shut it down.
async fn run_until_listening(config: Config) {
    let socket: PathBuf = config.socket.clone();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let daemon = tokio::spawn(alchemyd::run_with(config, Deps::new(FixedEngine), async {
        let _ = stop_rx.await;
    }));
    tokio::time::timeout(TIMEOUT, async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn daemon_creates_a_private_database_on_start() {
    let (_dir, config) = config();
    let state_dir = config.state_dir.clone();
    run_until_listening(config).await;

    let db = state_dir.join(DB_FILE);
    assert_eq!(mode(&state_dir), 0o700);
    assert_eq!(mode(&db), 0o600);
    let store: SharedStore = Arc::new(SqliteStore::open(&state_dir).await.unwrap());
    assert_eq!(store.setting::<String>("missing").await.unwrap(), None);
}

#[tokio::test]
async fn daemon_keeps_existing_settings() {
    let (_dir, config) = config();
    let state_dir = config.state_dir.clone();
    {
        let store: SharedStore = Arc::new(SqliteStore::open(&state_dir).await.unwrap());
        store.set_setting("stack_home", "/srv/apps").await.unwrap();
    }
    run_until_listening(config).await;

    let store: SharedStore = Arc::new(SqliteStore::open(&state_dir).await.unwrap());
    assert_eq!(
        store
            .setting::<String>("stack_home")
            .await
            .unwrap()
            .as_deref(),
        Some("/srv/apps")
    );
}

#[tokio::test]
async fn daemon_refuses_a_bad_database_before_listening() {
    let corrupt = |dir: &Path| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(DB_FILE), vec![0x5a; 8192]).unwrap();
    };
    let too_new = |dir: &Path| {
        std::fs::create_dir_all(dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO schema_migrations VALUES (1000000, 'from the future');",
        )
        .unwrap();
    };
    let cases: [(&str, Prepare, &str); 2] = [
        ("corrupt", corrupt, "corrupt or not a SQLite database"),
        ("too new", too_new, "upgrade alchemyd"),
    ];
    for (name, prepare, want) in cases {
        let (_dir, config) = config();
        prepare(&config.state_dir);
        let socket = config.socket.clone();
        let err = tokio::time::timeout(
            TIMEOUT,
            alchemyd::run_with(config, Deps::new(FixedEngine), std::future::pending()),
        )
        .await
        .unwrap()
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains(want), "{name}: {message}");
        assert!(!socket.exists(), "{name}");
    }
}
