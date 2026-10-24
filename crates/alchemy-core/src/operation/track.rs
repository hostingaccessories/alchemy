//! [`Tracker`]: from what a run reports to the [`Operation`] clients see.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    Action, MAX_OUTPUT_LINE, Operation, OperationState, Step, StepCommand, StepState, step_label,
};
use crate::StackName;
use crate::logs::{LineSplitter, TRUNCATED};

/// The most characters of a build step's name kept in the tag on each of
/// its log lines.
const TAG_CHARS: usize = 60;

/// Characters of a digest, after its algorithm, that tag the log lines of
/// a build step whose name is unknown.
const SHORT_DIGEST: usize = 12;

/// The most bytes kept of one build log line, leaving room under
/// [`MAX_OUTPUT_LINE`] for its tag.
const LOG_LINE: usize = MAX_OUTPUT_LINE - 1024;

/// How far along one resource is, as Compose reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResourceState {
    /// In progress, such as pulling or starting.
    Working,
    /// Finished, such as pulled or started.
    Done,
    /// Finished with a warning, or interrupted.
    Warning,
    /// Failed.
    Error,
    /// A status Compose sent that has no meaning here.
    Other,
}

/// One build step from `BuildKit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStep {
    /// Identifies the step across reports.
    pub digest: String,
    /// What the step does, such as `[2/3] RUN npm ci`.
    pub name: String,
    /// Whether it has started.
    pub started: bool,
    /// Whether it has finished.
    pub completed: bool,
    /// Whether it was a cache hit.
    pub cached: bool,
    /// Why it failed.
    pub error: Option<String>,
}

/// A chunk of what a build step wrote, as `BuildKit` sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildLog {
    /// The digest of the build step that wrote it.
    pub vertex: String,
    /// The step's stream it came from: 1 for standard output, 2 for
    /// standard error.
    pub stream: u8,
    /// The bytes, which need not end at a line or a character.
    pub data: Vec<u8>,
}

/// Something a run reported. `step` is an index into
/// [`Operation::steps`]; reports for a step that does not exist are
/// ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Progress {
    /// A step started.
    StepStarted {
        /// The step.
        step: usize,
    },
    /// A step finished successfully.
    StepFinished {
        /// The step.
        step: usize,
    },
    /// A line of output that is not progress.
    Text {
        /// The step that wrote it.
        step: usize,
        /// The line.
        text: String,
    },
    /// A container, network, volume, or image changed state.
    Resource {
        /// The step that reported it.
        step: usize,
        /// The resource, such as `Container blog-web-1`.
        id: String,
        /// The resource it is part of, such as the image of a layer.
        parent: Option<String>,
        /// How far along it is.
        state: ResourceState,
        /// What is happening, such as `Pulling` or `Started`.
        text: String,
        /// More detail, such as why it failed.
        details: Option<String>,
    },
    /// A message about the run as a whole.
    Message {
        /// The step that wrote it.
        step: usize,
        /// Whether it is the error that failed the run.
        error: bool,
        /// The message.
        message: String,
    },
    /// Build steps changed, or wrote output.
    ///
    /// Each complete line a build step writes becomes an output line
    /// `NAME | LINE`, tagged with the step's name, or the start of its
    /// digest before its name is known. Lines end at `\n`; on a line
    /// redrawn with `\r`, as progress bars are, only the last drawing is
    /// kept. A line still open when its step finishes or fails is written
    /// then.
    Build {
        /// The step that reported them.
        step: usize,
        /// The build steps in this report.
        vertexes: Vec<BuildStep>,
        /// What the build steps wrote, in order.
        logs: Vec<BuildLog>,
    },
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every step finished.
    Succeeded,
    /// A step failed, for the reason given.
    Failed(String),
    /// Someone cancelled it.
    Cancelled,
    /// The daemon stopped before it finished.
    Interrupted,
}

impl Outcome {
    fn state(&self) -> OperationState {
        match self {
            Self::Succeeded => OperationState::Succeeded,
            Self::Failed(_) => OperationState::Failed,
            Self::Cancelled => OperationState::Cancelled,
            Self::Interrupted => OperationState::Interrupted,
        }
    }

