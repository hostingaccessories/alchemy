//! Pure mappings between the Compose runner's terms and the operation
//! model's: events to [`Progress`], errors to sentences, plans to
//! [`ActionPlan`]s.

use alchemy_api::operations::{ActionPlan, PlannedStep};
use alchemy_core::StackName;
use alchemy_core::operation::{BuildLog, BuildStep, Progress, ResourceState, step_label};

use crate::compose::{
    BuildVertex, ComposeError, ComposeEvent, ComposePlan, OutputLine, ProgressEvent,
    ResourceStatus, VertexLog,
};

/// What `event` reports, in the operation model's terms.
pub fn progress(event: &ComposeEvent) -> Progress {
    match event {
        ComposeEvent::StepStarted { step, .. } => Progress::StepStarted { step: *step },
        ComposeEvent::StepFinished { step, .. } => Progress::StepFinished { step: *step },
        ComposeEvent::Output(line) => line_progress(line),
    }
}

fn line_progress(line: &OutputLine) -> Progress {
    let step = line.step;
    match &line.progress {
        Some(ProgressEvent::Resource(resource)) => Progress::Resource {
            step,
            id: resource.id.clone(),
            parent: resource.parent_id.clone(),
            state: resource_state(&resource.status),
            text: resource.text.clone(),
            details: resource.details.clone(),
        },
        Some(ProgressEvent::Message { error, message }) => Progress::Message {
            step,
            error: *error,
            message: message.clone(),
        },
        Some(ProgressEvent::Build(build)) => Progress::Build {
            step,
            vertexes: build.vertexes.iter().map(build_step).collect(),
            logs: build.logs.iter().map(build_log).collect(),
        },
        None => Progress::Text {
            step,
            text: line.text.clone(),
        },
    }
}

fn resource_state(status: &ResourceStatus) -> ResourceState {
    match status {
        ResourceStatus::Working => ResourceState::Working,
        ResourceStatus::Done => ResourceState::Done,
        ResourceStatus::Warning => ResourceState::Warning,
        ResourceStatus::Error => ResourceState::Error,
        ResourceStatus::Other(_) => ResourceState::Other,
    }
}

fn build_step(vertex: &BuildVertex) -> BuildStep {
    BuildStep {
        digest: vertex.digest.clone(),
        name: vertex.name.clone(),
        started: vertex.started,
        completed: vertex.completed,
        cached: vertex.cached,
        error: vertex.error.clone(),
    }
}

fn build_log(log: &VertexLog) -> BuildLog {
    BuildLog {
        vertex: log.vertex.clone(),
        stream: log.stream,
        data: log.data.clone(),
    }
}

/// Why a run failed, in a sentence safe to show.
pub fn failure_message(error: &ComposeError) -> String {
    match error {
        ComposeError::RelativeBinary { .. } => {
            "alchemyd couldn't find the docker command".to_owned()
        }
        ComposeError::MissingDir { dir } => {
            format!("the app's folder {} no longer exists", dir.display())
        }
        ComposeError::Spawn { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            "Docker isn't installed, or alchemyd can't find the docker command".to_owned()
        }
        ComposeError::Spawn { source, .. } => format!("alchemyd couldn't run docker: {source}"),
        ComposeError::Timeout { action, after } => format!(
            "docker compose {action} took longer than {} minutes and was stopped",
            after.as_secs().div_ceil(60)
        ),
        ComposeError::Cancelled { .. } => "Cancelled".to_owned(),
        ComposeError::Failed {
            command,
            code,
            message,
            ..
        } => {
            if message.trim().is_empty() {
                match code {
                    Some(code) => format!("docker compose {command} exited with status {code}"),
                    None => format!("docker compose {command} was stopped by a signal"),
                }
            } else {
                message.clone()
            }
        }
    }
}

