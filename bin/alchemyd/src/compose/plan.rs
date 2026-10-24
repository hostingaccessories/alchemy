//! What each action runs: a plan of one or more `docker compose` steps.

use std::ffi::OsString;
use std::fmt;
use std::str::FromStr;

use super::ComposeProject;

/// A lifecycle action on a Compose project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComposeAction {
    /// Create and start every service, detached.
    Up,
    /// Stop and remove containers and networks. Volumes are kept.
    Down,
    /// Start existing containers.
    Start,
    /// Stop running containers without removing them.
    Stop,
    /// Restart every service.
    Restart,
    /// Pull every service's image.
    Pull,
    /// Build every service that has a `build:` section.
    Build,
    /// Fetch or rebuild images, then recreate what changed. There is no
    /// image rollback.
    Update,
}

impl ComposeAction {
    /// Every action, in the order the UI lists them.
    pub const ALL: [Self; 8] = [
        Self::Up,
        Self::Down,
        Self::Start,
        Self::Stop,
        Self::Restart,
        Self::Pull,
        Self::Build,
        Self::Update,
    ];

    /// The action's name, such as `up`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Pull => "pull",
            Self::Build => "build",
            Self::Update => "update",
        }
    }
}

impl fmt::Display for ComposeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A name that is not a [`ComposeAction`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown compose action {name:?}")]
pub struct UnknownAction {
    /// The rejected name.
    pub name: String,
}

impl FromStr for ComposeAction {
    type Err = UnknownAction;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == name)
            .ok_or_else(|| UnknownAction {
                name: name.to_owned(),
            })
    }
}

/// A `docker compose` subcommand that a [`ComposeStep`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComposeCommand {
    /// `up`
    Up,
    /// `down`
    Down,
    /// `start`
    Start,
    /// `stop`
    Stop,
    /// `restart`
    Restart,
    /// `pull`
    Pull,
    /// `build`
    Build,
}

impl ComposeCommand {
    /// The subcommand, such as `up`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Pull => "pull",
            Self::Build => "build",
        }
    }
}

impl fmt::Display for ComposeCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a stack's images come from, which decides how
/// [`ComposeAction::Update`] refreshes them.
///
/// Whoever reads the compose file decides this; the plan only uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ImageSource {
    /// Every service names an image to pull.
    #[default]
    Registry,
    /// At least one service has a `build:` section.
    Build,
}

/// One `docker compose` run within a [`ComposePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComposeStep {
    command: ComposeCommand,
    args: Vec<OsString>,
}

impl ComposeStep {
    fn new(project: &ComposeProject, command: ComposeCommand, flags: &[&str]) -> Self {
        let mut args: Vec<OsString> = vec![
            "compose".into(),
            "--project-directory".into(),
            project.dir().as_os_str().to_owned(),
            "--progress".into(),
            "json".into(),
            command.as_str().into(),
        ];
        args.extend(flags.iter().map(OsString::from));
        Self { command, args }
    }

    /// The subcommand this step runs.
    pub fn command(&self) -> ComposeCommand {
        self.command
    }

    /// The argv after the `docker` binary, such as
    /// `compose --project-directory /srv/blog --progress json up --detach`.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

/// The steps that carry out one [`ComposeAction`] on one project, in
/// order. Each must succeed before the next runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComposePlan {
    action: ComposeAction,
    steps: Vec<ComposeStep>,
}

