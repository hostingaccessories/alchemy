//! `alchemy app up|down|start|stop|restart|pull|build|update NAME`: run a
//! lifecycle action on an app and follow it to the end.
//!
//! alchemyd runs the action as an operation. The command starts it, then
//! follows `/api/operations/{id}/events`: each step as it starts or fails
//! goes to stderr, and the result to stdout. With `--json`, every event is
//! one line on stdout instead. Ctrl-C stops following; the operation keeps
//! running in alchemyd until `alchemy app cancel NAME` stops it.

use std::io::Write;

use alchemy_api::apps::Saved;
use alchemy_api::operations::{
    Action, ActionPlan, ActionRequest, Operation, OperationEvent, OperationResponse,
    OperationState, OutputLine, StepState,
};

use super::path_segment;
use crate::context::Context;
use crate::daemon::{Daemon, EventStream as _};
use crate::error::{ClientError, ProtocolError};
use crate::prompt::Prompt;
use crate::safety::{self, Mode, Safety};
use crate::sse::SseEvent;

/// Arguments to every lifecycle action.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The app's name: its folder's name.
    pub name: String,

    /// The shared safety flags. `--force` acts on an app that shares its
    /// Compose project with another app, and starts one whose host ports
    /// something else holds.
    #[command(flatten)]
    pub safety: Safety,
}

/// The command as typed after `alchemy`, for errors.
pub(crate) fn name(action: Action) -> &'static str {
    match action {
        Action::Up => "app up",
        Action::Down => "app down",
        Action::Start => "app start",
        Action::Stop => "app stop",
        Action::Restart => "app restart",
        Action::Pull => "app pull",
        Action::Build => "app build",
        Action::Update => "app update",
    }
}

/// What `alchemy app down` asks before it goes ahead.
pub(crate) fn down_question(app: &str) -> String {
    format!(
        "Take {app} down? Its containers and networks are removed; its volumes and files are kept."
    )
}

/// Run `action` on the app `args.name`: confirm `down`, then show the plan
/// with `--dry-run`, or start the operation and follow it to the end.
pub(crate) async fn run<D: Daemon, W: Write, P: Prompt>(
    ctx: &mut Context<D, W>,
    prompt: &mut P,
    action: Action,
    args: Args,
) -> Result<(), ClientError> {
    let mode = if action == Action::Down {
        safety::confirm(
            prompt,
            &args.safety,
            name(action),
            &down_question(&args.name),
        )
        .await?
    } else if args.safety.dry_run {
        Mode::DryRun
    } else {
        Mode::Execute
    };
    let app = path_segment(&args.name);
    if mode == Mode::DryRun {
        let path = format!("/api/apps/{app}/plan?action={action}");
        let plan = ctx.get::<ActionPlan>(&path).await?;
        for line in plan_lines(&plan, ctx.json(), args.safety.force) {
            ctx.out().line(line)?;
        }
        return Ok(());
    }

    let request = ActionRequest {
        action,
        force: args.safety.force,
    };
    let path = format!("/api/apps/{app}/actions");
    let OperationResponse { operation } = ctx.post(&path, &request).await?;
    follow(ctx, &operation).await
}

/// Follow `operation`, just started, to the end: its steps as progress
/// and the result on stdout, or with `--json` every event as one line on
/// stdout. Fails unless it succeeds.
pub(crate) async fn follow<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    operation: &Operation,
) -> Result<(), ClientError> {
    let json = ctx.json();
    if !json {
        ctx.out().progress(started_line(operation));
    }
    let mut events = ctx
        .stream(&format!("/api/operations/{}/events", operation.id))
        .await?;
    let mut seen = Vec::new();
    while let Some(raw) = events.next().await? {
        let Some(event) = decode(&raw)? else {
            continue;
        };
        if json {
            let line = serde_json::to_string(&event).map_err(std::io::Error::other)?;
            ctx.out().line(line)?;
        } else if let OperationEvent::Operation(op) | OperationEvent::Done(op) = &event {
            for line in step_lines(&mut seen, op) {
                ctx.out().progress(line);
            }
        }
        if let OperationEvent::Done(op) = event {
            return finish(ctx, &op);
        }
    }
    Err(ClientError::Disconnected)
}

