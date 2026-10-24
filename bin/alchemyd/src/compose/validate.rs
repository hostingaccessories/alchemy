//! Validating files before they are saved, with `docker compose config`.
//!
//! Unlike [`ComposeConfigReader`](super::ComposeConfigReader), which reads
//! a project as Compose finds it, a [`ComposeCheck`] names the files to use
//! instead, so a staged copy is validated against the project directory it
//! will replace files in: relative paths, `env_file:` entries, and
//! override files resolve as they will once it is saved.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::future::BoxFuture;

use super::config::{CliComposeConfig, ComposeConfig, ConfigError, FakeConfig};
use super::{ComposeProject, ComposeProjectError};
use crate::engine::Endpoint;

/// One validation: the project directory, and the files Compose should
/// read in place of the ones it would pick there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeCheck {
    project: ComposeProject,
    files: Vec<PathBuf>,
    env_file: Option<PathBuf>,
}

impl ComposeCheck {
    /// Validate `project` as Compose finds it.
    pub fn new(project: ComposeProject) -> Self {
        Self {
            project,
            files: Vec::new(),
            env_file: None,
        }
    }

    /// Also read the compose file at `file`, as one `-f`. Once any is
    /// given, Compose reads only these, without its own overrides.
    pub fn with_file(mut self, file: impl Into<PathBuf>) -> Result<Self, ComposeProjectError> {
        self.files.push(absolute(file.into())?);
        Ok(self)
    }

    /// Read variables from `env_file` instead of the project's `.env`.
    pub fn with_env_file(
        mut self,
        env_file: impl Into<PathBuf>,
    ) -> Result<Self, ComposeProjectError> {
        self.env_file = Some(absolute(env_file.into())?);
        Ok(self)
    }

    /// The project directory.
    pub fn project(&self) -> &ComposeProject {
        &self.project
    }

    /// The compose files given with `-f`, in order.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The env file given with `--env-file`.
    pub fn env_file(&self) -> Option<&Path> {
        self.env_file.as_deref()
    }
}

/// A file path checked the way a project directory is.
fn absolute(path: PathBuf) -> Result<PathBuf, ComposeProjectError> {
    ComposeProject::new(path).map(|p| p.dir().to_owned())
}

/// The argv after the `docker` binary that validates `check` and prints
/// the resolved configuration as JSON.
///
/// No `-p`: Compose resolves the name as it would by hand, from the
/// directory, `name:`, or `COMPOSE_PROJECT_NAME`.
pub fn check_args(check: &ComposeCheck) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "compose".into(),
        "--project-directory".into(),
        check.project.dir().as_os_str().to_owned(),
    ];
    for file in &check.files {
        args.push("-f".into());
        args.push(file.as_os_str().to_owned());
    }
    if let Some(env_file) = &check.env_file {
        args.push("--env-file".into());
        args.push(env_file.as_os_str().to_owned());
    }
    args.extend(["config".into(), "--format".into(), "json".into()]);
    args
}

/// Validates files with Compose before they are saved.
///
/// Object safe, so the daemon can hold any implementation behind an
/// `Arc`. [`CliComposeConfig`] runs `docker compose config`;
/// [`FakeComposeValidator`] answers as told, for tests.
pub trait ComposeValidator: Send + Sync + fmt::Debug {
    /// Validate `check` with Compose pointed at `endpoint`, returning what
    /// Compose resolved. [`ConfigError::Invalid`] carries Compose's reason.
    fn validate<'a>(
        &'a self,
        check: &'a ComposeCheck,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>>;
}

impl ComposeValidator for CliComposeConfig {
    fn validate<'a>(
        &'a self,
        check: &'a ComposeCheck,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>> {
        Box::pin(async move {
            self.run(check.project().dir(), &check_args(check), endpoint)
                .await
        })
    }
}

type Hook = Arc<dyn Fn(&ComposeCheck) + Send + Sync>;

#[derive(Default)]
struct FakeInner {
    answer: Option<FakeConfig>,
    checks: Vec<ComposeCheck>,
    hook: Option<Hook>,
}

/// [`ComposeValidator`] that answers as told and records every check.
///
/// Until told otherwise it accepts everything, as a project named after
/// its directory with no services. Clones share their answer and records.
#[derive(Clone, Default)]
pub struct FakeComposeValidator {
    inner: Arc<Mutex<FakeInner>>,
}

impl fmt::Debug for FakeComposeValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeComposeValidator")
            .field("checks", &self.lock().checks)
            .finish_non_exhaustive()
    }
}

