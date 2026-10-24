//! Persistent daemon state: settings, metrics history, stop intent, and
//! operation history.
//!
//! Everything goes through the [`Store`] trait. The daemon runs on
//! [`SqliteStore`], a SQLite database in the per-user state directory
//! ([`default_state_dir`]); tests use [`MemoryStore`]. The schema is
//! versioned by embedded migrations; see `store/migrations.rs` to add one.
//!
//! Handlers hold the store as a [`SharedStore`] and use the typed accessors
//! on `dyn Store`: `store.setting::<T>(key)` and `store.set_setting(key, &value)`
//! for settings, and `store.metric_points(step, from, to)` and
//! `store.put_metric_points(step, &points)` for metrics history, and
//! `store.put_operation(&operation)` and `store.operations(app, limit)`
//! for operation history. Intents go through [`crate::intent`], which
//! also updates the live state.

mod dir;
mod memory;
mod migrations;
mod sqlite;

pub use dir::{DB_FILE, STATE_DIR_ENV, default_state_dir};
pub use memory::MemoryStore;
pub use sqlite::SqliteStore;

use std::path::PathBuf;
use std::sync::Arc;

use alchemy_core::engine::Intent;
use alchemy_core::metrics::MetricsPoint;
use alchemy_core::operation::{Operation, OperationState};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A store shared by every handler and background task.
pub type SharedStore = Arc<dyn Store>;

/// The daemon's persistent state.
///
/// Methods return boxed futures so the trait stays object safe and the
/// daemon can hold any implementation as a [`SharedStore`]. Values are
/// JSON; use the typed accessors on `dyn Store` rather than these raw
/// methods.
pub trait Store: Send + Sync + std::fmt::Debug {
    /// The raw JSON value of setting `key`, or `None` if it is unset.
    fn get_setting_json<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<serde_json::Value>, StoreError>>;

    /// Store `value` as setting `key`, replacing any previous value.
    fn set_setting_json<'a>(
        &'a self,
        key: &'a str,
        value: serde_json::Value,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// Store `rows`, keeping any row already stored at the same step and
    /// timestamp, so writing a rollup twice is harmless.
    fn insert_metrics(&self, rows: Vec<MetricRow>) -> BoxFuture<'_, Result<(), StoreError>>;

    /// The rows of tier `step` with `from <= ts < to`, oldest first.
    fn metric_rows(
        &self,
        step: i64,
        from: i64,
        to: i64,
    ) -> BoxFuture<'_, Result<Vec<MetricRow>, StoreError>>;

    /// The timestamps of those rows, without their data.
    fn metric_times(
        &self,
        step: i64,
        from: i64,
        to: i64,
    ) -> BoxFuture<'_, Result<Vec<i64>, StoreError>>;

    /// Delete the rows of tier `step` older than `before`, returning how
    /// many went.
    fn prune_metrics(&self, step: i64, before: i64) -> BoxFuture<'_, Result<u64, StoreError>>;

    /// Store `intent` as the last action Alchemy asked of Compose project
    /// `project`, replacing any earlier one.
    fn record_intent<'a>(
        &'a self,
        project: &'a str,
        intent: Intent,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    /// The last intent stored for `project`.
    fn intent<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<Option<Intent>, StoreError>>;

    /// Every stored intent, by project name. A row whose action this
    /// build does not know is left out.
    fn intents(&self) -> BoxFuture<'_, Result<Vec<(String, Intent)>, StoreError>>;

    /// Store `row`, replacing any operation stored with the same id.
    fn put_operation_row(&self, row: OperationRow) -> BoxFuture<'_, Result<(), StoreError>>;

    /// The operation stored as `id`, if any.
    fn operation_row(&self, id: String) -> BoxFuture<'_, Result<Option<OperationRow>, StoreError>>;

    /// Up to `limit` operations, of app `app` alone when given, newest
    /// first.
    fn operation_rows(
        &self,
        app: Option<String>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<OperationRow>, StoreError>>;

    /// Every operation stored in state `state`, oldest first.
    fn operation_rows_in(
        &self,
        state: String,
    ) -> BoxFuture<'_, Result<Vec<OperationRow>, StoreError>>;

    /// Delete all but the newest `keep` operations, returning how many
    /// went.
    fn prune_operations(&self, keep: usize) -> BoxFuture<'_, Result<u64, StoreError>>;
}

/// One stored operation: the columns it is looked up by, and the whole
/// [`Operation`] as JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRow {
    /// The operation's id.
    pub id: String,
    /// The app it ran on.
    pub app: String,
    /// Its [`OperationState`], as its wire name.
    pub state: String,
    /// When it started, in Unix seconds.
    pub started_at: i64,
    /// The operation, as JSON.
    pub data: String,
}

