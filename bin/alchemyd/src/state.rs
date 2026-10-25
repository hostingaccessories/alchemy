//! Live daemon state, written by background tasks such as the engine
//! monitor and read by both front doors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alchemy_api::MetricsResponse;
use alchemy_api::OperationList;
use alchemy_api::apps::{AppList, AppSettings};
use alchemy_api::network::{NetworkReport, NetworkSettings};
use alchemy_api::privileges::PrivilegeList;
use alchemy_api::reconcile::ReconcileStatus;
use alchemy_core::engine::{Containers, EngineState, EngineStatus};
use alchemy_core::route::EngineFacts;
use tokio::sync::watch;

use crate::apps::ConfigOutcome;
use crate::logs::LogStreams;
use crate::routing::{MdnsStatus, Routing};

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
    engine_facts: watch::Sender<Option<EngineFacts>>,
    containers: watch::Sender<Containers>,
    metrics: watch::Sender<MetricsResponse>,
    apps: watch::Sender<AppList>,
    app_configs: watch::Sender<BTreeMap<PathBuf, ConfigOutcome>>,
    app_settings: watch::Sender<AppSettings>,
    app_rescans: watch::Sender<u64>,
    app_scans: watch::Sender<u64>,
    operations: watch::Sender<OperationList>,
    network: watch::Sender<NetworkReport>,
    network_settings: watch::Sender<NetworkSettings>,
    mdns: watch::Sender<MdnsStatus>,
    routing: watch::Sender<Routing>,
    privileges: watch::Sender<PrivilegeList>,
    engine_link: watch::Sender<EngineLink>,
    wakes: watch::Sender<u64>,
    reconcile: watch::Sender<ReconcileStatus>,
    log_streams: LogStreams,
}

/// Whether the engine monitor's containers can be trusted: which
/// connection to the engine this is, and whether its first listing is in
/// [`State::containers`].
///
/// `connection` rises each time a connection's first listing lands, so
/// the first is 1. `listed` goes back to `false` when that connection
/// ends, such as when Docker quits or its event stream breaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineLink {
    /// The current or last connection, counting from 1, or 0 before any.
    pub connection: u64,
    /// Whether that connection is up and has listed every container.
    pub listed: bool,
}

impl State {
    /// Fresh state: the engine is reported as not running, with no
    /// containers, until the engine monitor's first probe says otherwise,
    /// and there are no metrics until the collector's first sample.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                engine: watch::Sender::new(EngineStatus::new(EngineState::NotRunning)),
                engine_facts: watch::Sender::new(None),
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
                network: watch::Sender::new(NetworkReport::default()),
                network_settings: watch::Sender::new(NetworkSettings::default()),
                mdns: watch::Sender::new(MdnsStatus::default()),
                routing: watch::Sender::new(Routing::default()),
                privileges: watch::Sender::new(PrivilegeList::default()),
                engine_link: watch::Sender::new(EngineLink::default()),
                wakes: watch::Sender::new(0),
                reconcile: watch::Sender::new(ReconcileStatus::default()),
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

    /// The facts about the ready engine that decide its
    /// [`Reachability`](alchemy_core::route::Reachability), or `None`
    /// while it is not ready.
    pub fn engine_facts(&self) -> Option<EngineFacts> {
        *self.inner.engine_facts.borrow()
    }