    fn error(&self) -> Option<String> {
        match self {
            Self::Succeeded => None,
            Self::Failed(reason) => Some(reason.clone()),
            Self::Cancelled => Some("Cancelled before it finished".to_owned()),
            Self::Interrupted => Some("alchemyd stopped before it finished".to_owned()),
        }
    }

    fn step_message(&self) -> Option<String> {
        match self {
            Self::Succeeded => None,
            Self::Failed(reason) => Some(reason.clone()),
            Self::Cancelled => Some("Cancelled".to_owned()),
            Self::Interrupted => Some("Stopped when alchemyd shut down".to_owned()),
        }
    }
}

/// What applying one report did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Whether the [`Operation`] changed.
    pub changed: bool,
    /// Output lines to keep, with the step that wrote each.
    pub lines: Vec<(usize, String)>,
}

/// What one step has seen, for its progress message and to write each
/// change once.
#[derive(Debug, Clone, Default)]
struct Tally {
    resources: BTreeMap<String, Seen>,
    vertexes: BTreeMap<String, bool>,
    written: BTreeSet<(String, bool)>,
    names: BTreeMap<String, String>,
    logs: BTreeMap<(String, u8), LineSplitter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    top: bool,
    state: ResourceState,
    text: String,
}

/// Keeps one [`Operation`] current as its run reports progress.
#[derive(Debug, Clone)]
pub struct Tracker {
    operation: Operation,
    tallies: Vec<Tally>,
}

impl Tracker {
    /// A running operation `id` of `action` on `app`, started at
    /// `started_at`, whose steps run `commands` in order.
    pub fn new(
        id: impl Into<String>,
        app: StackName,
        action: Action,
        commands: &[StepCommand],
        started_at: i64,
    ) -> Self {
        let steps = commands
            .iter()
            .map(|&command| Step {
                command,
                label: step_label(action, command).to_owned(),
                state: StepState::Pending,
                message: None,
            })
            .collect();
        Self::resume(Operation {
            id: id.into(),
            app,
            action,
            state: OperationState::Running,
            steps,
            started_at,
            finished_at: None,
            error: None,
        })
    }

    /// Keep tracking `operation`, such as one read back from storage, with
    /// no progress seen yet.
    pub fn resume(operation: Operation) -> Self {
        let tallies = vec![Tally::default(); operation.steps.len()];
        Self { operation, tallies }
    }

    /// The operation as it stands.
    pub fn operation(&self) -> &Operation {
        &self.operation
    }

    /// Take in one report. Reports after [`Self::finish`] change nothing.
    pub fn apply(&mut self, progress: &Progress) -> Applied {
        if self.operation.state.is_finished() {
            return Applied::default();
        }
        match progress {
            Progress::StepStarted { step } => self.start(*step),
            Progress::StepFinished { step } => self.set_state(*step, StepState::Done),
            Progress::Text { step, text } => self.line(*step, text.clone()),
            Progress::Message {
                step,
                error,
                message,
            } => {
                let text = if *error {
                    format!("Error: {message}")
                } else {
                    message.clone()
                };
                self.line(*step, text)
            }
            Progress::Resource {
                step,
                id,
                parent,
                state,
                text,
                details,
            } => self.resource(
                *step,
                id,
                parent.is_none(),
                *state,
                text,
                details.as_deref(),
            ),
            Progress::Build {
                step,
                vertexes,
                logs,
            } => self.build(*step, vertexes, logs),
        }
    }

    /// End the operation at `at` with `outcome`. The step that was running
    /// takes the outcome; steps that never ran are skipped, or done when
    /// it succeeded. Finishing twice changes nothing.
    pub fn finish(&mut self, outcome: &Outcome, at: i64) {
        if self.operation.state.is_finished() {
            return;
        }
        self.operation.state = outcome.state();
        self.operation.finished_at = Some(at);
        self.operation.error = outcome.error();
        let mut blamed = false;
        let has_active = self
            .operation
            .steps
            .iter()
            .any(|s| s.state == StepState::Active);
        for step in &mut self.operation.steps {
            let open = match step.state {
                StepState::Active => true,
                StepState::Pending => !has_active,
                _ => false,
            };
            step.state = match (outcome, step.state) {
                (Outcome::Succeeded, StepState::Active | StepState::Pending) => StepState::Done,
                (_, StepState::Active | StepState::Pending) if open && !blamed => {
                    blamed = true;
                    step.message = outcome.step_message();
                    StepState::Failed
                }
                (_, StepState::Active | StepState::Pending) => StepState::Skipped,
                (_, state) => state,
            };
        }
    }

