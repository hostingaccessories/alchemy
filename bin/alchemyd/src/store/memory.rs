//! An in-memory [`Store`] for tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use futures_util::future::BoxFuture;

use super::{MetricRow, Store, StoreError};

/// A [`Store`] that keeps everything in memory and forgets it on drop.
#[derive(Debug, Default)]
pub struct MemoryStore {
    settings: Mutex<BTreeMap<String, serde_json::Value>>,
    metrics: Mutex<BTreeMap<(i64, i64), String>>,
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
}
