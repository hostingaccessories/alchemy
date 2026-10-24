//! Live daemon state, written by background tasks such as the engine
//! monitor and read by both front doors.

use std::sync::Arc;

use alchemy_api::MetricsResponse;
use alchemy_core::engine::{Containers, EngineState, EngineStatus};
use tokio::sync::watch;

/// The daemon's live state. Cheap to clone: every clone shares the same
/// values.
///
/// Each part lives in a [`watch`] channel. Read the current value with
/// its getter, or subscribe with its `watch_*` method to be woken on
/// changes. Writers notify subscribers only when the value actually
/// changed.
#[derive(Debug, Clone)]
pub struct State {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    engine: watch::Sender<EngineStatus>,
    containers: watch::Sender<Containers>,
    metrics: watch::Sender<MetricsResponse>,
}

impl State {
    /// Fresh state: the engine is reported as not running, with no
    /// containers, until the engine monitor's first probe says otherwise,
    /// and there are no metrics until the collector's first sample.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                engine: watch::Sender::new(EngineStatus::new(EngineState::NotRunning)),
                containers: watch::Sender::new(Containers::new()),
                metrics: watch::Sender::new(MetricsResponse {
                    interval: u32::try_from(crate::metrics::INTERVAL.as_secs()).unwrap_or(u32::MAX),
                    ..MetricsResponse::default()
                }),
            }),
        }
    }

    /// The current engine status.
    pub fn engine(&self) -> EngineStatus {
        self.inner.engine.borrow().clone()
    }

    /// Subscribe to engine status changes.
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_engine(&self) -> watch::Receiver<EngineStatus> {
        self.inner.engine.subscribe()
    }

    /// Replace the engine status. Returns whether it changed; subscribers
    /// are only notified if it did.
    pub fn set_engine(&self, status: EngineStatus) -> bool {
        self.inner.engine.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status;
                true
            }
        })
    }

    /// A snapshot of every known container.
    pub fn containers(&self) -> Containers {
        self.inner.containers.borrow().clone()
    }

    /// Subscribe to container changes.
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_containers(&self) -> watch::Receiver<Containers> {
        self.inner.containers.subscribe()
    }

    /// Modify the containers in place. `update` returns whether it changed
    /// anything, as [`Containers::apply`] and [`Containers::replace_all`]
    /// do; subscribers are only notified if it did.
    pub fn update_containers(&self, update: impl FnOnce(&mut Containers) -> bool) -> bool {
        self.inner.containers.send_if_modified(update)
    }

    /// The latest metrics sample.
    pub fn metrics(&self) -> MetricsResponse {
        self.inner.metrics.borrow().clone()
    }

    /// Subscribe to new metrics samples.
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_metrics(&self) -> watch::Receiver<MetricsResponse> {
        self.inner.metrics.subscribe()
    }

    /// Replace the latest metrics. Returns whether they changed;
    /// subscribers are only notified if they did.
    pub fn set_metrics(&self, metrics: MetricsResponse) -> bool {
        self.inner.metrics.send_if_modified(|current| {
            if *current == metrics {
                false
            } else {
                *current = metrics;
                true
            }
        })
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_core::engine::{ContainerAction, ContainerEvent, ContainerState, ContainerStatus};

    use super::*;

    #[test]
    fn set_engine_notifies_only_on_change() {
        let state = State::new();
        let mut rx = state.watch_engine();
        let cases = [
            (EngineState::NotRunning, false),
            (EngineState::Starting, true),
            (EngineState::Starting, false),
            (EngineState::Ready, true),
        ];
        for (next, want) in cases {
            assert_eq!(state.set_engine(EngineStatus::new(next)), want, "{next:?}");
            assert_eq!(rx.has_changed().unwrap(), want, "{next:?}");
            rx.mark_unchanged();
            assert_eq!(state.engine().state, next);
        }
    }

    #[test]
    fn update_containers_notifies_only_on_change() {
        type Update<'a> = Box<dyn FnOnce(&mut Containers) -> bool + 'a>;
        let state = State::new();
        let mut rx = state.watch_containers();
        let running = ContainerStatus::new("abc", "web", ContainerState::Running);
        let cases: [(&str, Update, bool); 4] = [
            ("seed", Box::new(|c| c.replace_all([running.clone()])), true),
            (
                "same",
                Box::new(|c| c.replace_all([running.clone()])),
                false,
            ),
            (
                "start again",
                Box::new(|c| c.apply(&ContainerEvent::new("abc", ContainerAction::Start))),
                false,
            ),
            (
                "stop",
                Box::new(|c| c.apply(&ContainerEvent::new("abc", ContainerAction::Stop))),
                true,
            ),
        ];
        for (name, update, want) in cases {
            assert_eq!(state.update_containers(update), want, "{name}");
            assert_eq!(rx.has_changed().unwrap(), want, "{name}");
            rx.mark_unchanged();
        }
        assert_eq!(
            state.containers().get("abc").unwrap().state,
            ContainerState::Exited
        );
    }

    #[test]
    fn clones_share_values() {
        let state = State::new();
        let other = state.clone();
        other.set_engine(EngineStatus::new(EngineState::Ready));
        assert_eq!(state.engine().state, EngineState::Ready);
    }
}
