//! The reconcile task: keeps the last-seen-running set, and brings apps
//! back after alchemyd starts, the computer wakes, or Docker returns.
//!
//! The decisions are [`LastSeen`] and [`plan_reconcile`] in
//! `alchemy_core::lifecycle`. This task feeds them:
//!
//! - every app's status from [`State::apps`] while the engine's
//!   containers are listed ([`State::engine_link`]), with each change to
//!   the set stored, so it survives a reboot;
//! - each app that no longer exists, which is [forgotten](LastSeen::forget)
//!   so a new app with its name is not started as if it had been running;
//! - a disruption when the engine connection ends, the computer wakes
//!   ([`State::woke`]), or alchemyd stops;
//! - a trigger: `daemon_start` for the first listed connection,
//!   `engine_returned` for each one after, and `wake` for each
//!   [`State::woke`]. Triggers that come before a reconcile can run are
//!   [coalesced](ReconcileTrigger::coalesce) into one.
//!
//! A reconcile waits until the engine is listed and the apps task has
//! read every app again, then starts each app the plan says to as an
//! ordinary `up` operation with the trigger's reason. It runs once per
//! trigger: an app that fails stays down until the next one. The result
//! is in [`State::reconcile`], and each started app's outcome follows its
//! operation.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alchemy_api::apps::{App, AppDirState, AppList};
use alchemy_api::reconcile::{Reconcile, ReconcileApp, ReconcileOutcome, ReconcileStatus};
use alchemy_core::StackName;
use alchemy_core::engine::{Containers, EngineState, Intent};
use alchemy_core::lifecycle::{
    AppPlan, Decision, Disruption, LastSeen, LastSeenChange, ReconcilePlan, ReconcileTrigger,
    STOP_GRACE, plan_reconcile,
};
use alchemy_core::operation::{Action, Operation, OperationState};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::operations::{Operations, StartError};
use crate::store::SharedStore;
use crate::{EngineLink, State};

/// How long a reconcile waits for the apps task to read every app again
/// before it plans with what it has.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(60);

/// How the reconcile task is tuned.
#[derive(Debug, Clone, Copy)]
pub struct ReconcileDeps {
    /// How long Docker must stay up after an app stops for the stop to
    /// count as made by hand. [`STOP_GRACE`] by default.
    pub grace: Duration,
    /// How long to wait for the apps to be read again.
    pub scan_timeout: Duration,
}

impl Default for ReconcileDeps {
    fn default() -> Self {
        Self {
            grace: STOP_GRACE,
            scan_timeout: SCAN_TIMEOUT,
        }
    }
}

impl ReconcileDeps {
    /// The same, with a stop counting after `grace`.
    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }
}

/// Keep the last-seen set in `store` and reconcile the apps in `state`
/// through `operations` until `shutdown` becomes `true`.
pub async fn run(
    deps: ReconcileDeps,
    state: State,
    store: SharedStore,
    operations: Operations,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut task = Task::new(deps, state, store, operations).await;
    let mut link = task.state.watch_engine_link();
    let mut wakes = task.state.watch_wakes();
    let mut apps = task.state.watch_apps();
    let mut ops = task.state.watch_operations();
    let first = *link.borrow_and_update();
    task.on_link(first);
    loop {
        if task.linked && task.pending.is_some() {
            task.reconcile(&mut shutdown).await;
            task.observe().await;
        }
        let settle = task.seen.next_settle().map(|at| task.origin + at);
        tokio::select! {
            biased;
            () = stopping(&mut shutdown) => {
                let now = task.now();
                task.seen.disrupted(Disruption::DaemonStopping, now);
                return;
            }
            Ok(()) = link.changed() => {
                let current = *link.borrow_and_update();
                task.on_link(current);
            }
            Ok(()) = wakes.changed() => {
                wakes.borrow_and_update();
                task.on_wake();
            }
            Ok(()) = apps.changed() => {
                apps.borrow_and_update();
                task.observe().await;
            }
            Ok(()) = ops.changed() => {
                ops.borrow_and_update();
                task.follow_operations();
            }
            () = sleep_until(settle), if settle.is_some() => task.settle().await,
        }
    }
}

async fn stopping(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

async fn sleep_until(at: Option<Instant>) {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    }
}

struct Task {
    deps: ReconcileDeps,
    state: State,
    store: SharedStore,
    operations: Operations,
    seen: LastSeen,
    origin: Instant,
    connection: u64,
    linked: bool,
    pending: Option<ReconcileTrigger>,
    roots: BTreeMap<StackName, String>,
}

