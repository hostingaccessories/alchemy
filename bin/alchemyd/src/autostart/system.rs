//! The real [`ServiceManager`]s: `launchctl` in `gui/<uid>` on macOS and
//! `systemctl --user` on Linux, each run by absolute path with fixed
//! arguments, no shell, and a cleared environment.
//!
//! The command sequences follow the #112 spike. `launchctl bootout`
//! returns before the old process exits, and a `bootstrap` in that window
//! fails with exit 5 as if the job were still loaded, so every bootout is
//! followed by polling `print` until it exits 113 (not loaded). A plist
//! is never rewritten with the same bytes, since every rewrite shows
//! another macOS "can run in the background" notification.
//! `systemctl --user` needs `XDG_RUNTIME_DIR` (and the session bus) to
//! reach the user manager.

use std::fmt;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use alchemy_core::lifecycle::{
    LAUNCH_AGENT_LABEL, LOG_DIR, SYSTEMD_USER_UNIT, launch_agent_path, systemd_user_unit_path,
};
use futures_util::future::{self, BoxFuture};
use tokio::time::Instant;

use super::manager::{ServiceError, ServiceManager};

/// `launchctl`, by absolute path.
pub const LAUNCHCTL: &str = "/bin/launchctl";
/// `systemctl`, by absolute path.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// `launchctl print` exits with this when the job is not loaded.
const NOT_LOADED: i32 = 113;
/// `launchctl bootout` exits with this when the job is not loaded.
const NO_SUCH_PROCESS: i32 = 3;
/// `launchctl bootstrap` exits with this when the label is still loaded.
const IO_ERROR: i32 = 5;

/// How long to wait for a booted-out job to go: launchd waits up to
/// `ExitTimeOut`, 20 s by default, before it sends SIGKILL.
const GONE_TIMEOUT: Duration = Duration::from_secs(25);
/// How often to ask whether it has gone.
const POLL: Duration = Duration::from_millis(100);
/// How long one `launchctl` or `systemctl` may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// How a command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Its exit code, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// Its standard error.
    pub stderr: String,
}

/// Runs a fixed command: `program` by absolute path with `args` and only
/// the environment in `env`.
pub trait CommandRunner: Send + Sync + fmt::Debug {
    /// Run it and wait for it.
    fn run<'a>(
        &'a self,
        program: &'static str,
        args: &'a [String],
        env: &'a [(String, String)],
    ) -> BoxFuture<'a, io::Result<CommandOutput>>;
}

/// The real [`CommandRunner`]: no shell, stdin and stdout from and to
/// `/dev/null`, `LC_ALL=C` and a fixed `PATH` plus what it is given, and
/// killed if it takes longer than 30 s.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run<'a>(
        &'a self,
        program: &'static str,
        args: &'a [String],
        env: &'a [(String, String)],
    ) -> BoxFuture<'a, io::Result<CommandOutput>> {
        Box::pin(async move {
            let run = tokio::process::Command::new(program)
                .args(args)
                .env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                .env("LC_ALL", "C")
                .envs(env.iter().map(|(k, v)| (k, v)))
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output();
            let output = tokio::time::timeout(COMMAND_TIMEOUT, run)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "took too long"))??;
            Ok(CommandOutput {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }
}

/// The per-user launch agent, `computer.alchemy.alchemyd` in `gui/<uid>`.
#[derive(Debug, Clone)]
pub struct LaunchAgent {
    home: PathBuf,
    uid: u32,
    runner: Arc<dyn CommandRunner>,
    poll: Duration,
}