/// After a save sent with `start` when `asked`: follow the operation
/// alchemyd started, or fail with why it didn't start the app. The save
/// itself stands either way.
pub(crate) async fn follow_start<D: Daemon, W: Write, T>(
    ctx: &mut Context<D, W>,
    saved: &Saved<T>,
    app: &str,
    asked: bool,
) -> Result<(), ClientError> {
    match (&saved.operation, &saved.start_error) {
        (_, Some(refusal)) => Err(ClientError::Daemon(refusal.error.clone())),
        (Some(operation), None) => follow(ctx, operation).await,
        (None, None) if asked => Err(ClientError::Daemon(format!(
            "saved {app}, but this alchemyd doesn't start apps on save; run `alchemy app up {app}`"
        ))),
        (None, None) => Ok(()),
    }
}

fn finish<D: Daemon, W: Write>(ctx: &mut Context<D, W>, op: &Operation) -> Result<(), ClientError> {
    if op.state == OperationState::Succeeded {
        if !ctx.json() {
            ctx.out().line(done_line(op))?;
        }
        return Ok(());
    }
    Err(ClientError::OperationFailed {
        message: failed_message(op),
    })
}

/// An event from the operation stream, or `None` for a name this build
/// does not know.
fn decode(raw: &SseEvent) -> Result<Option<OperationEvent>, ClientError> {
    let data = raw.data.as_bytes();
    let decoded = match raw.event.as_str() {
        "operation" => serde_json::from_slice(data).map(OperationEvent::Operation),
        "done" => serde_json::from_slice(data).map(OperationEvent::Done),
        "output" => serde_json::from_slice::<OutputLine>(data).map(OperationEvent::Output),
        _ => return Ok(None),
    };
    decoded
        .map(Some)
        .map_err(|e| ProtocolError::Decode(e).into())
}

/// The first progress line: what started, that Ctrl-C leaves it running,
/// and how to stop it.
pub(crate) fn started_line(op: &Operation) -> String {
    format!(
        "{} {app} (operation {}). Ctrl-C stops watching; alchemyd keeps going until `alchemy app cancel {app}`.",
        doing(op.action),
        op.id,
        app = op.app,
    )
}

fn doing(action: Action) -> &'static str {
    match action {
        Action::Up => "Bringing up",
        Action::Down => "Taking down",
        Action::Start => "Starting",
        Action::Stop => "Stopping",
        Action::Restart => "Restarting",
        Action::Pull => "Downloading",
        Action::Build => "Building",
        Action::Update => "Updating",
    }
}

/// The progress to print for `op`, given the step states `seen` printed
/// so far, which it updates: a step's label as it starts, and the problem
/// when it fails.
pub(crate) fn step_lines(seen: &mut Vec<StepState>, op: &Operation) -> Vec<String> {
    seen.resize(op.steps.len(), StepState::Pending);
    let mut lines = Vec::new();
    for (step, was) in op.steps.iter().zip(seen.iter_mut()) {
        if step.state == *was {
            continue;
        }
        match step.state {
            StepState::Active => lines.push(format!("{}…", step.label)),
            StepState::Failed => lines.push(match &step.message {
                Some(message) => format!("{} failed: {message}", step.label),
                None => format!("{} failed", step.label),
            }),
            _ => {}
        }
        *was = step.state;
    }
    lines
}