/// `plan` for `app` as clients see it, each step's command line starting
/// with `docker`.
pub fn action_plan(app: StackName, plan: &ComposePlan) -> ActionPlan {
    let action = plan.action();
    ActionPlan {
        app,
        action,
        steps: plan
            .steps()
            .iter()
            .map(|step| PlannedStep {
                command: step.command(),
                label: step_label(action, step.command()).to_owned(),
                argv: std::iter::once("docker".to_owned())
                    .chain(
                        step.args()
                            .iter()
                            .map(|arg| arg.to_string_lossy().into_owned()),
                    )
                    .collect(),
            })
            .collect(),
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
    use std::time::Duration;

    use alchemy_core::operation::StepCommand;

    use super::*;
    use crate::compose::{ComposeAction, ComposeCommand, ComposeProject, ImageSource, Stream};

    #[test]
    #[allow(clippy::too_many_lines)]
    fn maps_events_to_progress() {
        let cases = [
            (
                ComposeEvent::StepStarted {
                    step: 1,
                    command: ComposeCommand::Up,
                },
                Progress::StepStarted { step: 1 },
            ),
            (
                ComposeEvent::StepFinished {
                    step: 1,
                    command: ComposeCommand::Up,
                },
                Progress::StepFinished { step: 1 },
            ),
            (
                ComposeEvent::Output(OutputLine::new(0, Stream::Stdout, "plain")),
                Progress::Text {
                    step: 0,
                    text: "plain".into(),
                },
            ),
            (
                ComposeEvent::Output(OutputLine::new(
                    0,
                    Stream::Stderr,
                    r#"{"id":"Container a","parent_id":"x","status":"Error","text":"Error","details":"boom"}"#,
                )),
                Progress::Resource {
                    step: 0,
                    id: "Container a".into(),
                    parent: Some("x".into()),
                    state: ResourceState::Error,
                    text: "Error".into(),
                    details: Some("boom".into()),
                },
            ),
            (
                ComposeEvent::Output(OutputLine::new(
                    0,
                    Stream::Stderr,
                    r#"{"id":"Image a","status":"Pending","text":"Waiting"}"#,
                )),
                Progress::Resource {
                    step: 0,
                    id: "Image a".into(),
                    parent: None,
                    state: ResourceState::Other,
                    text: "Waiting".into(),
                    details: None,
                },
            ),
            (
                ComposeEvent::Output(OutputLine::new(
                    2,
                    Stream::Stderr,
                    r#"{"error":true,"message":"pull access denied"}"#,
                )),
                Progress::Message {
                    step: 2,
                    error: true,
                    message: "pull access denied".into(),
                },
            ),
            (
                ComposeEvent::Output(OutputLine::new(
                    0,
                    Stream::Stdout,
                    r#"{"vertexes":[{"digest":"sha256:a","name":"[1/2] FROM node","started":"t","completed":"t","cached":true}]}"#,
                )),
                Progress::Build {
                    step: 0,
                    vertexes: vec![BuildStep {
                        digest: "sha256:a".into(),
                        name: "[1/2] FROM node".into(),
                        started: true,
                        completed: true,
                        cached: true,
                        error: None,
                    }],
                    logs: Vec::new(),
                },
            ),
            (
                ComposeEvent::Output(OutputLine::new(
                    1,
                    Stream::Stdout,
                    r#"{"logs":[{"vertex":"sha256:b","stream":2,"data":"aGkK","timestamp":"t"}]}"#,
                )),
                Progress::Build {
                    step: 1,
                    vertexes: Vec::new(),
                    logs: vec![BuildLog {
                        vertex: "sha256:b".into(),
                        stream: 2,
                        data: b"hi\n".to_vec(),
                    }],
                },
            ),
        ];
        for (event, want) in cases {
            assert_eq!(progress(&event), want, "{event:?}");
        }
    }

    #[test]
    fn explains_failures_in_a_sentence() {
        let failed = |code, message: &str| ComposeError::Failed {
            action: ComposeAction::Pull,
            command: ComposeCommand::Pull,
            code,
            message: message.to_owned(),
        };
        let cases = [
            (failed(Some(1), "pull access denied"), "pull access denied"),
            (
                failed(Some(18), "  "),
                "docker compose pull exited with status 18",
            ),
            (
                failed(None, ""),
                "docker compose pull was stopped by a signal",
            ),
            (
                ComposeError::MissingDir {
                    dir: "/srv/blog".into(),
                },
                "the app's folder /srv/blog no longer exists",
            ),
            (
                ComposeError::Spawn {
                    binary: "/usr/bin/docker".into(),
                    source: std::io::ErrorKind::NotFound.into(),
                },
                "Docker isn't installed, or alchemyd can't find the docker command",
            ),
            (
                ComposeError::Spawn {
                    binary: "/usr/bin/docker".into(),
                    source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
                },
                "alchemyd couldn't run docker: denied",
            ),
            (
                ComposeError::Timeout {
                    action: ComposeAction::Up,
                    after: Duration::from_mins(15),
                },
                "docker compose up took longer than 15 minutes and was stopped",
            ),
            (
                ComposeError::Cancelled {
                    action: ComposeAction::Up,
                },
                "Cancelled",
            ),
            (
                ComposeError::RelativeBinary {
                    binary: "docker".into(),
                },
                "alchemyd couldn't find the docker command",
            ),
        ];
        for (error, want) in cases {
            assert_eq!(failure_message(&error), want, "{error:?}");
        }
    }

    #[test]
    fn plans_start_with_docker() {
        let project = ComposeProject::new("/srv/my apps/blog").unwrap();
        let plan = ComposePlan::new(&project, ComposeAction::Update, ImageSource::Registry);
        let got = action_plan(StackName::new("blog").unwrap(), &plan);
        assert_eq!(got.action, ComposeAction::Update);
        assert_eq!(
            got.steps
                .iter()
                .map(|s| (s.command, s.label.as_str(), s.argv.join(" ")))
                .collect::<Vec<_>>(),
            [
                (
                    StepCommand::Pull,
                    "Downloading updates",
                    "docker compose --project-directory /srv/my apps/blog --progress json pull"
                        .to_owned()
                ),
                (
                    StepCommand::Up,
                    "Starting the new version",
                    "docker compose --project-directory /srv/my apps/blog --progress json up --detach"
                        .to_owned()
                ),
            ]
        );
        assert_eq!(got.steps[0].argv[3], "/srv/my apps/blog");
    }
}
