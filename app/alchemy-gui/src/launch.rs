//! Making sure a daemon runs: attach to one that answers, or install the
//! bundled alchemyd as the per-user service and wait for it.
//!
//! The shell never runs alchemyd as its child and never starts a second
//! one. Its only process call is `alchemyd service install --start`, which
//! installs or kicks the login service, so launchd or systemd owns the
//! daemon and it outlives the app. A service whose daemon finds the state
//! directory locked waits for it rather than running beside another.
//!
//! The decisions are pure ([`bundled_alchemyd`], [`after_lost`], the
//! messages); the daemon and the process sit behind [`Probe`] and
//! [`Installer`].

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use alchemy_core::lifecycle::transient_install;
use serde::Serialize;

/// How the shell stands with alchemyd. The web UI reads it through the
/// `launch_state` command while it can't reach the daemon itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Launch {
    /// Checking whether a daemon answers.
    Checking,
    /// The service was installed or kicked; waiting for alchemyd to
    /// answer.
    Starting,
    /// alchemyd answers.
    Running,
    /// alchemyd answered, then stopped; reconnecting.
    Lost,
    /// alchemyd stopped and hasn't come back on its own.
    Stopped,
    /// Starting alchemyd failed.
    Failed {
        /// Why, in a sentence safe to show.
        message: String,
    },
    /// The shell can't start alchemyd from where it runs.
    Unavailable {
        /// Why, in a sentence safe to show.
        message: String,
    },
}

impl Launch {
    /// Whether to offer starting alchemyd again.
    pub fn may_start(&self) -> bool {
        matches!(self, Self::Stopped | Self::Failed { .. })
    }

    /// Whether alchemyd answers.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Whether a daemon answers on the TCP listener.
pub trait Probe {
    /// Ask once; any failure is `false`.
    fn answers(&self) -> impl Future<Output = bool> + Send;
}

/// What running `alchemyd service install --start` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// Exit 0: the service is installed and started.
    Done,
    /// It ran and failed, with what it wrote to stderr.
    Failed(String),
    /// It could not be run, with why.
    NotRun(String),
}

/// Runs `alchemyd service install --start`.
pub trait Installer {
    /// Install and start the per-user service for the alchemyd at
    /// `alchemyd`.
    fn install(&self, alchemyd: &Path) -> impl Future<Output = Installed> + Send;
}

/// How long to wait, and how often to ask, after installing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Time between probes while waiting.
    pub poll: Duration,
    /// How long alchemyd has to answer after the install returns.
    pub start: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(500),
            start: Duration::from_secs(30),
        }
    }
}

/// How long a daemon that stopped answering may stay away before the
/// shell offers to start it again. A handover between services or a
/// restart for a privilege takes a few seconds.
pub const GONE_AFTER: Duration = Duration::from_secs(20);

/// What the shell is, for messages: a release, or a development build
/// that never installs a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// A release build, as bundled.
    Release,
    /// A debug build, run from `target/`.
    Dev,
}

impl Build {
    /// This build.
    pub fn current() -> Self {
        if cfg!(debug_assertions) {
            Self::Dev
        } else {
            Self::Release
        }
    }
}

/// The alchemyd bundled next to the shell at `exe`, its canonical path,
/// or why the shell can't install it as a service. `is_file` says whether
/// a path is a regular file; `appimage` whether `APPIMAGE` is set.
///
/// A development build never installs one, so `cargo run` and `tauri dev`
/// can't leave a service pointing into `target/`.
pub fn bundled_alchemyd(
    exe: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
    appimage: bool,
    build: Build,
) -> Result<PathBuf, String> {
    if build == Build::Dev {
        return Err("This is a development build, so Alchemy doesn't start alchemyd itself. Start it with `cargo run -p alchemyd`.".to_owned());
    }
    let Some(exe) = exe else {
        return Err("Alchemy can't tell where it is installed, so it can't start alchemyd. Start alchemyd yourself.".to_owned());
    };
    let alchemyd = exe.with_file_name("alchemyd");
    if let Some(why) = transient_install(&alchemyd, appimage) {
        return Err(why.to_string());
    }
    if !is_file(&alchemyd) {
        return Err(format!(
            "Alchemy can't find alchemyd at {}. Reinstall Alchemy, or start alchemyd yourself.",
            alchemyd.display()
        ));
    }
    Ok(alchemyd)
}

