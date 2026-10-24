//! Compose runner wiring: the real `CliComposeRunner` against stub
//! `docker` scripts.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alchemyd::compose::{
    CancelToken, CliComposeRunner, ComposeAction, ComposeCommand, ComposeError, ComposeEvent,
    ComposePlan, ComposeProject, ComposeRunner, ImageSource, OutputLine, ProgressEvent,
    ResourceStatus, Stream, cancellation,
};
use alchemyd::engine::Endpoint;
use tokio::sync::mpsc;

const STARTED: &str = r#"{"id":"Container blog-web-1","status":"Done","text":"Started"}"#;

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Every stub, written up front: exec'ing a file another thread still holds
/// open for writing fails with `ETXTBSY` on Linux, so the checks share one
/// test and nothing is written once children start.
struct Stubs {
    dir: tempfile::TempDir,
    argv_log: PathBuf,
    env_log: PathBuf,
    fails_pull_log: PathBuf,
    go: PathBuf,
    docker: PathBuf,
    env_dump: PathBuf,
    failing: PathBuf,
    fails_pull: PathBuf,
    waits: PathBuf,
    interruptible: PathBuf,
    stubborn: PathBuf,
    slow: PathBuf,
}

impl Stubs {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let argv_log = dir.path().join("argv.log");
        let env_log = dir.path().join("env.log");
        let fails_pull_log = dir.path().join("fails-pull.log");
        let go = dir.path().join("go");
        let docker = write_script(
            dir.path(),
            "docker",
            &format!(
                r#"{{ printf '[%s]' "$@"; echo; }} >> '{}'
read -r line && echo "stdin was open" >&2 && exit 9
echo '{STARTED}' >&2
echo "done $6"
exit 0"#,
                argv_log.display()
            ),
        );
        let env_dump = write_script(
            dir.path(),
            "env-dump",
            &format!("env > '{}'", env_log.display()),
        );
        let failing = write_script(
            dir.path(),
            "failing",
            "echo 'pulling...' >&2\necho 'no such service: web' >&2\nexit 14",
        );
        let fails_pull = write_script(
            dir.path(),
            "fails-pull",
            &format!(
                r#"{{ printf '[%s]' "$@"; echo; }} >> '{}'
[ "$6" = pull ] || exit 0
echo '{{"id":"Image nope:1","status":"Error","text":"Error","details":"denied"}}' >&2
echo '{{"error":true,"message":"pull access denied for nope"}}' >&2
exit 1"#,
                fails_pull_log.display()
            ),
        );
        let waits = write_script(
            dir.path(),
            "waits",
            &format!(
                "echo first >&2\nwhile [ ! -e '{}' ]; do sleep 0.05; done\necho second\nexit 0",
                go.display()
            ),
        );
        let interruptible = write_script(
            dir.path(),
            "interruptible",
            "trap 'kill $pid; echo interrupted >&2; exit 130' INT
sleep 30 >/dev/null 2>&1 &
pid=$!
echo ready >&2
wait",
        );
        let stubborn = write_script(
            dir.path(),
            "stubborn",
            "trap 'echo ignoring >&2' INT\necho ready >&2\nwhile :; do sleep 1; done",
        );
        let slow = write_script(dir.path(), "slow", "exec sleep 10");
        Self {
            dir,
            argv_log,
            env_log,
            fails_pull_log,
            go,
            docker,
            env_dump,
            failing,
            fails_pull,
            waits,
            interruptible,
            stubborn,
            slow,
        }
    }

    fn project(&self) -> ComposeProject {
        ComposeProject::new(self.dir.path().join("my stacks/blog")).unwrap()
    }

    fn plan(&self, action: ComposeAction) -> ComposePlan {
        ComposePlan::new(&self.project(), action, ImageSource::Registry)
    }
}

fn runner(binary: &Path) -> CliComposeRunner {
    CliComposeRunner::new(
        binary,
        Endpoint::unix("/run/user/1000/docker.sock").unwrap(),
    )
    .unwrap()
}

