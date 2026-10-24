//! Compose runner wiring: the real `CliComposeRunner` against a stub
//! `docker` binary.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alchemyd::compose::{
    CliComposeRunner, ComposeAction, ComposeCommand, ComposeError, ComposePlan, ComposeProject,
    ComposeRunner, ImageSource,
};
use alchemyd::engine::Endpoint;

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn plan(project: &ComposeProject, action: ComposeAction) -> ComposePlan {
    ComposePlan::new(project, action, ImageSource::Registry)
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

/// One test, so every fixture is written before any child is spawned:
/// exec'ing a file another thread still holds open for writing fails with
/// `ETXTBSY` on Linux.
#[tokio::test]
async fn cli_compose_runner_runs_docker_compose_with_fixed_argv() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let docker = write_script(
        dir.path(),
        "docker",
        &format!(
            r#"{{ echo "DOCKER_HOST=$DOCKER_HOST"; printf '[%s]\n' "$@"; echo; }} >> '{}'
read -r line && echo "stdin was open" >&2 && exit 9
exit 0"#,
            log.display()
        ),
    );
    let failing = write_script(
        dir.path(),
        "failing",
        "echo 'pulling...' >&2\necho 'no such service: web' >&2\nexit 14",
    );
    let slow = write_script(dir.path(), "slow", "exec sleep 10");
    let env_log = dir.path().join("env.log");
    let env_dump = write_script(
        dir.path(),
        "env-dump",
        &format!("env > '{}'", env_log.display()),
    );

    let endpoint = Endpoint::unix("/run/user/1000/docker.sock").unwrap();
    let project = ComposeProject::new(dir.path().join("my stacks/blog")).unwrap();
    let project_dir = project.dir().display().to_string();

    let runner = CliComposeRunner::new(&docker, endpoint.clone()).unwrap();
    runner
        .run(&plan(&project, ComposeAction::Up))
        .await
        .unwrap();
    runner
        .run(&plan(&project, ComposeAction::Update))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!(
            "DOCKER_HOST=unix:///run/user/1000/docker.sock\n\
             [compose]\n[--project-directory]\n[{project_dir}]\n[--progress]\n[json]\n[up]\n[--detach]\n\n\
             DOCKER_HOST=unix:///run/user/1000/docker.sock\n\
             [compose]\n[--project-directory]\n[{project_dir}]\n[--progress]\n[json]\n[pull]\n\n\
             DOCKER_HOST=unix:///run/user/1000/docker.sock\n\
             [compose]\n[--project-directory]\n[{project_dir}]\n[--progress]\n[json]\n[up]\n[--detach]\n\n"
        )
    );

    match CliComposeRunner::new(&failing, endpoint.clone())
        .unwrap()
        .run(&plan(&project, ComposeAction::Pull))
        .await
    {
        Err(ComposeError::Failed {
            action,
            command,
            code,
            stderr,
        }) => {
            assert_eq!(action, ComposeAction::Pull);
            assert_eq!(command, ComposeCommand::Pull);
            assert_eq!(code, Some(14));
            assert_eq!(stderr, "pulling...\nno such service: web");
        }
        other => panic!("expected a failure, got {other:?}"),
    }

    assert!(matches!(
        CliComposeRunner::new(dir.path().join("missing"), endpoint.clone())
            .unwrap()
            .run(&plan(&project, ComposeAction::Start))
            .await,
        Err(ComposeError::Spawn { .. })
    ));

    passes_only_the_allowlisted_environment(&env_dump, &env_log, &endpoint, &project).await;

    let started = std::time::Instant::now();
    let timeout = Duration::from_millis(200);
    assert!(matches!(
        CliComposeRunner::new(&slow, endpoint)
            .unwrap()
            .with_timeout(timeout)
            .run(&plan(&project, ComposeAction::Restart))
            .await,
        Err(ComposeError::Timeout { action: ComposeAction::Restart, after }) if after == timeout
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}

async fn passes_only_the_allowlisted_environment(
    env_dump: &Path,
    env_log: &Path,
    endpoint: &Endpoint,
    project: &ComposeProject,
) {
    CliComposeRunner::new(env_dump, endpoint.clone())
        .unwrap()
        .with_parent_env([
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/me"),
            ("LC_ALL", "C"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DOCKER_CONFIG", "/home/me/.docker"),
            ("AWS_SECRET_ACCESS_KEY", "hunter2"),
            ("COMPOSE_PROJECT_NAME", "other"),
            ("DOCKER_HOST", "tcp://elsewhere:2375"),
            ("DOCKER_CONTEXT", "remote"),
        ])
        .run(&plan(project, ComposeAction::Up))
        .await
        .unwrap();
    assert_eq!(
        child_env_of(&std::fs::read_to_string(env_log).unwrap()),
        [
            "COMPOSE_ANSI=never",
            "DOCKER_CLI_HINTS=false",
            "DOCKER_CONFIG=/home/me/.docker",
            "DOCKER_HOST=unix:///run/user/1000/docker.sock",
            "HOME=/home/me",
            "LC_ALL=C",
            "PATH=/usr/bin:/bin",
            "XDG_RUNTIME_DIR=/run/user/1000",
        ]
    );
}
