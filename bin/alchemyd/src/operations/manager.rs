//! [`Operations`]: the daemon's running and recent operations.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alchemy_api::OperationList;
use alchemy_api::apps::AppProblemKind;
use alchemy_api::operations::{ActionPlan, OperationEvent};
use alchemy_core::StackName;
use alchemy_core::engine::EngineState;
use alchemy_core::operation::{
    Action, OUTPUT_LINES, Operation, OperationState, Outcome, OutputBuffer, Tracker, intent_action,
};
use alchemy_core::ports::{PortConflict, find_conflicts};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::convert;
use super::runner::OperationRunner;
use crate::State;
use crate::apps;
use crate::compose::{
    CancelToken, Canceller, ComposeError, ComposeEvent, ComposePlan, ComposeProjectError,
    ComposeStep, cancellation,
};
use crate::engine::Endpoint;
use crate::ports::PortRegistry;
use crate::store::{SharedStore, StoreError};

/// How long a finished operation, and its output, stays in memory.
pub const RETENTION: Duration = Duration::from_hours(1);

/// The most finished operations kept in memory at once; the oldest go
/// first.
pub const MAX_FINISHED: usize = 100;

/// How many operations the state database keeps; older ones are deleted
/// at startup.
pub const HISTORY_KEPT: usize = 1000;

/// Events a run may queue before it waits for them to be recorded.
const EVENT_QUEUE: usize = 256;

/// Why an operation was not started.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    /// No app has the name.
    #[error("no app is named {name:?}")]
    AppNotFound {
        /// The name asked for.
        name: String,
    },
    /// Compose has not read the app yet, or cannot.
    #[error("Compose hasn't read {name} yet, so Alchemy can't run it; check the app for problems")]
    NotRead {
        /// The app.
        name: String,
    },
    /// Docker is not ready.
    #[error("Docker isn't running, so Alchemy can't {action} {name}")]
    DockerNotReady {
        /// The app.
        name: String,
        /// The action asked for.
        action: Action,
    },
    /// Another app resolves to the same Compose project.
    #[error(
        "{name} shares Compose project {project:?} with another app, so {action} would reach both; force it to go ahead"
    )]
    SharedProject {
        /// The app.
        name: String,
        /// The shared project.
        project: String,
        /// The action asked for.
        action: Action,
    },
    /// A host port the app publishes is held by something else.
    #[error("{}", port_conflict_message(name, *action, conflicts))]
    PortConflict {
        /// The app.
        name: String,
        /// The action asked for.
        action: Action,
        /// Each port the app wants and what holds it, at least one.
        conflicts: Vec<PortConflict>,
    },
    /// Another operation is running on the app.
    #[error("{name} is busy with another operation ({operation}); wait for it to finish")]
    Busy {
        /// The app.
        name: String,
        /// The running operation's id.
        operation: String,
    },
    /// The daemon is shutting down.
    #[error("alchemyd is shutting down")]
    ShuttingDown,
    /// The app's folder is not one Compose can be pointed at.
    #[error("{name}'s folder can't be used")]
    Project {
        /// The app.
        name: String,
        /// Why.
        #[source]
        source: ComposeProjectError,
    },
}

/// Why an operation was not cancelled.
#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    /// No operation in memory has the id.
    #[error("no running operation has id {id:?}")]
    NotFound {
        /// The id asked for.
        id: String,
    },
    /// The operation has already ended.
    #[error("operation {} has already finished", operation.id)]
    Finished {
        /// The operation as it ended.
        operation: Box<Operation>,
    },
}

/// Why a running operation is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Interrupted,
}

#[derive(Debug)]
struct Entry {
    tracker: Tracker,
    output: OutputBuffer,
    cancel: Option<Canceller>,
    stop: Option<Stop>,
    changes: watch::Sender<u64>,
    finished: Option<Instant>,
}

impl Entry {
    fn operation(&self) -> &Operation {
        self.tracker.operation()
    }

    fn is_running(&self) -> bool {
        !self.operation().state.is_finished()
    }

    fn notify(&self) {
        self.changes.send_modify(|n| *n = n.wrapping_add(1));
    }
}

struct Inner {
    runner: Arc<dyn OperationRunner>,
    state: State,
    store: SharedStore,
    ports: PortRegistry,
    entries: Mutex<BTreeMap<String, Entry>>,
    tasks: Mutex<JoinSet<()>>,
    closed: AtomicBool,
    seq: AtomicU64,
}