/// Run `plan` to the end, collecting every event.
async fn run_all(
    runner: &CliComposeRunner,
    plan: &ComposePlan,
) -> (Vec<ComposeEvent>, Result<(), ComposeError>) {
    let (tx, mut rx) = mpsc::channel(16);
    let (result, events) = tokio::join!(runner.run(plan, tx, CancelToken::never()), async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    (events, result)
}

async fn next_line(rx: &mut mpsc::Receiver<ComposeEvent>) -> OutputLine {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(ComposeEvent::Output(line))) => return line,
            Ok(Some(_)) => {}
            other => panic!("expected an output line, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn cli_compose_runner_runs_stub_docker() {
    let stubs = Stubs::new();
    runs_every_action_with_fixed_argv(&stubs).await;
    stops_a_plan_at_the_first_failing_step(&stubs).await;
    reports_failures(&stubs).await;
    passes_only_the_allowlisted_environment(&stubs).await;
    streams_lines_before_the_child_exits(&stubs).await;
    cancels_with_sigint(&stubs).await;
    kills_after_the_grace_period(&stubs).await;
    times_out(&stubs).await;
}

async fn runs_every_action_with_fixed_argv(stubs: &Stubs) {
    let project = stubs.project();
    let dir = project.dir().display().to_string();
    let runner = runner(&stubs.docker);
    let cases: [(ComposeAction, ImageSource, &[&str]); 9] = [
        (
            ComposeAction::Up,
            ImageSource::Registry,
            &["[up][--detach]"],
        ),
        (ComposeAction::Down, ImageSource::Registry, &["[down]"]),
        (ComposeAction::Start, ImageSource::Registry, &["[start]"]),
        (ComposeAction::Stop, ImageSource::Registry, &["[stop]"]),
        (
            ComposeAction::Restart,
            ImageSource::Registry,
            &["[restart]"],
        ),
        (ComposeAction::Pull, ImageSource::Registry, &["[pull]"]),
        (ComposeAction::Build, ImageSource::Build, &["[build]"]),
        (
            ComposeAction::Update,
            ImageSource::Registry,
            &["[pull]", "[up][--detach]"],
        ),
        (
            ComposeAction::Update,
            ImageSource::Build,
            &[
                "[pull][--ignore-buildable]",
                "[build][--pull]",
                "[up][--detach]",
            ],
        ),
    ];
    let mut want_log = String::new();
    for (action, images, steps) in cases {
        let plan = ComposePlan::new(&project, action, images);
        let (events, result) = run_all(&runner, &plan).await;
        result.unwrap();
        for step in steps {
            writeln!(
                want_log,
                "[compose][--project-directory][{dir}][--progress][json]{step}"
            )
            .unwrap();
        }
        assert_eq!(events.len(), steps.len() * 4, "{action} {events:?}");
        for (index, step) in plan.steps().iter().enumerate() {
            check_step_events(&events[index * 4..index * 4 + 4], index, step.command());
        }
    }
    assert_eq!(std::fs::read_to_string(&stubs.argv_log).unwrap(), want_log);
}

/// A step of the `docker` stub: started, one line on each stream in either
/// order, finished.
fn check_step_events(events: &[ComposeEvent], step: usize, command: ComposeCommand) {
    assert_eq!(events[0], ComposeEvent::StepStarted { step, command });
    assert_eq!(events[3], ComposeEvent::StepFinished { step, command });
    let mut lines: Vec<&OutputLine> = events[1..3]
        .iter()
        .map(|event| match event {
            ComposeEvent::Output(line) => line,
            other => panic!("expected output, got {other:?}"),
        })
        .collect();
    lines.sort_by_key(|line| line.stream == Stream::Stderr);
    assert_eq!(
        *lines[0],
        OutputLine::new(step, Stream::Stdout, format!("done {command}"))
    );
    assert_eq!(*lines[1], OutputLine::new(step, Stream::Stderr, STARTED));
    assert!(matches!(
        &lines[1].progress,
        Some(ProgressEvent::Resource(r)) if r.status == ResourceStatus::Done
    ));
}

async fn stops_a_plan_at_the_first_failing_step(stubs: &Stubs) {
    let plan = stubs.plan(ComposeAction::Update);
    let (events, result) = run_all(&runner(&stubs.fails_pull), &plan).await;
    match result {
        Err(ComposeError::Failed {
            action: ComposeAction::Update,
            command: ComposeCommand::Pull,
            code: Some(1),
            message,
        }) => assert_eq!(message, "pull access denied for nope"),
        other => panic!("expected a failed pull, got {other:?}"),
    }
    assert!(!events.iter().any(|event| matches!(
        event,
        ComposeEvent::StepFinished { .. } | ComposeEvent::StepStarted { step: 1, .. }
    )));
    let log = std::fs::read_to_string(&stubs.fails_pull_log).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
}

async fn reports_failures(stubs: &Stubs) {
    match run_all(&runner(&stubs.failing), &stubs.plan(ComposeAction::Pull))
        .await
        .1
    {
        Err(ComposeError::Failed {
            action,
            command,
            code,
            message,
        }) => {
            assert_eq!(action, ComposeAction::Pull);
            assert_eq!(command, ComposeCommand::Pull);
            assert_eq!(code, Some(14));
            assert_eq!(message, "pulling...\nno such service: web");
        }
        other => panic!("expected a failure, got {other:?}"),
    }

    let missing = runner(&stubs.dir.path().join("missing"));
    assert!(matches!(
        run_all(&missing, &stubs.plan(ComposeAction::Start)).await.1,
        Err(ComposeError::Spawn { .. })
    ));
}

/// The variables in `env` output, sorted, without the ones `/bin/sh` sets
/// for itself.
fn child_env_of(env: &str) -> Vec<&str> {
    let mut vars: Vec<&str> = env
        .lines()
        .filter(|line| {
            let name = line.split_once('=').map_or(*line, |(name, _)| name);
            !matches!(name, "PWD" | "OLDPWD" | "SHLVL" | "_")
        })
        .collect();
    vars.sort_unstable();
    vars
}

async fn passes_only_the_allowlisted_environment(stubs: &Stubs) {
    let runner = runner(&stubs.env_dump).with_parent_env([
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/me"),
        ("USER", "me"),
        ("LANG", "en_US.UTF-8"),
        ("LC_ALL", "C"),
        ("TZ", "UTC"),
        ("TMPDIR", "/tmp/me"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("DOCKER_CONFIG", "/home/me/.docker"),
        ("AWS_SECRET_ACCESS_KEY", "hunter2"),
        ("ALCHEMY_LOG", "debug"),
        ("COMPOSE_PROJECT_NAME", "other"),
        ("COMPOSE_FILE", "other.yaml"),
        ("COMPOSE_ANSI", "always"),
        ("DOCKER_HOST", "tcp://elsewhere:2375"),
        ("DOCKER_CONTEXT", "remote"),
    ]);
    run_all(&runner, &stubs.plan(ComposeAction::Up))
        .await
        .1
        .unwrap();
    assert_eq!(
        child_env_of(&std::fs::read_to_string(&stubs.env_log).unwrap()),
        [
            "COMPOSE_ANSI=never",
            "DOCKER_CLI_HINTS=false",
            "DOCKER_CONFIG=/home/me/.docker",
            "DOCKER_HOST=unix:///run/user/1000/docker.sock",
            "HOME=/home/me",
            "LANG=en_US.UTF-8",
            "LC_ALL=C",
            "PATH=/usr/bin:/bin",
            "TMPDIR=/tmp/me",
            "TZ=UTC",
            "USER=me",
            "XDG_RUNTIME_DIR=/run/user/1000",
        ]
    );
}

async fn streams_lines_before_the_child_exits(stubs: &Stubs) {
    let plan = stubs.plan(ComposeAction::Up);
    let runner = runner(&stubs.waits);
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(async move { runner.run(&plan, tx, CancelToken::never()).await });

    assert_eq!(
        next_line(&mut rx).await,
        OutputLine::new(0, Stream::Stderr, "first")
    );
    assert!(!run.is_finished());
    std::fs::write(&stubs.go, "").unwrap();
    assert_eq!(
        next_line(&mut rx).await,
        OutputLine::new(0, Stream::Stdout, "second")
    );
    run.await.unwrap().unwrap();
}

async fn cancels_with_sigint(stubs: &Stubs) {
    let plan = stubs.plan(ComposeAction::Up);
    let runner = runner(&stubs.interruptible).with_grace(Duration::from_secs(30));
    let (canceller, token) = cancellation();
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(async move { runner.run(&plan, tx, token).await });

    assert_eq!(next_line(&mut rx).await.text, "ready");
    let started = Instant::now();
    canceller.cancel();
    assert_eq!(next_line(&mut rx).await.text, "interrupted");
    assert!(matches!(
        run.await.unwrap(),
        Err(ComposeError::Cancelled {
            action: ComposeAction::Up
        })
    ));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(rx.recv().await.is_none());
}

async fn kills_after_the_grace_period(stubs: &Stubs) {
    let plan = stubs.plan(ComposeAction::Down);
    let grace = Duration::from_millis(300);
    let runner = runner(&stubs.stubborn).with_grace(grace);
    let (canceller, token) = cancellation();
    let (tx, mut rx) = mpsc::channel(16);
    let run = tokio::spawn(async move { runner.run(&plan, tx, token).await });

    assert_eq!(next_line(&mut rx).await.text, "ready");
    let started = Instant::now();
    canceller.cancel();
    assert!(matches!(
        run.await.unwrap(),
        Err(ComposeError::Cancelled {
            action: ComposeAction::Down
        })
    ));
    assert!(started.elapsed() >= grace);
    assert!(started.elapsed() < Duration::from_secs(5));
}

async fn times_out(stubs: &Stubs) {
    let timeout = Duration::from_millis(200);
    let runner = runner(&stubs.slow).with_timeout(timeout);
    let started = Instant::now();
    assert!(matches!(
        run_all(&runner, &stubs.plan(ComposeAction::Restart)).await.1,
        Err(ComposeError::Timeout { action: ComposeAction::Restart, after }) if after == timeout
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}
