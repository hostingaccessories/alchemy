//! An in-memory [`Store`] for tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use futures_util::future::BoxFuture;

use super::{Store, StoreError};

/// A [`Store`] that keeps everything in memory and forgets it on drop.
#[derive(Debug, Default)]
pub struct MemoryStore {
    settings: Mutex<BTreeMap<String, serde_json::Value>>,
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
}
