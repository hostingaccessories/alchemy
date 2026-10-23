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

use alchemy_core::StackName;
use alchemyd::compose::{
    CliComposeRunner, ComposeAction, ComposeError, ComposeProject, ComposeRunner,
};
use alchemyd::engine::Endpoint;

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
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

    let endpoint = Endpoint::unix("/run/user/1000/docker.sock").unwrap();
    let project = ComposeProject::new(
        dir.path().join("my stacks/blog"),
        StackName::new("blog").unwrap(),
    )
    .unwrap();
    let project_dir = project.dir().display().to_string();

    let runner = CliComposeRunner::new(&docker, endpoint.clone()).unwrap();
    runner.run(&project, ComposeAction::Up).await.unwrap();
    runner.run(&project, ComposeAction::Down).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!(
            "DOCKER_HOST=unix:///run/user/1000/docker.sock\n\
             [compose]\n[--project-directory]\n[{project_dir}]\n[-p]\n[blog]\n[up]\n[--detach]\n\n\
             DOCKER_HOST=unix:///run/user/1000/docker.sock\n\
             [compose]\n[--project-directory]\n[{project_dir}]\n[-p]\n[blog]\n[down]\n\n"
        )
    );

    match CliComposeRunner::new(&failing, endpoint.clone())
        .unwrap()
        .run(&project, ComposeAction::Pull)
        .await
    {
        Err(ComposeError::Failed {
            action,
            code,
            stderr,
        }) => {
            assert_eq!(action, ComposeAction::Pull);
            assert_eq!(code, Some(14));
            assert_eq!(stderr, "pulling...\nno such service: web");
        }
        other => panic!("expected a failure, got {other:?}"),
    }

    assert!(matches!(
        CliComposeRunner::new(dir.path().join("missing"), endpoint.clone())
            .unwrap()
            .run(&project, ComposeAction::Start)
            .await,
        Err(ComposeError::Spawn { .. })
    ));

    let started = std::time::Instant::now();
    let timeout = Duration::from_millis(200);
    assert!(matches!(
        CliComposeRunner::new(&slow, endpoint)
            .unwrap()
            .with_timeout(timeout)
            .run(&project, ComposeAction::Restart)
            .await,
        Err(ComposeError::Timeout { action: ComposeAction::Restart, after }) if after == timeout
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}
