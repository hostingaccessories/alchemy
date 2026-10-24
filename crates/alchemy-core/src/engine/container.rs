//! Containers as seen through the Engine API and its event stream.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::stop::{InspectedExit, Intent, StopReason, Trail, stop_reason};
use crate::ports::PublishedPort;

/// A container's lifecycle state, as Docker names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContainerState {
    /// Created but never started.
    Created,
    /// Running.
    Running,
    /// Paused with its processes frozen.
    Paused,
    /// Being restarted by its restart policy.
    Restarting,
    /// Being removed.
    Removing,
    /// Stopped after running.
    Exited,
    /// Failed to stop or be removed cleanly.
    Dead,
}

impl ContainerState {
    /// Parse Docker's state string, such as `running` from `/containers/json`.
    pub fn from_docker(state: &str) -> Option<Self> {
        match state.trim() {
            "created" => Some(Self::Created),
            "running" => Some(Self::Running),
            "paused" => Some(Self::Paused),
            "restarting" => Some(Self::Restarting),
            "removing" => Some(Self::Removing),
            "exited" => Some(Self::Exited),
            "dead" => Some(Self::Dead),
            _ => None,
        }
    }
}

/// A container's healthcheck result.
///
/// Containers without a healthcheck have no [`Health`] at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Health {
    /// The healthcheck has not passed yet.
    Starting,
    /// The last healthcheck passed.
    Healthy,
    /// The healthcheck failed enough times in a row.
    Unhealthy,
}

impl Health {
    /// Parse Docker's health string. `none` and unknown values are `None`.
    pub fn from_docker(health: &str) -> Option<Self> {
        match health.trim() {
            "starting" => Some(Self::Starting),
            "healthy" => Some(Self::Healthy),
            "unhealthy" => Some(Self::Unhealthy),
            _ => None,
        }
    }
}

/// What the daemon knows about one container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContainerStatus {
    /// Full container ID.
    pub id: String,
    /// Container name, without Docker's leading `/`.
    pub name: String,
    /// Image reference the container was created from.
    #[serde(default)]
    pub image: Option<String>,
    /// Compose project, from the `com.docker.compose.project` label.
    #[serde(default)]
    pub project: Option<String>,
    /// Compose service, from the `com.docker.compose.service` label.
    #[serde(default)]
    pub service: Option<String>,
    /// Compose project directory, from the
    /// `com.docker.compose.project.working_dir` label.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Lifecycle state.
    pub state: ContainerState,
    /// Healthcheck result, if the container has a healthcheck.
    ///
    /// Only meaningful while the container is running.
    #[serde(default)]
    pub health: Option<Health>,
    /// Why it is or is not running. [`Containers`] keeps this current
    /// from the container's events and its project's [`Intent`].
    #[serde(default)]
    pub stop_reason: StopReason,
    /// The host ports it publishes, from the engine's container listing.
    /// Empty while it is not running, and left off the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PublishedPort>,
    /// The history `stop_reason` is decided from. Not on the wire.
    #[serde(skip)]
    trail: Trail,
}

impl ContainerStatus {
    /// A container with no image, Compose labels, or health known.
    pub fn new(id: impl Into<String>, name: impl Into<String>, state: ContainerState) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            image: None,
            project: None,
            service: None,
            working_dir: None,
            state,
            health: None,
            stop_reason: stop_reason(state, &Trail::default(), None),
            ports: Vec::new(),
            trail: Trail::default(),
        }
    }

    /// The history its [`stop_reason`](Self::stop_reason) is decided from.
    pub fn trail(&self) -> &Trail {
        &self.trail
    }

    fn classify(&mut self, intent: Option<&Intent>) {
        self.stop_reason = stop_reason(self.state, &self.trail, intent);
    }

    fn same_view(&self, other: &Self) -> bool {
        Self {
            trail: other.trail,
            ..self.clone()
        } == *other
    }
}

/// A container event action the model cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContainerAction {
    /// The container was created.
    Create,
    /// The container started.
    Start,
    /// The container was restarted and is running again.
    Restart,
    /// The container was stopped on request.
    Stop,
    /// The container's main process exited.
    Die,
    /// A signal was sent to the container; it may or may not exit.
    Kill,
    /// The container ran out of memory; a `die` usually follows.
    Oom,
    /// The container was paused.
    Pause,
    /// The container was unpaused.
    Unpause,
    /// The container was removed.
    Destroy,
    /// The container was renamed; the event carries the new name.
    Rename,
    /// A healthcheck reported a new result.
    Health(Health),
}