impl Task {
    async fn new(
        deps: ReconcileDeps,
        state: State,
        store: SharedStore,
        operations: Operations,
    ) -> Self {
        let persisted = match store.last_seen().await {
            Ok(names) => names,
            Err(error) => {
                tracing::warn!(%error, "cannot read the apps last seen running; none will be brought back");
                Vec::new()
            }
        };
        let running = persisted
            .into_iter()
            .filter_map(|name| StackName::new(name).ok());
        Self {
            seen: LastSeen::with_grace(running, deps.grace),
            deps,
            state,
            store,
            operations,
            origin: Instant::now(),
            connection: 0,
            linked: false,
            pending: None,
            roots: BTreeMap::new(),
        }
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn trigger(&mut self, trigger: ReconcileTrigger) {
        self.pending = Some(
            self.pending
                .map_or(trigger, |pending| pending.coalesce(trigger)),
        );
    }

    fn on_link(&mut self, link: EngineLink) {
        let now = self.now();
        let new_connection = link.listed && link.connection != self.connection;
        if self.linked && (!link.listed || new_connection) {
            self.seen.disrupted(Disruption::EngineGone, now);
            self.linked = false;
        }
        if new_connection {
            let trigger = if self.connection == 0 {
                ReconcileTrigger::DaemonStart
            } else {
                ReconcileTrigger::EngineReturned
            };
            self.connection = link.connection;
            self.linked = true;
            self.trigger(trigger);
        }
    }

    fn on_wake(&mut self) {
        let now = self.now();
        self.seen.disrupted(Disruption::Woke, now);
        self.trigger(ReconcileTrigger::Wake);
    }

    /// Forget the apps that no longer exist, then note every app's
    /// status, while the engine's containers are listed.
    async fn observe(&mut self) {
        let list = self.state.apps();
        let forgotten: Vec<LastSeenChange> = gone(self.seen.running(), &list, &self.roots)
            .iter()
            .filter_map(|app| self.seen.forget(app))
            .collect();
        self.persist(forgotten).await;
        self.roots.extend(
            list.apps
                .iter()
                .map(|app| (app.name.clone(), app.root.clone())),
        );
        if !self.linked || self.state.engine().state != EngineState::Ready {
            return;
        }
        let now = self.now();
        let changes: Vec<LastSeenChange> = list
            .apps
            .iter()
            .filter_map(|app| self.seen.observe(&app.name, app.status, now))
            .collect();
        self.persist(changes).await;
    }

    async fn settle(&mut self) {
        let now = self.now();
        let changes = self.seen.settle(now);
        self.persist(changes).await;
    }

    async fn persist(&self, changes: Vec<LastSeenChange>) {
        for change in changes {
            let (app, running) = match &change {
                LastSeenChange::Added(app) => (app, true),
                LastSeenChange::Removed(app) => (app, false),
            };
            tracing::debug!(%app, running, "last seen running");
            if let Err(error) = self.store.set_last_seen(app.as_str(), running).await {
                tracing::warn!(%error, %app, "cannot store whether the app was last seen running");
            }
        }
    }

    /// Wait for the apps to be read again, then plan and start.
    async fn reconcile(&mut self, shutdown: &mut watch::Receiver<bool>) {
        let connection = self.connection;
        let generation = self.state.rescan_apps();
        tokio::select! {
            biased;
            () = stopping(shutdown) => return,
            () = self.state.app_scan_done(generation) => {}
            () = tokio::time::sleep(self.deps.scan_timeout) => {
                tracing::warn!("the apps weren't all read again in time; reconciling with what is known");
            }
        }
        let link = self.state.engine_link();
        if !link.listed || link.connection != connection {
            return;
        }
        let Some(trigger) = self.pending.take() else {
            return;
        };
        self.observe().await;
        let apps = self.state.apps().apps;
        let intents = intents(&apps, &self.state.containers());
        let plan = plan_reconcile(
            trigger,
            apps.iter().map(|app| (app.name.clone(), app.status)),
            &intents,
            self.seen.running(),
        );
        let mut started = BTreeMap::new();
        for app in plan.to_start() {
            let result = self
                .operations
                .start_because(app.as_str(), Action::Up, plan.start_reason())
                .await;
            started.insert(app.clone(), result);
        }
        let reconcile = report(&plan, &mut started, now_secs());
        tracing::info!(
            %trigger,
            started = reconcile.apps.iter().filter(|a| a.outcome == ReconcileOutcome::Starting).count(),
            failed = reconcile.apps.iter().filter(|a| a.outcome == ReconcileOutcome::Failed).count(),
            "reconciled apps"
        );
        self.state.set_reconcile(ReconcileStatus {
            last: Some(reconcile),
        });
        self.follow_operations();
    }

    /// Mark each app whose operation has ended as started or failed.
    fn follow_operations(&self) {
        let mut status = self.state.reconcile();
        let Some(last) = status.last.as_mut() else {
            return;
        };
        if follow(last, &self.state.operations().operations) {
            self.state.set_reconcile(status);
        }
    }
}

/// The apps in `running` that no longer exist in `list`, given the folder
/// each app was last found in, by name, in `roots`.
///
/// An app is gone when its folder is no longer searched, or was read and
/// no longer holds it. One whose folder couldn't be read, such as a
/// drive that isn't mounted, is kept. One never seen this run, as after a
/// restart, is gone only when every folder was read. Nothing is gone
/// before the folders are listed.
fn gone(
    running: &BTreeSet<StackName>,
    list: &AppList,
    roots: &BTreeMap<StackName, String>,
) -> Vec<StackName> {
    if list.dirs.is_empty() {
        return Vec::new();
    }
    let read = |path: &str| {
        list.dirs
            .iter()
            .find(|dir| dir.path == path)
            .is_none_or(|dir| dir.state == AppDirState::Ok)
    };
    let all_read = list.dirs.iter().all(|dir| dir.state == AppDirState::Ok);
    running
        .iter()
        .filter(|name| !list.apps.iter().any(|app| &app.name == *name))
        .filter(|name| roots.get(*name).map_or(all_read, |root| read(root)))
        .cloned()
        .collect()
}

/// Alchemy's last intent for each app, by name, from its Compose
/// project's intent in `containers`.
fn intents(apps: &[App], containers: &Containers) -> BTreeMap<StackName, Intent> {
    apps.iter()
        .filter_map(|app| {
            let project = app.project.as_deref()?;
            Some((app.name.clone(), *containers.intent(project)?))
        })
        .collect()
}

/// The API's view of `plan`, given how starting each app went.
fn report(
    plan: &ReconcilePlan,
    started: &mut BTreeMap<StackName, Result<Operation, StartError>>,
    at: i64,
) -> Reconcile {
    let apps = plan
        .apps
        .iter()
        .map(|AppPlan { app, decision }| {
            let entry = |outcome, operation, reason, message| ReconcileApp {
                app: app.clone(),
                outcome,
                operation,
                reason,
                message,
            };
            match (decision, started.remove(app)) {
                (Decision::Skip { reason }, _) => entry(
                    ReconcileOutcome::Skipped,
                    None,
                    Some(*reason),
                    Some(reason.message().to_owned()),
                ),
                (Decision::Start, Some(Ok(operation))) => {
                    entry(ReconcileOutcome::Starting, Some(operation.id), None, None)
                }
                (Decision::Start, Some(Err(error))) => entry(
                    ReconcileOutcome::Failed,
                    None,
                    None,
                    Some(error.to_string()),
                ),
                (Decision::Start, None) => entry(
                    ReconcileOutcome::Failed,
                    None,
                    None,
                    Some("Alchemy didn't get to start it".to_owned()),
                ),
            }
        })
        .collect();
    Reconcile {
        trigger: plan.trigger,
        at,
        reason: plan.start_reason().to_owned(),
        apps,
    }
}

/// Move each app in `reconcile` still starting whose operation in
/// `operations` has ended to started or failed. Returns whether any did.
fn follow(reconcile: &mut Reconcile, operations: &[Operation]) -> bool {
    let mut changed = false;
    for app in &mut reconcile.apps {
        if app.outcome != ReconcileOutcome::Starting {
            continue;
        }
        let Some(operation) = app
            .operation
            .as_deref()
            .and_then(|id| operations.iter().find(|op| op.id == id))
        else {
            continue;
        };
        match operation.state {
            OperationState::Running => continue,
            OperationState::Succeeded => app.outcome = ReconcileOutcome::Started,
            state => {
                app.outcome = ReconcileOutcome::Failed;
                app.message = Some(
                    operation
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("its operation was {}", state.as_str())),
                );
            }
        }
        changed = true;
    }
    changed
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
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
    use alchemy_core::lifecycle::SkipReason;
    use alchemy_core::operation::{StepCommand, Tracker};