/// One stored metrics point: its tier's step in seconds, its Unix
/// timestamp, and the [`MetricsPoint`] as JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricRow {
    /// Seconds per point of the tier the row belongs to.
    pub step: i64,
    /// The point's time, in Unix seconds.
    pub ts: i64,
    /// The point, as JSON.
    pub data: String,
}

impl dyn Store {
    /// Setting `key` decoded as `T`, or `None` if it is unset.
    pub async fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StoreError> {
        match self.get_setting_json(key).await? {
            Some(value) => {
                serde_json::from_value(value)
                    .map(Some)
                    .map_err(|source| StoreError::Decode {
                        key: key.to_owned(),
                        source,
                    })
            }
            None => Ok(None),
        }
    }

    /// Store `value` as setting `key`, replacing any previous value.
    pub async fn set_setting<T: Serialize + ?Sized>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_value(value).map_err(|source| StoreError::Encode {
            key: key.to_owned(),
            source,
        })?;
        self.set_setting_json(key, json).await
    }
}

impl dyn Store {
    /// Store `points` in tier `step`, keeping any already stored at the
    /// same time.
    pub async fn put_metric_points(
        &self,
        step: i64,
        points: &[MetricsPoint],
    ) -> Result<(), StoreError> {
        let rows = points
            .iter()
            .map(|point| {
                serde_json::to_string(point)
                    .map(|data| MetricRow {
                        step,
                        ts: point.t,
                        data,
                    })
                    .map_err(|source| StoreError::Encode {
                        key: format!("metrics/{step}/{}", point.t),
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Ok(());
        }
        self.insert_metrics(rows).await
    }

    /// The points of tier `step` with `from <= t < to`, oldest first. A row
    /// that no longer decodes is skipped with a warning rather than failing
    /// the whole range.
    pub async fn metric_points(
        &self,
        step: i64,
        from: i64,
        to: i64,
    ) -> Result<Vec<MetricsPoint>, StoreError> {
        let rows = self.metric_rows(step, from, to).await?;
        Ok(rows
            .into_iter()
            .filter_map(
                |row| match serde_json::from_str::<MetricsPoint>(&row.data) {
                    Ok(mut point) => {
                        point.t = row.ts;
                        Some(point)
                    }
                    Err(error) => {
                        tracing::warn!(step, ts = row.ts, %error, "skipping a metrics row that does not decode");
                        None
                    }
                },
            )
            .collect())
    }
}

impl dyn Store {
    /// Store `operation`, replacing any earlier copy of it.
    pub async fn put_operation(&self, operation: &Operation) -> Result<(), StoreError> {
        let data = serde_json::to_string(operation).map_err(|source| StoreError::Encode {
            key: format!("operations/{}", operation.id),
            source,
        })?;
        self.put_operation_row(OperationRow {
            id: operation.id.clone(),
            app: operation.app.to_string(),
            state: operation.state.as_str().to_owned(),
            started_at: operation.started_at,
            data,
        })
        .await
    }

    /// The operation stored as `id`, if any and if it still decodes.
    pub async fn operation(&self, id: &str) -> Result<Option<Operation>, StoreError> {
        let row = self.operation_row(id.to_owned()).await?;
        Ok(row.as_ref().and_then(decode_operation))
    }

    /// Up to `limit` stored operations, of `app` alone when given, newest
    /// first. A row that no longer decodes is skipped with a warning.
    pub async fn operations(
        &self,
        app: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Operation>, StoreError> {
        let rows = self.operation_rows(app.map(str::to_owned), limit).await?;
        Ok(rows.iter().filter_map(decode_operation).collect())
    }

    /// Every stored operation in `state`, oldest first.
    pub async fn operations_in(&self, state: OperationState) -> Result<Vec<Operation>, StoreError> {
        let rows = self.operation_rows_in(state.as_str().to_owned()).await?;
        Ok(rows.iter().filter_map(decode_operation).collect())
    }
}

fn decode_operation(row: &OperationRow) -> Option<Operation> {
    match serde_json::from_str(&row.data) {
        Ok(operation) => Some(operation),
        Err(error) => {
            tracing::warn!(id = row.id, %error, "skipping an operation row that does not decode");
            None
        }
    }
}

/// Why a [`Store`] operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The state directory or database file could not be created or
    /// secured.
    #[error("cannot prepare state path {path}")]
    Path {
        /// The directory or file that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not a SQLite database, or SQLite found it damaged.
    #[error(
        "state database {path} is corrupt or not a SQLite database; move it aside to start fresh"
    )]
    Corrupt {
        /// The database file.
        path: PathBuf,
        /// What SQLite reported.
        #[source]
        source: rusqlite::Error,
    },
    /// The database was written by a newer Alchemy whose schema this
    /// build does not know.
    #[error(
        "state database {path} has schema version {found}, newer than the {supported} this alchemyd supports; upgrade alchemyd"
    )]
    TooNew {
        /// The database file.
        path: PathBuf,
        /// The highest migration recorded in the database.
        found: i64,
        /// The highest migration this build knows.
        supported: i64,
    },
    /// The recorded migrations do not match the ones this build embeds.
    #[error("state database {path} has an unexpected migration history: {detail}")]
    History {
        /// The database file.
        path: PathBuf,
        /// What did not match.
        detail: String,
    },
    /// Any other SQLite failure.
    #[error("state database error")]
    Sqlite(#[from] rusqlite::Error),
    /// A stored setting is not valid JSON for the requested type.
    #[error("setting {key:?} has an unexpected value")]
    Decode {
        /// The setting key.
        key: String,
        /// Why decoding failed.
        #[source]
        source: serde_json::Error,
    },
    /// A value could not be encoded as JSON.
    #[error("{key:?} cannot be encoded as JSON")]
    Encode {
        /// The setting key, `metrics/STEP/TS` for a metrics point, or
        /// `operations/ID` for an operation.
        key: String,
        /// Why encoding failed.
        #[source]
        source: serde_json::Error,
    },
    /// The store's worker thread could not be started.
    #[error("cannot start the state store thread")]
    Spawn(#[source] std::io::Error),
    /// The store's worker thread has stopped.
    #[error("state store is closed")]
    Closed,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Home {
        dirs: Vec<String>,
    }

    #[tokio::test]
    async fn typed_settings_round_trip() {
        let store: SharedStore = Arc::new(MemoryStore::new());
        assert_eq!(store.setting::<Home>("home").await.unwrap(), None);
        let home = Home {
            dirs: vec!["/srv/apps".into()],
        };
        store.set_setting("home", &home).await.unwrap();
        assert_eq!(store.setting::<Home>("home").await.unwrap(), Some(home));
    }

    fn point(t: i64, containers: usize) -> MetricsPoint {
        MetricsPoint {
            t,
            containers: (0..containers)
                .map(|i| alchemy_core::metrics::ContainerUsage {
                    id: i.to_string(),
                    name: format!("c{i}"),
                    ..Default::default()
                })
                .collect(),
            ..MetricsPoint::default()
        }
    }

    /// Every store behaves the same: idempotent inserts, half-open ranges
    /// per step, and pruning one step at a time.
    async fn metrics_behave(store: SharedStore) {
        store
            .put_metric_points(60, &[point(60, 1), point(120, 1), point(180, 1)])
            .await
            .unwrap();
        store
            .put_metric_points(60, &[point(120, 5), point(240, 1)])
            .await
            .unwrap();
        store.put_metric_points(600, &[point(0, 2)]).await.unwrap();
        store.put_metric_points(600, &[]).await.unwrap();

        let got = store.metric_points(60, 100, 240).await.unwrap();
        assert_eq!(got, [point(120, 1), point(180, 1)], "first write wins");
        assert_eq!(
            store.metric_times(60, 0, 1_000).await.unwrap(),
            [60, 120, 180, 240]
        );
        assert_eq!(store.metric_times(600, 0, 1_000).await.unwrap(), [0]);
        assert_eq!(
            store.metric_times(60, 500, 100).await.unwrap(),
            Vec::<i64>::new()
        );

        assert_eq!(store.prune_metrics(60, 180).await.unwrap(), 2);
        assert_eq!(store.metric_times(60, 0, 1_000).await.unwrap(), [180, 240]);
        assert_eq!(store.metric_times(600, 0, 1_000).await.unwrap(), [0]);

        store
            .insert_metrics(vec![MetricRow {
                step: 60,
                ts: 300,
                data: "not json".into(),
            }])
            .await
            .unwrap();
        assert_eq!(
            store.metric_points(60, 0, 1_000).await.unwrap(),
            [point(180, 1), point(240, 1)],
            "a broken row is skipped"
        );
    }

    #[tokio::test]
    async fn memory_store_metrics() {
        metrics_behave(Arc::new(MemoryStore::new())).await;
    }

    #[tokio::test]
    async fn sqlite_store_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        metrics_behave(Arc::new(SqliteStore::open(tmp.path()).await.unwrap())).await;
        let reopened: SharedStore = Arc::new(SqliteStore::open(tmp.path()).await.unwrap());
        assert_eq!(
            reopened.metric_times(60, 0, 1_000).await.unwrap(),
            [180, 240, 300]
        );
    }

    /// Every store keeps one intent per project, the latest.
    async fn intents_behave(store: SharedStore) {
        use alchemy_core::engine::IntentAction;
        assert_eq!(store.intent("blog").await.unwrap(), None);
        assert_eq!(store.intents().await.unwrap(), Vec::new());
        let stop = Intent::new(IntentAction::Stop, 1_000);
        let up = Intent::new(IntentAction::Up, 2_000);
        let down = Intent::new(IntentAction::Down, 3_000);
        store.record_intent("blog", stop).await.unwrap();
        store.record_intent("shop", up).await.unwrap();
        store.record_intent("blog", down).await.unwrap();
        assert_eq!(store.intent("blog").await.unwrap(), Some(down));
        assert_eq!(
            store.intents().await.unwrap(),
            [("blog".to_owned(), down), ("shop".to_owned(), up)]
        );
    }

    #[tokio::test]
    async fn memory_store_intents() {
        intents_behave(Arc::new(MemoryStore::new())).await;
    }

    #[tokio::test]
    async fn sqlite_store_intents() {
        let tmp = tempfile::tempdir().unwrap();
        intents_behave(Arc::new(SqliteStore::open(tmp.path()).await.unwrap())).await;
        let reopened: SharedStore = Arc::new(SqliteStore::open(tmp.path()).await.unwrap());
        assert_eq!(reopened.intents().await.unwrap().len(), 2);
    }

    fn operation(id: &str, app: &str, started_at: i64, state: OperationState) -> Operation {
        let mut tracker = alchemy_core::operation::Tracker::new(
            id,
            alchemy_core::StackName::new(app).unwrap(),
            alchemy_core::operation::Action::Stop,
            &[alchemy_core::operation::StepCommand::Stop],
            started_at,
        );
        let outcome = match state {
            OperationState::Succeeded => Some(alchemy_core::operation::Outcome::Succeeded),
            OperationState::Failed => Some(alchemy_core::operation::Outcome::Failed("x".into())),
            _ => None,
        };
        if let Some(outcome) = outcome {
            tracker.finish(&outcome, started_at + 1);
        }
        tracker.operation().clone()
    }

    /// Every store behaves the same: upserts by id, newest first, filtered
    /// by app or state, and pruned to the newest.
    async fn operations_behave(store: SharedStore) {
        let a = operation("a", "blog", 10, OperationState::Succeeded);
        let b = operation("b", "wiki", 20, OperationState::Running);
        let c = operation("c", "blog", 30, OperationState::Running);
        for op in [&a, &b, &c] {
            store.put_operation(op).await.unwrap();
        }
        let ids = |ops: Vec<Operation>| ops.into_iter().map(|o| o.id).collect::<Vec<_>>();
        assert_eq!(
            ids(store.operations(None, 10).await.unwrap()),
            ["c", "b", "a"]
        );
        assert_eq!(ids(store.operations(None, 2).await.unwrap()), ["c", "b"]);
        assert_eq!(
            ids(store.operations(Some("blog"), 10).await.unwrap()),
            ["c", "a"]
        );
        assert_eq!(
            ids(store.operations_in(OperationState::Running).await.unwrap()),
            ["b", "c"]
        );

        let c_done = operation("c", "blog", 30, OperationState::Failed);
        store.put_operation(&c_done).await.unwrap();
        assert_eq!(store.operation("c").await.unwrap(), Some(c_done));
        assert_eq!(store.operation("nope").await.unwrap(), None);
        assert_eq!(
            ids(store.operations_in(OperationState::Running).await.unwrap()),
            ["b"]
        );

        store
            .put_operation_row(OperationRow {
                id: "broken".into(),
                app: "blog".into(),
                state: "running".into(),
                started_at: 5,
                data: "not json".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            ids(store.operations(None, 10).await.unwrap()),
            ["c", "b", "a"]
        );
        assert_eq!(store.operation("broken").await.unwrap(), None);

        assert_eq!(store.prune_operations(2).await.unwrap(), 2);
        assert_eq!(ids(store.operations(None, 10).await.unwrap()), ["c", "b"]);
    }

    #[tokio::test]
    async fn memory_store_operations() {
        operations_behave(Arc::new(MemoryStore::new())).await;
    }

    #[tokio::test]
    async fn sqlite_store_operations() {
        let tmp = tempfile::tempdir().unwrap();
        operations_behave(Arc::new(SqliteStore::open(tmp.path()).await.unwrap())).await;
        let reopened: SharedStore = Arc::new(SqliteStore::open(tmp.path()).await.unwrap());
        assert_eq!(reopened.operations(None, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn decode_error_names_the_key() {
        let store: SharedStore = Arc::new(MemoryStore::new());
        store.set_setting("port", "not a number").await.unwrap();
        let err = store.setting::<u16>("port").await.unwrap_err();
        assert!(
            matches!(err, StoreError::Decode { ref key, .. } if key == "port"),
            "{err:?}"
        );
    }
}