impl LaunchAgent {
    /// The agent for the user with `home` and `uid`, run through `runner`.
    pub fn new(home: PathBuf, uid: u32, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            home,
            uid,
            runner,
            poll: POLL,
        }
    }

    /// Poll every `poll` instead, for tests.
    #[must_use]
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    fn path(&self) -> PathBuf {
        launch_agent_path(&self.home)
    }

    fn target(&self) -> String {
        format!("gui/{}/{LAUNCH_AGENT_LABEL}", self.uid)
    }

    async fn launchctl(&self, args: &[&str]) -> Result<CommandOutput, ServiceError> {
        let args: Vec<String> = args.iter().map(|&a| a.to_owned()).collect();
        self.runner
            .run(LAUNCHCTL, &args, &[])
            .await
            .map_err(|source| ServiceError::Spawn {
                program: LAUNCHCTL,
                source,
            })
    }

    async fn must(&self, args: &[&str], ok: &[i32]) -> Result<(), ServiceError> {
        let output = self.launchctl(args).await?;
        if output
            .code
            .is_some_and(|code| code == 0 || ok.contains(&code))
        {
            return Ok(());
        }
        Err(failed(LAUNCHCTL, args, &output))
    }

    async fn loaded(&self) -> Result<bool, ServiceError> {
        let target = self.target();
        let output = self.launchctl(&["print", &target]).await?;
        Ok(output.code != Some(NOT_LOADED))
    }

    /// Wait until `print` says the job is no longer loaded.
    async fn gone(&self) -> Result<(), ServiceError> {
        let deadline = Instant::now() + GONE_TIMEOUT;
        while self.loaded().await? {
            if Instant::now() >= deadline {
                return Err(ServiceError::Timeout {
                    what: format!("{LAUNCH_AGENT_LABEL} launch agent"),
                });
            }
            tokio::time::sleep(self.poll).await;
        }
        Ok(())
    }

    async fn boot_out(&self) -> Result<(), ServiceError> {
        let target = self.target();
        self.must(&["bootout", &target], &[NO_SUCH_PROCESS]).await?;
        self.gone().await
    }

    async fn bootstrap(&self) -> Result<(), ServiceError> {
        let domain = format!("gui/{}", self.uid);
        let path = self.path();
        let plist = path.to_string_lossy();
        let args = ["bootstrap", domain.as_str(), &plist];
        let output = self.launchctl(&args).await?;
        if output.code == Some(0) {
            return Ok(());
        }
        if output.code != Some(IO_ERROR) {
            return Err(failed(LAUNCHCTL, &args, &output));
        }
        self.gone().await?;
        self.must(&args, &[]).await
    }

    async fn do_install(&self, file: &str, start: bool) -> Result<bool, ServiceError> {
        let path = self.path();
        create_dir(&self.home.join(LOG_DIR))?;
        if let Some(dir) = path.parent() {
            create_dir(dir)?;
        }
        let same = read(&path)?.as_deref() == Some(file);
        if !start {
            if !same {
                write(&path, file)?;
            }
            return Ok(!same);
        }
        let loaded = self.loaded().await?;
        if same && loaded {
            let target = self.target();
            self.must(&["kickstart", &target], &[]).await?;
            return Ok(false);
        }
        if loaded {
            self.boot_out().await?;
        }
        if !same {
            write(&path, file)?;
        }
        self.bootstrap().await?;
        Ok(true)
    }

    async fn do_remove(&self, stop: bool) -> Result<bool, ServiceError> {
        let loaded = stop && self.loaded().await?;
        if loaded {
            self.boot_out().await?;
        }
        let existed = remove(&self.path())?;
        Ok(loaded || existed)
    }
}

impl ServiceManager for LaunchAgent {
    fn read(&self) -> BoxFuture<'_, Result<Option<String>, ServiceError>> {
        Box::pin(future::ready(read(&self.path())))
    }

    fn available(&self) -> BoxFuture<'_, bool> {
        Box::pin(future::ready(true))
    }

    fn install<'a>(
        &'a self,
        file: &'a str,
        start: bool,
    ) -> BoxFuture<'a, Result<bool, ServiceError>> {
        Box::pin(self.do_install(file, start))
    }

    fn remove(&self, stop: bool) -> BoxFuture<'_, Result<bool, ServiceError>> {
        Box::pin(self.do_remove(stop))
    }
}

/// The per-user `systemd --user` unit, `alchemyd.service`.
#[derive(Debug, Clone)]
pub struct UserUnit {
    path: PathBuf,
    runtime_dir: PathBuf,
    bus: Option<String>,
    runner: Arc<dyn CommandRunner>,
}