impl ContainerAction {
    fn implied_state(self) -> Option<ContainerState> {
        match self {
            Self::Create => Some(ContainerState::Created),
            Self::Start | Self::Restart | Self::Unpause => Some(ContainerState::Running),
            Self::Pause => Some(ContainerState::Paused),
            Self::Stop | Self::Die => Some(ContainerState::Exited),
            Self::Kill | Self::Oom | Self::Destroy | Self::Rename | Self::Health(_) => None,
        }
    }
}

const HEALTH_STATUS: &str = "health_status";

/// Parse a container event's `Action` string.
///
/// Returns `None` for actions that don't change what the model tracks,
/// such as `exec_start: sh`, `attach`, or `top`, and for unknown actions.
pub fn parse_action(action: &str) -> Option<ContainerAction> {
    let action = action.trim();
    if let Some(rest) = action.strip_prefix(HEALTH_STATUS) {
        return rest
            .strip_prefix(':')
            .and_then(Health::from_docker)
            .map(ContainerAction::Health);
    }
    match action {
        "create" => Some(ContainerAction::Create),
        "start" => Some(ContainerAction::Start),
        "restart" => Some(ContainerAction::Restart),
        "stop" => Some(ContainerAction::Stop),
        "die" => Some(ContainerAction::Die),
        "kill" => Some(ContainerAction::Kill),
        "oom" => Some(ContainerAction::Oom),
        "pause" => Some(ContainerAction::Pause),
        "unpause" => Some(ContainerAction::Unpause),
        "destroy" => Some(ContainerAction::Destroy),
        "rename" => Some(ContainerAction::Rename),
        _ => None,
    }
}

/// One container event from the Engine API event stream.
///
/// The optional fields come from the event's actor attributes. When
/// present they overwrite what [`Containers`] already knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContainerEvent {
    /// Full container ID.
    pub id: String,
    /// What happened.
    pub action: ContainerAction,
    /// The `name` attribute.
    #[serde(default)]
    pub name: Option<String>,
    /// The `image` attribute.
    #[serde(default)]
    pub image: Option<String>,
    /// The `com.docker.compose.project` attribute.
    #[serde(default)]
    pub project: Option<String>,
    /// The `com.docker.compose.service` attribute.
    #[serde(default)]
    pub service: Option<String>,
    /// The `com.docker.compose.project.working_dir` attribute.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// When it happened, in Unix milliseconds.
    #[serde(default)]
    pub at: Option<i64>,
    /// The `exitCode` attribute of a `die` event.
    #[serde(default)]
    pub exit_code: Option<i64>,
    /// The `signal` attribute of a `kill` event.
    #[serde(default)]
    pub signal: Option<i32>,
}

impl ContainerEvent {
    /// An event with no actor attributes.
    pub fn new(id: impl Into<String>, action: ContainerAction) -> Self {
        Self {
            id: id.into(),
            action,
            name: None,
            image: None,
            project: None,
            service: None,
            working_dir: None,
            at: None,
            exit_code: None,
            signal: None,
        }
    }
}

/// The daemon's view of every container, keyed by ID.
///
/// Seed it with [`Containers::replace_all`] from a full listing, then keep
/// it current with [`Containers::apply`] for each event. Iteration order
/// is by ID, so output is deterministic.
///
/// It also holds the last [`Intent`] Alchemy recorded for each Compose
/// project, set with [`Containers::set_intent`], and keeps every
/// container's [`ContainerStatus::stop_reason`] current with both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Containers {
    by_id: BTreeMap<String, ContainerStatus>,
    intents: BTreeMap<String, Intent>,
}

impl Containers {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace every container with a fresh listing. Returns whether
    /// anything changed.
    ///
    /// A listing carries no history, so a container already known keeps
    /// its trail. Intents are kept.
    pub fn replace_all(&mut self, containers: impl IntoIterator<Item = ContainerStatus>) -> bool {
        let next: BTreeMap<_, _> = containers
            .into_iter()
            .map(|mut c| {
                if let Some(known) = self.by_id.get(&c.id) {
                    c.trail = known.trail;
                }
                c.classify(intent_for(&self.intents, &c));
                (c.id.clone(), c)
            })
            .collect();
        let changed = next != self.by_id;
        self.by_id = next;
        changed
    }