/// The daemon's operations. Cheap to clone: every clone shares them.
#[derive(Clone)]
pub struct Operations {
    inner: Arc<Inner>,
}

impl fmt::Debug for Operations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Operations")
            .field("runner", &self.inner.runner)
            .finish_non_exhaustive()
    }
}

/// What starting an action on an app needs.
struct Prepared {
    app: StackName,
    project: String,
    plan: ComposePlan,
}

impl Operations {
    /// Operations run through `runner` on the apps in `state`, stored in
    /// `store`, checking the host ports an app publishes against `ports`
    /// before starting it.
    pub fn new(
        runner: Arc<dyn OperationRunner>,
        state: State,
        store: SharedStore,
        ports: PortRegistry,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                runner,
                state,
                store,
                ports,
                entries: Mutex::new(BTreeMap::new()),
                tasks: Mutex::new(JoinSet::new()),
                closed: AtomicBool::new(false),
                seq: AtomicU64::new(0),
            }),
        }
    }

    /// Mark operations the store still has as running, left by a daemon
    /// that stopped without finishing them, as interrupted, and keep only
    /// the newest [`HISTORY_KEPT`]. Call once at startup.
    pub async fn recover(&self) -> Result<(), StoreError> {
        let store = &self.inner.store;
        for operation in store.operations_in(OperationState::Running).await? {
            let at = operation.started_at;
            let mut tracker = Tracker::resume(operation);
            tracker.finish(&Outcome::Interrupted, at.max(now_secs()));
            store.put_operation(tracker.operation()).await?;
            tracing::info!(
                id = tracker.operation().id,
                "marked an unfinished operation interrupted"
            );
        }
        store.prune_operations(HISTORY_KEPT).await?;
        Ok(())
    }

    /// Start `action` on the app named `name` and return the operation,
    /// running. `force` goes ahead when another app shares its Compose
    /// project, or when a host port the app publishes is taken.
    pub async fn start(
        &self,
        name: &str,
        action: Action,
        force: bool,
    ) -> Result<Operation, StartError> {
        let prepared = self.prepare(name, action, force)?;
        if self.inner.state.engine().state != EngineState::Ready {
            return Err(StartError::DockerNotReady {
                name: name.to_owned(),
                action,
            });
        }
        if !force {
            let conflicts = self.port_conflicts(&prepared.app, action).await;
            if !conflicts.is_empty() {
                return Err(StartError::PortConflict {
                    name: name.to_owned(),
                    action,
                    conflicts,
                });
            }
        }
        let commands: Vec<_> = prepared
            .plan
            .steps()
            .iter()
            .map(ComposeStep::command)
            .collect();
        let tracker = Tracker::new(
            self.next_id(),
            prepared.app.clone(),
            action,
            &commands,
            now_secs(),
        );
        let operation = tracker.operation().clone();
        let (canceller, token) = cancellation();
        {
            let mut entries = self.inner.lock();
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(StartError::ShuttingDown);
            }
            if let Some(busy) = entries
                .values()
                .find(|e| e.is_running() && e.operation().app == prepared.app)
            {
                return Err(StartError::Busy {
                    name: name.to_owned(),
                    operation: busy.operation().id.clone(),
                });
            }
            entries.insert(
                operation.id.clone(),
                Entry {
                    tracker,
                    output: OutputBuffer::new(OUTPUT_LINES),
                    cancel: Some(canceller),
                    stop: None,
                    changes: watch::Sender::new(0),
                    finished: None,
                },
            );
        }
        tracing::info!(id = operation.id, app = name, %action, "operation started");
        self.inner.publish();
        if let Err(error) = self.inner.store.put_operation(&operation).await {
            tracing::warn!(%error, id = operation.id, "storing a started operation failed");
        }
        let inner = self.inner.clone();
        let id = operation.id.clone();
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while tasks.try_join_next().is_some() {}
        tasks.spawn(drive(inner, id, prepared.project, prepared.plan, token));
        Ok(operation)
    }

    /// The plan `action` would run on the app named `name`, changing
    /// nothing, with the port conflicts that would refuse it.
    pub async fn plan(&self, name: &str, action: Action) -> Result<ActionPlan, StartError> {
        let prepared = self.prepare(name, action, true)?;
        let conflicts = self.port_conflicts(&prepared.app, action).await;
        Ok(convert::action_plan(
            prepared.app,
            &prepared.plan,
            &conflicts,
        ))
    }

    /// The host ports `app` publishes, read from its cached Compose
    /// configuration, that something else holds now, when `action`
    /// starts containers.
    ///
    /// A registry that cannot be read finds none, so a broken state
    /// database never stops an app starting: Docker still refuses a port
    /// that is really taken.
    async fn port_conflicts(&self, app: &StackName, action: Action) -> Vec<PortConflict> {
        if !starts_containers(action) {
            return Vec::new();
        }
        match self.inner.ports.gather().await {
            Ok(gathered) => {
                let claims = gathered.input.merge();
                find_conflicts(app.as_str(), &gathered.input.declared, &claims)
            }
            Err(error) => {
                tracing::warn!(%error, %app, "cannot check the app's ports before starting it");
                Vec::new()
            }
        }
    }

    fn prepare(&self, name: &str, action: Action, force: bool) -> Result<Prepared, StartError> {
        let list = self.inner.state.apps();
        let app = apps::find(&list, name).ok_or_else(|| StartError::AppNotFound {
            name: name.to_owned(),
        })?;
        let project = app.project.clone().ok_or_else(|| StartError::NotRead {
            name: name.to_owned(),
        })?;
        if !force
            && app
                .problems
                .iter()
                .any(|p| p.kind == AppProblemKind::SharedProject)
        {
            return Err(StartError::SharedProject {
                name: name.to_owned(),
                project,
                action,
            });
        }
        let compose = apps::compose_project(app).map_err(|source| StartError::Project {
            name: name.to_owned(),
            source,
        })?;
        let plan = ComposePlan::new(&compose, action, app.images.unwrap_or_default());
        Ok(Prepared {
            app: app.name.clone(),
            project,
            plan,
        })
    }

    /// The operation `id` as it stands: from memory while it is kept
    /// there, else from the store.
    pub async fn get(&self, id: &str) -> Result<Option<Operation>, StoreError> {
        if let Some(operation) = self.inner.lock().get(id).map(|e| e.operation().clone()) {
            return Ok(Some(operation));
        }
        self.inner.store.operation(id).await
    }

    /// Up to `limit` recent operations, of the app named `app` alone when
    /// given, newest first: the stored history with the ones in memory as
    /// they stand now.
    pub async fn recent(
        &self,
        app: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Operation>, StoreError> {
        let stored = self.inner.store.operations(app, limit).await?;
        let live: BTreeMap<String, Operation> = self
            .inner
            .lock()
            .values()
            .map(Entry::operation)
            .filter(|op| app.is_none_or(|app| op.app.as_str() == app))
            .map(|op| (op.id.clone(), op.clone()))
            .collect();
        let mut merged: BTreeMap<String, Operation> =
            stored.into_iter().map(|op| (op.id.clone(), op)).collect();
        merged.extend(live);
        let mut operations: Vec<Operation> = merged.into_values().collect();
        operations.sort_by(newest_first);
        operations.truncate(limit);
        Ok(operations)
    }

    /// Stop the running operation `id`. It ends as cancelled once the
    /// running step has stopped.
    pub fn cancel(&self, id: &str) -> Result<Operation, CancelError> {
        let mut entries = self.inner.lock();
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| CancelError::NotFound { id: id.to_owned() })?;
        if !entry.is_running() {
            return Err(CancelError::Finished {
                operation: Box::new(entry.operation().clone()),
            });
        }
        if entry.stop.is_none() {
            entry.stop = Some(Stop::Cancelled);
        }
        if let Some(cancel) = &entry.cancel {
            cancel.cancel();
        }
        tracing::info!(id, "operation cancelled");
        Ok(entry.operation().clone())
    }

    /// Follow the operation `id`, if it is in memory.
    pub fn follow(&self, id: &str) -> Option<Follower> {
        let rx = self.inner.lock().get(id)?.changes.subscribe();
        Some(Follower {
            inner: self.inner.clone(),
            id: id.to_owned(),
            rx,
            next_seq: 0,
            sent: None,
            pending: VecDeque::new(),
            done: false,
        })
    }

    /// Refuse new operations, interrupt the running ones, and wait for
    /// them to end and be stored.
    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        for entry in self.inner.lock().values_mut().filter(|e| e.is_running()) {
            entry.stop = Some(Stop::Interrupted);
            if let Some(cancel) = &entry.cancel {
                cancel.cancel();
            }
        }
        let mut tasks = std::mem::take(
            &mut *self
                .inner
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        while tasks.join_next().await.is_some() {}
    }

    fn next_id(&self) -> String {
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        format!("{:012x}{:04x}", now_ms(), seq & 0xffff)
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drop expired finished operations and put the rest in [`State`].
    fn publish(&self) {
        let operations = {
            let mut entries = self.lock();
            purge(&mut entries, Instant::now());
            let mut operations: Vec<Operation> =
                entries.values().map(|e| e.operation().clone()).collect();
            operations.sort_by(newest_first);
            operations
        };
        self.state.set_operations(OperationList { operations });
    }

    fn record(&self, id: &str, event: &ComposeEvent) {
        let progress = convert::progress(event);
        let changed = {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            let applied = entry.tracker.apply(&progress);
            let wrote = !applied.lines.is_empty();
            for (step, text) in applied.lines {
                entry.output.push(step, text);
            }
            if applied.changed || wrote {
                entry.notify();
            }
            applied.changed
        };
        if changed {
            self.publish();
        }
    }

    async fn finish(&self, id: &str, result: Result<(), ComposeError>) {
        let operation = {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            let outcome = match result {
                Ok(()) => Outcome::Succeeded,
                Err(ComposeError::Cancelled { .. }) if entry.stop == Some(Stop::Interrupted) => {
                    Outcome::Interrupted
                }
                Err(ComposeError::Cancelled { .. }) => Outcome::Cancelled,
                Err(error) => Outcome::Failed(convert::failure_message(&error)),
            };
            entry.tracker.finish(&outcome, now_secs());
            entry.cancel = None;
            entry.finished = Some(Instant::now());
            entry.notify();
            entry.operation().clone()
        };
        tracing::info!(
            id,
            app = %operation.app,
            action = %operation.action,
            state = operation.state.as_str(),
            error = operation.error.as_deref().unwrap_or(""),
            "operation finished"
        );
        if let Err(error) = self.store.put_operation(&operation).await {
            tracing::warn!(%error, id, "storing a finished operation failed");
        }
        self.publish();
        self.state.rescan_apps();
    }
}

