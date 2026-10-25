//! Autostart: alchemyd installs, removes, and reports its own service.
//!
//! [`Autostart`] reads which services are installed, lets
//! [`alchemy_core::lifecycle`] decide the mode, the modes on offer, and
//! the steps to another mode, and publishes the result in [`State`] for
//! `GET /api/machine/autostart` and the `autostart` event. The per-user
//! service goes through a [`ServiceManager`] without root; the system
//! service goes through [`Privileges`], behind the same prompt as M8's
//! `system_service` action, so a switch to or from `boot` answers
//! `pending` and finishes in the background.
//!
//! **Handover.** A switch can be asked of the alchemyd that the service
//! being removed runs. The other service is always installed and started
//! first; its alchemyd waits for the state directory's lock (see
//! [`Supervisor::waits_for_state`]). Then:
//!
//! - leaving `login` for `boot` from inside the per-user service
//!   unregisters it without stopping it, answers `restarting`, and
//!   [`Autostart::handing_over`] resolves a moment later, so `run`
//!   returns [`Stopped::HandedOver`](crate::Stopped::HandedOver) and
//!   `main` exits 0, which neither launchd nor systemd restarts;
//! - leaving `boot` from inside the system service runs `remove-service`
//!   through the helper last, and stopping the unit stops this alchemyd
//!   with SIGTERM;
//! - turning autostart off from inside the per-user service unregisters
//!   it and leaves this alchemyd running until it exits.
//!
//! `alchemyd service install|remove` ([`install_user_service`],
//! [`remove_user_service`]) is the same per-user service from the
//! command line, for the desktop app to run when no alchemyd answers.

mod manager;
mod system;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alchemy_api::autostart::{
    Autostart as Status, AutostartAttempt, AutostartChange, AutostartOutcome,
};
use alchemy_api::privileges::{AttemptResult, ChangeOutcome, HelperCommand, PrivilegeAttempt};
use alchemy_core::lifecycle::{
    AutostartMode, AutostartStep, InstalledServices, LogHint, ModeSupport, ServiceCall,
    ServiceFileError, Supervisor, UserService, autostart_steps, available_modes,
    launch_agent_plist, log_hint, service_call, supervisor_from_cgroup, supervisor_from_launchd,
    systemd_user_unit, transient_install,
};
use alchemy_core::privilege::{Action, PrivilegeFacts, PrivilegeState};
use alchemy_core::route::HostOs;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

pub use manager::{FakeCall, FakeServiceManager, ServiceError, ServiceManager};
pub use system::{
    CommandOutput, CommandRunner, LAUNCHCTL, LaunchAgent, SYSTEMCTL, SystemRunner, UserUnit,
};

use crate::State;
use crate::engine::Os;
use crate::privileges::{ChangeError, FactsError, Privileges};

/// How long after answering a handover alchemyd waits before it stops,
/// so the answer reaches the client.
pub const HANDOVER_DELAY: Duration = Duration::from_millis(500);

/// What [`Autostart`] reads and changes the per-user service through, and
/// what it knows about this alchemyd.
#[derive(Debug, Clone)]
pub struct AutostartDeps {
    /// The OS.
    pub os: HostOs,
    /// What runs this alchemyd.
    pub supervisor: Supervisor,
    /// The user's home, for the launch agent and its log.
    pub home: Option<PathBuf>,
    /// Whether alchemyd runs from inside an `AppImage`.
    pub appimage: bool,
    /// The per-user service.
    pub manager: Arc<dyn ServiceManager>,
    /// How long to wait before handing over.
    pub handover_delay: Duration,
    /// How often autostart is read again when nothing prompts it, to see
    /// changes made by `alchemyd service` or by hand.
    pub interval: Duration,
}