    fn start(&mut self, index: usize) -> Applied {
        if index >= self.operation.steps.len() {
            return Applied::default();
        }
        for step in self.operation.steps.iter_mut().take(index) {
            if step.state == StepState::Active {
                step.state = StepState::Done;
            }
        }
        self.set_state(index, StepState::Active)
    }

    fn set_state(&mut self, index: usize, state: StepState) -> Applied {
        match self.operation.steps.get_mut(index) {
            Some(step) if step.state != state => {
                step.state = state;
                Applied {
                    changed: true,
                    lines: Vec::new(),
                }
            }
            _ => Applied::default(),
        }
    }

    fn line(&self, step: usize, text: String) -> Applied {
        if step >= self.operation.steps.len() {
            return Applied::default();
        }
        Applied {
            changed: false,
            lines: vec![(step, text)],
        }
    }

    fn resource(
        &mut self,
        index: usize,
        id: &str,
        top: bool,
        state: ResourceState,
        text: &str,
        details: Option<&str>,
    ) -> Applied {
        let Some(tally) = self.tallies.get_mut(index) else {
            return Applied::default();
        };
        let seen = Seen {
            top,
            state,
            text: text.to_owned(),
        };
        if tally.resources.get(id) == Some(&seen) {
            return Applied::default();
        }
        tally.resources.insert(id.to_owned(), seen);
        let mut lines = Vec::new();
        if top || state != ResourceState::Working {
            let line = match details {
                Some(details) => format!("{id} {text}: {details}"),
                None => format!("{id} {text}"),
            };
            lines.push((index, line));
        }
        Applied {
            changed: self.refresh_message(index),
            lines,
        }
    }

    /// Write each build step's start, then the log lines in the report,
    /// then any line left open by a step that is now done, then each
    /// step's end, so a step's output always comes between the two.
    fn build(&mut self, index: usize, vertexes: &[BuildStep], logs: &[BuildLog]) -> Applied {
        let Some(tally) = self.tallies.get_mut(index) else {
            return Applied::default();
        };
        let mut lines = Vec::new();
        for vertex in vertexes {
            if vertex.name.is_empty() {
                continue;
            }
            tally
                .names
                .insert(vertex.digest.clone(), vertex.name.clone());
            let done = vertex.completed || vertex.error.is_some();
            let entry = tally.vertexes.entry(vertex.digest.clone()).or_insert(false);
            *entry |= done;
            if vertex.started && !done && tally.written.insert((vertex.digest.clone(), false)) {
                lines.push((index, vertex.name.clone()));
            }
        }
        for log in logs {
            let splitter = tally
                .logs
                .entry((log.vertex.clone(), log.stream))
                .or_insert_with(|| LineSplitter::with_max(LOG_LINE));
            for line in splitter.push(&log.data) {
                lines.push((index, log_line(&tally.names, &log.vertex, &line)));
            }
        }
        let closed: Vec<(String, u8)> = tally
            .logs
            .keys()
            .filter(|(digest, _)| tally.vertexes.get(digest) == Some(&true))
            .cloned()
            .collect();
        for key in closed {
            if let Some(line) = tally.logs.remove(&key).and_then(|mut s| s.finish()) {
                lines.push((index, log_line(&tally.names, &key.0, &line)));
            }
        }
        for vertex in vertexes {
            if vertex.name.is_empty() {
                continue;
            }
            let done = vertex.completed || vertex.error.is_some();
            if done && tally.written.insert((vertex.digest.clone(), true)) {
                let line = match (&vertex.error, vertex.cached) {
                    (Some(error), _) => format!("{}: error: {error}", vertex.name),
                    (None, true) => format!("{}: cached", vertex.name),
                    (None, false) => format!("{}: done", vertex.name),
                };
                lines.push((index, line));
            }
        }
        Applied {
            changed: self.refresh_message(index),
            lines,
        }
    }

    fn refresh_message(&mut self, index: usize) -> bool {
        let (Some(step), Some(tally)) =
            (self.operation.steps.get_mut(index), self.tallies.get(index))
        else {
            return false;
        };
        let message = progress_message(step.command, tally);
        if message.is_none() || step.message == message {
            return false;
        }
        step.message = message;
        true
    }
}