/// What a successful action prints on stdout.
pub(crate) fn done_line(op: &Operation) -> String {
    let app = &op.app;
    match op.action {
        Action::Up => format!("{app} is up"),
        Action::Down => format!("{app} is down"),
        Action::Start => format!("started {app}"),
        Action::Stop => format!("stopped {app}"),
        Action::Restart => format!("restarted {app}"),
        Action::Pull => format!("downloaded {app}'s images"),
        Action::Build => format!("built {app}"),
        Action::Update => format!("updated {app}"),
    }
}

/// The error for an operation that did not succeed.
pub(crate) fn failed_message(op: &Operation) -> String {
    let what = match op.state {
        OperationState::Cancelled => "was cancelled",
        OperationState::Interrupted => "was interrupted",
        _ => "failed",
    };
    match &op.error {
        Some(error) => format!("{} {} {what}: {error}", op.action, op.app),
        None => format!("{} {} {what}", op.action, op.app),
    }
}

/// What `--dry-run` prints: each command line alchemyd would run, then
/// each port that is taken and, unless `force`, that alchemyd would refuse
/// because of them. With `json`, `{"dry_run":true,"plan":<ActionPlan>}`.
pub(crate) fn plan_lines(plan: &ActionPlan, json: bool, force: bool) -> Vec<String> {
    if json {
        return vec![serde_json::json!({ "dry_run": true, "plan": plan }).to_string()];
    }
    let mut lines: Vec<String> = plan
        .steps
        .iter()
        .map(|step| {
            let argv: Vec<String> = step.argv.iter().map(|arg| shell_quote(arg)).collect();
            format!("would run: {}", argv.join(" "))
        })
        .collect();
    lines.extend(
        plan.conflicts
            .iter()
            .map(|conflict| format!("port taken: {}", conflict.message)),
    );
    if !force && !plan.conflicts.is_empty() {
        lines.push(format!(
            "would refuse to {} {} while its ports are taken; --force goes ahead anyway",
            plan.action, plan.app
        ));
    }
    lines
}