    /// Take the ports of each container in `listed` whose state still
    /// matches the one known. Returns whether any changed.
    ///
    /// Events carry no ports, so a container started after the last full
    /// listing learns them from a later one. Nothing else is taken from
    /// it, and a container whose state has moved on since is left alone,
    /// so a listing that raced an event cannot undo it.
    pub fn update_ports(&mut self, listed: impl IntoIterator<Item = ContainerStatus>) -> bool {
        listed.into_iter().fold(false, |changed, fresh| {
            match self.by_id.get_mut(&fresh.id) {
                Some(known) if known.state == fresh.state && known.ports != fresh.ports => {
                    known.ports = fresh.ports;
                    true
                }
                _ => changed,
            }
        })
    }

    /// Record what Alchemy asked of Compose project `project`, replacing
    /// any earlier intent. Returns whether any container's reason changed.
    pub fn set_intent(&mut self, project: &str, intent: Intent) -> bool {
        self.intents.insert(project.to_owned(), intent);
        self.by_id
            .values_mut()
            .filter(|c| c.project.as_deref() == Some(project))
            .fold(false, |changed, c| {
                let before = c.stop_reason;
                c.classify(Some(&intent));
                let this = c.stop_reason != before;
                changed || this
            })
    }

    /// The last intent recorded for `project`.
    pub fn intent(&self, project: &str) -> Option<&Intent> {
        self.intents.get(project)
    }

    /// Stopped Compose containers whose last exit is unknown, because they
    /// stopped before the daemon was watching. Inspect them and pass each
    /// result to [`Containers::record_inspect`].
    pub fn needs_inspect(&self) -> Vec<String> {
        self.by_id
            .values()
            .filter(|c| {
                c.project.is_some()
                    && matches!(c.state, ContainerState::Exited | ContainerState::Dead)
                    && !c.trail.knows_exit()
            })
            .map(|c| c.id.clone())
            .collect()
    }

    /// Fill in the last exit of container `id` from an inspect. Ignored
    /// if the container is gone, or its events have reported an exit
    /// since. Returns whether anything changed.
    pub fn record_inspect(&mut self, id: &str, inspected: InspectedExit) -> bool {
        let Some(container) = self.by_id.get_mut(id) else {
            return false;
        };
        if container.trail.knows_exit() {
            return false;
        }
        let before = container.stop_reason;
        container.trail = Trail::from_inspect(inspected);
        container.classify(intent_for(&self.intents, container));
        container.stop_reason != before
    }

    /// Apply one event. Returns whether anything a client sees changed;
    /// an event that only adds to a container's trail, such as `kill`, is
    /// recorded without counting as a change.
    ///
    /// An event for an unknown container adds it when the action implies
    /// a state (such as `start` or `die`), so a missed `create` heals.
    /// Other events for unknown containers are ignored.
    pub fn apply(&mut self, event: &ContainerEvent) -> bool {
        if event.action == ContainerAction::Destroy {
            return self.by_id.remove(&event.id).is_some();
        }
        let current = self.by_id.get(&event.id);
        let Some(mut next) = current.cloned().or_else(|| {
            event.action.implied_state().map(|state| {
                ContainerStatus::new(
                    event.id.clone(),
                    event.name.clone().unwrap_or_else(|| event.id.clone()),
                    state,
                )
            })
        }) else {
            return false;
        };
        merge_attributes(&mut next, event);
        transition(&mut next, event.action);
        next.trail.record(event);
        next.classify(intent_for(&self.intents, &next));
        if current == Some(&next) {
            return false;
        }
        let visible = current.is_none_or(|c| !c.same_view(&next));
        self.by_id.insert(next.id.clone(), next);
        visible
    }

    /// The container with this ID.
    pub fn get(&self, id: &str) -> Option<&ContainerStatus> {
        self.by_id.get(id)
    }

    /// Every container, ordered by ID.
    pub fn iter(&self) -> impl Iterator<Item = &ContainerStatus> {
        self.by_id.values()
    }

