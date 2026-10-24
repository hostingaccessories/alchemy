//! A project's resolved configuration, read with `docker compose config`.
//!
//! Compose itself resolves the project name (from the directory, a `name:`
//! in the compose file, or `COMPOSE_PROJECT_NAME` in `.env`) and merges
//! overrides, so reading its output back is the only way to agree with a
//! hand-run `docker compose` in the same directory.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use rustix::process::Signal;
use serde::Deserialize;
use serde::de::IgnoredAny;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use super::cli::{docker_command, process_group, signal};
use super::runner::tail;
use super::{ComposeProject, ImageSource, child_env};
use crate::engine::{
    Endpoint, HostEnv, Os, SystemProbe, docker_binary_candidates, find_docker_binary,
};

/// What Alchemy uses from a project's resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeConfig {
    /// The project name Compose resolved.
    pub name: String,
    /// The services enabled without any profile, by name.
    pub services: Vec<ServiceConfig>,
}

/// One service of a [`ComposeConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// The service name.
    pub name: String,
    /// Whether it has a `build:` section.
    pub builds: bool,
}

#[derive(Deserialize)]
struct RawConfig {
    name: String,
    #[serde(default)]
    services: BTreeMap<String, RawService>,
}

#[derive(Deserialize)]
struct RawService {
    #[serde(default)]
    build: Option<IgnoredAny>,
}

impl ComposeConfig {
    /// Parse the output of `docker compose config --format json`.
    pub fn parse(json: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: RawConfig = serde_json::from_slice(json)?;
        Ok(Self {
            name: raw.name,
            services: raw
                .services
                .into_iter()
                .map(|(name, service)| ServiceConfig {
                    name,
                    builds: service.build.is_some(),
                })
                .collect(),
        })
    }

    /// Where the project's images come from: built if any service builds.
    pub fn images(&self) -> ImageSource {
        if self.services.iter().any(|s| s.builds) {
            ImageSource::Build
        } else {
            ImageSource::Registry
        }
    }
}

/// The argv after the `docker` binary that prints `project`'s resolved
/// configuration as JSON.
///
/// The same `--project-directory` form as every [`ComposeStep`], with no
/// `-p` or `-f`, so Compose picks the file and the name itself.
///
/// [`ComposeStep`]: super::ComposeStep
pub fn config_args(project: &ComposeProject) -> Vec<OsString> {
    vec![
        "compose".into(),
        "--project-directory".into(),
        project.dir().as_os_str().to_owned(),
        "config".into(),
        "--format".into(),
        "json".into(),
    ]
}

/// The reason to report from a failed `config`'s standard error.
///
/// Compose logs warnings (such as an unset variable) as
/// `time=... level=warning msg=...` lines before the error itself; those
/// are dropped, and the end of what is left is kept.
pub fn config_error_message(stderr: &str) -> String {
    let kept: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains("level=warning"))
        .collect();
    tail(&kept.join("\n"))
}

/// Why a project's configuration could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// No `docker` binary was found.
    #[error("the docker command wasn't found")]
    NoDocker,
    /// The binary could not be started.
    #[error("failed to run {}", binary.display())]
    Spawn {
        /// The binary that was run.
        binary: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Compose did not answer in time and was stopped.
    #[error("docker compose config did not finish within {after:?}")]
    Timeout {
        /// How long it was given.
        after: Duration,
    },
    /// Compose rejected the project; `message` is its own reason.
    #[error("{message}")]
    Invalid {
        /// The exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// Compose's message.
        message: String,
    },
    /// Compose printed something that is not a configuration.
    #[error("docker compose config printed output Alchemy can't read")]
    Unreadable(#[source] Option<serde_json::Error>),
}

impl ConfigError {
    /// Whether Compose itself rejected the project's files, as opposed to
    /// Compose not running at all.
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid { .. })
    }
}

/// Reads projects' resolved configuration.
///
/// Object safe, so the daemon can hold any implementation behind an
/// `Arc`. [`CliComposeConfig`] runs `docker compose config`;
/// [`FakeComposeConfig`] answers from a table, for tests.
pub trait ComposeConfigReader: Send + Sync + fmt::Debug {
    /// The resolved configuration of `project`, with Compose pointed at
    /// `endpoint`.
    fn read<'a>(
        &'a self,
        project: &'a ComposeProject,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>>;
}

/// [`ComposeConfigReader`] that runs `docker compose config` as a child.
///
/// It looks for the `docker` binary on every read, so installing Docker
/// later is picked up, and runs it as [`CliComposeRunner`] does: an
/// absolute path, the [`config_args`], no shell, and exactly [`child_env`]
/// of the daemon's environment. A run past the timeout is killed.
///
/// [`CliComposeRunner`]: super::CliComposeRunner
#[derive(Debug, Clone)]
pub struct CliComposeConfig {
    binaries: Vec<PathBuf>,
    parent_env: Vec<(OsString, OsString)>,
    timeout: Duration,
}

