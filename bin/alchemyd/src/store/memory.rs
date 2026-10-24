//! An in-memory [`Store`] for tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use alchemy_core::engine::Intent;
use futures_util::future::BoxFuture;

use super::{MetricRow, OperationRow, Store, StoreError};

/// A [`Store`] that keeps everything in memory and forgets it on drop.
#[derive(Debug, Default)]
pub struct MemoryStore {
    settings: Mutex<BTreeMap<String, serde_json::Value>>,
    metrics: Mutex<BTreeMap<(i64, i64), String>>,
    intents: Mutex<BTreeMap<String, Intent>>,
    operations: Mutex<BTreeMap<String, OperationRow>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn settings(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, serde_json::Value>> {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn metrics(&self) -> std::sync::MutexGuard<'_, BTreeMap<(i64, i64), String>> {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn intent_map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Intent>> {
        self.intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn operations(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, OperationRow>> {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every row, newest first, as the SQLite store orders them.
    fn newest_first(&self) -> Vec<OperationRow> {
        let mut rows: Vec<OperationRow> = self.operations().values().cloned().collect();
        rows.sort_by(|a, b| (b.started_at, &b.id).cmp(&(a.started_at, &a.id)));
        rows
    }

    fn metric_range(&self, step: i64, from: i64, to: i64) -> Vec<MetricRow> {
        if from >= to {
            return Vec::new();
        }
        self.metrics()
            .range((step, from)..(step, to))
            .map(|(&(step, ts), data)| MetricRow {
                step,
                ts,
                data: data.clone(),
            })
            .collect()
    }
}

impl Store for MemoryStore {
    fn get_setting_json<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<serde_json::Value>, StoreError>> {
        Box::pin(async move { Ok(self.settings().get(key).cloned()) })
    }

    fn set_setting_json<'a>(
        &'a self,
        key: &'a str,
        value: serde_json::Value,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.settings().insert(key.to_owned(), value);
            Ok(())
        })
    }

    fn insert_metrics(&self, rows: Vec<MetricRow>) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            let mut metrics = self.metrics();
            for row in rows {
                metrics.entry((row.step, row.ts)).or_insert(row.data);
            }
            Ok(())
        })
    }

    fn metric_rows(
        &self,
        step: i64,
        from: i64,
        to: i64,
    ) -> BoxFuture<'_, Result<Vec<MetricRow>, StoreError>> {
        Box::pin(async move { Ok(self.metric_range(step, from, to)) })
    }

    fn metric_times(
        &self,
        step: i64,
        from: i64,
        to: i64,
    ) -> BoxFuture<'_, Result<Vec<i64>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .metric_range(step, from, to)
                .into_iter()
                .map(|row| row.ts)
                .collect())
        })
    }

    fn prune_metrics(&self, step: i64, before: i64) -> BoxFuture<'_, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut metrics = self.metrics();
            let old: Vec<_> = metrics
                .range((step, i64::MIN)..(step, before))
                .map(|(key, _)| *key)
                .collect();
            for key in &old {
                metrics.remove(key);
            }
            Ok(u64::try_from(old.len()).unwrap_or(u64::MAX))
        })
    }

    fn record_intent<'a>(
        &'a self,
        project: &'a str,
        intent: Intent,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.intent_map().insert(project.to_owned(), intent);
            Ok(())
        })
    }

    fn intent<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<Option<Intent>, StoreError>> {
        Box::pin(async move { Ok(self.intent_map().get(project).copied()) })
    }

    fn intents(&self) -> BoxFuture<'_, Result<Vec<(String, Intent)>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .intent_map()
                .iter()
                .map(|(project, intent)| (project.clone(), *intent))
                .collect())
        })
    }

    fn put_operation_row(&self, row: OperationRow) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            self.operations().insert(row.id.clone(), row);
            Ok(())
        })
    }

    fn operation_row(&self, id: String) -> BoxFuture<'_, Result<Option<OperationRow>, StoreError>> {
        Box::pin(async move { Ok(self.operations().get(&id).cloned()) })
    }

    fn operation_rows(
        &self,
        app: Option<String>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<OperationRow>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .newest_first()
                .into_iter()
                .filter(|row| app.as_ref().is_none_or(|app| &row.app == app))
                .take(limit)
                .collect())
        })
    }

    fn operation_rows_in(
        &self,
        state: String,
    ) -> BoxFuture<'_, Result<Vec<OperationRow>, StoreError>> {
        Box::pin(async move {
            let mut rows: Vec<OperationRow> = self
                .newest_first()
                .into_iter()
                .filter(|row| row.state == state)
                .collect();
            rows.reverse();
            Ok(rows)
        })
    }

    fn prune_operations(&self, keep: usize) -> BoxFuture<'_, Result<u64, StoreError>> {
        Box::pin(async move {
            let old: Vec<String> = self
                .newest_first()
                .into_iter()
                .skip(keep)
                .map(|row| row.id)
                .collect();
            let mut operations = self.operations();
            for id in &old {
                operations.remove(id);
            }
            Ok(u64::try_from(old.len()).unwrap_or(u64::MAX))
        })
    }
}