/// `line` from the build step `digest`, tagged with the step's name.
fn log_line(names: &BTreeMap<String, String>, digest: &str, line: &str) -> String {
    let shown = line.rsplit('\r').next().unwrap_or(line);
    let tag = names.get(digest).map_or_else(
        || {
            let hex = digest.split_once(':').map_or(digest, |(_, hex)| hex);
            hex.chars().take(SHORT_DIGEST).collect()
        },
        |name| {
            let first = name.lines().next().unwrap_or_default();
            let mut tag: String = first.chars().take(TAG_CHARS).collect();
            if tag.len() < name.len() {
                tag.push_str(TRUNCATED);
            }
            tag
        },
    );
    format!("{tag} | {shown}")
}

/// The one-line progress of a step running `command`, from what it has
/// seen so far, or `None` before there is anything to count.
fn progress_message(command: StepCommand, tally: &Tally) -> Option<String> {
    let finished = |state: ResourceState| {
        matches!(
            state,
            ResourceState::Done | ResourceState::Warning | ResourceState::Error
        )
    };
    let (done, total, noun, verb) = match command {
        StepCommand::Build => {
            let total = tally.vertexes.len();
            let done = tally.vertexes.values().filter(|done| **done).count();
            (done, total, "build steps", "finished")
        }
        StepCommand::Pull => {
            let images = tally.resources.values().filter(|seen| seen.top);
            let (done, total) = images.fold((0, 0), |(done, total), seen| {
                (done + usize::from(finished(seen.state)), total + 1)
            });
            (done, total, "images", "downloaded")
        }
        command => {
            let containers = tally
                .resources
                .iter()
                .filter(|(id, seen)| seen.top && id.starts_with("Container "));
            let (done, total) = containers.fold((0, 0), |(done, total), (_, seen)| {
                (done + usize::from(finished(seen.state)), total + 1)
            });
            let verb = match command {
                StepCommand::Stop => "stopped",
                StepCommand::Down => "removed",
                StepCommand::Restart => "restarted",
                _ => "started",
            };
            (done, total, "containers", verb)
        }
    };
    (total > 0).then(|| format!("{done} of {total} {noun} {verb}"))
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

    fn tracker(action: Action, commands: &[StepCommand]) -> Tracker {
        Tracker::new(
            "op1",
            StackName::new("blog").unwrap(),
            action,
            commands,
            100,
        )
    }

    fn resource(step: usize, id: &str, state: ResourceState, text: &str) -> Progress {
        Progress::Resource {
            step,
            id: id.to_owned(),
            parent: None,
            state,
            text: text.to_owned(),
            details: None,
        }
    }

    fn layer(step: usize, id: &str, state: ResourceState, text: &str) -> Progress {
        Progress::Resource {
            step,
            id: id.to_owned(),
            parent: Some("Image nginx".to_owned()),
            state,
            text: text.to_owned(),
            details: None,
        }
    }

    fn vertex(digest: &str, name: &str, started: bool, completed: bool) -> BuildStep {
        BuildStep {
            digest: digest.to_owned(),
            name: name.to_owned(),
            started,
            completed,
            cached: false,
            error: None,
        }
    }

    fn states(tracker: &Tracker) -> Vec<(StepState, Option<&str>)> {
        tracker
            .operation()
            .steps
            .iter()
            .map(|s| (s.state, s.message.as_deref()))
            .collect()
    }

    #[test]
    fn starts_running_with_pending_labelled_steps() {
        let t = tracker(Action::Update, &[StepCommand::Pull, StepCommand::Up]);
        let op = t.operation();
        assert_eq!(op.id, "op1");
        assert_eq!(op.app.as_str(), "blog");
        assert_eq!(op.state, OperationState::Running);
        assert_eq!(op.started_at, 100);
        assert_eq!(op.finished_at, None);
        assert_eq!(
            op.steps
                .iter()
                .map(|s| (s.command, s.label.as_str(), s.state))
                .collect::<Vec<_>>(),
            [
                (StepCommand::Pull, "Downloading updates", StepState::Pending),
                (
                    StepCommand::Up,
                    "Starting the new version",
                    StepState::Pending
                ),
            ]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn maps_reports_to_steps_and_lines() {
        use ResourceState::{Done, Error, Working};
        type Case = (
            &'static str,
            Progress,
            bool,
            &'static [(usize, &'static str)],
            [(StepState, Option<&'static str>); 2],
        );
        let pending = (StepState::Pending, None);
        let cases: Vec<Case> = vec![
            (
                "first step starts",
                Progress::StepStarted { step: 0 },
                true,
                &[],
                [(StepState::Active, None), pending],
            ),
            (
                "an image starts pulling",
                resource(0, "Image nginx", Working, "Pulling"),
                true,
                &[(0, "Image nginx Pulling")],
                [
                    (StepState::Active, Some("0 of 1 images downloaded")),
                    pending,
                ],
            ),
            (
                "the same report again",
                resource(0, "Image nginx", Working, "Pulling"),
                false,
                &[],
                [
                    (StepState::Active, Some("0 of 1 images downloaded")),
                    pending,
                ],
            ),
            (
                "a layer downloads quietly",
                layer(0, "abc", Working, "Downloading"),
                false,
                &[],
                [
                    (StepState::Active, Some("0 of 1 images downloaded")),
                    pending,
                ],
            ),
            (
                "a layer finishes",
                layer(0, "abc", Done, "Pull complete"),
                false,
                &[(0, "abc Pull complete")],
                [
                    (StepState::Active, Some("0 of 1 images downloaded")),
                    pending,
                ],
            ),
            (
                "a second image",
                resource(0, "Image redis", Working, "Pulling"),
                true,
                &[(0, "Image redis Pulling")],
                [
                    (StepState::Active, Some("0 of 2 images downloaded")),
                    pending,
                ],
            ),
            (
                "the first image is done",
                resource(0, "Image nginx", Done, "Pulled"),
                true,
                &[(0, "Image nginx Pulled")],
                [
                    (StepState::Active, Some("1 of 2 images downloaded")),
                    pending,
                ],
            ),
            (
                "plain text",
                Progress::Text {
                    step: 0,
                    text: "hello".to_owned(),
                },
                false,
                &[(0, "hello")],
                [
                    (StepState::Active, Some("1 of 2 images downloaded")),
                    pending,
                ],
            ),
            (
                "the step finishes and keeps its message",
                Progress::StepFinished { step: 0 },
                true,
                &[],
                [(StepState::Done, Some("1 of 2 images downloaded")), pending],
            ),
            (
                "the next step starts",
                Progress::StepStarted { step: 1 },
                true,
                &[],
                [
                    (StepState::Done, Some("1 of 2 images downloaded")),
                    (StepState::Active, None),
                ],
            ),
            (
                "a network is not counted",
                resource(1, "Network blog_default", Done, "Created"),
                false,
                &[(1, "Network blog_default Created")],
                [
                    (StepState::Done, Some("1 of 2 images downloaded")),
                    (StepState::Active, None),
                ],
            ),
            (
                "a container starts",
                resource(1, "Container blog-web-1", Working, "Starting"),
                true,
                &[(1, "Container blog-web-1 Starting")],
                [
                    (StepState::Done, Some("1 of 2 images downloaded")),
                    (StepState::Active, Some("0 of 1 containers started")),
                ],
            ),
            (
                "a container fails, with details",
                Progress::Resource {
                    step: 1,
                    id: "Container blog-db-1".to_owned(),
                    parent: None,
                    state: Error,
                    text: "Error".to_owned(),
                    details: Some("port is already allocated".to_owned()),
                },
                true,
                &[(1, "Container blog-db-1 Error: port is already allocated")],
                [
                    (StepState::Done, Some("1 of 2 images downloaded")),
                    (StepState::Active, Some("1 of 2 containers started")),
                ],
            ),
            (
                "an error message",
                Progress::Message {
                    step: 1,
                    error: true,
                    message: "boom".to_owned(),
                },
                false,
                &[(1, "Error: boom")],
                [
                    (StepState::Done, Some("1 of 2 images downloaded")),
                    (StepState::Active, Some("1 of 2 containers started")),
                ],
            ),
            (
                "a report for a step that does not exist",
                Progress::Text {
                    step: 9,
                    text: "lost".to_owned(),
                },
                false,
                &[],
                [
                    (StepState::Done, Some("1 of 2 images downloaded")),
                    (StepState::Active, Some("1 of 2 containers started")),
                ],
            ),
        ];
        let mut t = tracker(Action::Update, &[StepCommand::Pull, StepCommand::Up]);
        for (name, progress, changed, lines, want) in cases {
            let applied = t.apply(&progress);
            assert_eq!(applied.changed, changed, "{name}");
            let got: Vec<(usize, &str)> = applied
                .lines
                .iter()
                .map(|(s, l)| (*s, l.as_str()))
                .collect();
            assert_eq!(got, lines, "{name}");
            assert_eq!(states(&t), want, "{name}");
        }
    }

    #[test]
    fn counts_build_steps_and_writes_each_once() {
        let mut t = tracker(Action::Build, &[StepCommand::Build]);
        t.apply(&Progress::StepStarted { step: 0 });
        let reports = [
            (
                vec![vertex("a", "[1/2] FROM node", true, false)],
                vec!["[1/2] FROM node"],
                "0 of 1 build steps finished",
            ),
            (
                vec![
                    vertex("a", "[1/2] FROM node", true, true),
                    vertex("b", "[2/2] RUN npm ci", false, false),
                ],
                vec!["[1/2] FROM node: done"],
                "1 of 2 build steps finished",
            ),
            (
                vec![BuildStep {
                    cached: true,
                    ..vertex("b", "[2/2] RUN npm ci", true, true)
                }],
                vec!["[2/2] RUN npm ci: cached"],
                "2 of 2 build steps finished",
            ),
            (
                vec![
                    vertex("a", "[1/2] FROM node", true, true),
                    vertex("c", "", true, true),
                ],
                vec![],
                "2 of 2 build steps finished",
            ),
            (
                vec![BuildStep {
                    error: Some("exit code 1".to_owned()),
                    ..vertex("d", "[3/3] RUN make", true, false)
                }],
                vec!["[3/3] RUN make: error: exit code 1"],
                "3 of 3 build steps finished",
            ),
        ];
        for (vertexes, lines, message) in reports {
            let applied = t.apply(&Progress::Build {
                step: 0,
                vertexes,
                logs: Vec::new(),
            });
            let got: Vec<&str> = applied.lines.iter().map(|(_, l)| l.as_str()).collect();
            assert_eq!(got, lines);
            assert_eq!(t.operation().steps[0].message.as_deref(), Some(message));
        }
    }

    fn log(vertex: &str, stream: u8, data: &[u8]) -> BuildLog {
        BuildLog {
            vertex: vertex.to_owned(),
            stream,
            data: data.to_vec(),
        }
    }

    fn build_lines(reports: Vec<(Vec<BuildStep>, Vec<BuildLog>)>) -> Vec<(usize, String)> {
        let mut t = tracker(Action::Build, &[StepCommand::Build]);
        t.apply(&Progress::StepStarted { step: 0 });
        reports
            .into_iter()
            .flat_map(|(vertexes, logs)| {
                t.apply(&Progress::Build {
                    step: 0,
                    vertexes,
                    logs,
                })
                .lines
            })
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn writes_build_step_logs_as_tagged_lines() {
        const RUN: &str = "sha256:aaa";
        type Report = (Vec<BuildStep>, Vec<BuildLog>);
        let running = || vertex(RUN, "[2/2] RUN make", true, false);
        let finished = || vertex(RUN, "[2/2] RUN make", true, true);
        let failed = || BuildStep {
            error: Some("exit code: 2".to_owned()),
            ..running()
        };
        let long_name = format!("[2/2] RUN {}", "é".repeat(80));
        let long_line = format!("[2/2] RUN {}{TRUNCATED} | x", "é".repeat(50));
        let cases: Vec<(&str, Vec<Report>, Vec<&str>)> = vec![
            (
                "a log in several chunks",
                vec![(
                    vec![running()],
                    vec![log(RUN, 1, b"hello\n"), log(RUN, 1, b"world\n")],
                )],
                vec![
                    "[2/2] RUN make",
                    "[2/2] RUN make | hello",
                    "[2/2] RUN make | world",
                ],
            ),
            (
                "a line split across reports",
                vec![
                    (vec![running()], vec![log(RUN, 1, b"hel")]),
                    (vec![], vec![log(RUN, 1, b"lo\nwor")]),
                    (vec![], vec![log(RUN, 1, b"ld\n")]),
                ],
                vec![
                    "[2/2] RUN make",
                    "[2/2] RUN make | hello",
                    "[2/2] RUN make | world",
                ],
            ),
            (
                "crlf",
                vec![(vec![running()], vec![log(RUN, 1, b"a\r\nb\r\n")])],
                vec!["[2/2] RUN make", "[2/2] RUN make | a", "[2/2] RUN make | b"],
            ),
            (
                "a progress bar redrawn with cr",
                vec![(vec![running()], vec![log(RUN, 2, b"10%\r50%\r100%\n")])],
                vec!["[2/2] RUN make", "[2/2] RUN make | 100%"],
            ),
            (
                "stdout and stderr kept apart",
                vec![(
                    vec![running()],
                    vec![
                        log(RUN, 1, b"out"),
                        log(RUN, 2, b"err\n"),
                        log(RUN, 1, b"put\n"),
                    ],
                )],
                vec![
                    "[2/2] RUN make",
                    "[2/2] RUN make | err",
                    "[2/2] RUN make | output",
                ],
            ),
            (
                "invalid utf-8",
                vec![(vec![running()], vec![log(RUN, 1, b"a\xffb\n")])],
                vec!["[2/2] RUN make", "[2/2] RUN make | a\u{fffd}b"],
            ),
            (
                "an unknown step is tagged with its digest",
                vec![(vec![], vec![log("sha256:0123456789abcdef0123", 1, b"hi\n")])],
                vec!["0123456789ab | hi"],
            ),
            (
                "a digest without an algorithm",
                vec![(vec![], vec![log("abc", 1, b"hi\n")])],
                vec!["abc | hi"],
            ),
            (
                "an open line is written when the step fails",
                vec![
                    (
                        vec![running()],
                        vec![log(RUN, 2, b"make: *** [all] Error 2")],
                    ),
                    (vec![failed()], vec![]),
                ],
                vec![
                    "[2/2] RUN make",
                    "[2/2] RUN make | make: *** [all] Error 2",
                    "[2/2] RUN make: error: exit code: 2",
                ],
            ),
            (
                "logs come before the end of the step that wrote them",
                vec![(vec![finished()], vec![log(RUN, 1, b"one\nlast")])],
                vec![
                    "[2/2] RUN make | one",
                    "[2/2] RUN make | last",
                    "[2/2] RUN make: done",
                ],
            ),
            (
                "a log after its step finished",
                vec![
                    (vec![finished()], vec![]),
                    (vec![], vec![log(RUN, 1, b"late")]),
                ],
                vec!["[2/2] RUN make: done", "[2/2] RUN make | late"],
            ),
            (
                "an open line of a step still running stays open",
                vec![(vec![running()], vec![log(RUN, 1, b"partial")])],
                vec!["[2/2] RUN make"],
            ),
            (
                "a long name is shortened in the tag",
                vec![(
                    vec![vertex(RUN, &long_name, false, false)],
                    vec![log(RUN, 1, b"x\n")],
                )],
                vec![&long_line],
            ),
        ];
        for (name, reports, want) in cases {
            let got = build_lines(reports);
            assert!(got.iter().all(|(step, _)| *step == 0), "{name}");
            let got: Vec<&str> = got.iter().map(|(_, l)| l.as_str()).collect();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn caps_build_log_lines() {
        let name = "😀".repeat(200);
        let long = vec![b'x'; MAX_OUTPUT_LINE * 2];
        let cases: [(&str, Vec<BuildLog>, bool); 2] = [
            (
                "a long line",
                vec![log("d", 1, &long), log("d", 1, b"\nnext\n")],
                false,
            ),
            (
                "a long open line, many chunks",
                long.chunks(1000).map(|chunk| log("d", 1, chunk)).collect(),
                true,
            ),
        ];
        for (case, logs, finish) in cases {
            let mut reports = vec![(vec![vertex("d", &name, true, false)], logs)];
            if finish {
                reports.push((vec![vertex("d", &name, true, true)], vec![]));
            }
            let lines = build_lines(reports);
            let cut: Vec<&String> = lines
                .iter()
                .map(|(_, l)| l)
                .filter(|l| l.contains("xxx"))
                .collect();
            assert_eq!(cut.len(), 1, "{case}");
            assert!(cut[0].len() <= MAX_OUTPUT_LINE, "{case}: {}", cut[0].len());
            assert!(cut[0].ends_with(&format!("xx{TRUNCATED}")), "{case}");
        }
    }

    #[test]
    fn names_what_each_command_counts() {
        let cases = [
            (StepCommand::Up, "1 of 1 containers started"),
            (StepCommand::Start, "1 of 1 containers started"),
            (StepCommand::Restart, "1 of 1 containers restarted"),
            (StepCommand::Stop, "1 of 1 containers stopped"),
            (StepCommand::Down, "1 of 1 containers removed"),
        ];
        for (command, want) in cases {
            let mut t = tracker(Action::Up, &[command]);
            t.apply(&resource(0, "Container x", ResourceState::Done, "Done"));
            assert_eq!(
                t.operation().steps[0].message.as_deref(),
                Some(want),
                "{command}"
            );
        }
    }

    #[test]
    fn finishing_settles_every_step() {
        use StepState::{Active, Done, Failed, Skipped};
        type Case = (
            Outcome,
            usize,
            OperationState,
            Option<&'static str>,
            [(StepState, Option<&'static str>); 3],
        );
        let three = [StepCommand::Pull, StepCommand::Build, StepCommand::Up];
        let cases: [Case; 6] = [
            (
                Outcome::Succeeded,
                2,
                OperationState::Succeeded,
                None,
                [(Done, None), (Done, None), (Done, None)],
            ),
            (
                Outcome::Failed("pull access denied".to_owned()),
                0,
                OperationState::Failed,
                Some("pull access denied"),
                [
                    (Failed, Some("pull access denied")),
                    (Skipped, None),
                    (Skipped, None),
                ],
            ),
            (
                Outcome::Failed("build failed".to_owned()),
                1,
                OperationState::Failed,
                Some("build failed"),
                [
                    (Done, None),
                    (Failed, Some("build failed")),
                    (Skipped, None),
                ],
            ),
            (
                Outcome::Cancelled,
                1,
                OperationState::Cancelled,
                Some("Cancelled before it finished"),
                [(Done, None), (Failed, Some("Cancelled")), (Skipped, None)],
            ),
            (
                Outcome::Interrupted,
                2,
                OperationState::Interrupted,
                Some("alchemyd stopped before it finished"),
                [
                    (Done, None),
                    (Done, None),
                    (Failed, Some("Stopped when alchemyd shut down")),
                ],
            ),
            (
                Outcome::Failed("docker is not installed".to_owned()),
                usize::MAX,
                OperationState::Failed,
                Some("docker is not installed"),
                [
                    (Failed, Some("docker is not installed")),
                    (Skipped, None),
                    (Skipped, None),
                ],
            ),
        ];
        for (outcome, running, state, error, want) in cases {
            let mut t = tracker(Action::Update, &three);
            for step in 0..three.len() {
                if step <= running && running != usize::MAX {
                    t.apply(&Progress::StepStarted { step });
                }
            }
            if running != usize::MAX {
                assert_eq!(t.operation().steps[running].state, Active, "{outcome:?}");
            }
            t.finish(&outcome, 160);
            let op = t.operation();
            assert_eq!(op.state, state, "{outcome:?}");
            assert_eq!(op.finished_at, Some(160), "{outcome:?}");
            assert_eq!(op.error.as_deref(), error, "{outcome:?}");
            assert_eq!(states(&t), want, "{outcome:?}");

            let before = op.clone();
            t.finish(&Outcome::Succeeded, 999);
            assert_eq!(
                t.apply(&Progress::StepStarted { step: 0 }),
                Applied::default()
            );
            assert_eq!(t.operation(), &before, "{outcome:?}");
        }
    }

    #[test]
    fn resumes_a_stored_operation_to_interrupt_it() {
        let mut t = tracker(Action::Stop, &[StepCommand::Stop]);
        t.apply(&Progress::StepStarted { step: 0 });
        let mut resumed = Tracker::resume(t.operation().clone());
        resumed.finish(&Outcome::Interrupted, 200);
        assert_eq!(resumed.operation().state, OperationState::Interrupted);
        assert_eq!(resumed.operation().steps[0].state, StepState::Failed);
    }
}