/// The largest configuration read, in bytes.
const MAX_CONFIG: usize = 8 * 1024 * 1024;

/// The most standard error kept, in bytes.
const MAX_STDERR: usize = 64 * 1024;

impl CliComposeConfig {
    /// How long one read may take by default.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Look for `docker` where the engine monitor does, given this
    /// process's environment.
    pub fn system() -> Self {
        let env = HostEnv::current();
        Self::with_candidates(docker_binary_candidates(
            env.path.as_deref(),
            env.home.as_deref(),
            Os::current(),
        ))
    }

    /// Always run the binary at `binary`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self::with_candidates(vec![binary.into()])
    }

    fn with_candidates(binaries: Vec<PathBuf>) -> Self {
        Self {
            binaries: binaries.into_iter().filter(|b| b.is_absolute()).collect(),
            parent_env: std::env::vars_os().collect(),
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Build the child's environment from `parent` instead of the daemon's.
    #[must_use]
    pub fn with_parent_env<K, V>(mut self, parent: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.parent_env = parent
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    /// Use `timeout` for each read instead of the default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run `docker` with `args`, which end in `config --format json`, and
    /// parse what it prints.
    pub(super) async fn run(
        &self,
        args: &[OsString],
        endpoint: &Endpoint,
    ) -> Result<ComposeConfig, ConfigError> {
        let binary =
            find_docker_binary(&SystemProbe, &self.binaries).ok_or(ConfigError::NoDocker)?;
        let env = child_env(self.parent_env.iter().cloned(), endpoint.docker_host());
        let mut child = docker_command(&binary, args, &env)
            .spawn()
            .map_err(|source| ConfigError::Spawn {
                binary: binary.clone(),
                source,
            })?;
        let group = process_group(&child);
        let stdout = read_capped(child.stdout.take(), MAX_CONFIG + 1);
        let stderr = read_capped(child.stderr.take(), MAX_STDERR);
        let finished = async { tokio::join!(stdout, stderr, child.wait()) };
        let Ok((stdout, stderr, status)) = tokio::time::timeout(self.timeout, finished).await
        else {
            signal(group, Signal::KILL);
            return Err(ConfigError::Timeout {
                after: self.timeout,
            });
        };
        let status = status.map_err(|source| ConfigError::Spawn {
            binary: binary.clone(),
            source,
        })?;
        if !status.success() {
            return Err(ConfigError::Invalid {
                code: status.code(),
                message: config_error_message(&String::from_utf8_lossy(&stderr)),
            });
        }
        if stdout.len() > MAX_CONFIG {
            return Err(ConfigError::Unreadable(None));
        }
        ComposeConfig::parse(&stdout).map_err(|e| ConfigError::Unreadable(Some(e)))
    }
}

impl ComposeConfigReader for CliComposeConfig {
    fn read<'a>(
        &'a self,
        project: &'a ComposeProject,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>> {
        Box::pin(async move { self.run(&config_args(project), endpoint).await })
    }
}

/// Read `reader` to its end, keeping at most `max` bytes. A read error ends
/// it like end of file: the exit status decides the outcome.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, max: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let Some(mut reader) = reader else {
        return kept;
    };
    let mut buffer = vec![0; 8 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                let room = max.saturating_sub(kept.len());
                kept.extend_from_slice(buffer.get(..read.min(room)).unwrap_or_default());
            }
        }
    }
}

/// What [`FakeComposeConfig`] answers for one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeConfig {
    /// This configuration.
    Config(ComposeConfig),
    /// [`ConfigError::Invalid`] with this message.
    Invalid(String),
    /// [`ConfigError::NoDocker`].
    NoDocker,
}

/// [`ComposeConfigReader`] that answers from a table of project
/// directories and counts reads, for tests of what sits above it.
///
/// A directory not in the table reads as `Invalid("no configuration file
/// provided: not found")`, as Compose says for a directory without one.
#[derive(Debug, Clone, Default)]
pub struct FakeComposeConfig {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug, Default)]
struct FakeInner {
    answers: HashMap<PathBuf, FakeConfig>,
    reads: HashMap<PathBuf, usize>,
}

impl FakeComposeConfig {
    /// A reader with no projects.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `answer` for the project in `dir` from now on. Clones share
    /// the table, so a test can change answers while the daemon runs.
    pub fn set(&self, dir: impl Into<PathBuf>, answer: FakeConfig) {
        self.lock().answers.insert(dir.into(), answer);
    }

    /// Answer a project named `name` with `services`, none of which build.
    pub fn project(&self, dir: impl Into<PathBuf>, name: &str, services: &[&str]) {
        self.set(
            dir,
            FakeConfig::Config(ComposeConfig {
                name: name.to_owned(),
                services: services
                    .iter()
                    .map(|s| ServiceConfig {
                        name: (*s).to_owned(),
                        builds: false,
                    })
                    .collect(),
            }),
        );
    }

