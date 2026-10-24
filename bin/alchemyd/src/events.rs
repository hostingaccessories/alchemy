//! Topics for the `GET /api/events` stream.
//!
//! A topic is one live value the daemon publishes, such as the engine
//! status. Every connection subscribes to all topics and merges them into
//! one stream of [`ApiEvent`]s. Each topic sends its current value on
//! connect, then again on every change.
//!
//! Adding a topic takes an [`ApiEvent`] variant in `alchemy-api` and one
//! entry in [`topics`]. The HTTP handler does not change.

use std::fmt;
use std::sync::Arc;

use alchemy_api::ApiEvent;
use futures_util::stream::{self, BoxStream};
use futures_util::{Stream, StreamExt as _};
use tokio::sync::watch;

use crate::State;

/// The events one topic sends to one connection.
pub type EventStream = BoxStream<'static, ApiEvent>;

/// One live value on the event stream, subscribed to once per connection.
#[derive(Clone)]
pub struct Topic {
    subscribe: Arc<dyn Fn(&State) -> EventStream + Send + Sync>,
}

impl Topic {
    /// A topic over a [`watch`] channel: `watch` subscribes to it, and
    /// `event` turns each value into an [`ApiEvent`].
    ///
    /// The topic ends when the channel's sender is dropped.
    pub fn watch<T, W, E>(watch: W, event: E) -> Self
    where
        T: Clone + Send + Sync + 'static,
        W: Fn(&State) -> watch::Receiver<T> + Send + Sync + 'static,
        E: Fn(T) -> ApiEvent + Clone + Send + Sync + 'static,
    {
        Self {
            subscribe: Arc::new(move |state| values(watch(state)).map(event.clone()).boxed()),
        }
    }

    fn subscribe(&self, state: &State) -> EventStream {
        (self.subscribe)(state)
    }
}

impl fmt::Debug for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Topic").finish_non_exhaustive()
    }
}

/// Every topic the daemon publishes on `/api/events`.
pub fn topics() -> Vec<Topic> {
    vec![
        Topic::watch(State::watch_engine, ApiEvent::Engine),
        Topic::watch(State::watch_metrics, ApiEvent::Metrics),
        Topic::watch(State::watch_apps, ApiEvent::Apps),
        Topic::watch(State::watch_operations, ApiEvent::Operations),
        Topic::watch(State::watch_privileges, ApiEvent::Privileges),
    ]
}

/// Merge every topic into one stream, until `shutdown` becomes `true` or
/// its sender is dropped.
pub fn stream(
    state: &State,
    topics: &[Topic],
    mut shutdown: watch::Receiver<bool>,
) -> impl Stream<Item = ApiEvent> + Send + use<> {
    let merged = stream::select_all(topics.iter().map(|topic| topic.subscribe(state)));
    merged.take_until(async move {
        let _ = shutdown.wait_for(|stop| *stop).await;
    })
}

/// The receiver's current value, then one per change, until the sender is
/// dropped.
fn values<T>(rx: watch::Receiver<T>) -> impl Stream<Item = T> + Send + use<T>
where
    T: Clone + Send + Sync + 'static,
{
    stream::unfold((rx, true), |(mut rx, first)| async move {
        if !first {
            rx.changed().await.ok()?;
        }
        let value = rx.borrow_and_update().clone();
        Some((value, (rx, false)))
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use alchemy_api::{EngineState, EngineStatus};

    fn fake(rx: watch::Receiver<EngineState>) -> Topic {
        Topic::watch(
            move |_| rx.clone(),
            |s| ApiEvent::Engine(EngineStatus::new(s)),
        )
    }

    fn engine(state: EngineState) -> ApiEvent {
        ApiEvent::Engine(EngineStatus::new(state))
    }

    #[tokio::test]
    async fn values_sends_current_then_changes_until_sender_drops() {
        let (tx, rx) = watch::channel(1);
        let mut values = Box::pin(values(rx));
        assert_eq!(values.next().await, Some(1));
        tx.send_replace(2);
        assert_eq!(values.next().await, Some(2));
        drop(tx);
        assert_eq!(values.next().await, None);
    }

    #[tokio::test]
    async fn a_finished_topic_does_not_end_the_others() {
        let (a, a_rx) = watch::channel(EngineState::Starting);
        let (b, b_rx) = watch::channel(EngineState::Ready);
        let (_stop, stop_rx) = watch::channel(false);
        let mut events = Box::pin(stream(&State::new(), &[fake(a_rx), fake(b_rx)], stop_rx));

        let mut first = vec![events.next().await.unwrap(), events.next().await.unwrap()];
        first.sort_by_key(|e| format!("{e:?}"));
        let mut want = vec![engine(EngineState::Starting), engine(EngineState::Ready)];
        want.sort_by_key(|e| format!("{e:?}"));
        assert_eq!(first, want);

        drop(a);
        b.send_replace(EngineState::NotRunning);
        assert_eq!(events.next().await, Some(engine(EngineState::NotRunning)));
        drop(b);
        assert_eq!(events.next().await, None);
    }

    #[tokio::test]
    async fn shutdown_before_connect_sends_nothing() {
        let (_a, a_rx) = watch::channel(EngineState::Ready);
        let (_stop, stop_rx) = watch::channel(true);
        let mut events = Box::pin(stream(&State::new(), &[fake(a_rx)], stop_rx));
        assert_eq!(events.next().await, None);
    }
}
