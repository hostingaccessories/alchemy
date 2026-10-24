//! Persistent daemon state: settings now, and later operation history,
//! stop intent, and metrics.
//!
//! Everything goes through the [`Store`] trait. The daemon runs on
//! [`SqliteStore`], a SQLite database in the per-user state directory
//! ([`default_state_dir`]); tests use [`MemoryStore`]. The schema is
//! versioned by embedded migrations; see `store/migrations.rs` to add one.
//!
//! Handlers hold the store as a [`SharedStore`] and use the typed accessors
//! on `dyn Store`: `store.setting::<T>(key)` and `store.set_setting(key, &value)`.

mod dir;
mod memory;
mod migrations;
mod sqlite;

pub use dir::{DB_FILE, STATE_DIR_ENV, default_state_dir};
pub use memory::MemoryStore;
pub use sqlite::SqliteStore;

use std::path::PathBuf;
use std::sync::Arc;

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
    #[error("setting {key:?} cannot be encoded as JSON")]
    Encode {
        /// The setting key.
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