impl AutostartDeps {
    /// The per-user service on this machine, for this process, from its
    /// environment: `HOME`, `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`,
    /// `DBUS_SESSION_BUS_ADDRESS`, and `APPIMAGE`.
    pub fn system() -> Self {
        let uid = rustix::process::getuid().as_raw();
        let os = host_os();
        let home = absolute_env("HOME");
        let manager: Arc<dyn ServiceManager> = match os {
            HostOs::MacOs => Arc::new(LaunchAgent::new(
                home.clone().unwrap_or_default(),
                uid,
                Arc::new(SystemRunner),
            )),
            HostOs::Linux => Arc::new(UserUnit::new(
                absolute_env("XDG_CONFIG_HOME").as_deref(),
                home.as_deref().unwrap_or(Path::new("/")),
                absolute_env("XDG_RUNTIME_DIR")
                    .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}"))),
                std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
                Arc::new(SystemRunner),
            )),
        };
        Self {
            supervisor: detect_supervisor(),
            home,
            appimage: std::env::var_os("APPIMAGE").is_some(),
            ..Self::new(manager).with_os(os)
        }
    }

    /// The per-user service through `manager`, on this OS, for an
    /// alchemyd started by hand with no home.
    pub fn new(manager: Arc<dyn ServiceManager>) -> Self {
        Self {
            os: host_os(),
            supervisor: Supervisor::Manual,
            home: None,
            appimage: false,
            manager,
            handover_delay: HANDOVER_DELAY,
            interval: crate::privileges::DEFAULT_INTERVAL,
        }
    }

    /// A [`FakeServiceManager`] with nothing installed, for tests.
    pub fn fake() -> Self {
        Self::new(Arc::new(FakeServiceManager::new()))
    }

    /// Decide for `os` instead.
    #[must_use]
    pub fn with_os(mut self, os: HostOs) -> Self {
        self.os = os;
        self
    }

    /// Say `supervisor` runs this alchemyd.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Supervisor) -> Self {
        self.supervisor = supervisor;
        self
    }

    /// Use `home` as the user's home.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Wait `delay` before handing over.
    #[must_use]
    pub fn with_handover_delay(mut self, delay: Duration) -> Self {
        self.handover_delay = delay;
        self
    }

    /// Read autostart again every `interval`.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

fn host_os() -> HostOs {
    match Os::current() {
        Os::Linux => HostOs::Linux,
        Os::MacOs => HostOs::MacOs,
    }
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// What runs this process: from `/proc/self/cgroup` on Linux, and from
/// `XPC_SERVICE_NAME`, which launchd sets to a job's label, on macOS.
pub fn detect_supervisor() -> Supervisor {
    let uid = rustix::process::getuid().as_raw();
    match host_os() {
        HostOs::Linux => std::fs::read_to_string("/proc/self/cgroup")
            .map_or(Supervisor::Manual, |text| {
                supervisor_from_cgroup(&text, uid)
            }),
        HostOs::MacOs => {
            let label = std::env::var("XPC_SERVICE_NAME").ok();
            supervisor_from_launchd(label.as_deref(), uid)
        }
    }
}

/// Why autostart could not be read or changed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AutostartError {
    /// A switch is already in flight.
    #[error("Alchemy is already changing autostart.")]
    Busy,
    /// The mode isn't available on this machine.
    #[error("{why}")]
    Unavailable {
        /// The mode.
        mode: AutostartMode,
        /// Why, for people.
        why: String,
    },
    /// The system service is installed, so the per-user one can't be.
    #[error(
        "Alchemy already starts at boot as a system service. Switch autostart to login to use the per-user service instead."
    )]
    SystemServiceInstalled,
    /// There is no home to put the launch agent in.
    #[error("HOME is not set to an absolute path.")]
    NoHome,
    /// The service file could not be rendered for alchemyd's path.
    #[error(transparent)]
    File(#[from] ServiceFileError),
    /// The per-user service could not be changed.
    #[error(transparent)]
    Service(#[from] ServiceError),
    /// The facts could not be read.
    #[error(transparent)]
    Facts(#[from] FactsError),
}

impl AutostartError {
    fn from_change(error: ChangeError, mode: AutostartMode) -> Self {
        match error {
            ChangeError::Busy => Self::Busy,
            ChangeError::Unsupported(reason) => Self::Unavailable {
                mode,
                why: reason.to_string(),
            },
            ChangeError::Facts(error) => Self::Facts(error),
        }
    }
}

/// The per-user service file for the alchemyd at `alchemyd` on `os`.
pub fn render(os: HostOs, alchemyd: &Path, home: Option<&Path>) -> Result<String, AutostartError> {
    match os {
        HostOs::MacOs => Ok(launch_agent_plist(
            alchemyd,
            home.ok_or(AutostartError::NoHome)?,
        )?),
        HostOs::Linux => Ok(systemd_user_unit(alchemyd)?),
    }
}

/// `alchemyd service install`: install the per-user service for the
/// alchemyd at `alchemyd` (its canonical path) and, with `start`, start
/// it now. Refused while the system service is installed. Returns
/// whether anything changed.
pub async fn install_user_service(
    deps: &AutostartDeps,
    alchemyd: &Path,
    system_installed: bool,
    start: bool,
) -> Result<bool, AutostartError> {
    if system_installed {
        return Err(AutostartError::SystemServiceInstalled);
    }
    if let Some(why) = transient_install(alchemyd, deps.appimage) {
        return Err(AutostartError::Unavailable {
            mode: AutostartMode::Login,
            why: why.to_string(),
        });
    }
    let file = render(deps.os, alchemyd, deps.home.as_deref())?;
    Ok(deps.manager.install(&file, start).await?)
}

/// `alchemyd service remove`: stop and remove the per-user service.
/// Returns whether anything changed.
pub async fn remove_user_service(deps: &AutostartDeps) -> Result<bool, AutostartError> {
    Ok(deps.manager.remove(true).await?)
}

/// `alchemyd service install [--start]` on this machine, for the running
/// binary's canonical path: what to print on success.
pub async fn service_install(start: bool) -> Result<String, AutostartError> {
    let deps = AutostartDeps::system();
    let alchemyd = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| AutostartError::Facts(FactsError::CurrentExe(e.to_string())))?;
    let uid = rustix::process::getuid().as_raw();
    let system_installed = crate::privileges::service_file(deps.os, uid).exists();
    let changed = install_user_service(&deps, &alchemyd, system_installed, start).await?;
    Ok(match (changed, start) {
        (true, true) => "Installed and started the per-user service.",
        (true, false) => "Installed the per-user service; it starts at the next login.",
        (false, _) => "The per-user service is already installed.",
    }
    .to_owned())
}

/// `alchemyd service remove` on this machine: what to print on success.
pub async fn service_remove() -> Result<String, AutostartError> {
    let changed = remove_user_service(&AutostartDeps::system()).await?;
    Ok(if changed {
        "Stopped and removed the per-user service."
    } else {
        "The per-user service is not installed."
    }
    .to_owned())
}

/// Why each mode can't be switched to, if it can't.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Support {
    login: Option<String>,
    boot: Option<String>,
}

