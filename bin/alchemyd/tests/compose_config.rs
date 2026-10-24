//! Config reader wiring: the real `CliComposeConfig` against stub `docker`
//! scripts.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alchemyd::compose::{
    CliComposeConfig, ComposeConfigReader, ComposeProject, ConfigError, ImageSource,
};
use alchemyd::engine::Endpoint;

const CONFIG: &str =
    r#"{"name":"fancy","services":{"web":{"image":"nginx"},"api":{"build":{"context":"."}}}}"#;

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Every stub is written before any runs: exec'ing a file another thread
/// still holds open for writing fails with `ETXTBSY` on Linux.
#[tokio::test]
async fn reads_the_resolved_config_through_the_docker_binary() {
    let dir = tempfile::tempdir().unwrap();
    let argv_log = dir.path().join("argv.log");
    let env_log = dir.path().join("env.log");
    let ok = write_script(
        dir.path(),
        "ok",
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\nenv > '{}'\necho 'time=\"x\" level=warning msg=\"unset\"' >&2\necho '{CONFIG}'",
            argv_log.display(),
            env_log.display()
        ),
    );
    let failing = write_script(
        dir.path(),
        "failing",
        "echo 'time=\"x\" level=warning msg=\"The X variable is not set\"' >&2\necho 'yaml: line 4: did not find expected node content' >&2\nexit 15",
    );
    let garbage = write_script(dir.path(), "garbage", "echo 'not json'");
    let slow = write_script(dir.path(), "slow", "sleep 30");
    let project = ComposeProject::new("/srv/blog").unwrap();
    let endpoint = Endpoint::unix("/run/user/1000/docker.sock").unwrap();

    let reader = CliComposeConfig::new(&ok).with_parent_env([
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/me"),
        ("SECRET", "hunter2"),
        ("COMPOSE_PROJECT_NAME", "other"),
    ]);
    let config = reader.read(&project, &endpoint).await.unwrap();
    assert_eq!(config.name, "fancy");
    assert_eq!(config.images(), ImageSource::Build);
    assert_eq!(
        std::fs::read_to_string(&argv_log).unwrap(),
        "compose\n--project-directory\n/srv/blog\nconfig\n--format\njson\n"
    );
    let env = std::fs::read_to_string(&env_log).unwrap();
    assert!(
        env.contains("DOCKER_HOST=unix:///run/user/1000/docker.sock"),
        "{env}"
    );
    assert!(env.contains("HOME=/home/me"), "{env}");
    assert!(!env.contains("SECRET"), "{env}");
    assert!(!env.contains("COMPOSE_PROJECT_NAME"), "{env}");

    let err = CliComposeConfig::new(&failing)
        .read(&project, &endpoint)
        .await
        .unwrap_err();
    match err {
        ConfigError::Invalid { code, message } => {
            assert_eq!(code, Some(15));
            assert_eq!(message, "yaml: line 4: did not find expected node content");
        }
        other => panic!("{other:?}"),
    }

    let err = CliComposeConfig::new(&garbage)
        .read(&project, &endpoint)
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigError::Unreadable(_)), "{err:?}");

    let err = CliComposeConfig::new(dir.path().join("missing"))
        .read(&project, &endpoint)
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigError::NoDocker), "{err:?}");

    let err = CliComposeConfig::new("docker")
        .read(&project, &endpoint)
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigError::NoDocker), "{err:?}");

    let started = Instant::now();
    let err = CliComposeConfig::new(&slow)
        .with_timeout(Duration::from_millis(200))
        .read(&project, &endpoint)
        .await
        .unwrap_err();
    assert!(matches!(err, ConfigError::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}