    /// Subscribe to changes to [`Self::engine_facts`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_engine_facts(&self) -> watch::Receiver<Option<EngineFacts>> {
        self.inner.engine_facts.subscribe()
    }

    /// Replace the engine facts. Returns whether they changed;
    /// subscribers are only notified if they did.
    pub fn set_engine_facts(&self, facts: Option<EngineFacts>) -> bool {
        send_if_changed(&self.inner.engine_facts, facts)
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

    /// Subscribe to changes to [`Self::app_configs`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_app_configs(&self) -> watch::Receiver<BTreeMap<PathBuf, ConfigOutcome>> {
        self.inner.app_configs.subscribe()
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

    /// The latest check of Docker's networks against the host's.
    pub fn network(&self) -> NetworkReport {
        self.inner.network.borrow().clone()
    }

    /// Subscribe to changes to [`Self::network`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_network(&self) -> watch::Receiver<NetworkReport> {
        self.inner.network.subscribe()
    }

    /// Replace the network check. Returns whether it changed; subscribers
    /// are only notified if it did.
    pub fn set_network(&self, network: NetworkReport) -> bool {
        send_if_changed(&self.inner.network, network)
    }

    /// The network settings in effect.
    pub fn network_settings(&self) -> NetworkSettings {
        *self.inner.network_settings.borrow()
    }

    /// Subscribe to changes to [`Self::network_settings`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_network_settings(&self) -> watch::Receiver<NetworkSettings> {
        self.inner.network_settings.subscribe()
    }

    /// Replace the network settings in effect. Returns whether they
    /// changed; subscribers are only notified if they did.
    pub fn set_network_settings(&self, settings: NetworkSettings) -> bool {
        send_if_changed(&self.inner.network_settings, settings)
    }

    /// What the mDNS announcer last reported: the names it holds, those
    /// another device answers for, and how it is doing.
    pub fn mdns(&self) -> MdnsStatus {
        self.inner.mdns.borrow().clone()
    }

    /// Subscribe to changes to [`Self::mdns`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_mdns(&self) -> watch::Receiver<MdnsStatus> {
        self.inner.mdns.subscribe()
    }

    /// Replace what the mDNS announcer reports. Returns whether it
    /// changed; subscribers are only notified if it did.
    pub fn set_mdns(&self, status: MdnsStatus) -> bool {
        send_if_changed(&self.inner.mdns, status)
    }

    /// What the routing task last decided: each app's web addresses and
    /// route problems, and the proxy's listeners.
    pub fn routing(&self) -> Routing {
        self.inner.routing.borrow().clone()
    }

    /// Subscribe to changes to [`Self::routing`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_routing(&self) -> watch::Receiver<Routing> {
        self.inner.routing.subscribe()
    }

    /// Replace the routing. Returns whether it changed; subscribers are
    /// only notified if it did.
    pub fn set_routing(&self, routing: Routing) -> bool {
        send_if_changed(&self.inner.routing, routing)
    }

    /// Every privilege and where it stands, as
    /// [`Privileges`](crate::privileges::Privileges) last read them. Empty
    /// until the first read.
    pub fn privileges(&self) -> PrivilegeList {
        self.inner.privileges.borrow().clone()
    }

    /// Subscribe to changes to [`Self::privileges`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_privileges(&self) -> watch::Receiver<PrivilegeList> {
        self.inner.privileges.subscribe()
    }

    /// Replace the privileges. Returns whether they changed; subscribers
    /// are only notified if they did.
    pub fn set_privileges(&self, privileges: PrivilegeList) -> bool {
        send_if_changed(&self.inner.privileges, privileges)
    }

    /// Whether the engine's containers are listed, and on which
    /// connection.
    pub fn engine_link(&self) -> EngineLink {
        *self.inner.engine_link.borrow()
    }

    /// Subscribe to changes to [`Self::engine_link`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_engine_link(&self) -> watch::Receiver<EngineLink> {
        self.inner.engine_link.subscribe()
    }

    /// Record that a new connection to the engine has put its first
    /// listing in [`Self::containers`]. Call it after the listing.
    pub fn engine_listed(&self) {
        self.inner.engine_link.send_modify(|link| {
            link.connection = link.connection.saturating_add(1);
            link.listed = true;
        });
    }

    /// Record that the engine connection ended, or that the engine isn't
    /// ready. Returns whether a listed connection ended.
    pub fn engine_lost(&self) -> bool {
        self.inner.engine_link.send_if_modified(|link| {
            let was = link.listed;
            link.listed = false;
            was
        })
    }

    /// How many wakes from sleep have been noticed since alchemyd started.
    pub fn wakes(&self) -> u64 {
        *self.inner.wakes.borrow()
    }

    /// Subscribe to [`Self::woke`], by count.
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_wakes(&self) -> watch::Receiver<u64> {
        self.inner.wakes.subscribe()
    }

    /// Record that the computer woke from sleep. The reconcile task
    /// brings back what was running, once the engine is listed.
    pub fn woke(&self) {
        self.inner.wakes.send_modify(|n| *n = n.saturating_add(1));
    }

    /// The last reconcile, and how each app it started is doing.
    pub fn reconcile(&self) -> ReconcileStatus {
        self.inner.reconcile.borrow().clone()
    }

    /// Subscribe to changes to [`Self::reconcile`].
    ///
    /// The receiver starts with the current value marked as seen.
    pub fn watch_reconcile(&self) -> watch::Receiver<ReconcileStatus> {
        self.inner.reconcile.subscribe()
    }

    /// Replace the last reconcile. Returns whether it changed;
    /// subscribers are only notified if it did.
    pub fn set_reconcile(&self, reconcile: ReconcileStatus) -> bool {
        send_if_changed(&self.inner.reconcile, reconcile)
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
    fn engine_link_counts_connections_and_notes_losses() {
        let state = State::new();
        let rx = state.watch_engine_link();
        assert!(!state.engine_lost());
        assert!(!rx.has_changed().unwrap());
        state.engine_listed();
        assert_eq!(
            state.engine_link(),
            EngineLink {
                connection: 1,
                listed: true
            }
        );
        assert!(state.engine_lost());
        assert!(!state.engine_lost());
        state.engine_listed();
        assert_eq!(
            state.engine_link(),
            EngineLink {
                connection: 2,
                listed: true
            }
        );
    }

    #[test]
    fn wakes_count_up() {
        let state = State::new();
        let mut rx = state.watch_wakes();
        state.woke();
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), 1);
        state.woke();
        assert_eq!(state.wakes(), 2);
    }

    #[test]
    fn clones_share_values() {
        let state = State::new();
        let other = state.clone();
        other.set_engine(EngineStatus::new(EngineState::Ready));
        assert_eq!(state.engine().state, EngineState::Ready);
    }
}