/// Run one operation's plan to the end and record what it reports.
///
/// The stop intent is recorded before Compose runs, so the containers it
/// stops are known to have been stopped on purpose. It stays recorded when
/// the operation fails: Compose may have stopped some of them already.
async fn drive(
    inner: Arc<Inner>,
    id: String,
    project: String,
    plan: ComposePlan,
    token: CancelToken,
) {
    if token.is_cancelled() {
        let action = plan.action();
        inner
            .finish(&id, Err(ComposeError::Cancelled { action }))
            .await;
        return;
    }
    if let Some(intent) = intent_action(plan.action())
        && let Err(error) =
            crate::intent::record(inner.store.as_ref(), &inner.state, &project, intent).await
    {
        tracing::warn!(%error, id, project, "recording the stop intent failed");
    }
    let endpoint = inner
        .state
        .engine()
        .endpoint
        .and_then(|e| Endpoint::parse(&e).ok())
        .unwrap_or_else(Endpoint::system);
    let (tx, mut rx) = mpsc::channel(EVENT_QUEUE);
    let run = inner.runner.run(&plan, endpoint, tx, token);
    let pump = async {
        while let Some(event) = rx.recv().await {
            inner.record(&id, &event);
        }
    };
    let (result, ()) = tokio::join!(run, pump);
    inner.finish(&id, result).await;
}