impl FakeComposeValidator {
    /// A validator that accepts everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `answer` from now on.
    pub fn answer(&self, answer: FakeConfig) {
        self.lock().answer = Some(answer);
    }

    /// Call `hook` with each check while it runs, before answering, as a
    /// stand-in for whatever happens on disk while Compose runs. The hook
    /// may set the answer, such as a message naming the staged file.
    pub fn on_validate(&self, hook: impl Fn(&ComposeCheck) + Send + Sync + 'static) {
        self.lock().hook = Some(Arc::new(hook));
    }

    /// Every check so far, oldest first.
    pub fn checks(&self) -> Vec<ComposeCheck> {
        self.lock().checks.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ComposeValidator for FakeComposeValidator {
    fn validate<'a>(
        &'a self,
        check: &'a ComposeCheck,
        _endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>> {
        let hook = {
            let mut inner = self.lock();
            inner.checks.push(check.clone());
            inner.hook.clone()
        };
        if let Some(hook) = hook {
            hook(check);
        }
        let answer = self.lock().answer.clone();
        let dir_name = check
            .project()
            .dir()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Box::pin(async move {
            match answer {
                None => Ok(ComposeConfig {
                    name: dir_name,
                    ..ComposeConfig::default()
                }),
                Some(FakeConfig::Config(config)) => Ok(config),
                Some(FakeConfig::Invalid(message)) => Err(ConfigError::Invalid {
                    code: Some(1),
                    message,
                }),
                Some(FakeConfig::NoDocker) => Err(ConfigError::NoDocker),
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

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn check_args_name_the_staged_files() {
        let project = || ComposeProject::new("/srv/apps/blog").unwrap();
        let cases: [(ComposeCheck, &[&str]); 3] = [
            (
                ComposeCheck::new(project()),
                &[
                    "compose",
                    "--project-directory",
                    "/srv/apps/blog",
                    "config",
                    "--format",
                    "json",
                ],
            ),
            (
                ComposeCheck::new(project())
                    .with_file("/srv/apps/.alchemy-tmp-blog-1/compose.yaml")
                    .unwrap()
                    .with_file("/srv/apps/blog/compose.override.yaml")
                    .unwrap(),
                &[
                    "compose",
                    "--project-directory",
                    "/srv/apps/blog",
                    "-f",
                    "/srv/apps/.alchemy-tmp-blog-1/compose.yaml",
                    "-f",
                    "/srv/apps/blog/compose.override.yaml",
                    "config",
                    "--format",
                    "json",
                ],
            ),
            (
                ComposeCheck::new(project())
                    .with_env_file("/srv/apps/.alchemy-tmp-blog-1/.env")
                    .unwrap(),
                &[
                    "compose",
                    "--project-directory",
                    "/srv/apps/blog",
                    "--env-file",
                    "/srv/apps/.alchemy-tmp-blog-1/.env",
                    "config",
                    "--format",
                    "json",
                ],
            ),
        ];
        for (check, want) in cases {
            assert_eq!(strings(check_args(&check)), want, "{check:?}");
        }
    }

    #[test]
    fn checks_refuse_relative_or_dotted_files() {
        let check = || ComposeCheck::new(ComposeProject::new("/srv/blog").unwrap());
        assert!(check().with_file("compose.yaml").is_err());
        assert!(check().with_file("/srv/blog/../x/compose.yaml").is_err());
        assert!(check().with_env_file("./.env").is_err());
    }

    #[tokio::test]
    async fn fake_accepts_until_told_otherwise_and_records() {
        let fake = FakeComposeValidator::new();
        let seen = Arc::new(Mutex::new(0));
        fake.on_validate({
            let seen = seen.clone();
            move |_| *seen.lock().unwrap() += 1
        });
        let check = ComposeCheck::new(ComposeProject::new("/srv/apps/blog").unwrap());
        let endpoint = Endpoint::system();

        let config = fake.validate(&check, &endpoint).await.unwrap();
        assert_eq!(config.name, "blog");
        fake.answer(FakeConfig::Invalid("yaml: bad".into()));
        let err = fake.validate(&check, &endpoint).await.unwrap_err();
        assert!(err.is_invalid());
        assert_eq!(err.to_string(), "yaml: bad");

        assert_eq!(fake.checks(), [check.clone(), check]);
        assert_eq!(*seen.lock().unwrap(), 2);
    }
}