    /// How many times the project in `dir` has been read.
    pub fn reads(&self, dir: &Path) -> usize {
        self.lock().reads.get(dir).copied().unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ComposeConfigReader for FakeComposeConfig {
    fn read<'a>(
        &'a self,
        project: &'a ComposeProject,
        _endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>> {
        let answer = {
            let mut inner = self.lock();
            *inner.reads.entry(project.dir().to_owned()).or_default() += 1;
            inner.answers.get(project.dir()).cloned()
        };
        Box::pin(async move {
            match answer {
                Some(FakeConfig::Config(config)) => Ok(config),
                Some(FakeConfig::Invalid(message)) => Err(ConfigError::Invalid {
                    code: Some(1),
                    message,
                }),
                Some(FakeConfig::NoDocker) => Err(ConfigError::NoDocker),
                None => Err(ConfigError::Invalid {
                    code: Some(1),
                    message: "no configuration file provided: not found".to_owned(),
                }),
            }
        })
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
    fn parses_the_name_services_and_builds() {
        let json = br#"{
            "name": "blog",
            "networks": {"default": {"name": "blog_default"}},
            "services": {
                "web": {"image": "nginx", "command": null},
                "api": {"build": {"context": "/srv/blog", "dockerfile": "Dockerfile"}},
                "worker": {"image": "x", "build": null}
            }
        }"#;
        let config = ComposeConfig::parse(json).unwrap();
        assert_eq!(config.name, "blog");
        let services: Vec<_> = config
            .services
            .iter()
            .map(|s| (s.name.as_str(), s.builds))
            .collect();
        assert_eq!(services, [("api", true), ("web", false), ("worker", false)]);
        assert_eq!(config.images(), ImageSource::Build);
    }

    #[test]
    fn decides_where_images_come_from() {
        let cases: [(&str, ImageSource); 3] = [
            (r#"{"name":"a"}"#, ImageSource::Registry),
            (
                r#"{"name":"a","services":{"x":{"image":"redis"}}}"#,
                ImageSource::Registry,
            ),
            (
                r#"{"name":"a","services":{"x":{"image":"redis"},"y":{"build":"."}}}"#,
                ImageSource::Build,
            ),
        ];
        for (json, want) in cases {
            assert_eq!(
                ComposeConfig::parse(json.as_bytes()).unwrap().images(),
                want,
                "{json}"
            );
        }
    }

    #[test]
    fn rejects_output_without_a_name() {
        for json in [r#"{"services":{}}"#, "not json", "", r#"{"name":1}"#] {
            assert!(ComposeConfig::parse(json.as_bytes()).is_err(), "{json}");
        }
    }

    #[test]
    fn config_args_let_compose_pick_the_file_and_name() {
        let project = ComposeProject::new("/srv/blog").unwrap();
        let args: Vec<_> = config_args(&project)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "compose",
                "--project-directory",
                "/srv/blog",
                "config",
                "--format",
                "json"
            ]
        );
    }

    #[test]
    fn error_message_drops_warnings() {
        let cases = [
            ("", ""),
            (
                "no configuration file provided: not found\n",
                "no configuration file provided: not found",
            ),
            (
                concat!(
                    "time=\"2026-10-23T20:54:55-04:00\" level=warning msg=\"The \\\"X\\\" variable is not set.\"\n",
                    "\n",
                    "yaml: line 4: did not find expected node content\n",
                ),
                "yaml: line 4: did not find expected node content",
            ),
            ("a\nb\n", "a\nb"),
        ];
        for (stderr, want) in cases {
            assert_eq!(config_error_message(stderr), want, "{stderr:?}");
        }
    }

    #[tokio::test]
    async fn fake_answers_from_its_table_and_counts_reads() {
        let fake = FakeComposeConfig::new();
        let blog = ComposeProject::new("/srv/blog").unwrap();
        let other = ComposeProject::new("/srv/other").unwrap();
        fake.project("/srv/blog", "blog", &["web"]);
        let endpoint = Endpoint::system();

        let config = fake.read(&blog, &endpoint).await.unwrap();
        assert_eq!(config.name, "blog");
        assert_eq!(config.images(), ImageSource::Registry);
        let err = fake.read(&other, &endpoint).await.unwrap_err();
        assert!(err.is_invalid(), "{err:?}");
        fake.set("/srv/blog", FakeConfig::NoDocker);
        let err = fake.read(&blog, &endpoint).await.unwrap_err();
        assert!(matches!(err, ConfigError::NoDocker), "{err:?}");
        assert!(!err.is_invalid());

        assert_eq!(fake.reads(Path::new("/srv/blog")), 2);
        assert_eq!(fake.reads(Path::new("/srv/other")), 1);
        assert_eq!(fake.reads(Path::new("/srv/none")), 0);
    }
}