impl Support {
    fn modes(&self) -> ModeSupport {
        ModeSupport {
            user_service: self.login.is_none(),
            system_service: self.boot.is_none(),
        }
    }

    fn why_not(&self, mode: AutostartMode) -> Option<&str> {
        match mode {
            AutostartMode::Off => None,
            AutostartMode::Login => self.login.as_deref(),
            AutostartMode::Boot => self.boot.as_deref(),
        }
    }
}

/// How a switch went, before it is reported.
enum Switched {
    Unchanged,
    Done,
    HandOver,
    Pending {
        alchemyd: PathBuf,
        rest: Vec<AutostartStep>,
    },
    Terminal {
        command: Option<HelperCommand>,
        message: Option<String>,
    },
}

/// The daemon's autostart. Cheap to clone: every clone shares the same
/// switch in flight and handover request.
#[derive(Debug, Clone)]
pub struct Autostart {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    deps: AutostartDeps,
    privileges: Privileges,
    state: State,
    pending: Mutex<Option<AutostartMode>>,
    last: Mutex<Option<AutostartAttempt>>,
    handover: watch::Sender<bool>,
}

impl Autostart {
    /// Autostart through `deps` and `privileges`, published in `state`.
    pub fn new(deps: AutostartDeps, privileges: Privileges, state: State) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                privileges,
                state,
                pending: Mutex::new(None),
                last: Mutex::new(None),
                handover: watch::Sender::new(false),
            }),
        }
    }

    /// What runs this alchemyd.
    pub fn supervisor(&self) -> Supervisor {
        self.inner.deps.supervisor
    }

    /// Read autostart now.
    pub async fn read(&self) -> Result<Status, AutostartError> {
        let facts = self.inner.privileges.facts().await?;
        let installed = self.installed(&facts).await?;
        let support = self.support(&facts).await;
        Ok(self.status(&facts, installed, &support))
    }

    /// Read autostart and publish it in [`State`]. A failed read keeps
    /// what was published before.
    pub async fn refresh(&self) {
        match self.read().await {
            Ok(status) => {
                self.inner.state.set_autostart(status);
            }
            Err(error) => tracing::warn!(%error, "cannot read autostart"),
        }
    }

    /// Switch to `target`.
    ///
    /// A system prompt runs in the background: this returns
    /// [`AutostartOutcome::Pending`] at once, and [`Status::last`] says
    /// how it ended.
    pub async fn set(&self, target: AutostartMode) -> Result<AutostartChange, AutostartError> {
        if !self.begin(target) {
            return Err(AutostartError::Busy);
        }
        self.publish().await;
        let switched = self.switch(target).await;
        let (outcome, command, message) = match switched {
            Err(error) => {
                let record = !matches!(
                    error,
                    AutostartError::Busy | AutostartError::Unavailable { .. }
                );
                let attempt = record.then(|| failed(target, &error));
                self.end(attempt).await;
                return Err(error);
            }
            Ok(Switched::Unchanged) => {
                self.end(None).await;
                (AutostartOutcome::Unchanged, None, None)
            }
            Ok(Switched::Done) => {
                self.end(Some(done(target))).await;
                (AutostartOutcome::Done, None, None)
            }
            Ok(Switched::HandOver) => {
                self.end(Some(done(target))).await;
                self.hand_over();
                (AutostartOutcome::Restarting, None, None)
            }
            Ok(Switched::Terminal { command, message }) => {
                self.end(None).await;
                (AutostartOutcome::NeedsTerminal, command, message)
            }
            Ok(Switched::Pending { alchemyd, rest }) => {
                tokio::spawn(self.clone().after_prompt(target, alchemyd, rest));
                (AutostartOutcome::Pending, None, None)
            }
        };
        self.publish().await;
        Ok(AutostartChange {
            outcome,
            command,
            message,
            autostart: self.current().await?,
        })
    }

    /// Resolve once a switch handed over to another alchemyd, so this one
    /// should stop.
    pub async fn handing_over(&self) {
        let mut handover = self.inner.handover.subscribe();
        let _ = handover.wait_for(|go| *go).await;
    }

    /// Keep the published autostart current until `shutdown` flips to
    /// `true` or its sender is dropped: at start, whenever the privileges
    /// change, and every [`AutostartDeps::interval`].
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut privileges = self.inner.state.watch_privileges();
        let mut tick = tokio::time::interval(self.inner.deps.interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => return,
                Ok(()) = privileges.changed() => {
                    privileges.borrow_and_update();
                }
                _ = tick.tick() => {}
            }
            self.refresh().await;
        }
    }

    async fn current(&self) -> Result<Status, AutostartError> {
        match self.inner.state.autostart() {
            Some(status) => Ok(status),
            None => self.read().await,
        }
    }

    async fn publish(&self) {
        self.refresh().await;
    }

    async fn installed(&self, facts: &PrivilegeFacts) -> Result<InstalledServices, AutostartError> {
        let on_disk = self.inner.deps.manager.read().await?;
        let rendered = render(
            self.inner.deps.os,
            &facts.alchemyd,
            self.inner.deps.home.as_deref(),
        )
        .ok();
        Ok(InstalledServices {
            user: UserService::compare(on_disk.as_deref(), rendered.as_deref()),
            system: facts.service.installed,
        })
    }

    async fn support(&self, facts: &PrivilegeFacts) -> Support {
        let deps = &self.inner.deps;
        if let Some(why) = transient_install(&facts.alchemyd, deps.appimage) {
            let why = why.to_string();
            return Support {
                login: Some(why.clone()),
                boot: Some(why),
            };
        }
        let login = match render(deps.os, &facts.alchemyd, deps.home.as_deref()) {
            Err(error) => Some(error.to_string()),
            Ok(_) if !deps.manager.available().await => {
                Some("No systemd --user manager is running for this user.".to_owned())
            }
            Ok(_) => None,
        };
        let mut wanted = facts.clone();
        wanted.service.wanted = true;
        let boot = match alchemy_core::privilege::state(Action::SystemService, &wanted) {
            PrivilegeState::Unsupported { reason } => Some(reason.to_string()),
            _ => None,
        };
        Support { login, boot }
    }

    fn status(
        &self,
        facts: &PrivilegeFacts,
        installed: InstalledServices,
        support: &Support,
    ) -> Status {
        let deps = &self.inner.deps;
        let mode = installed.mode();
        let log_hint = match (deps.os, mode, deps.home.as_deref()) {
            (HostOs::MacOs, AutostartMode::Login, None) => LogHint::Terminal,
            (os, mode, home) => log_hint(os, mode, home.unwrap_or(Path::new("/")), facts.uid),
        };
        Status {
            mode,
            available_modes: available_modes(mode, support.modes()),
            log_hint,
            supervisor: deps.supervisor,
            pending: self.inner.pending.lock().ok().and_then(|p| *p),
            last: self.inner.last.lock().ok().and_then(|l| l.clone()),
        }
    }

    /// Mark a switch to `target` in flight, unless one already is.
    fn begin(&self, target: AutostartMode) -> bool {
        let Ok(mut pending) = self.inner.pending.lock() else {
            return false;
        };
        if pending.is_some() {
            return false;
        }
        *pending = Some(target);
        true
    }

    /// End the switch in flight, recording `attempt` if there is one.
    async fn end(&self, attempt: Option<AutostartAttempt>) {
        if let Some(attempt) = attempt
            && let Ok(mut last) = self.inner.last.lock()
        {
            *last = Some(attempt);
        }
        if let Ok(mut pending) = self.inner.pending.lock() {
            *pending = None;
        }
        self.publish().await;
    }

    fn hand_over(&self) {
        let delay = self.inner.deps.handover_delay;
        let handover = self.inner.handover.clone();
        tracing::info!("handing over to the system service");
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            handover.send_replace(true);
        });
    }

    async fn switch(&self, target: AutostartMode) -> Result<Switched, AutostartError> {
        let facts = self.inner.privileges.facts().await?;
        let installed = self.installed(&facts).await?;
        let support = self.support(&facts).await;
        if target != installed.mode()
            && let Some(why) = support.why_not(target)
        {
            return Err(AutostartError::Unavailable {
                mode: target,
                why: why.to_owned(),
            });
        }
        self.inner.privileges.request_mode(target);
        let steps = autostart_steps(installed, target);
        if steps.is_empty() {
            self.inner.privileges.refresh().await;
            return Ok(Switched::Unchanged);
        }
        tracing::info!(from = %installed.mode(), to = %target, ?steps, "switching autostart");
        self.run_steps(target, &facts.alchemyd, &steps).await
    }

    /// Run `steps` in order, stopping at a prompt or a terminal command.
    async fn run_steps(
        &self,
        target: AutostartMode,
        alchemyd: &Path,
        steps: &[AutostartStep],
    ) -> Result<Switched, AutostartError> {
        let deps = &self.inner.deps;
        let mut switched = Switched::Done;
        for (i, step) in steps.iter().enumerate() {
            match service_call(*step, deps.supervisor, target) {
                ServiceCall::InstallUser { start } => {
                    let file = render(deps.os, alchemyd, deps.home.as_deref())?;
                    deps.manager.install(&file, start).await?;
                }
                ServiceCall::RemoveUser { stop } => {
                    deps.manager.remove(stop).await?;
                }
                ServiceCall::HandOver => {
                    deps.manager.remove(false).await?;
                    switched = Switched::HandOver;
                }
                call @ (ServiceCall::InstallSystem | ServiceCall::RemoveSystem) => {
                    let grant = call == ServiceCall::InstallSystem;
                    let change = self
                        .inner
                        .privileges
                        .change(Action::SystemService, grant)
                        .await
                        .map_err(|e| AutostartError::from_change(e, target))?;
                    match change.outcome {
                        ChangeOutcome::Pending => {
                            return Ok(Switched::Pending {
                                alchemyd: alchemyd.to_path_buf(),
                                rest: steps.get(i + 1..).unwrap_or_default().to_vec(),
                            });
                        }
                        ChangeOutcome::NeedsTerminal => {
                            return Ok(Switched::Terminal {
                                command: change.command,
                                message: change.message,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(switched)
    }

    /// Finish a switch once its prompt ends: the remaining steps if the
    /// helper succeeded, otherwise record why not.
    async fn after_prompt(
        self,
        target: AutostartMode,
        alchemyd: PathBuf,
        rest: Vec<AutostartStep>,
    ) {
        let attempt = self.inner.privileges.settled(Action::SystemService).await;
        let finished = match attempt {
            Some(PrivilegeAttempt {
                result: AttemptResult::Done,
                ..
            }) => match self.run_steps(target, &alchemyd, &rest).await {
                Ok(Switched::HandOver) => {
                    self.hand_over();
                    done(target)
                }
                Ok(_) => done(target),
                Err(error) => failed(target, &error),
            },
            Some(attempt) => AutostartAttempt {
                mode: target,
                result: attempt.result,
                message: attempt.message,
                at: attempt.at,
            },
            None => AutostartAttempt {
                mode: target,
                result: AttemptResult::Failed,
                message: Some("The system prompt ended without a result.".into()),
                at: unix_now(),
            },
        };
        match finished.result {
            AttemptResult::Done => tracing::info!(mode = %target, "autostart switched"),
            result => {
                tracing::warn!(mode = %target, ?result, message = ?finished.message, "autostart not switched");
            }
        }
        self.end(Some(finished)).await;
    }
}

fn done(mode: AutostartMode) -> AutostartAttempt {
    AutostartAttempt {
        mode,
        result: AttemptResult::Done,
        message: None,
        at: unix_now(),
    }
}

fn failed(mode: AutostartMode, error: &AutostartError) -> AutostartAttempt {
    AutostartAttempt {
        mode,
        result: AttemptResult::Failed,
        message: Some(error.to_string()),
        at: unix_now(),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}
