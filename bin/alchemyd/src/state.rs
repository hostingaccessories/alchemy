//! Live daemon state, written by background tasks such as the engine
//! monitor and read by both front doors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alchemy_api::MetricsResponse;
use alchemy_api::OperationList;
use alchemy_api::apps::{AppList, AppSettings};
use alchemy_core::engine::{Containers, EngineState, EngineStatus};
use tokio::sync::watch;

use crate::apps::ConfigOutcome;
use crate::logs::LogStreams;

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
    apps: watch::Sender<AppList>,
    app_configs: watch::Sender<BTreeMap<PathBuf, ConfigOutcome>>,
    app_settings: watch::Sender<AppSettings>,
    app_rescans: watch::Sender<u64>,
    app_scans: watch::Sender<u64>,
    operations: watch::Sender<OperationList>,
    log_streams: LogStreams,
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
                apps: watch::Sender::new(AppList::default()),
                app_configs: watch::Sender::new(BTreeMap::new()),
                app_settings: watch::Sender::new(AppSettings {
                    home: None,
                    default_home: None,
                    extra: Vec::new(),
                }),
                app_rescans: watch::Sender::new(0),
                app_scans: watch::Sender::new(0),
                operations: watch::Sender::new(OperationList::default()),
                log_streams: LogStreams::default(),
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
    /// The current app list.
    pub fn apps(&self) -> AppList {
        self.inner.apps.borrow().clone()
    }

    /// Subscribe to app list changes.
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_apps(&self) -> watch::Receiver<AppList> {
        self.inner.apps.subscribe()
    }

    /// Replace the app list. Returns whether it changed; subscribers are
    /// only notified if it did.
    pub fn set_apps(&self, apps: AppList) -> bool {
        send_if_changed(&self.inner.apps, apps)
    }

    /// What reading each app's configuration last came to, by app
    /// folder: the ports and networks Compose declared, without running
    /// Compose again.
    pub fn app_configs(&self) -> BTreeMap<PathBuf, ConfigOutcome> {
        self.inner.app_configs.borrow().clone()
    }

    /// What reading the configuration of the app in `dir` last came to,
    /// or `None` if it has not been read.
    pub fn app_config(&self, dir: &Path) -> Option<ConfigOutcome> {
        self.inner.app_configs.borrow().get(dir).cloned()
    }

    /// Replace every app's configuration outcome. Returns whether they
    /// changed.
    pub fn set_app_configs(&self, configs: BTreeMap<PathBuf, ConfigOutcome>) -> bool {
        send_if_changed(&self.inner.app_configs, configs)
    }

    /// The app folder settings in effect.
    pub fn app_settings(&self) -> AppSettings {
        self.inner.app_settings.borrow().clone()
    }

    /// Replace the app folder settings in effect. Returns whether they
    /// changed.
    pub fn set_app_settings(&self, settings: AppSettings) -> bool {
        send_if_changed(&self.inner.app_settings, settings)
    }

    /// Ask the apps task to reload its settings, list every app folder,
    /// and read every app's configuration again, without waiting for its
    /// next scan.
    ///
    /// Returns the request's generation, which rises with every request,
    /// to pass to [`Self::app_scan_done`].
    pub fn rescan_apps(&self) -> u64 {
        let mut generation = 0;
        self.inner.app_rescans.send_modify(|n| {
            *n = n.saturating_add(1);
            generation = *n;
        });
        generation
    }

    /// Subscribe to [`Self::rescan_apps`] requests, by generation.
    pub fn watch_app_rescans(&self) -> watch::Receiver<u64> {
        self.inner.app_rescans.subscribe()
    }

    /// Record that [`Self::apps`] holds the results of every rescan up to
    /// `generation`: the folders listed and every app read again after it
    /// was asked for. Generations only rise, so an older one changes
    /// nothing. Returns whether it changed.
    pub fn set_app_scan_done(&self, generation: u64) -> bool {
        self.inner.app_scans.send_if_modified(|done| {
            if generation > *done {
                *done = generation;
                true
            } else {
                false
            }
        })
    }

    /// Wait until [`Self::apps`] holds the results of the rescan
    /// `generation`, as [`Self::rescan_apps`] returned it. This never
    /// times out on its own: the apps task may not be running.
    pub async fn app_scan_done(&self, generation: u64) {
        let mut done = self.inner.app_scans.subscribe();
        let _ = done.wait_for(|done| *done >= generation).await;
    }

    /// Every operation held in memory, newest first.
    pub fn operations(&self) -> OperationList {
        self.inner.operations.borrow().clone()
    }

    /// Subscribe to changes to [`Self::operations`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_operations(&self) -> watch::Receiver<OperationList> {
        self.inner.operations.subscribe()
    }

    /// Replace the operations held in memory. Returns whether they
    /// changed; subscribers are only notified if they did.
    pub fn set_operations(&self, operations: OperationList) -> bool {
        send_if_changed(&self.inner.operations, operations)
    }

    /// The log streams open on either listener, held to
    /// [`MAX_STREAMS`](crate::logs::MAX_STREAMS). Unlike the parts above,
    /// it is a count to take a place in, not a value to watch.
    pub fn log_streams(&self) -> &LogStreams {
        &self.inner.log_streams
    }
}

fn send_if_changed<T: PartialEq>(sender: &watch::Sender<T>, value: T) -> bool {
    sender.send_if_modified(|current| {
        if *current == value {
            false
        } else {
            *current = value;
            true
        }
    })
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
    fn set_apps_notifies_only_on_change() {
        let state = State::new();
        let rx = state.watch_apps();
        assert!(!state.set_apps(AppList::default()));
        assert!(!rx.has_changed().unwrap());
        let mut apps = AppList::default();
        apps.unmanaged.push(alchemy_api::apps::UnmanagedApp {
            project: "legacy".into(),
            dir: None,
            status: alchemy_api::apps::StackStatus::Running,
            services: vec![],
            stop_reason: alchemy_api::apps::StopReason::Running,
        });
        assert!(state.set_apps(apps.clone()));
        assert!(rx.has_changed().unwrap());
        assert_eq!(state.apps(), apps);
    }

    #[test]
    fn rescans_wake_subscribers() {
        let state = State::new();
        let mut rx = state.watch_app_rescans();
        assert_eq!(state.rescan_apps(), 1);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), 1);
        assert_eq!(state.rescan_apps(), 2);
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn scans_done_only_rise() {
        let state = State::new();
        let cases = [(2, true), (1, false), (2, false), (3, true)];
        for (generation, want) in cases {
            assert_eq!(state.set_app_scan_done(generation), want, "{generation}");
        }
    }

    #[tokio::test]
    async fn waits_for_a_scan_to_be_done() {
        let state = State::new();
        let ticket = state.rescan_apps();
        let waiting = tokio::spawn({
            let state = state.clone();
            async move { state.app_scan_done(ticket).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        state.set_app_scan_done(ticket);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state.app_scan_done(ticket - 1),
        )
        .await
        .unwrap();
    }

    #[test]
    fn clones_share_values() {
        let state = State::new();
        let other = state.clone();
        other.set_engine(EngineStatus::new(EngineState::Ready));
        assert_eq!(state.engine().state, EngineState::Ready);
    }
}
