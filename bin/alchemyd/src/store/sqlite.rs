//! The SQLite [`Store`] the daemon runs on.
//!
//! One connection lives on a dedicated thread and runs jobs sent to it in
//! order. SQLite calls block, sometimes for up to the busy timeout, and a
//! `rusqlite::Connection` is not `Sync`. A long-lived thread keeps them
//! off the async runtime without tying up the `spawn_blocking` pool,
//! serializes every write through the one connection, and needs no lock.
//! The thread exits, closing the database, once every handle is dropped.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;
use rusqlite::{Connection, ErrorCode, OptionalExtension as _};
use tokio::sync::{mpsc, oneshot};

use super::migrations::{self, MIGRATIONS, MigrateError};
use super::{DB_FILE, MetricRow, Store, StoreError};
use crate::secure_fs;

/// How long a statement waits on a lock held by another connection, such
/// as `sqlite3` run by hand, before failing with `SQLITE_BUSY`.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Jobs queued for the store thread before senders wait.
const QUEUE: usize = 64;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// A handle to the daemon's SQLite database. Cheap to clone: every clone
/// talks to the same connection.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    jobs: mpsc::Sender<Job>,
    path: PathBuf,
}

impl SqliteStore {
    /// Open or create `dir/alchemy.db` and migrate it to the latest schema.
    ///
    /// `dir` is created `0o700` and the database `0o600`. A file that is
    /// not a SQLite database, or one from a newer `alchemyd`, is an error
    /// rather than something to overwrite.
    pub async fn open(dir: &Path) -> Result<Self, StoreError> {
        let dir = dir.to_owned();
        let path = dir.join(DB_FILE);
        let (jobs, queue) = mpsc::channel(QUEUE);
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread_path = path.clone();
        std::thread::Builder::new()
            .name("alchemyd-store".into())
            .spawn(move || match open_connection(&dir, &thread_path) {
                Ok(conn) => {
                    if ready_tx.send(Ok(())).is_ok() {
                        serve(conn, queue);
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(StoreError::Spawn)?;
        ready_rx.await.map_err(|_| StoreError::Closed)??;
        Ok(Self { jobs, path })
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move |conn| {
                let _ = tx.send(job(conn));
            }))
            .await
            .map_err(|_| StoreError::Closed)?;
        rx.await
            .map_err(|_| StoreError::Closed)?
            .map_err(|e| classify(&self.path, e))
    }
}

impl Store for SqliteStore {
    fn get_setting_json<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<serde_json::Value>, StoreError>> {
        let owned = key.to_owned();
        Box::pin(async move {
            let text = self
                .call(move |conn| {
                    conn.query_row(
                        "SELECT value FROM settings WHERE key = ?1",
                        [owned],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                })
                .await?;
            text.map(|text| serde_json::from_str(&text))
                .transpose()
                .map_err(|source| StoreError::Decode {
                    key: key.to_owned(),
                    source,
                })
        })
    }

    fn set_setting_json<'a>(
        &'a self,
        key: &'a str,
        value: serde_json::Value,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        let owned = key.to_owned();
        Box::pin(async move {
            let text = value.to_string();
            self.call(move |conn| {
                conn.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    (owned, text),
                )
                .map(drop)
            })
            .await
        })
    }

    fn insert_metrics(&self, rows: Vec<MetricRow>) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(self.call(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO metrics (step, ts, data) VALUES (?1, ?2, ?3)
                     ON CONFLICT (step, ts) DO NOTHING",
                )?;
                for row in rows {
                    stmt.execute((row.step, row.ts, row.data))?;
                }
            }
            tx.commit()
        }))
    }

    fn metric_rows(
        &self,
        step: i64,
        from: i64,
        to: i64,
    ) -> BoxFuture<'_, Result<Vec<MetricRow>, StoreError>> {
        Box::pin(self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT ts, data FROM metrics
                 WHERE step = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts",
            )?;
            stmt.query_map((step, from, to), |row| {
                Ok(MetricRow {
                    step,
                    ts: row.get(0)?,
                    data: row.get(1)?,
                })
            })?
            .collect()
        }))
    }

    fn metric_times(
        &self,
        step: i64,
        from: i64,
        to: i64,
    ) -> BoxFuture<'_, Result<Vec<i64>, StoreError>> {
        Box::pin(self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT ts FROM metrics WHERE step = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts",
            )?;
            stmt.query_map((step, from, to), |row| row.get(0))?
                .collect()
        }))
    }

    fn prune_metrics(&self, step: i64, before: i64) -> BoxFuture<'_, Result<u64, StoreError>> {
        Box::pin(self.call(move |conn| {
            let deleted = conn.execute(
                "DELETE FROM metrics WHERE step = ?1 AND ts < ?2",
                (step, before),
            )?;
            Ok(u64::try_from(deleted).unwrap_or(u64::MAX))
        }))
    }
}