/// [`bundled_alchemyd`] for this process.
pub fn this_alchemyd() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok();
    bundled_alchemyd(
        exe.as_deref(),
        Path::is_file,
        std::env::var_os("APPIMAGE").is_some(),
        Build::current(),
    )
}

/// What went wrong with an install, for people.
pub fn install_message(installed: &Installed) -> Option<String> {
    match installed {
        Installed::Done => None,
        Installed::Failed(stderr) => {
            let why = stderr.trim();
            let why = why.strip_prefix("alchemyd: ").unwrap_or(why);
            Some(if why.is_empty() {
                "Alchemy couldn't set alchemyd up to start at login.".to_owned()
            } else {
                format!("Alchemy couldn't set alchemyd up to start at login: {why}")
            })
        }
        Installed::NotRun(why) => Some(format!("Alchemy couldn't run alchemyd: {why}")),
    }
}

/// The platform, for where alchemyd's log is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// macOS: a log file.
    MacOs,
    /// Linux: the user journal.
    Linux,
}

impl Os {
    /// This platform.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

/// What to say when alchemyd was started but never answered in `waited`.
pub fn not_started_message(os: Os, waited: Duration) -> String {
    let log = match os {
        Os::MacOs => "Its log is in ~/Library/Logs/Alchemy/alchemyd.log.",
        Os::Linux => "See its log with `journalctl --user -u alchemyd.service`.",
    };
    format!(
        "alchemyd was started but didn't answer within {} seconds. {log}",
        waited.as_secs()
    )
}

/// The state after the event stream ended, from the state before it and
/// how long the daemon has been away, given up on after `gone_after`
/// (normally [`GONE_AFTER`]). Only a daemon that was running is waited
/// for; one that never ran keeps its message.
pub fn after_lost(before: &Launch, away: Duration, gone_after: Duration) -> Launch {
    match before {
        Launch::Running | Launch::Lost if away >= gone_after => Launch::Stopped,
        Launch::Running | Launch::Lost => Launch::Lost,
        other => other.clone(),
    }
}

/// Make sure a daemon runs: attach if one answers, else install and start
/// the service for `alchemyd` and wait for it. `report` hears each step;
/// the result is where it ended.
pub async fn ensure<P: Probe, I: Installer>(
    probe: &P,
    installer: &I,
    alchemyd: Result<PathBuf, String>,
    timing: Timing,
    mut report: impl FnMut(&Launch),
) -> Launch {
    report(&Launch::Checking);
    if probe.answers().await {
        return Launch::Running;
    }
    let alchemyd = match alchemyd {
        Ok(path) => path,
        Err(message) => return Launch::Unavailable { message },
    };
    report(&Launch::Starting);
    let outcome = installer.install(&alchemyd).await;
    if let Some(message) = install_message(&outcome) {
        return Launch::Failed { message };
    }
    let deadline = tokio::time::Instant::now() + timing.start;
    loop {
        if probe.answers().await {
            return Launch::Running;
        }
        if tokio::time::Instant::now() >= deadline {
            return Launch::Failed {
                message: not_started_message(Os::current(), timing.start),
            };
        }
        tokio::time::sleep(timing.poll).await;
    }
}

/// Runs the real `alchemyd service install --start`, with the shell's
/// environment, which carries what launchctl and `systemctl --user` need.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemInstaller;

/// Upper bound on the install. On macOS it may wait up to 25 seconds for
/// an old agent to go before loading the new one.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(90);

impl Installer for SystemInstaller {
    async fn install(&self, alchemyd: &Path) -> Installed {
        let run = tokio::process::Command::new(alchemyd)
            .args(["service", "install", "--start"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(INSTALL_TIMEOUT, run).await {
            Ok(Ok(out)) if out.status.success() => Installed::Done,
            Ok(Ok(out)) => Installed::Failed(String::from_utf8_lossy(&out.stderr).into_owned()),
            Ok(Err(e)) => Installed::NotRun(e.to_string()),
            Err(_) => Installed::NotRun(format!(
                "it didn't finish within {} seconds",
                INSTALL_TIMEOUT.as_secs()
            )),
        }
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn bundled_alchemyd_table() {
        let app = "/Applications/Alchemy.app/Contents/MacOS/alchemy-gui";
        #[allow(clippy::type_complexity)]
        let cases: [(Option<&str>, bool, bool, Build, Result<&str, &str>); 7] = [
            (
                Some(app),
                true,
                false,
                Build::Release,
                Ok("/Applications/Alchemy.app/Contents/MacOS/alchemyd"),
            ),
            (
                Some("/usr/bin/alchemy-gui"),
                true,
                false,
                Build::Release,
                Ok("/usr/bin/alchemyd"),
            ),
            (Some(app), true, false, Build::Dev, Err("development build")),
            (
                Some(app),
                false,
                false,
                Build::Release,
                Err("can't find alchemyd"),
            ),
            (None, true, false, Build::Release, Err("can't tell where")),
            (
                Some("/tmp/.mount_AlchXy/usr/bin/alchemy-gui"),
                true,
                false,
                Build::Release,
                Err("AppImage"),
            ),
            (
                Some(
                    "/private/var/folders/x/AppTranslocation/U/d/Alchemy.app/Contents/MacOS/alchemy-gui",
                ),
                true,
                false,
                Build::Release,
                Err("Applications folder"),
            ),
        ];
        for (exe, exists, appimage, build, want) in cases {
            let got = bundled_alchemyd(exe.map(Path::new), |_| exists, appimage, build);
            match (got, want) {
                (Ok(path), Ok(want)) => assert_eq!(path, Path::new(want), "{exe:?}"),
                (Err(message), Err(want)) => assert!(message.contains(want), "{exe:?}: {message}"),
                (got, want) => panic!("{exe:?}: got {got:?}, want {want:?}"),
            }
        }
        let got = bundled_alchemyd(
            Some(Path::new("/usr/bin/alchemy-gui")),
            |_| true,
            true,
            Build::Release,
        );
        assert!(got.unwrap_err().contains("AppImage"));
    }

    #[test]
    fn install_message_table() {
        let cases = [
            (Installed::Done, None),
            (
                Installed::Failed("alchemyd: the system service is installed\n".to_owned()),
                Some(
                    "Alchemy couldn't set alchemyd up to start at login: the system service is installed",
                ),
            ),
            (
                Installed::Failed(String::new()),
                Some("Alchemy couldn't set alchemyd up to start at login."),
            ),
            (
                Installed::NotRun("permission denied".to_owned()),
                Some("Alchemy couldn't run alchemyd: permission denied"),
            ),
        ];
        for (installed, want) in cases {
            assert_eq!(
                install_message(&installed).as_deref(),
                want,
                "{installed:?}"
            );
        }
    }

    #[test]
    fn not_started_message_names_the_log() {
        let wait = Duration::from_secs(30);
        assert!(
            not_started_message(Os::MacOs, wait).contains("~/Library/Logs/Alchemy/alchemyd.log")
        );
        assert!(
            not_started_message(Os::Linux, wait).contains("journalctl --user -u alchemyd.service")
        );
        assert!(not_started_message(Os::Linux, wait).contains("30 seconds"));
    }

    #[test]
    fn after_lost_table() {
        let failed = Launch::Failed {
            message: "x".to_owned(),
        };
        let short = Duration::from_secs(1);
        let cases = [
            (Launch::Running, short, Launch::Lost),
            (Launch::Lost, short, Launch::Lost),
            (Launch::Running, GONE_AFTER, Launch::Stopped),
            (Launch::Lost, GONE_AFTER, Launch::Stopped),
            (Launch::Stopped, short, Launch::Stopped),
            (failed.clone(), GONE_AFTER, failed.clone()),
            (Launch::Starting, GONE_AFTER, Launch::Starting),
        ];
        for (before, away, want) in cases {
            assert_eq!(
                after_lost(&before, away, GONE_AFTER),
                want,
                "{before:?} {away:?}"
            );
        }
    }

    #[test]
    fn may_start_only_when_stopped_or_failed() {
        let cases = [
            (Launch::Checking, false),
            (Launch::Starting, false),
            (Launch::Running, false),
            (Launch::Lost, false),
            (Launch::Stopped, true),
            (
                Launch::Failed {
                    message: String::new(),
                },
                true,
            ),
            (
                Launch::Unavailable {
                    message: String::new(),
                },
                false,
            ),
        ];
        for (launch, want) in cases {
            assert_eq!(launch.may_start(), want, "{launch:?}");
        }
    }

    #[test]
    fn launch_serializes_with_a_kind() {
        let cases = [
            (Launch::Starting, r#"{"kind":"starting"}"#),
            (
                Launch::Failed {
                    message: "m".to_owned(),
                },
                r#"{"kind":"failed","message":"m"}"#,
            ),
        ];
        for (launch, want) in cases {
            assert_eq!(serde_json::to_string(&launch).unwrap(), want);
        }
    }

    /// Answers `false` until `up_after` probes, then `true`.
    struct FakeProbe {
        probes: AtomicUsize,
        up_after: usize,
    }

    impl Probe for FakeProbe {
        fn answers(&self) -> impl Future<Output = bool> + Send {
            std::future::ready(self.probes.fetch_add(1, Ordering::SeqCst) >= self.up_after)
        }
    }

    struct FakeInstaller {
        result: Installed,
        calls: Mutex<Vec<PathBuf>>,
    }

    impl Installer for FakeInstaller {
        fn install(&self, alchemyd: &Path) -> impl Future<Output = Installed> + Send {
            self.calls.lock().unwrap().push(alchemyd.to_path_buf());
            std::future::ready(self.result.clone())
        }
    }

    fn installer(result: Installed) -> FakeInstaller {
        FakeInstaller {
            result,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn probe(up_after: usize) -> FakeProbe {
        FakeProbe {
            probes: AtomicUsize::new(0),
            up_after,
        }
    }

    async fn run(
        probe: &FakeProbe,
        installer: &FakeInstaller,
        alchemyd: Result<PathBuf, String>,
    ) -> (Launch, Vec<Launch>) {
        let mut steps = Vec::new();
        let end = ensure(probe, installer, alchemyd, Timing::default(), |s| {
            steps.push(s.clone());
        })
        .await;
        (end, steps)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn bundled() -> Result<PathBuf, String> {
        Ok(PathBuf::from("/usr/bin/alchemyd"))
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_answers_is_attached_without_installing() {
        let installer = installer(Installed::Done);
        let (end, steps) = run(&probe(0), &installer, bundled()).await;
        assert_eq!(end, Launch::Running);
        assert_eq!(steps, [Launch::Checking]);
        assert!(installer.calls.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn no_daemon_installs_the_service_and_waits_for_it() {
        let installer = installer(Installed::Done);
        let (end, steps) = run(&probe(4), &installer, bundled()).await;
        assert_eq!(end, Launch::Running);
        assert_eq!(steps, [Launch::Checking, Launch::Starting]);
        assert_eq!(
            *installer.calls.lock().unwrap(),
            [PathBuf::from("/usr/bin/alchemyd")]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_install_says_why() {
        let installer = installer(Installed::Failed("alchemyd: nope".to_owned()));
        let (end, _) = run(&probe(usize::MAX), &installer, bundled()).await;
        assert_eq!(
            end,
            Launch::Failed {
                message: "Alchemy couldn't set alchemyd up to start at login: nope".to_owned()
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_never_answers_fails_after_the_timeout() {
        let installer = installer(Installed::Done);
        let started = tokio::time::Instant::now();
        let (end, _) = run(&probe(usize::MAX), &installer, bundled()).await;
        assert!(
            matches!(&end, Launch::Failed { message } if message.contains("30 seconds")),
            "{end:?}"
        );
        assert!(started.elapsed() >= Timing::default().start);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_to_install_is_unavailable_and_runs_nothing() {
        let installer = installer(Installed::Done);
        let (end, steps) = run(&probe(usize::MAX), &installer, Err("AppImage".to_owned())).await;
        assert_eq!(
            end,
            Launch::Unavailable {
                message: "AppImage".to_owned()
            }
        );
        assert_eq!(steps, [Launch::Checking]);
        assert!(installer.calls.lock().unwrap().is_empty());
    }
}