/// `arg` as a POSIX shell would need it to be typed.
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
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
    use alchemy_api::operations::{PlannedStep, Step, StepCommand};
    use alchemy_api::ports::{PortClaim, PortConflict, PortHolder, PortSource};
    use alchemy_api::{Protocol, PublishedPort};
    use alchemy_core::StackName;

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::{FakeDaemon, sse};
    use crate::prompt::FakePrompt;

    fn step(command: StepCommand, label: &str, state: StepState, message: Option<&str>) -> Step {
        Step {
            command,
            label: label.into(),
            state,
            message: message.map(Into::into),
        }
    }

    fn op(action: Action, state: OperationState, steps: Vec<Step>) -> Operation {
        Operation {
            id: "op1".into(),
            app: StackName::new("blog").unwrap(),
            action,
            state,
            steps,
            started_at: 0,
            finished_at: None,
            error: None,
            reason: None,
        }
    }

    fn update(states: [StepState; 2], message: Option<&str>) -> Operation {
        op(
            Action::Update,
            OperationState::Running,
            vec![
                step(StepCommand::Pull, "Downloading updates", states[0], None),
                step(
                    StepCommand::Up,
                    "Starting the new version",
                    states[1],
                    message,
                ),
            ],
        )
    }

    fn event(event: &OperationEvent) -> SseEvent {
        sse(event.name(), &serde_json::to_string(&event.data()).unwrap())
    }

    #[test]
    fn prints_each_step_as_it_starts_or_fails() {
        use StepState::{Active, Done, Failed, Pending, Skipped};
        let cases = [
            (update([Pending, Pending], None), vec![]),
            (
                update([Active, Pending], None),
                vec!["Downloading updates…"],
            ),
            (update([Active, Pending], None), vec![]),
            (
                update([Done, Active], None),
                vec!["Starting the new version…"],
            ),
            (
                update([Done, Failed], Some("port is already allocated")),
                vec!["Starting the new version failed: port is already allocated"],
            ),
        ];
        let mut seen = Vec::new();
        for (op, want) in cases {
            assert_eq!(step_lines(&mut seen, &op), want);
        }
        let mut seen = Vec::new();
        assert_eq!(
            step_lines(&mut seen, &update([Failed, Skipped], None)),
            ["Downloading updates failed"]
        );
    }

    #[test]
    fn words_the_result() {
        let cases = [
            (Action::Up, "blog is up"),
            (Action::Down, "blog is down"),
            (Action::Start, "started blog"),
            (Action::Stop, "stopped blog"),
            (Action::Restart, "restarted blog"),
            (Action::Pull, "downloaded blog's images"),
            (Action::Build, "built blog"),
            (Action::Update, "updated blog"),
        ];
        for (action, want) in cases {
            assert_eq!(
                done_line(&op(action, OperationState::Succeeded, vec![])),
                want
            );
            assert!(name(action).starts_with("app "), "{action}");
            assert!(name(action).ends_with(action.as_str()), "{action}");
        }
        let failed = |state, error: Option<&str>| Operation {
            error: error.map(Into::into),
            ..op(Action::Update, state, vec![])
        };
        let cases = [
            (
                failed(OperationState::Failed, Some("pull access denied")),
                "update blog failed: pull access denied",
            ),
            (failed(OperationState::Failed, None), "update blog failed"),
            (
                failed(
                    OperationState::Cancelled,
                    Some("Cancelled before it finished"),
                ),
                "update blog was cancelled: Cancelled before it finished",
            ),
            (
                failed(OperationState::Interrupted, None),
                "update blog was interrupted",
            ),
        ];
        for (op, want) in cases {
            assert_eq!(failed_message(&op), want);
        }
    }

    fn plan() -> ActionPlan {
        ActionPlan {
            app: StackName::new("blog").unwrap(),
            action: Action::Down,
            steps: vec![PlannedStep {
                command: StepCommand::Down,
                label: "Removing the app's containers".into(),
                argv: [
                    "docker",
                    "compose",
                    "--project-directory",
                    "/srv/my apps/it's",
                    "--progress",
                    "json",
                    "down",
                ]
                .map(String::from)
                .to_vec(),
            }],
            conflicts: Vec::new(),
        }
    }

    fn conflicted() -> ActionPlan {
        let tcp = PublishedPort::new(Protocol::Tcp, Some("8080".parse().unwrap()));
        ActionPlan {
            action: Action::Up,
            conflicts: vec![PortConflict {
                service: "app".into(),
                wanted: tcp,
                claim: PortClaim {
                    port: tcp,
                    holder: PortHolder::App {
                        app: "wiki".into(),
                        service: "web".into(),
                    },
                    source: PortSource::Live,
                },
                message: "Port 8080/tcp for service app is in use by service web of app wiki."
                    .into(),
            }],
            ..plan()
        }
    }

    #[test]
    fn plans_print_taken_ports_and_whether_they_refuse() {
        let taken =
            "port taken: Port 8080/tcp for service app is in use by service web of app wiki.";
        let cases: [(bool, &[&str]); 2] = [
            (
                false,
                &[
                    taken,
                    "would refuse to up blog while its ports are taken; --force goes ahead anyway",
                ],
            ),
            (true, &[taken]),
        ];
        for (force, want) in cases {
            let lines = plan_lines(&conflicted(), false, force);
            assert!(lines[0].starts_with("would run: "), "{lines:?}");
            assert_eq!(&lines[1..], want, "force: {force}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&plan_lines(&conflicted(), true, false)[0]).unwrap();
        assert_eq!(json["plan"]["conflicts"][0]["service"], "app");
    }

    #[test]
    fn plans_print_as_shell_command_lines() {
        assert_eq!(
            plan_lines(&plan(), false, false),
            [
                r"would run: docker compose --project-directory '/srv/my apps/it'\''s' --progress json down"
            ]
        );
        let json: serde_json::Value =
            serde_json::from_str(&plan_lines(&plan(), true, false)[0]).unwrap();
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["plan"]["action"], "down");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a=b"), "a=b");
    }

    fn args(name: &str, dry_run: bool, yes: bool) -> Args {
        Args {
            name: name.into(),
            safety: Safety {
                dry_run,
                force: false,
                yes,
            },
        }
    }

    fn started(action: Action) -> FakeDaemon {
        let running = update([StepState::Pending, StepState::Pending], None);
        let running = Operation { action, ..running };
        FakeDaemon::new().version("1.2.3", 1).ok(
            Method::POST,
            "/api/apps/blog/actions",
            &OperationResponse { operation: running },
        )
    }

    fn finished(state: OperationState, error: Option<&str>) -> Operation {
        Operation {
            error: error.map(Into::into),
            ..op(
                Action::Update,
                state,
                vec![
                    step(
                        StepCommand::Pull,
                        "Downloading updates",
                        StepState::Done,
                        None,
                    ),
                    step(
                        StepCommand::Up,
                        "Starting the new version",
                        if state == OperationState::Succeeded {
                            StepState::Done
                        } else {
                            StepState::Failed
                        },
                        error,
                    ),
                ],
            )
        }
    }

    const EVENTS: &str = "/api/operations/op1/events";

    #[tokio::test]
    async fn follows_an_action_to_success() {
        let fake = started(Action::Update).stream(
            EVENTS,
            [
                Ok(event(&OperationEvent::Operation(update(
                    [StepState::Active, StepState::Pending],
                    None,
                )))),
                Ok(event(&OperationEvent::Output(OutputLine {
                    seq: 0,
                    step: 0,
                    text: "Image nginx Pulled".into(),
                }))),
                Ok(sse("from_the_future", "{}")),
                Ok(event(&OperationEvent::Done(finished(
                    OperationState::Succeeded,
                    None,
                )))),
            ],
        );
        let mut ctx = Context::buffered(fake, false, name(Action::Update));
        run(
            &mut ctx,
            &mut FakePrompt::no_tty(),
            Action::Update,
            args("blog", false, false),
        )
        .await
        .unwrap();
        assert_eq!(ctx.written(), "updated blog\n");
        assert_eq!(
            ctx.progress_written(),
            "Updating blog (operation op1). Ctrl-C stops watching; alchemyd keeps going until `alchemy app cancel blog`.\n\
             Downloading updates…\n"
        );
        assert_eq!(
            ctx.daemon().requests(),
            [
                "GET /api/version",
                "POST /api/apps/blog/actions",
                "GET /api/operations/op1/events"
            ]
        );
        assert_eq!(
            ctx.daemon().bodies(),
            [r#"{"action":"update","force":false}"#]
        );
    }

    #[tokio::test]
    async fn prints_every_event_as_json_and_fails_with_the_reason() {
        let done =
            OperationEvent::Done(finished(OperationState::Failed, Some("pull access denied")));
        let fake = started(Action::Update).stream(EVENTS, [Ok(event(&done))]);
        let mut ctx = Context::buffered(fake, true, name(Action::Update));
        let err = run(
            &mut ctx,
            &mut FakePrompt::no_tty(),
            Action::Update,
            args("blog", false, false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.kind(), "operation_failed");
        assert_eq!(err.to_string(), "update blog failed: pull access denied");
        assert!(
            ctx.written().starts_with(r#"{"event":"done","data":{"#),
            "{}",
            ctx.written()
        );
        let line: serde_json::Value = serde_json::from_str(ctx.written().trim()).unwrap();
        assert_eq!(line["event"], "done");
        assert_eq!(line["data"]["state"], "failed");
        assert_eq!(ctx.progress_written(), "");
    }

    #[tokio::test]
    async fn a_stream_that_ends_early_is_a_disconnect() {
        let fake = started(Action::Stop).stream(EVENTS, []);
        let mut ctx = Context::buffered(fake, false, name(Action::Stop));
        let err = run(
            &mut ctx,
            &mut FakePrompt::no_tty(),
            Action::Stop,
            args("blog", false, false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 69);
    }

    #[tokio::test]
    async fn refusals_come_back_as_daemon_errors() {
        let fake = FakeDaemon::new().version("1.2.3", 1).error(
            Method::POST,
            "/api/apps/blog/actions",
            409,
            "conflict",
            "blog is busy with another operation (x); wait for it to finish",
        );
        let mut ctx = Context::buffered(fake, false, name(Action::Start));
        let err = run(
            &mut ctx,
            &mut FakePrompt::no_tty(),
            Action::Start,
            args("blog", false, false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("busy"), "{err}");
    }

    #[tokio::test]
    async fn down_asks_first_and_dry_runs_change_nothing() {
        let plan_path = "/api/apps/blog/plan?action=down";
        let cases = [
            (
                FakeDaemon::new(),
                FakePrompt::tty([false]),
                args("blog", false, false),
                Err(3),
                vec![],
            ),
            (
                FakeDaemon::new(),
                FakePrompt::no_tty(),
                args("blog", false, false),
                Err(3),
                vec![],
            ),
            (
                FakeDaemon::new()
                    .version("1.2.3", 1)
                    .ok(Method::GET, plan_path, &plan()),
                FakePrompt::no_tty(),
                args("blog", true, false),
                Ok(()),
                vec!["GET /api/version", "GET /api/apps/blog/plan?action=down"],
            ),
            (
                started(Action::Down).stream(
                    EVENTS,
                    [Ok(event(&OperationEvent::Done(Operation {
                        action: Action::Down,
                        ..finished(OperationState::Succeeded, None)
                    })))],
                ),
                FakePrompt::tty([true]),
                args("blog", false, false),
                Ok(()),
                vec![
                    "GET /api/version",
                    "POST /api/apps/blog/actions",
                    "GET /api/operations/op1/events",
                ],
            ),
            (
                started(Action::Down).stream(
                    EVENTS,
                    [Ok(event(&OperationEvent::Done(Operation {
                        action: Action::Down,
                        ..finished(OperationState::Succeeded, None)
                    })))],
                ),
                FakePrompt::no_tty(),
                args("blog", false, true),
                Ok(()),
                vec![
                    "GET /api/version",
                    "POST /api/apps/blog/actions",
                    "GET /api/operations/op1/events",
                ],
            ),
        ];
        for (fake, mut prompt, args, want, requests) in cases {
            let mut ctx = Context::buffered(fake, false, name(Action::Down));
            let got = run(&mut ctx, &mut prompt, Action::Down, args.clone())
                .await
                .map_err(|e| e.exit_code());
            assert_eq!(got, want, "{args:?}");
            assert_eq!(ctx.daemon().requests(), requests, "{args:?}");
        }
        let mut prompt = FakePrompt::tty([false]);
        let mut ctx = Context::buffered(FakeDaemon::new(), false, name(Action::Down));
        let _ = run(
            &mut ctx,
            &mut prompt,
            Action::Down,
            args("blog", false, false),
        )
        .await;
        assert_eq!(prompt.asked(), [down_question("blog")]);
    }

    #[tokio::test]
    async fn other_actions_dry_run_without_asking() {
        let fake = FakeDaemon::new().version("1.2.3", 1).ok(
            Method::GET,
            "/api/apps/blog/plan?action=stop",
            &ActionPlan {
                action: Action::Stop,
                ..plan()
            },
        );
        let mut prompt = FakePrompt::tty([]);
        let mut ctx = Context::buffered(fake, false, name(Action::Stop));
        run(
            &mut ctx,
            &mut prompt,
            Action::Stop,
            args("blog", true, false),
        )
        .await
        .unwrap();
        assert!(
            ctx.written().starts_with("would run: docker compose"),
            "{}",
            ctx.written()
        );
        assert_eq!(prompt.asked(), [] as [String; 0]);
    }
}
