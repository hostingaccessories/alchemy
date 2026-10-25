//! Topics for the `GET /api/events` stream.
//!
//! A topic is one live value the daemon publishes, such as the engine
//! status. Every connection subscribes to all topics and merges them into
//! one stream of [`ApiEvent`]s. Each topic sends its current value on
//! connect, then again on every change, except one built with
//! [`Topic::changes`], such as the `woke` event, which reports only what
//! happens after connecting.
//!
//! Adding a topic takes an [`ApiEvent`] variant in `alchemy-api` and one
//! entry in [`topics`]. The HTTP handler does not change.

use std::fmt;
use std::sync::Arc;

use alchemy_api::ApiEvent;
use futures_util::future;
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

    /// A topic over a [`watch`] channel of optional values, like
    /// [`Self::watch`], that sends nothing while the value is `None`.
    pub fn watch_some<T, W, E>(watch: W, event: E) -> Self
    where
        T: Clone + Send + Sync + 'static,
        W: Fn(&State) -> watch::Receiver<Option<T>> + Send + Sync + 'static,
        E: Fn(T) -> ApiEvent + Clone + Send + Sync + 'static,
    {
        Self {
            subscribe: Arc::new(move |state| {
                let event = event.clone();
                values(watch(state))
                    .filter_map(move |value| future::ready(value.map(event.clone())))
                    .boxed()
            }),
        }
    }

    /// A topic over a [`watch`] channel that sends only the values set
    /// after the connection subscribed, never the current one: for events
    /// that report something that happened. `event` turns each into an
    /// [`ApiEvent`], or `None` to send nothing.
    ///
    /// The topic ends when the channel's sender is dropped.
    pub fn changes<T, W, E>(watch: W, event: E) -> Self
    where
        T: Clone + Send + Sync + 'static,
        W: Fn(&State) -> watch::Receiver<T> + Send + Sync + 'static,
        E: Fn(T) -> Option<ApiEvent> + Clone + Send + Sync + 'static,
    {
        Self {
            subscribe: Arc::new(move |state| {
                let event = event.clone();
                changes(watch(state))
                    .filter_map(move |value| future::ready(event(value)))
                    .boxed()
            }),
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
        Topic::watch(State::watch_reconcile, ApiEvent::Reconcile),
        Topic::changes(State::watch_last_wake, |woke| {
            woke.map(|woke| ApiEvent::Woke(crate::wake::api_wake(woke)))
        }),
        Topic::watch_some(State::watch_autostart, ApiEvent::Autostart),
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

/// One value per change after `rx` subscribed, until the sender is
/// dropped.
fn changes<T>(rx: watch::Receiver<T>) -> impl Stream<Item = T> + Send + use<T>
where
    T: Clone + Send + Sync + 'static,
{
    stream::unfold(rx, |mut rx| async move {
        rx.changed().await.ok()?;
        let value = rx.borrow_and_update().clone();
        Some((value, rx))
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
    async fn changes_skips_the_current_value_and_what_maps_to_nothing() {
        let (tx, rx) = watch::channel(Some(EngineState::Ready));
        let topic = Topic::changes(move |_| rx.clone(), |s: Option<EngineState>| s.map(engine));
        let mut events = topic.subscribe(&State::new());
        tx.send_replace(None);
        tx.send_replace(Some(EngineState::Starting));
        assert_eq!(events.next().await, Some(engine(EngineState::Starting)));
        tx.send_replace(Some(EngineState::Starting));
        assert_eq!(
            events.next().await,
            Some(engine(EngineState::Starting)),
            "a repeat is sent again"
        );
        drop(tx);
        assert_eq!(events.next().await, None);
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