/// Drop finished entries older than [`RETENTION`], then the oldest
/// finished ones beyond [`MAX_FINISHED`].
fn purge(entries: &mut BTreeMap<String, Entry>, now: Instant) {
    entries.retain(|_, e| {
        e.finished
            .is_none_or(|at| now.saturating_duration_since(at) < RETENTION)
    });
    let mut finished: Vec<(Instant, String)> = entries
        .iter()
        .filter_map(|(id, e)| e.finished.map(|at| (at, id.clone())))
        .collect();
    if finished.len() > MAX_FINISHED {
        finished.sort();
        for (_, id) in finished.iter().take(finished.len() - MAX_FINISHED) {
            entries.remove(id);
        }
    }
}

fn newest_first(a: &Operation, b: &Operation) -> std::cmp::Ordering {
    (b.started_at, &b.id).cmp(&(a.started_at, &a.id))
}

/// Reads one operation's events: the operation, every kept output line,
/// then new lines and changes as they happen, and a final
/// [`OperationEvent::Done`].
pub struct Follower {
    inner: Arc<Inner>,
    id: String,
    rx: watch::Receiver<u64>,
    next_seq: u64,
    sent: Option<Operation>,
    pending: VecDeque<OperationEvent>,
    done: bool,
}

impl fmt::Debug for Follower {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Follower")
            .field("id", &self.id)
            .field("next_seq", &self.next_seq)
            .finish_non_exhaustive()
    }
}