impl UserUnit {
    /// The unit under `xdg_config_home` or `home`, reaching the user
    /// manager through `runtime_dir` and, if set, the session `bus`
    /// address, run through `runner`.
    pub fn new(
        xdg_config_home: Option<&Path>,
        home: &Path,
        runtime_dir: PathBuf,
        bus: Option<String>,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            path: systemd_user_unit_path(xdg_config_home, home),
            runtime_dir,
            bus,
            runner,
        }
    }

    fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![(
            "XDG_RUNTIME_DIR".to_owned(),
            self.runtime_dir.to_string_lossy().into_owned(),
        )];
        if let Some(bus) = &self.bus {
            env.push(("DBUS_SESSION_BUS_ADDRESS".to_owned(), bus.clone()));
        }
        env
    }

    async fn systemctl(&self, args: &[&str]) -> Result<CommandOutput, ServiceError> {
        let args: Vec<String> = std::iter::once("--user")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        self.runner
            .run(SYSTEMCTL, &args, &self.env())
            .await
            .map_err(|source| ServiceError::Spawn {
                program: SYSTEMCTL,
                source,
            })
    }

    async fn query(&self, args: &[&str]) -> Result<bool, ServiceError> {
        Ok(self.systemctl(args).await?.code == Some(0))
    }

    async fn must(&self, args: &[&str]) -> Result<(), ServiceError> {
        let output = self.systemctl(args).await?;
        if output.code == Some(0) {
            return Ok(());
        }
        let mut full = vec!["--user"];
        full.extend_from_slice(args);
        Err(failed(SYSTEMCTL, &full, &output))
    }

    async fn do_install(&self, file: &str, start: bool) -> Result<bool, ServiceError> {
        if let Some(dir) = self.path.parent() {
            create_dir(dir)?;
        }
        let same = read(&self.path)?.as_deref() == Some(file);
        if !same {
            write(&self.path, file)?;
            self.must(&["daemon-reload"]).await?;
        }
        let enabled = self
            .query(&["is-enabled", "--quiet", SYSTEMD_USER_UNIT])
            .await?;
        if !enabled {
            self.must(&["enable", SYSTEMD_USER_UNIT]).await?;
        }
        if !start {
            return Ok(!same || !enabled);
        }
        let active = self
            .query(&["is-active", "--quiet", SYSTEMD_USER_UNIT])
            .await?;
        if !active {
            self.must(&["start", SYSTEMD_USER_UNIT]).await?;
        } else if !same {
            self.must(&["restart", SYSTEMD_USER_UNIT]).await?;
        }
        Ok(!same || !enabled || !active)
    }

    async fn do_remove(&self, stop: bool) -> Result<bool, ServiceError> {
        if read(&self.path)?.is_none() {
            return Ok(false);
        }
        if stop {
            self.must(&["disable", "--now", SYSTEMD_USER_UNIT]).await?;
        } else {
            self.must(&["disable", SYSTEMD_USER_UNIT]).await?;
        }
        remove(&self.path)?;
        self.must(&["daemon-reload"]).await?;
        Ok(true)
    }
}

impl ServiceManager for UserUnit {
    fn read(&self) -> BoxFuture<'_, Result<Option<String>, ServiceError>> {
        Box::pin(future::ready(read(&self.path)))
    }

    fn available(&self) -> BoxFuture<'_, bool> {
        let manager = self.runtime_dir.join("systemd/private");
        Box::pin(future::ready(
            Path::new(SYSTEMCTL).is_file() && manager.exists(),
        ))
    }

    fn install<'a>(
        &'a self,
        file: &'a str,
        start: bool,
    ) -> BoxFuture<'a, Result<bool, ServiceError>> {
        Box::pin(self.do_install(file, start))
    }

    fn remove(&self, stop: bool) -> BoxFuture<'_, Result<bool, ServiceError>> {
        Box::pin(self.do_remove(stop))
    }
}

fn failed(program: &str, args: &[&str], output: &CommandOutput) -> ServiceError {
    ServiceError::Command {
        command: std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" "),
        code: output.code,
        stderr: output
            .stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned(),
    }
}