    use super::*;

    fn name(s: &str) -> StackName {
        StackName::new(s).unwrap()
    }

    fn operation(id: &str, app: &str, state: OperationState, error: Option<&str>) -> Operation {
        let mut operation = Tracker::new(id, name(app), Action::Up, &[StepCommand::Up], 1)
            .operation()
            .clone();
        operation.state = state;
        operation.error = error.map(str::to_owned);
        operation
    }

    fn plan() -> ReconcilePlan {
        ReconcilePlan {
            trigger: ReconcileTrigger::Wake,
            apps: vec![
                AppPlan {
                    app: name("blog"),
                    decision: Decision::Start,
                },
                AppPlan {
                    app: name("shop"),
                    decision: Decision::Start,
                },
                AppPlan {
                    app: name("wiki"),
                    decision: Decision::Skip {
                        reason: SkipReason::StopIntent,
                    },
                },
            ],
        }
    }

    fn dir(path: &str, state: AppDirState) -> alchemy_api::apps::AppDir {
        alchemy_api::apps::AppDir {
            path: path.into(),
            home: false,
            state,
            message: None,
            skipped: Vec::new(),
        }
    }

    #[test]
    fn finds_apps_that_are_gone() {
        use AppDirState::{Missing, Ok, Unreadable};
        type Case = (&'static str, Vec<(&'static str, AppDirState)>, bool, bool);
        let blog = name("blog");
        let roots = BTreeMap::from([(blog.clone(), "/apps".to_owned())]);
        let none = BTreeMap::new();
        let cases: [Case; 7] = [
            ("its folder was read", vec![("/apps", Ok)], true, true),
            (
                "its folder is no longer searched",
                vec![("/other", Ok)],
                true,
                true,
            ),
            (
                "its folder can't be read",
                vec![("/apps", Unreadable)],
                true,
                false,
            ),
            (
                "its folder went missing",
                vec![("/apps", Missing)],
                true,
                false,
            ),
            (
                "not seen this run, all read",
                vec![("/apps", Ok)],
                false,
                true,
            ),
            (
                "not seen this run, one unread",
                vec![("/apps", Ok), ("/usb", Missing)],
                false,
                false,
            ),
            ("nothing listed yet", vec![], true, false),
        ];
        for (what, dirs, known, want) in cases {
            let list = AppList {
                dirs: dirs.into_iter().map(|(p, s)| dir(p, s)).collect(),
                ..AppList::default()
            };
            let running = BTreeSet::from([blog.clone()]);
            let got = gone(&running, &list, if known { &roots } else { &none });
            assert_eq!(got.contains(&blog), want, "{what}");
        }
    }

    #[test]
    fn reports_each_app() {
        let mut started = BTreeMap::from([
            (
                name("blog"),
                Ok(operation("op1", "blog", OperationState::Running, None)),
            ),
            (
                name("shop"),
                Err(StartError::DockerNotReady {
                    name: "shop".into(),
                    action: Action::Up,
                }),
            ),
        ]);
        let reconcile = report(&plan(), &mut started, 42);
        assert_eq!(reconcile.trigger, ReconcileTrigger::Wake);
        assert_eq!(reconcile.at, 42);
        assert_eq!(reconcile.reason, "was running before the computer slept");
        let got: Vec<_> = reconcile
            .apps
            .iter()
            .map(|a| {
                (
                    a.app.as_str(),
                    a.outcome,
                    a.operation.as_deref(),
                    a.reason,
                    a.message.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("blog", ReconcileOutcome::Starting, Some("op1"), None, None),
                (
                    "shop",
                    ReconcileOutcome::Failed,
                    None,
                    None,
                    Some("Docker isn't running, so Alchemy can't up shop")
                ),
                (
                    "wiki",
                    ReconcileOutcome::Skipped,
                    None,
                    Some(SkipReason::StopIntent),
                    Some("you stopped it in Alchemy")
                ),
            ]
        );
    }

    #[test]
    fn follows_each_operation_to_its_end() {
        let starting = |id: &str| ReconcileApp {
            app: name(id),
            outcome: ReconcileOutcome::Starting,
            operation: Some(id.to_owned()),
            reason: None,
            message: None,
        };
        let mut reconcile = Reconcile {
            trigger: ReconcileTrigger::DaemonStart,
            at: 1,
            reason: String::new(),
            apps: vec![starting("a"), starting("b"), starting("c"), starting("d")],
        };
        let operations = [
            operation("a", "a", OperationState::Running, None),
            operation("b", "b", OperationState::Succeeded, None),
            operation("c", "c", OperationState::Failed, Some("port 80 is taken")),
            operation("d", "d", OperationState::Interrupted, None),
        ];
        assert!(follow(&mut reconcile, &operations));
        let got: Vec<_> = reconcile
            .apps
            .iter()
            .map(|a| (a.outcome, a.message.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                (ReconcileOutcome::Starting, None),
                (ReconcileOutcome::Started, None),
                (ReconcileOutcome::Failed, Some("port 80 is taken")),
                (
                    ReconcileOutcome::Failed,
                    Some("its operation was interrupted")
                ),
            ]
        );
        assert!(!follow(&mut reconcile, &operations));
    }
}