impl Follower {
    /// The next event, or `None` after [`OperationEvent::Done`], or once
    /// the operation is no longer kept in memory.
    pub async fn next(&mut self) -> Option<OperationEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            self.rx.mark_unchanged();
            self.catch_up();
            if self.pending.is_empty() && !self.done && self.rx.changed().await.is_err() {
                self.catch_up();
                self.done = true;
            }
        }
    }

    fn catch_up(&mut self) {
        let entries = self.inner.lock();
        let Some(entry) = entries.get(&self.id) else {
            self.done = true;
            return;
        };
        let operation = entry.operation();
        if self.sent.is_none() {
            self.pending
                .push_back(OperationEvent::Operation(operation.clone()));
            self.sent = Some(operation.clone());
        }
        for line in entry.output.since(self.next_seq) {
            self.next_seq = line.seq + 1;
            self.pending.push_back(OperationEvent::Output(line));
        }
        if operation.state.is_finished() {
            self.pending
                .push_back(OperationEvent::Done(operation.clone()));
            self.done = true;
        } else if self.sent.as_ref() != Some(operation) {
            self.pending
                .push_back(OperationEvent::Operation(operation.clone()));
            self.sent = Some(operation.clone());
        }
    }
}

/// Whether `action` can bind host ports, so its ports are checked first.
fn starts_containers(action: Action) -> bool {
    matches!(
        action,
        Action::Up | Action::Start | Action::Restart | Action::Update
    )
}

