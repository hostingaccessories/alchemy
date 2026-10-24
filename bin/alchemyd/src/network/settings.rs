//! The network settings: how they are stored, loaded, and changed.

use alchemy_api::network::{NetworkSettings, NetworkSettingsUpdate};
use serde::{Deserialize, Serialize};

use crate::State;
use crate::store::{SharedStore, StoreError};

/// The [`Store`](crate::store::Store) key holding [`StoredMdns`].
pub const MDNS_KEY: &str = "network.mdns";

/// The `network.mdns` setting as stored. Unset means on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMdns {
    /// Whether apps' `.local` names are announced.
    pub enabled: bool,
}

/// The settings `mdns` means, with an unset setting taking its default.
pub fn effective(mdns: Option<StoredMdns>) -> NetworkSettings {
    let default = NetworkSettings::default();
    NetworkSettings {
        mdns: mdns.map_or(default.mdns, |m| m.enabled),
    }
}

/// What to store for `update` applied to `current`, and the settings
/// that are then in effect.
pub fn apply(
    current: NetworkSettings,
    update: NetworkSettingsUpdate,
) -> (StoredMdns, NetworkSettings) {
    let next = current.updated(update);
    (StoredMdns { enabled: next.mdns }, next)
}

/// Load the network settings from `store` into `state`.
///
/// The daemon calls this before serving, so no request and no task sees
/// the defaults in place of what was saved. Unreadable settings fall back
/// to the defaults.
pub async fn load(state: &State, store: &SharedStore) {
    let mdns = match store.setting::<StoredMdns>(MDNS_KEY).await {
        Ok(mdns) => mdns,
        Err(error) => {
            tracing::warn!(%error, "network settings unreadable; using the defaults");
            None
        }
    };
    state.set_network_settings(effective(mdns));
}

/// Store `update` applied to the settings in effect, then put the result
/// in effect in `state` and return it. Nothing changes if storing fails.
pub async fn save(
    state: &State,
    store: &SharedStore,
    update: NetworkSettingsUpdate,
) -> Result<NetworkSettings, StoreError> {
    let (stored, next) = apply(state.network_settings(), update);
    store.set_setting(MDNS_KEY, &stored).await?;
    state.set_network_settings(next);
    Ok(next)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::MemoryStore;

    fn on(mdns: bool) -> NetworkSettings {
        NetworkSettings { mdns }
    }

    #[test]
    fn effective_settings_default_to_on() {
        let cases = [
            (None, on(true)),
            (Some(StoredMdns { enabled: true }), on(true)),
            (Some(StoredMdns { enabled: false }), on(false)),
        ];
        for (stored, want) in cases {
            assert_eq!(effective(stored), want, "{stored:?}");
        }
    }

    #[test]
    fn updates_keep_what_they_leave_out() {
        let cases = [
            (on(true), None, true),
            (on(true), Some(false), false),
            (on(false), None, false),
            (on(false), Some(true), true),
        ];
        for (current, mdns, want) in cases {
            let (stored, next) = apply(current, NetworkSettingsUpdate { mdns });
            assert_eq!(stored, StoredMdns { enabled: want }, "{current:?} {mdns:?}");
            assert_eq!(next, on(want));
        }
    }

    #[tokio::test]
    async fn saved_settings_load_again() {
        let store: SharedStore = Arc::new(MemoryStore::new());
        let state = State::new();
        load(&state, &store).await;
        assert_eq!(state.network_settings(), on(true));

        let saved = save(&state, &store, NetworkSettingsUpdate { mdns: Some(false) })
            .await
            .unwrap();
        assert_eq!(saved, on(false));
        assert_eq!(
            store.setting::<StoredMdns>(MDNS_KEY).await.unwrap(),
            Some(StoredMdns { enabled: false })
        );

        let fresh = State::new();
        load(&fresh, &store).await;
        assert_eq!(fresh.network_settings(), on(false));
    }

    #[tokio::test]
    async fn unreadable_settings_load_as_the_defaults() {
        let store: SharedStore = Arc::new(MemoryStore::new());
        store.set_setting(MDNS_KEY, &"yes").await.unwrap();
        let state = State::new();
        state.set_network_settings(on(false));
        load(&state, &store).await;
        assert_eq!(state.network_settings(), on(true));
    }
}
