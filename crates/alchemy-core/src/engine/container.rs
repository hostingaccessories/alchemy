//! Containers as seen through the Engine API and its event stream.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
    /// Lifecycle state.
    pub state: ContainerState,
    /// Healthcheck result, if the container has a healthcheck.
    ///
    /// Only meaningful while the container is running.
    #[serde(default)]
    pub health: Option<Health>,
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
            state,
            health: None,
        }
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
        }
    }
}

/// The daemon's view of every container, keyed by ID.
///
/// Seed it with [`Containers::replace_all`] from a full listing, then keep
/// it current with [`Containers::apply`] for each event. Iteration order
/// is by ID, so output is deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Containers {
    by_id: BTreeMap<String, ContainerStatus>,
}

impl Containers {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace everything with a fresh listing. Returns whether anything
    /// changed.
    pub fn replace_all(&mut self, containers: impl IntoIterator<Item = ContainerStatus>) -> bool {
        let next: BTreeMap<_, _> = containers.into_iter().map(|c| (c.id.clone(), c)).collect();
        let changed = next != self.by_id;
        self.by_id = next;
        changed
    }

    /// Apply one event. Returns whether anything changed.
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
        if current == Some(&next) {
            return false;
        }
        self.by_id.insert(next.id.clone(), next);
        true
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
            assert_eq!(containers.get("a"), want.as_ref(), "{action:?}");
            assert_eq!(changed, want.as_ref() != Some(&start), "{action:?}");
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
        assert!(containers.apply(&create));

        let mut want = ContainerStatus::new("abc", "web-1", ContainerState::Created);
        want.image = Some("nginx:1".into());
        want.project = Some("web".into());
        want.service = Some("nginx".into());
        assert_eq!(containers.get("abc"), Some(&want));

        let mut rename = ContainerEvent::new("abc", ContainerAction::Rename);
        rename.name = Some("web-2".into());
        assert!(containers.apply(&rename));
        want.name = "web-2".into();
        assert_eq!(containers.get("abc"), Some(&want));

        assert!(!containers.apply(&rename));
        assert!(containers.apply(&ContainerEvent::new("abc", ContainerAction::Start)));
        want.state = ContainerState::Running;
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
                "state": "running",
                "health": "healthy",
            })
        );
        assert_eq!(
            serde_json::from_value::<ContainerStatus>(json).unwrap(),
            status
        );
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
}
