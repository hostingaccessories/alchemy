//! What Alchemy last asked of each Compose project, so a stop it made is
//! told apart from a crash or a stop made by hand.
//!
//! Every lifecycle operation calls [`record`] with its project and action
//! before it runs Compose. The intent is stored, so it survives a daemon
//! restart, and handed to [`State`]'s containers, which reclassify the
//! project's [`stop_reason`](alchemy_core::engine::ContainerStatus::stop_reason)
//! at once. [`load`] restores every stored intent at startup.

use std::time::{SystemTime, UNIX_EPOCH};

use alchemy_core::engine::{Intent, IntentAction};

use crate::State;
use crate::store::{Store, StoreError};

/// Record that Alchemy is about to `action` Compose project `project`, at
/// the current time. Call it before running Compose, so the containers'
/// exits come after it.
pub async fn record(
    store: &dyn Store,
    state: &State,
    project: &str,
    action: IntentAction,
) -> Result<Intent, StoreError> {
    let intent = Intent::new(action, now_ms());
    record_at(store, state, project, intent).await?;
    Ok(intent)
}

/// [`record`] with the time given.
pub async fn record_at(
    store: &dyn Store,
    state: &State,
    project: &str,
    intent: Intent,
) -> Result<(), StoreError> {
    store.record_intent(project, intent).await?;
    state.update_containers(|containers| containers.set_intent(project, intent));
    Ok(())
}

/// Hand every stored intent to `state`, returning how many there were.
pub async fn load(store: &dyn Store, state: &State) -> Result<usize, StoreError> {
    let intents = store.intents().await?;
    let count = intents.len();
    state.update_containers(|containers| {
        intents.iter().fold(false, |changed, (project, intent)| {
            containers.set_intent(project, *intent) || changed
        })
    });
    Ok(count)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_core::engine::{ContainerState, ContainerStatus, StopReason};

    use super::*;
    use crate::store::MemoryStore;

    fn exited(id: &str, project: &str) -> ContainerStatus {
        let mut status = ContainerStatus::new(id, id, ContainerState::Exited);
        status.project = Some(project.into());
        status
    }

    fn reason(state: &State, id: &str) -> StopReason {
        state.containers().get(id).unwrap().stop_reason
    }

    #[tokio::test]
    async fn record_stores_the_intent_and_reclassifies_its_project() {
        let store = MemoryStore::new();
        let state = State::new();
        state.update_containers(|c| c.replace_all([exited("a", "blog"), exited("b", "shop")]));
        let rx = state.watch_containers();
        assert_eq!(reason(&state, "a"), StopReason::Unknown);

        let intent = record(&store, &state, "blog", IntentAction::Stop)
            .await
            .unwrap();
        assert!(intent.at > 0);
        assert_eq!(store.intent("blog").await.unwrap(), Some(intent));
        assert_eq!(reason(&state, "a"), StopReason::StoppedByAlchemy);
        assert_eq!(reason(&state, "b"), StopReason::Unknown);
        assert!(rx.has_changed().unwrap());
    }

    #[tokio::test]
    async fn load_restores_stored_intents() {
        let store = MemoryStore::new();
        store
            .record_intent("blog", Intent::new(IntentAction::Down, 5))
            .await
            .unwrap();
        store
            .record_intent("shop", Intent::new(IntentAction::Up, 5))
            .await
            .unwrap();
        let state = State::new();
        assert_eq!(load(&store, &state).await.unwrap(), 2);
        state.update_containers(|c| c.replace_all([exited("a", "blog"), exited("b", "shop")]));
        assert_eq!(reason(&state, "a"), StopReason::StoppedByAlchemy);
        assert_eq!(reason(&state, "b"), StopReason::Unknown);
        assert_eq!(
            state.containers().intent("blog"),
            Some(&Intent::new(IntentAction::Down, 5))
        );
    }
}