/// `path`'s contents, or `None` if it doesn't exist.
fn read(path: &Path) -> Result<Option<String>, ServiceError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ServiceError::File {
            action: "read",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Create `dir` and its parents, 0755, if missing. launchd would create
/// the log directory 0744 itself (#112).
fn create_dir(dir: &Path) -> Result<(), ServiceError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .map_err(|source| ServiceError::File {
            action: "create",
            path: dir.to_path_buf(),
            source,
        })
}

/// Replace `path` with `contents` atomically, 0644: a new file next to
/// it, then a rename over it.
fn write(path: &Path, contents: &str) -> Result<(), ServiceError> {
    use std::io::Write as _;
    let err = |action, source| ServiceError::File {
        action,
        path: path.to_path_buf(),
        source,
    };
    let dir = path
        .parent()
        .ok_or_else(|| err("write", io::ErrorKind::InvalidInput.into()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".alchemyd-")
        .permissions(std::fs::Permissions::from_mode(0o644))
        .tempfile_in(dir)
        .map_err(|e| err("write", e))?;
    temp.write_all(contents.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|e| err("write", e))?;
    temp.persist(path).map_err(|e| err("write", e.error))?;
    Ok(())
}

/// Delete `path`. Returns whether it existed.
fn remove(path: &Path) -> Result<bool, ServiceError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ServiceError::File {
            action: "remove",
            path: path.to_path_buf(),
            source,
        }),
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
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use super::*;

    type Ran = (String, Vec<(String, String)>);

    /// Answers each command line with the next scripted exit code, the
    /// last repeating, or 0; records every line and its environment.
    #[derive(Debug, Default)]
    struct Script {
        codes: Mutex<HashMap<String, VecDeque<i32>>>,
        ran: Mutex<Vec<Ran>>,
    }

    impl Script {
        fn answer(self, line: &str, codes: &[i32]) -> Self {
            self.codes
                .lock()
                .unwrap()
                .insert(line.to_owned(), codes.iter().copied().collect());
            self
        }

        fn lines(&self) -> Vec<String> {
            self.ran
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.0.clone())
                .collect()
        }
    }

    impl CommandRunner for Script {
        fn run<'a>(
            &'a self,
            program: &'static str,
            args: &'a [String],
            env: &'a [(String, String)],
        ) -> BoxFuture<'a, io::Result<CommandOutput>> {
            let line = std::iter::once(program.to_owned())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            self.ran.lock().unwrap().push((line.clone(), env.to_vec()));
            let mut codes = self.codes.lock().unwrap();
            let code = codes.get_mut(&line).map_or(0, |queue| {
                if queue.len() > 1 {
                    queue.pop_front().unwrap_or(0)
                } else {
                    queue.front().copied().unwrap_or(0)
                }
            });
            Box::pin(future::ready(Ok(CommandOutput {
                code: Some(code),
                stderr: if code == 0 {
                    String::new()
                } else {
                    format!("failed {code}\n")
                },
            })))
        }
    }

    const PRINT: &str = "/bin/launchctl print gui/501/computer.alchemy.alchemyd";
    const BOOTOUT: &str = "/bin/launchctl bootout gui/501/computer.alchemy.alchemyd";
    const KICKSTART: &str = "/bin/launchctl kickstart gui/501/computer.alchemy.alchemyd";

    fn agent(script: Script) -> (tempfile::TempDir, Arc<Script>, LaunchAgent) {
        let home = tempfile::tempdir().unwrap();
        let script = Arc::new(script);
        let agent = LaunchAgent::new(home.path().to_path_buf(), 501, script.clone())
            .with_poll(Duration::from_millis(1));
        (home, script, agent)
    }

    fn bootstrap(home: &Path) -> String {
        format!(
            "/bin/launchctl bootstrap gui/501 {}",
            home.join("Library/LaunchAgents/computer.alchemy.alchemyd.plist")
                .display()
        )
    }

    #[tokio::test]
    async fn launch_agent_install_from_scratch() {
        let (home, script, agent) = agent(Script::default().answer(PRINT, &[NOT_LOADED]));
        assert!(agent.install("plist", true).await.unwrap());
        assert_eq!(script.lines(), [PRINT.to_owned(), bootstrap(home.path())]);
        assert_eq!(agent.read().await.unwrap().as_deref(), Some("plist"));
        let path = home
            .path()
            .join("Library/LaunchAgents/computer.alchemy.alchemyd.plist");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o644);
        let logs = std::fs::metadata(home.path().join("Library/Logs/Alchemy")).unwrap();
        assert!(logs.is_dir());
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&logs.permissions()) & 0o777,
            0o755
        );
    }

    #[tokio::test]
    async fn launch_agent_install_again_only_kickstarts() {
        let (home, script, agent) = agent(Script::default());
        agent.install("plist", false).await.unwrap();
        let before = std::fs::metadata(
            home.path()
                .join("Library/LaunchAgents/computer.alchemy.alchemyd.plist"),
        )
        .unwrap()
        .modified()
        .unwrap();
        assert!(!agent.install("plist", true).await.unwrap());
        assert_eq!(script.lines(), [PRINT, KICKSTART]);
        let after = std::fs::metadata(
            home.path()
                .join("Library/LaunchAgents/computer.alchemy.alchemyd.plist"),
        )
        .unwrap()
        .modified()
        .unwrap();
        assert_eq!(before, after, "never rewritten with the same bytes");
    }

    #[tokio::test]
    async fn launch_agent_reinstall_waits_for_the_old_job() {
        let (home, script, agent) = agent(Script::default().answer(PRINT, &[0, 0, 0, NOT_LOADED]));
        agent.install("old", false).await.unwrap();
        assert!(agent.install("new", true).await.unwrap());
        assert_eq!(
            script.lines(),
            [
                PRINT.to_owned(),
                BOOTOUT.to_owned(),
                PRINT.to_owned(),
                PRINT.to_owned(),
                PRINT.to_owned(),
                bootstrap(home.path()),
            ]
        );
        assert_eq!(agent.read().await.unwrap().as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn launch_agent_bootstrap_retries_once_after_the_race() {
        let (home, _, _) = agent(Script::default());
        let line = bootstrap(home.path());
        let script = Arc::new(
            Script::default()
                .answer(PRINT, &[NOT_LOADED, 0, NOT_LOADED])
                .answer(&line, &[IO_ERROR, 0]),
        );
        let agent = LaunchAgent::new(home.path().to_path_buf(), 501, script.clone())
            .with_poll(Duration::from_millis(1));
        assert!(agent.install("plist", true).await.unwrap());
        assert_eq!(
            script.lines(),
            [
                PRINT.to_owned(),
                line.clone(),
                PRINT.to_owned(),
                PRINT.to_owned(),
                line.clone()
            ]
        );

        let other = home.path().join("other");
        let script = Arc::new(
            Script::default()
                .answer(PRINT, &[NOT_LOADED])
                .answer(&bootstrap(&other), &[1]),
        );
        let agent = LaunchAgent::new(other, 501, script);
        let err = agent.install("plist", true).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::Command { code: Some(1), .. }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn launch_agent_remove() {
        let (_home, script, outside) =
            agent(Script::default().answer(PRINT, &[0, NOT_LOADED, NOT_LOADED]));
        outside.install("plist", false).await.unwrap();
        assert!(outside.remove(true).await.unwrap());
        assert_eq!(script.lines(), [PRINT, BOOTOUT, PRINT]);
        assert_eq!(outside.read().await.unwrap(), None);
        assert!(!outside.remove(true).await.unwrap(), "again is unchanged");

        let (_home, script, inside) = agent(Script::default());
        inside.install("plist", false).await.unwrap();
        assert!(inside.remove(false).await.unwrap());
        assert_eq!(
            script.lines(),
            Vec::<String>::new(),
            "from inside: no launchctl"
        );
    }

    #[tokio::test]
    async fn launch_agent_install_from_inside_never_runs_launchctl() {
        let (_home, script, agent) = agent(Script::default());
        assert!(agent.install("plist", false).await.unwrap());
        assert!(!agent.install("plist", false).await.unwrap());
        assert!(agent.install("new", false).await.unwrap());
        assert_eq!(script.lines(), Vec::<String>::new());
    }

    fn unit(script: Script) -> (tempfile::TempDir, Arc<Script>, UserUnit) {
        let home = tempfile::tempdir().unwrap();
        let script = Arc::new(script);
        let unit = UserUnit::new(
            None,
            home.path(),
            PathBuf::from("/run/user/1000"),
            Some("unix:path=/run/user/1000/bus".into()),
            script.clone(),
        );
        (home, script, unit)
    }

    const IS_ENABLED: &str = "/usr/bin/systemctl --user is-enabled --quiet alchemyd.service";
    const IS_ACTIVE: &str = "/usr/bin/systemctl --user is-active --quiet alchemyd.service";
    const RELOAD: &str = "/usr/bin/systemctl --user daemon-reload";

    #[tokio::test]
    async fn user_unit_install_and_remove() {
        let (home, script, unit) = unit(
            Script::default()
                .answer(IS_ENABLED, &[1, 0])
                .answer(IS_ACTIVE, &[3, 0]),
        );
        assert!(unit.install("unit", true).await.unwrap());
        assert_eq!(
            script.lines(),
            [
                RELOAD,
                IS_ENABLED,
                "/usr/bin/systemctl --user enable alchemyd.service",
                IS_ACTIVE,
                "/usr/bin/systemctl --user start alchemyd.service",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join(".config/systemd/user/alchemyd.service"))
                .unwrap(),
            "unit"
        );
        let env = script.ran.lock().unwrap()[0].1.clone();
        assert_eq!(
            env,
            [
                ("XDG_RUNTIME_DIR".to_owned(), "/run/user/1000".to_owned()),
                (
                    "DBUS_SESSION_BUS_ADDRESS".to_owned(),
                    "unix:path=/run/user/1000/bus".to_owned()
                ),
            ]
        );

        script.ran.lock().unwrap().clear();
        assert!(!unit.install("unit", true).await.unwrap());
        assert_eq!(script.lines(), [IS_ENABLED, IS_ACTIVE]);

        script.ran.lock().unwrap().clear();
        assert!(unit.install("new", true).await.unwrap());
        assert_eq!(
            script.lines(),
            [
                RELOAD,
                IS_ENABLED,
                IS_ACTIVE,
                "/usr/bin/systemctl --user restart alchemyd.service"
            ]
        );

        script.ran.lock().unwrap().clear();
        assert!(unit.install("newer", false).await.unwrap());
        assert_eq!(script.lines(), [RELOAD, IS_ENABLED]);

        script.ran.lock().unwrap().clear();
        assert!(unit.remove(true).await.unwrap());
        assert_eq!(
            script.lines(),
            [
                "/usr/bin/systemctl --user disable --now alchemyd.service",
                RELOAD
            ]
        );
        assert_eq!(unit.read().await.unwrap(), None);
        script.ran.lock().unwrap().clear();
        assert!(!unit.remove(true).await.unwrap(), "again is unchanged");
        assert_eq!(script.lines(), Vec::<String>::new());

        unit.install("unit", false).await.unwrap();
        script.ran.lock().unwrap().clear();
        assert!(unit.remove(false).await.unwrap());
        assert_eq!(
            script.lines(),
            ["/usr/bin/systemctl --user disable alchemyd.service", RELOAD]
        );
    }

    #[tokio::test]
    async fn user_unit_failures_keep_the_file() {
        let (_home, _script, unit) = unit(Script::default().answer(
            "/usr/bin/systemctl --user disable --now alchemyd.service",
            &[1],
        ));
        unit.install("unit", false).await.unwrap();
        let err = unit.remove(true).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "`/usr/bin/systemctl --user disable --now alchemyd.service` failed (exit 1): failed 1"
        );
        assert_eq!(unit.read().await.unwrap().as_deref(), Some("unit"));
    }
}