fn open_connection(dir: &Path, path: &Path) -> Result<Connection, StoreError> {
    secure_fs::create_private_dir(dir).map_err(|source| StoreError::Path {
        path: dir.to_owned(),
        source,
    })?;
    secure_fs::create_private_file(path).map_err(|source| StoreError::Path {
        path: path.to_owned(),
        source,
    })?;
    let mut conn = Connection::open(path).map_err(|e| classify(path, e))?;
    configure(&conn).map_err(|e| classify(path, e))?;
    let from = migrations::apply(&mut conn, MIGRATIONS).map_err(|e| match e {
        MigrateError::TooNew { found, supported } => StoreError::TooNew {
            path: path.to_owned(),
            found,
            supported,
        },
        MigrateError::History(detail) => StoreError::History {
            path: path.to_owned(),
            detail,
        },
        MigrateError::Sqlite(e) => classify(path, e),
    })?;
    let to = migrations::latest();
    if from == to {
        tracing::debug!(path = %path.display(), version = to, "state database opened");
    } else {
        tracing::info!(path = %path.display(), from, to, "state database migrated");
    }
    Ok(conn)
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(mode, "state database is not in WAL mode");
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)
}

fn serve(mut conn: Connection, mut queue: mpsc::Receiver<Job>) {
    while let Some(job) = queue.blocking_recv() {
        job(&mut conn);
    }
}

fn classify(path: &Path, error: rusqlite::Error) -> StoreError {
    match error.sqlite_error_code() {
        Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => StoreError::Corrupt {
            path: path.to_owned(),
            source: error,
        },
        _ => StoreError::Sqlite(error),
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    use super::*;
    use crate::store::SharedStore;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn creates_private_dir_and_db_in_wal_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let store = SqliteStore::open(&dir).await.unwrap();
        assert_eq!(store.path(), dir.join(DB_FILE));
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(store.path()), 0o600);
        let shared: SharedStore = Arc::new(store.clone());
        shared.set_setting("k", &1).await.unwrap();
        let wal = dir.join(format!("{DB_FILE}-wal"));
        assert!(wal.exists());
        assert_eq!(mode(&wal), 0o600);
        let journal = store
            .call(|conn| conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0)))
            .await
            .unwrap();
        assert_eq!(journal, "wal");
    }

    #[tokio::test]
    async fn settings_round_trip_and_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let cases = [
            ("string", serde_json::json!("/srv/apps")),
            ("list", serde_json::json!(["/a", "/b"])),
            ("object", serde_json::json!({"n": 1, "ok": true})),
            ("null", serde_json::Value::Null),
        ];
        {
            let store: SharedStore = Arc::new(SqliteStore::open(tmp.path()).await.unwrap());
            for (key, value) in &cases {
                assert_eq!(store.get_setting_json(key).await.unwrap(), None, "{key}");
                store.set_setting_json(key, value.clone()).await.unwrap();
            }
            store.set_setting("string", "/srv/other").await.unwrap();
        }
        let store: SharedStore = Arc::new(SqliteStore::open(tmp.path()).await.unwrap());
        for (key, value) in &cases {
            let want = if *key == "string" {
                serde_json::json!("/srv/other")
            } else {
                value.clone()
            };
            assert_eq!(
                store.get_setting_json(key).await.unwrap(),
                Some(want),
                "{key}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_a_database_from_a_newer_alchemyd() {
        let tmp = tempfile::tempdir().unwrap();
        drop(SqliteStore::open(tmp.path()).await.unwrap());
        let conn = Connection::open(tmp.path().join(DB_FILE)).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, 'future')",
            [migrations::latest() + 1],
        )
        .unwrap();
        drop(conn);
        let err = SqliteStore::open(tmp.path()).await.unwrap_err();
        assert!(matches!(err, StoreError::TooNew { .. }), "{err:?}");
        assert!(err.to_string().contains("upgrade alchemyd"), "{err}");
    }

    #[tokio::test]
    async fn refuses_a_file_that_is_not_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(DB_FILE);
        std::fs::write(&path, vec![0x5a; 8192]).unwrap();
        let err = SqliteStore::open(tmp.path()).await.unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), vec![0x5a; 8192]);
    }
}