    /// How many containers are known.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether no containers are known.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

fn intent_for<'a>(
    intents: &'a BTreeMap<String, Intent>,
    container: &ContainerStatus,
) -> Option<&'a Intent> {
    container.project.as_deref().and_then(|p| intents.get(p))
}

fn merge_attributes(status: &mut ContainerStatus, event: &ContainerEvent) {
    if let Some(name) = &event.name {
        status.name.clone_from(name);
    }
    if event.image.is_some() {
        status.image.clone_from(&event.image);
    }
    if event.project.is_some() {
        status.project.clone_from(&event.project);
    }
    if event.service.is_some() {
        status.service.clone_from(&event.service);
    }
    if event.working_dir.is_some() {
        status.working_dir.clone_from(&event.working_dir);
    }
}

/// Starting a container restarts its healthcheck, so a known health
/// resets to [`Health::Starting`] rather than keeping a stale result.
fn transition(status: &mut ContainerStatus, action: ContainerAction) {
    if let ContainerAction::Health(health) = action {
        status.health = Some(health);
        return;
    }
    if matches!(action, ContainerAction::Start | ContainerAction::Restart) {
        status.health = status.health.map(|_| Health::Starting);
    }
    if let Some(state) = action.implied_state() {
        status.state = state;
        if !matches!(state, ContainerState::Running | ContainerState::Paused) {
            status.ports.clear();
        }
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
    use super::*;

    #[test]
    fn parse_action_recognises_tracked_actions() {
        let cases = [
            ("create", Some(ContainerAction::Create)),
            ("start", Some(ContainerAction::Start)),
            ("restart", Some(ContainerAction::Restart)),
            ("stop", Some(ContainerAction::Stop)),
            ("die", Some(ContainerAction::Die)),
            ("kill", Some(ContainerAction::Kill)),
            ("oom", Some(ContainerAction::Oom)),
            ("pause", Some(ContainerAction::Pause)),
            ("unpause", Some(ContainerAction::Unpause)),
            ("destroy", Some(ContainerAction::Destroy)),
            ("rename", Some(ContainerAction::Rename)),
            (
                "health_status: healthy",
                Some(ContainerAction::Health(Health::Healthy)),
            ),
            (
                "health_status: unhealthy",
                Some(ContainerAction::Health(Health::Unhealthy)),
            ),
            (
                "health_status: starting",
                Some(ContainerAction::Health(Health::Starting)),
            ),
            (
                "health_status:healthy",
                Some(ContainerAction::Health(Health::Healthy)),
            ),
            ("health_status", None),
            ("health_status: bogus", None),
            ("health_statusX: healthy", None),
            ("exec_create: sh -c true", None),
            ("exec_start: sh -c true", None),
            ("exec_die", None),
            ("attach", None),
            ("top", None),
            ("", None),
            ("START", None),
        ];
        for (action, want) in cases {
            assert_eq!(parse_action(action), want, "{action:?}");
        }
    }

    #[test]
    fn container_state_from_docker() {
        let cases = [
            ("created", Some(ContainerState::Created)),
            ("running", Some(ContainerState::Running)),
            ("paused", Some(ContainerState::Paused)),
            ("restarting", Some(ContainerState::Restarting)),
            ("removing", Some(ContainerState::Removing)),
            ("exited", Some(ContainerState::Exited)),
            ("dead", Some(ContainerState::Dead)),
            ("Running", None),
            ("", None),
        ];
        for (state, want) in cases {
            assert_eq!(ContainerState::from_docker(state), want, "{state:?}");
        }
    }

    #[test]
    fn health_from_docker() {
        let cases = [
            ("starting", Some(Health::Starting)),
            ("healthy", Some(Health::Healthy)),
            ("unhealthy", Some(Health::Unhealthy)),
            ("none", None),
            ("", None),
        ];
        for (health, want) in cases {
            assert_eq!(Health::from_docker(health), want, "{health:?}");
        }
    }

    fn running(id: &str) -> ContainerStatus {
        ContainerStatus::new(id, id, ContainerState::Running)
    }

    fn with_health(mut status: ContainerStatus, health: Health) -> ContainerStatus {
        status.health = Some(health);
        status
    }

    fn with_state(mut status: ContainerStatus, state: ContainerState) -> ContainerStatus {
        status.state = state;
        status
    }

    #[test]
    fn apply_transitions_known_containers() {
        use ContainerAction as A;
        use ContainerState as S;
        let healthy = with_health(running("a"), Health::Healthy);
        let cases = [
            (
                running("a"),
                A::Stop,
                Some(with_state(running("a"), S::Exited)),
            ),
            (
                running("a"),
                A::Die,
                Some(with_state(running("a"), S::Exited)),
            ),
            (
                running("a"),
                A::Pause,
                Some(with_state(running("a"), S::Paused)),
            ),
            (
                with_state(running("a"), S::Paused),
                A::Unpause,
                Some(running("a")),
            ),
            (
                with_state(running("a"), S::Exited),
                A::Start,
                Some(running("a")),
            ),
            (
                with_state(healthy.clone(), S::Exited),
                A::Start,
                Some(with_health(running("a"), Health::Starting)),
            ),
            (
                healthy.clone(),
                A::Restart,
                Some(with_health(running("a"), Health::Starting)),
            ),
            (
                running("a"),
                A::Health(Health::Healthy),
                Some(healthy.clone()),
            ),
            (
                healthy.clone(),
                A::Health(Health::Unhealthy),
                Some(with_health(running("a"), Health::Unhealthy)),
            ),
            (running("a"), A::Kill, Some(running("a"))),
            (running("a"), A::Oom, Some(running("a"))),
            (running("a"), A::Destroy, None),
        ];
        for (start, action, want) in cases {
            let mut containers = Containers::new();
            containers.replace_all([start.clone()]);
            let changed = containers.apply(&ContainerEvent::new("a", action));
            let view = |c: &ContainerStatus| (c.state, c.health);
            let got = containers.get("a").map(view);
            assert_eq!(got, want.as_ref().map(view), "{action:?}");
            assert_eq!(changed, got != Some(view(&start)), "{action:?}");
        }
    }

    #[test]
    fn apply_to_unknown_containers() {
        use ContainerAction as A;
        use ContainerState as S;
        let cases = [
            (A::Create, Some(S::Created)),
            (A::Start, Some(S::Running)),
            (A::Restart, Some(S::Running)),
            (A::Unpause, Some(S::Running)),
            (A::Pause, Some(S::Paused)),
            (A::Stop, Some(S::Exited)),
            (A::Die, Some(S::Exited)),
            (A::Kill, None),
            (A::Oom, None),
            (A::Rename, None),
            (A::Destroy, None),
            (A::Health(Health::Healthy), None),
        ];
        for (action, want) in cases {
            let mut containers = Containers::new();
            let changed = containers.apply(&ContainerEvent::new("a", action));
            assert_eq!(containers.get("a").map(|c| c.state), want, "{action:?}");
            assert_eq!(changed, want.is_some(), "{action:?}");
        }
    }

    #[test]
    fn apply_merges_event_attributes() {
        let mut containers = Containers::new();
        let mut create = ContainerEvent::new("abc", ContainerAction::Create);
        create.name = Some("web-1".into());
        create.image = Some("nginx:1".into());
        create.project = Some("web".into());
        create.service = Some("nginx".into());
        create.working_dir = Some("/srv/web".into());
        assert!(containers.apply(&create));

        let mut want = ContainerStatus::new("abc", "web-1", ContainerState::Created);
        want.image = Some("nginx:1".into());
        want.project = Some("web".into());
        want.service = Some("nginx".into());
        want.working_dir = Some("/srv/web".into());
        assert_eq!(containers.get("abc"), Some(&want));

        let mut rename = ContainerEvent::new("abc", ContainerAction::Rename);
        rename.name = Some("web-2".into());
        assert!(containers.apply(&rename));
        want.name = "web-2".into();
        assert_eq!(containers.get("abc"), Some(&want));

        assert!(!containers.apply(&rename));
        assert!(containers.apply(&ContainerEvent::new("abc", ContainerAction::Start)));
        want.state = ContainerState::Running;
        want.stop_reason = StopReason::Running;
        assert_eq!(containers.get("abc"), Some(&want));
    }

    #[test]
    fn unknown_container_without_name_uses_id() {
        let mut containers = Containers::new();
        containers.apply(&ContainerEvent::new("abc", ContainerAction::Start));
        assert_eq!(containers.get("abc").unwrap().name, "abc");
    }

    #[test]
    fn replace_all_reports_changes() {
        let mut containers = Containers::new();
        assert!(!containers.replace_all([]));
        assert!(containers.replace_all([running("b"), running("a")]));
        assert!(!containers.replace_all([running("a"), running("b")]));
        assert_eq!(
            containers.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(containers.len(), 2);
        assert!(containers.replace_all([running("c")]));
        assert_eq!(containers.get("a"), None);
        assert_eq!(containers.len(), 1);
        assert!(containers.replace_all([]));
        assert!(containers.is_empty());
    }

    #[test]
    fn container_status_wire_shape() {
        let mut status = ContainerStatus::new("abc", "web-1", ContainerState::Running);
        status.image = Some("nginx:1".into());
        status.project = Some("web".into());
        status.service = Some("nginx".into());
        status.working_dir = Some("/srv/web".into());
        status.health = Some(Health::Healthy);
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": "abc",
                "name": "web-1",
                "image": "nginx:1",
                "project": "web",
                "service": "nginx",
                "working_dir": "/srv/web",
                "state": "running",
                "health": "healthy",
                "stop_reason": { "kind": "running" },
            })
        );
        assert_eq!(
            serde_json::from_value::<ContainerStatus>(json).unwrap(),
            status
        );
    }

    #[test]
    fn ports_are_on_the_wire_only_when_published() {
        use crate::ports::Protocol;
        let mut status = ContainerStatus::new("abc", "web-1", ContainerState::Running);
        status.ports = vec![
            PublishedPort::new(Protocol::Tcp, Some("8080".parse().unwrap()))
                .with_target(Some("80".parse().unwrap())),
        ];
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json["ports"],
            serde_json::json!([
                {"protocol": "tcp", "host_ip": null, "published": "8080", "target": "80"}
            ])
        );
        assert_eq!(
            serde_json::from_value::<ContainerStatus>(json).unwrap(),
            status
        );
        let bare = ContainerStatus::new("abc", "web-1", ContainerState::Running);
        let json = serde_json::to_value(&bare).unwrap();
        assert!(json.get("ports").is_none(), "{json}");
        assert_eq!(
            serde_json::from_value::<ContainerStatus>(json).unwrap(),
            bare
        );
    }

    fn publishing(id: &str, state: ContainerState, port: &str) -> ContainerStatus {
        let mut status = ContainerStatus::new(id, id, state);
        status.ports = vec![crate::ports::PublishedPort::new(
            crate::ports::Protocol::Tcp,
            Some(port.parse().unwrap()),
        )];
        status
    }

    #[test]
    fn update_ports_takes_only_ports_of_containers_in_the_same_state() {
        let mut containers = Containers::new();
        containers.replace_all([running("a"), running("b"), running("c")]);
        containers.apply(&ContainerEvent::new("b", ContainerAction::Die));

        let changed = containers.update_ports([
            publishing("a", ContainerState::Running, "8080"),
            publishing("b", ContainerState::Running, "9090"),
            publishing("x", ContainerState::Running, "7070"),
        ]);
        assert!(changed);
        let ports = |id: &str| {
            containers
                .get(id)
                .map(|c| c.ports.iter().map(ToString::to_string).collect::<Vec<_>>())
        };
        assert_eq!(ports("a"), Some(vec!["8080/tcp".to_owned()]));
        assert_eq!(ports("b"), Some(vec![]), "stopped since the listing");
        assert_eq!(ports("c"), Some(vec![]), "not listed");
        assert_eq!(ports("x"), None, "unknown containers are not added");
        assert!(!containers.update_ports([publishing("a", ContainerState::Running, "8080")]));
    }

    #[test]
    fn stopping_releases_ports() {
        let cases = [
            (ContainerAction::Stop, false),
            (ContainerAction::Die, false),
            (ContainerAction::Pause, true),
            (ContainerAction::Health(Health::Healthy), true),
            (ContainerAction::Kill, true),
        ];
        for (action, kept) in cases {
            let mut containers = Containers::new();
            containers.replace_all([publishing("a", ContainerState::Running, "8080")]);
            containers.apply(&ContainerEvent::new("a", action));
            assert_eq!(
                containers.get("a").unwrap().ports.is_empty(),
                !kept,
                "{action:?}"
            );
        }
    }

    #[test]
    fn container_action_wire_shape() {
        let cases = [
            (ContainerAction::Start, serde_json::json!("start")),
            (
                ContainerAction::Health(Health::Unhealthy),
                serde_json::json!({ "health": "unhealthy" }),
            ),
        ];
        for (action, want) in cases {
            assert_eq!(serde_json::to_value(action).unwrap(), want);
            assert_eq!(
                serde_json::from_value::<ContainerAction>(want).unwrap(),
                action
            );
        }
    }

    fn in_project(mut status: ContainerStatus, project: &str) -> ContainerStatus {
        status.project = Some(project.into());
        status
    }

    fn event(action: ContainerAction, at_ms: i64) -> ContainerEvent {
        let mut event = ContainerEvent::new("a", action);
        event.at = Some(at_ms);
        event
    }

    fn reason(containers: &Containers, id: &str) -> StopReason {
        containers.get(id).unwrap().stop_reason
    }

    #[test]
    fn events_and_intents_decide_the_stop_reason() {
        use crate::engine::{Intent, IntentAction};
        let mut containers = Containers::new();
        containers.replace_all([
            in_project(running("a"), "blog"),
            in_project(running("b"), "shop"),
        ]);
        assert_eq!(reason(&containers, "a"), StopReason::Running);

        let mut kill = event(ContainerAction::Kill, 2_000);
        kill.signal = Some(15);
        assert!(
            !containers.apply(&kill),
            "a kill alone changes nothing visible"
        );
        let mut die = event(ContainerAction::Die, 2_100);
        die.exit_code = Some(0);
        assert!(containers.apply(&die));
        assert_eq!(reason(&containers, "a"), StopReason::StoppedOutside);

        assert!(containers.set_intent("blog", Intent::new(IntentAction::Stop, 1_900)));
        assert_eq!(reason(&containers, "a"), StopReason::StoppedByAlchemy);
        assert_eq!(reason(&containers, "b"), StopReason::Running);
        assert!(!containers.set_intent("shop", Intent::new(IntentAction::Down, 1_900)));

        containers.replace_all([in_project(
            ContainerStatus::new("a", "a", ContainerState::Exited),
            "blog",
        )]);
        assert_eq!(
            reason(&containers, "a"),
            StopReason::StoppedByAlchemy,
            "a relisting keeps the trail and the intent"
        );
        assert_eq!(
            containers.intent("blog"),
            Some(&Intent::new(IntentAction::Stop, 1_900))
        );

        assert!(containers.set_intent("blog", Intent::new(IntentAction::Start, 3_000)));
        assert_eq!(reason(&containers, "a"), StopReason::StoppedOutside);
    }

    #[test]
    fn stopped_containers_without_history_are_inspected() {
        let exited = |id: &str| ContainerStatus::new(id, id, ContainerState::Exited);
        let mut containers = Containers::new();
        containers.replace_all([
            in_project(exited("a"), "blog"),
            in_project(running("b"), "blog"),
            exited("c"),
            in_project(ContainerStatus::new("d", "d", ContainerState::Dead), "blog"),
        ]);
        assert_eq!(containers.needs_inspect(), ["a", "d"]);
        assert_eq!(reason(&containers, "a"), StopReason::Unknown);

        let inspected = InspectedExit {
            exit_code: Some(1),
            ..InspectedExit::default()
        };
        assert!(containers.record_inspect("a", inspected));
        assert_eq!(
            reason(&containers, "a"),
            StopReason::Crashed {
                exit_code: Some(1),
                oom: false
            }
        );
        assert_eq!(containers.needs_inspect(), ["d"]);
        assert!(!containers.record_inspect("a", InspectedExit::default()));
        assert!(!containers.record_inspect("gone", inspected));

        let mut die = ContainerEvent::new("d", ContainerAction::Die);
        die.exit_code = Some(0);
        containers.apply(&die);
        assert!(
            !containers.record_inspect("d", inspected),
            "an inspect never overrides an event"
        );
        assert_eq!(reason(&containers, "d"), StopReason::Finished);
    }
}