/// [`StartError::PortConflict`]'s message: each conflict's sentence, then
/// how to go ahead anyway.
fn port_conflict_message(name: &str, action: Action, conflicts: &[PortConflict]) -> String {
    let mut message: Vec<String> = conflicts.iter().map(ToString::to_string).collect();
    let ports = if conflicts.len() == 1 {
        "that port"
    } else {
        "those ports"
    };
    message.push(format!(
        "Free {ports}, or force {action} to start {name} anyway."
    ));
    message.join(" ")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
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
    use std::path::PathBuf;

    use alchemy_api::AppList;
    use alchemy_api::apps::App;
    use alchemy_core::engine::{ContainerState, ContainerStatus, EngineStatus, IntentAction};
    use alchemy_core::operation::{StepCommand, StepState};
    use alchemy_core::ports::{Listener, PortRange, Protocol, PublishedPort};

    use super::*;
    use crate::apps::ConfigOutcome;
    use crate::compose::{
        ComposeAction, ComposeConfig, FakeComposeRunner, FakeOutcome, FakeScript, ServiceConfig,
        Stream,
    };
    use crate::network::FakeNetwork;
    use crate::store::{MemoryStore, PortReservation};

    fn app(name: &str, project: Option<&str>, problems: &serde_json::Value) -> App {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "status": "running",
            "dir": format!("/srv/apps/{name}"),
            "root": "/srv/apps",
            "compose_file": "compose.yaml",
            "project": project,
            "images": project.map(|_| "build"),
            "services": [],
            "problems": problems,
        }))
        .unwrap()
    }

    fn setup(runner: FakeComposeRunner) -> (Operations, State, SharedStore) {
        setup_with(runner, &FakeNetwork::new())
    }

    fn setup_with(
        runner: FakeComposeRunner,
        network: &FakeNetwork,
    ) -> (Operations, State, SharedStore) {
        let state = State::new();
        state.set_engine(EngineStatus::new(EngineState::Ready));
        state.set_apps(AppList {
            apps: vec![
                app("blog", Some("blog"), &serde_json::json!([])),
                app("new", None, &serde_json::json!([])),
                app(
                    "twin",
                    Some("blog"),
                    &serde_json::json!([{"kind": "shared_project", "message": "x"}]),
                ),
            ],
            ..AppList::default()
        });
        let store: SharedStore = Arc::new(MemoryStore::new());
        let ports = PortRegistry::new(state.clone(), store.clone(), Arc::new(network.clone()));
        let ops = Operations::new(Arc::new(runner), state.clone(), store.clone(), ports);
        (ops, state, store)
    }

    fn tcp(port: u16) -> PublishedPort {
        PublishedPort::new(Protocol::Tcp, PortRange::single(port).ok())
    }

    fn declares(project: &str, service: &str, ports: &[u16]) -> ConfigOutcome {
        ConfigOutcome::Ready(ComposeConfig {
            name: project.into(),
            services: vec![ServiceConfig {
                name: service.into(),
                ports: ports.iter().copied().map(tcp).collect(),
                ..ServiceConfig::default()
            }],
            ..ComposeConfig::default()
        })
    }

    /// `blog` running `web` on 8080; `wiki`, stopped, wanting 8080 and
    /// 9000 for `app`; `docs`, stopped, wanting 9100 for `web`.
    fn with_ports(state: &State) {
        let mut list = state.apps();
        list.apps
            .push(app("wiki", Some("wiki"), &serde_json::json!([])));
        list.apps
            .push(app("docs", Some("docs"), &serde_json::json!([])));
        state.set_app_configs(BTreeMap::from([
            (
                PathBuf::from("/srv/apps/blog"),
                declares("blog", "web", &[8080]),
            ),
            (
                PathBuf::from("/srv/apps/wiki"),
                declares("wiki", "app", &[8080, 9000]),
            ),
            (
                PathBuf::from("/srv/apps/docs"),
                declares("docs", "web", &[9100]),
            ),
        ]));
        state.set_apps(list);
        let mut web = ContainerStatus::new("abc", "blog-web-1", ContainerState::Running);
        web.project = Some("blog".into());
        web.service = Some("web".into());
        web.ports = vec![tcp(8080)];
        state.update_containers(|c| c.replace_all([web]));
    }

    async fn follow_to_end(ops: &Operations, id: &str) -> Vec<OperationEvent> {
        let mut follower = ops.follow(id).unwrap();
        let mut events = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(5), follower.next())
            .await
            .unwrap()
        {
            events.push(event);
        }
        events
    }

    fn last_done(events: &[OperationEvent]) -> &Operation {
        match events.last() {
            Some(OperationEvent::Done(op)) => op,
            other => panic!("expected done last, got {other:?}"),
        }
    }

    fn output(events: &[OperationEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                OperationEvent::Output(line) => Some(line.text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn runs_an_update_and_follows_it_to_done() {
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Update,
            FakeScript::new()
                .line(
                    Stream::Stderr,
                    r#"{"id":"Container blog-web-1","status":"Done","text":"Started"}"#,
                )
                .line(Stream::Stdout, "plain"),
        );
        let (ops, state, store) = setup(runner.clone());
        let started = ops.start("blog", Action::Update, false).await.unwrap();
        assert_eq!(started.state, OperationState::Running);
        assert_eq!(
            started.steps.iter().map(|s| s.command).collect::<Vec<_>>(),
            [StepCommand::Pull, StepCommand::Build, StepCommand::Up]
        );

        let events = follow_to_end(&ops, &started.id).await;
        assert!(matches!(events.first(), Some(OperationEvent::Operation(_))));
        assert_eq!(output(&events), ["Container blog-web-1 Started", "plain"]);
        let done = last_done(&events);
        assert_eq!(done.state, OperationState::Succeeded);
        assert!(done.steps.iter().all(|s| s.state == StepState::Done));
        assert_eq!(
            done.steps[2].message.as_deref(),
            Some("1 of 1 containers started")
        );

        assert_eq!(runner.runs()[0].action(), ComposeAction::Update);
        assert_eq!(
            store.operation(&started.id).await.unwrap().as_ref(),
            Some(done)
        );
        assert_eq!(
            store.intent("blog").await.unwrap().map(|i| i.action),
            Some(IntentAction::Up)
        );
        assert_eq!(state.operations().operations, std::slice::from_ref(done));
        assert_eq!(ops.get(&started.id).await.unwrap().as_ref(), Some(done));

        let late = follow_to_end(&ops, &started.id).await;
        assert!(matches!(late[0], OperationEvent::Operation(_)));
        assert_eq!(
            output(&late),
            output(&events),
            "a late follower replays the output"
        );
        assert_eq!(last_done(&late), done);
    }

    #[tokio::test]
    async fn refuses_what_it_cannot_run() {
        let (ops, state, _) = setup(FakeComposeRunner::new());
        let cases: [(&str, bool, &str); 4] = [
            ("nope", false, "no app is named \"nope\""),
            ("new", false, "Compose hasn't read new yet"),
            ("twin", false, "shares Compose project \"blog\""),
            ("twin", true, ""),
        ];
        for (name, force, want) in cases {
            match ops.start(name, Action::Stop, force).await {
                Ok(_) => assert_eq!(want, "", "{name}"),
                Err(error) => assert!(error.to_string().contains(want), "{name}: {error}"),
            }
        }
        state.set_engine(EngineStatus::new(EngineState::NotRunning));
        let error = ops.start("blog", Action::Up, false).await.unwrap_err();
        assert!(
            matches!(error, StartError::DockerNotReady { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn refuses_to_start_on_a_taken_port_unless_forced() {
        let (ops, state, _) = setup(FakeComposeRunner::new());
        with_ports(&state);
        let taken = "Port 8080/tcp for service app is in use by service web of app blog.";
        let cases: [(&str, Action, bool, Option<&str>); 9] = [
            ("wiki", Action::Up, false, Some(taken)),
            ("wiki", Action::Start, false, Some(taken)),
            ("wiki", Action::Restart, false, Some(taken)),
            ("wiki", Action::Update, false, Some(taken)),
            ("wiki", Action::Stop, false, None),
            ("wiki", Action::Pull, false, None),
            ("wiki", Action::Up, true, None),
            ("blog", Action::Restart, false, None),
            ("docs", Action::Up, false, None),
        ];
        for (name, action, force, want) in cases {
            match (ops.start(name, action, force).await, want) {
                (Ok(op), None) => {
                    let done = last_done(&follow_to_end(&ops, &op.id).await).clone();
                    assert_eq!(done.state, OperationState::Succeeded, "{name} {action}");
                }
                (Err(StartError::PortConflict { conflicts, .. }), Some(want)) => {
                    assert_eq!(conflicts.len(), 1, "{name} {action}");
                    assert_eq!(conflicts[0].to_string(), want, "{name} {action}");
                }
                (got, _) => panic!("{name} {action} force {force}: {got:?}"),
            }
        }
    }

    #[tokio::test]
    async fn names_every_holder_of_a_taken_port() {
        let network = FakeNetwork::new();
        network.set_listeners(vec![Listener {
            protocol: Protocol::Tcp,
            addr: "0.0.0.0".parse().unwrap(),
            port: 9100,
            process: Some("nc".into()),
            pid: Some(42),
        }]);
        let (ops, state, store) = setup_with(FakeComposeRunner::new(), &network);
        with_ports(&state);
        store
            .add_port_reservation(PortReservation {
                owner: "proxy".into(),
                port: 9000,
                protocol: Protocol::Tcp,
                host_ip: None,
                created: 1,
            })
            .await
            .unwrap();

        let error = ops.start("wiki", Action::Up, false).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Port 8080/tcp for service app is in use by service web of app blog. \
             Port 9000/tcp for service app is reserved for proxy. \
             Free those ports, or force up to start wiki anyway."
        );
        let error = ops.start("docs", Action::Start, false).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Port 9100/tcp for service web is in use by another program (nc, pid 42). \
             Free that port, or force start to start docs anyway."
        );

        let plan = ops.plan("wiki", Action::Up).await.unwrap();
        assert_eq!(
            plan.conflicts
                .iter()
                .map(|c| c.message.as_str())
                .collect::<Vec<_>>(),
            [
                "Port 8080/tcp for service app is in use by service web of app blog.",
                "Port 9000/tcp for service app is reserved for proxy.",
            ]
        );
        assert_eq!(ops.plan("wiki", Action::Down).await.unwrap().conflicts, []);
        assert_eq!(ops.plan("blog", Action::Up).await.unwrap().conflicts, []);
        assert_eq!(ops.recent(None, 10).await.unwrap(), []);
    }

    #[tokio::test]
    async fn allows_one_running_operation_per_app() {
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Up,
            FakeScript::new().outcome(FakeOutcome::UntilCancelled),
        );
        let (ops, _, store) = setup(runner);
        let first = ops.start("blog", Action::Up, false).await.unwrap();
        let busy = ops.start("blog", Action::Stop, false).await.unwrap_err();
        assert!(
            matches!(&busy, StartError::Busy { operation, .. } if *operation == first.id),
            "{busy}"
        );
        assert!(ops.start("twin", Action::Stop, true).await.is_ok());

        assert_eq!(ops.cancel(&first.id).unwrap().id, first.id);
        let events = follow_to_end(&ops, &first.id).await;
        let done = last_done(&events);
        assert_eq!(done.state, OperationState::Cancelled);
        assert_eq!(done.steps[0].state, StepState::Failed);
        assert!(matches!(
            ops.cancel(&first.id),
            Err(CancelError::Finished { .. })
        ));
        assert!(matches!(
            ops.cancel("nope"),
            Err(CancelError::NotFound { .. })
        ));
        assert_eq!(
            store.operation(&first.id).await.unwrap().unwrap().state,
            OperationState::Cancelled
        );

        let again = ops.start("blog", Action::Stop, false).await.unwrap();
        assert_ne!(again.id, first.id);
    }

    #[tokio::test]
    async fn reports_failures_in_the_failing_step() {
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Pull,
            FakeScript::new().outcome(FakeOutcome::Fail {
                code: 1,
                message: "pull access denied".into(),
            }),
        );
        let (ops, _, store) = setup(runner);
        let op = ops.start("blog", Action::Pull, false).await.unwrap();
        let done = last_done(&follow_to_end(&ops, &op.id).await).clone();
        assert_eq!(done.state, OperationState::Failed);
        assert_eq!(done.error.as_deref(), Some("pull access denied"));
        assert_eq!(done.steps[0].state, StepState::Failed);
        assert_eq!(
            store.intent("blog").await.unwrap(),
            None,
            "pull records no intent"
        );
    }

    #[tokio::test]
    async fn shutdown_interrupts_and_stores_running_operations() {
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Stop,
            FakeScript::new().outcome(FakeOutcome::UntilCancelled),
        );
        let (ops, _, store) = setup(runner);
        let op = ops.start("blog", Action::Stop, false).await.unwrap();
        let mut follower = ops.follow(&op.id).unwrap();
        ops.shutdown().await;
        let stored = store.operation(&op.id).await.unwrap().unwrap();
        assert_eq!(stored.state, OperationState::Interrupted);
        let mut last = None;
        while let Some(event) = follower.next().await {
            last = Some(event);
        }
        assert!(
            matches!(&last, Some(OperationEvent::Done(op)) if op.state == OperationState::Interrupted),
            "{last:?}"
        );
        assert!(matches!(
            ops.start("blog", Action::Up, false).await,
            Err(StartError::ShuttingDown)
        ));
    }

    #[tokio::test]
    async fn recovers_operations_a_stopped_daemon_left_running() {
        let (ops, _, store) = setup(FakeComposeRunner::new());
        let left = Tracker::new(
            "old",
            StackName::new("blog").unwrap(),
            Action::Stop,
            &[StepCommand::Stop],
            100,
        );
        store.put_operation(left.operation()).await.unwrap();
        ops.recover().await.unwrap();
        let op = store.operation("old").await.unwrap().unwrap();
        assert_eq!(op.state, OperationState::Interrupted);
        assert!(
            ops.recent(Some("blog"), 10)
                .await
                .unwrap()
                .iter()
                .any(|o| o.id == "old")
        );
        assert_eq!(ops.recent(Some("wiki"), 10).await.unwrap(), []);
    }

    #[tokio::test]
    async fn plans_without_running() {
        let runner = FakeComposeRunner::new();
        let (ops, _, _) = setup(runner.clone());
        let plan = ops.plan("twin", Action::Update).await.unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].argv[0], "docker");
        assert_eq!(runner.runs(), []);
        assert!(matches!(
            ops.plan("nope", Action::Up).await,
            Err(StartError::AppNotFound { .. })
        ));
    }

    #[test]
    fn purges_old_and_excess_finished_operations() {
        let now = Instant::now() + RETENTION * 2;
        let entry = |finished: Option<Instant>| Entry {
            tracker: Tracker::new(
                "x",
                StackName::new("blog").unwrap(),
                Action::Stop,
                &[StepCommand::Stop],
                0,
            ),
            output: OutputBuffer::new(1),
            cancel: None,
            stop: None,
            changes: watch::Sender::new(0),
            finished,
        };
        let mut entries = BTreeMap::new();
        entries.insert("running".to_owned(), entry(None));
        entries.insert("old".to_owned(), entry(Some(now - RETENTION)));
        for i in 0..=MAX_FINISHED {
            let ago = u64::try_from(MAX_FINISHED - i).unwrap() + 1;
            entries.insert(
                format!("f{i:03}"),
                entry(Some(now - Duration::from_secs(ago))),
            );
        }
        purge(&mut entries, now);
        assert!(entries.contains_key("running"));
        assert!(!entries.contains_key("old"));
        assert!(!entries.contains_key("f000"));
        assert!(entries.contains_key("f001"));
        assert_eq!(entries.len(), MAX_FINISHED + 1);
    }

    #[tokio::test]
    async fn ids_sort_by_start() {
        let (ops, _, _) = setup(FakeComposeRunner::new());
        let a = ops.next_id();
        let b = ops.next_id();
        assert_eq!(a.len(), 16);
        assert!(a < b, "{a} {b}");
    }
}