impl ComposePlan {
    /// The plan for `action` on `project`. `images` matters only to
    /// [`ComposeAction::Update`].
    ///
    /// Update pulls, then recreates with `up --detach`. When the stack
    /// builds, the pull skips buildable services and a `build --pull`
    /// refreshes their base images instead, so services that pull and
    /// services that build are both updated.
    pub fn new(project: &ComposeProject, action: ComposeAction, images: ImageSource) -> Self {
        use ComposeCommand as C;
        let steps: &[(ComposeCommand, &[&str])] = match (action, images) {
            (ComposeAction::Up, _) => &[(C::Up, &["--detach"])],
            (ComposeAction::Down, _) => &[(C::Down, &[])],
            (ComposeAction::Start, _) => &[(C::Start, &[])],
            (ComposeAction::Stop, _) => &[(C::Stop, &[])],
            (ComposeAction::Restart, _) => &[(C::Restart, &[])],
            (ComposeAction::Pull, _) => &[(C::Pull, &[])],
            (ComposeAction::Build, _) => &[(C::Build, &[])],
            (ComposeAction::Update, ImageSource::Registry) => {
                &[(C::Pull, &[]), (C::Up, &["--detach"])]
            }
            (ComposeAction::Update, ImageSource::Build) => &[
                (C::Pull, &["--ignore-buildable"]),
                (C::Build, &["--pull"]),
                (C::Up, &["--detach"]),
            ],
        };
        Self {
            action,
            steps: steps
                .iter()
                .map(|(command, flags)| ComposeStep::new(project, *command, flags))
                .collect(),
        }
    }

    /// The action this plan carries out.
    pub fn action(&self) -> ComposeAction {
        self.action
    }

    /// The steps, in the order they run. Never empty.
    pub fn steps(&self) -> &[ComposeStep] {
        &self.steps
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

    fn argv(step: &ComposeStep) -> String {
        step.args()
            .iter()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn plans_every_action() {
        let blog = ComposeProject::new("/srv/stacks/blog").unwrap();
        let prefix = "compose --project-directory /srv/stacks/blog --progress json";
        let cases: [(ComposeAction, ImageSource, &[&str]); 10] = [
            (ComposeAction::Up, ImageSource::Registry, &["up --detach"]),
            (ComposeAction::Down, ImageSource::Registry, &["down"]),
            (ComposeAction::Start, ImageSource::Registry, &["start"]),
            (ComposeAction::Stop, ImageSource::Registry, &["stop"]),
            (ComposeAction::Restart, ImageSource::Registry, &["restart"]),
            (ComposeAction::Pull, ImageSource::Registry, &["pull"]),
            (ComposeAction::Build, ImageSource::Registry, &["build"]),
            (ComposeAction::Build, ImageSource::Build, &["build"]),
            (
                ComposeAction::Update,
                ImageSource::Registry,
                &["pull", "up --detach"],
            ),
            (
                ComposeAction::Update,
                ImageSource::Build,
                &["pull --ignore-buildable", "build --pull", "up --detach"],
            ),
        ];
        for (action, images, want) in cases {
            let plan = ComposePlan::new(&blog, action, images);
            assert_eq!(plan.action(), action);
            let got: Vec<String> = plan.steps().iter().map(argv).collect();
            let want: Vec<String> = want.iter().map(|w| format!("{prefix} {w}")).collect();
            assert_eq!(got, want, "{action} {images:?}");
            for step in plan.steps() {
                assert_eq!(step.args()[5], step.command().as_str());
                assert!(
                    !step
                        .args()
                        .iter()
                        .any(|a| a == "-p" || a == "--project-name")
                );
            }
        }
    }

    #[test]
    fn plans_every_action_with_at_least_one_step() {
        let blog = ComposeProject::new("/srv/blog").unwrap();
        for action in ComposeAction::ALL {
            for images in [ImageSource::Registry, ImageSource::Build] {
                assert_ne!(ComposePlan::new(&blog, action, images).steps(), []);
            }
        }
    }

    #[test]
    fn keeps_a_directory_with_spaces_as_one_argument() {
        let project = ComposeProject::new("/srv/my stacks/blog").unwrap();
        let plan = ComposePlan::new(&project, ComposeAction::Stop, ImageSource::Registry);
        let args = plan.steps()[0].args();
        assert_eq!(args[2], OsString::from("/srv/my stacks/blog"));
        assert_eq!(args.len(), 6);
    }

    #[test]
    fn parses_action_names() {
        for action in ComposeAction::ALL {
            assert_eq!(action.as_str().parse(), Ok(action));
            assert_eq!(action.to_string(), action.as_str());
        }
        for name in ["", "UP", "kill", "up --detach"] {
            assert_eq!(
                name.parse::<ComposeAction>(),
                Err(UnknownAction {
                    name: name.to_owned()
                })
            );
        }
    }
}
