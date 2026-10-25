//! Privileges: what needs root on this machine, and the one elevation
//! flow the daemon owns.
//!
//! [`Privileges`] reads the facts through a [`PrivilegeSystem`], lets
//! [`alchemy_core::privilege`] decide each action's state and what to
//! run, and publishes the result in [`State`] for
//! `GET /api/machine/privileges` and the `privileges` event. A grant or
//! undo ([`Privileges::change`]) never waits for the prompt: it marks the
//! action pending and runs the command through an [`Elevator`] in a task
//! of its own, so a client that goes away doesn't leave it pending. The
//! facts are read afresh right before every prompt, so the helper's trust
//! is checked as late as possible.
//!
//! Low ports only take effect when alchemyd starts, since Linux reads
//! file capabilities at exec. After a grant or undo of low ports, or a
//! grant made from a terminal, [`Privileges::restarting`] resolves and
//! the daemon shuts down cleanly so `main` can re-execute it with
//! [`restart::exec_self`](crate::restart::exec_self).
//!
//! When pkexec finds no polkit agent, that is remembered until the next
//! start, and every action that would prompt needs a terminal instead.

mod elevate;
mod facts;
mod system;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alchemy_api::network::{ListenerState, ProxyListener};
use alchemy_api::privileges::{
    AttemptResult, ChangeOutcome, HelperCommand, Privilege, PrivilegeAction, PrivilegeAttempt,
    PrivilegeChange, PrivilegeList, PrivilegeReason, PrivilegeState as WireState,
};
use alchemy_core::lifecycle::{AutostartMode, Supervisor};
use alchemy_core::privilege::{
    Action, CommandLine, HelperFacts, HostListener, LowPortFacts, PrivilegeFacts, PrivilegeState,
    ProxyPorts, ServiceFacts, Step, TerminalReason, UnsupportedReason, helper_trust, states, step,
};
use alchemy_core::route::HostOs;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

pub use elevate::{Elevator, ElevatorOutput, Ended, FakeElevator, SystemElevator, classify};
pub use facts::{
    CAP_NET_BIND_SERVICE, FactsError, FakePrivilegeSystem, HelperReading, PrivilegeSystem,
    ProcStatus, SystemFacts, grants_net_bind_service, linux_firewall, macos_firewall,
    parse_proc_status, service_file,
};
pub use system::SystemPrivileges;

use crate::State;
use crate::engine::Os;

/// How long a system prompt may stay up before the attempt fails.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the privileges are read again when nothing prompts it.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// What [`Privileges`] reads the machine and elevates through.
#[derive(Debug, Clone)]
pub struct PrivilegeDeps {
    /// The OS the states are decided for.
    pub os: HostOs,
    /// Where the facts are read.
    pub system: Arc<dyn PrivilegeSystem>,
    /// What runs the helper behind a system prompt.
    pub elevator: Arc<dyn Elevator>,
    /// How long a prompt may stay up.
    pub prompt_timeout: Duration,
    /// How often the facts are read again when nothing prompts it.
    pub interval: Duration,
    /// What runs this alchemyd. Under the system unit, low ports are not
    /// needed.
    pub supervisor: Supervisor,
}

impl PrivilegeDeps {
    /// This machine: its own facts, and `pkexec` or `osascript`.
    pub fn system() -> Self {
        Self::new(Arc::new(SystemPrivileges), Arc::new(SystemElevator))
    }

    /// Facts from `system`, elevating through `elevator`, on this OS.
    pub fn new(system: Arc<dyn PrivilegeSystem>, elevator: Arc<dyn Elevator>) -> Self {
        Self {
            os: match Os::current() {
                Os::Linux => HostOs::Linux,
                Os::MacOs => HostOs::MacOs,
            },
            system,
            elevator,
            prompt_timeout: PROMPT_TIMEOUT,
            interval: DEFAULT_INTERVAL,
            supervisor: Supervisor::Manual,
        }
    }

    /// Say `supervisor` runs this alchemyd.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Supervisor) -> Self {
        self.supervisor = supervisor;
        self
    }

    /// A [`FakePrivilegeSystem::plain`] machine and a [`FakeElevator`]
    /// that succeeds, for tests.
    pub fn fake() -> Self {
        Self::new(
            Arc::new(FakePrivilegeSystem::plain()),
            Arc::new(FakeElevator::new()),
        )
    }

    /// Decide the states for `os` instead.
    #[must_use]
    pub fn with_os(mut self, os: HostOs) -> Self {
        self.os = os;
        self
    }

    /// Give a prompt `timeout` instead.
    #[must_use]
    pub fn with_prompt_timeout(mut self, timeout: Duration) -> Self {
        self.prompt_timeout = timeout;
        self
    }

    /// Read the facts again every `interval` instead.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

/// Why [`Privileges::change`] did nothing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChangeError {
    /// A grant or undo of the action is already in flight.
    #[error("Alchemy is already changing this privilege.")]
    Busy,
    /// Alchemy can't change it on this machine.
    #[error("{0}")]
    Unsupported(UnsupportedReason),
    /// The facts could not be read.
    #[error(transparent)]
    Facts(#[from] FactsError),
}

/// The daemon's privileges. Cheap to clone: every clone shares the same
/// pending set, results, and restart request.
#[derive(Debug, Clone)]
pub struct Privileges {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    deps: PrivilegeDeps,
    state: State,
    proxy_ports: ProxyPorts,
    pending: watch::Sender<BTreeSet<Action>>,
    last: Mutex<BTreeMap<Action, PrivilegeAttempt>>,
    no_agent: AtomicBool,
    restart: watch::Sender<bool>,
    requested: Mutex<Option<AutostartMode>>,
}

impl Privileges {
    /// Privileges read through `deps`, published in `state`, with the
    /// firewall opening `proxy_ports`.
    pub fn new(deps: PrivilegeDeps, state: State, proxy_ports: ProxyPorts) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                state,
                proxy_ports,
                pending: watch::Sender::new(BTreeSet::new()),
                last: Mutex::new(BTreeMap::new()),
                no_agent: AtomicBool::new(false),
                restart: watch::Sender::new(false),
                requested: Mutex::new(None),
            }),
        }
    }

    /// Record that autostart was asked to switch to `mode`, which decides
    /// whether the system service is wanted. Until the first request it
    /// is wanted exactly when it is installed.
    pub fn request_mode(&self, mode: AutostartMode) {
        if let Ok(mut requested) = self.inner.requested.lock() {
            *requested = Some(mode);
        }
    }

    /// Wait until no grant or undo of `action` is in flight, and return
    /// how the last one ended.
    pub async fn settled(&self, action: Action) -> Option<PrivilegeAttempt> {
        let mut pending = self.inner.pending.subscribe();
        let _ = pending.wait_for(|p| !p.contains(&action)).await;
        self.last().remove(&action)
    }

    /// Read the facts now, with the actions in flight.
    pub async fn facts(&self) -> Result<PrivilegeFacts, FactsError> {
        let system = self.inner.deps.system.read().await?;
        let host = host_listener(&self.inner.state.routing().proxy.host);
        Ok(privilege_facts(
            system,
            &Context {
                os: self.inner.deps.os,
                host,
                proxy_ports: self.inner.proxy_ports,
                no_agent: self.inner.no_agent.load(Ordering::SeqCst),
                pending: self.inner.pending.borrow().clone(),
                supervisor: self.inner.deps.supervisor,
                requested: self.inner.requested.lock().ok().and_then(|r| *r),
            },
        ))
    }

    /// Read the facts and publish every privilege in [`State`]. A failed
    /// read keeps what was published before.
    pub async fn refresh(&self) {
        match self.facts().await {
            Ok(facts) => {
                let last = self.last();
                self.inner
                    .state
                    .set_privileges(privilege_list(&facts, &last));
            }
            Err(error) => tracing::warn!(%error, "cannot read the privileges"),
        }
    }

    /// Grant (`grant` true) or undo `action`.
    ///
    /// A prompt runs in the background: this returns
    /// [`ChangeOutcome::Pending`] at once, and the action's
    /// [`Privilege::last`] says how it ended.
    pub async fn change(
        &self,
        action: Action,
        grant: bool,
    ) -> Result<PrivilegeChange, ChangeError> {
        let facts = self.facts().await?;
        let (outcome, command, message) = match step(action, grant, &facts) {
            Step::Busy => return Err(ChangeError::Busy),
            Step::Unsupported(reason) => return Err(ChangeError::Unsupported(reason)),
            Step::Nothing => (ChangeOutcome::Unchanged, None, None),
            Step::Terminal { command, reason } => (
                ChangeOutcome::NeedsTerminal,
                Some(helper_command(&command)),
                Some(reason.to_string()),
            ),
            Step::Restart => {
                if !self.start(action) {
                    return Err(ChangeError::Busy);
                }
                tracing::info!(%action, "restarting to put a privilege in effect");
                self.inner.restart.send_replace(true);
                (ChangeOutcome::Restarting, None, None)
            }
            Step::Prompt(command) => {
                if !self.start(action) {
                    return Err(ChangeError::Busy);
                }
                tokio::spawn(self.clone().attempt(action, grant, command));
                (ChangeOutcome::Pending, None, None)
            }
        };
        self.refresh().await;
        Ok(PrivilegeChange {
            outcome,
            command,
            message,
            privilege: self.current(action),
        })
    }

    /// Resolve once a change asked for alchemyd to restart.
    pub async fn restarting(&self) {
        let mut restart = self.inner.restart.subscribe();
        let _ = restart.wait_for(|restart| *restart).await;
    }

    /// Whether a change asked for alchemyd to restart.
    pub fn restart_requested(&self) -> bool {
        *self.inner.restart.borrow()
    }

    /// Keep the published privileges current until `shutdown` flips to
    /// `true` or its sender is dropped: at start, whenever the Host
    /// listener or the actions in flight change, and every
    /// [`PrivilegeDeps::interval`].
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut routing = self.inner.state.watch_routing();
        let mut pending = self.inner.pending.subscribe();
        let mut tick = tokio::time::interval(self.inner.deps.interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut host = None;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => return,
                Ok(()) = routing.changed() => {
                    let now = host_listener(&routing.borrow_and_update().proxy.host);
                    if host == Some(now) {
                        continue;
                    }
                    host = Some(now);
                }
                Ok(()) = pending.changed() => {
                    pending.borrow_and_update();
                }
                _ = tick.tick() => {}
            }
            self.refresh().await;
        }
    }

    /// Mark `action` in flight, unless it already is.
    fn start(&self, action: Action) -> bool {
        self.inner.pending.send_if_modified(|p| p.insert(action))
    }

    fn finish(&self, action: Action) {
        self.inner.pending.send_if_modified(|p| p.remove(&action));
    }

    fn last(&self) -> BTreeMap<Action, PrivilegeAttempt> {
        self.inner
            .last
            .lock()
            .map(|last| last.clone())
            .unwrap_or_default()
    }

    fn current(&self, action: Action) -> Privilege {
        let wanted = PrivilegeAction::from(action);
        self.inner
            .state
            .privileges()
            .privileges
            .into_iter()
            .find(|p| p.action == wanted)
            .unwrap_or_else(|| Privilege {
                action: wanted,
                state: WireState::Other,
                reason: None,
                message: None,
                command: None,
                restarts: action.restarts_daemon(),
                last: None,
            })
    }

    /// Run `command`, record how it ended, and restart when the change
    /// needs it.
    async fn attempt(self, action: Action, grant: bool, command: CommandLine) {
        let subcommand = command
            .argv()
            .iter()
            .skip_while(|arg| !arg.ends_with(alchemy_core::privilege::HELPER))
            .nth(1)
            .cloned()
            .unwrap_or_default();
        tracing::info!(%action, grant, program = command.program(), "asking for a privilege");
        let run = self.inner.deps.elevator.run(&command);
        let ended = match tokio::time::timeout(self.inner.deps.prompt_timeout, run).await {
            Err(_) => Ended::Failed("The system prompt timed out.".into()),
            Ok(Err(error)) => Ended::Failed(format!(
                "Alchemy couldn't run {}: {error}",
                command.program()
            )),
            Ok(Ok(output)) => classify(&command, &subcommand, &output),
        };
        match &ended {
            Ended::Done { changed } => tracing::info!(%action, grant, changed, "privilege changed"),
            other => tracing::warn!(%action, grant, ended = ?other, "privilege not changed"),
        }
        if ended == Ended::NoAgent {
            self.inner.no_agent.store(true, Ordering::SeqCst);
        }
        if let Ok(mut last) = self.inner.last.lock() {
            last.insert(action, attempt(grant, &ended, unix_now()));
        }
        if matches!(ended, Ended::Done { .. }) && action.restarts_daemon() {
            self.refresh().await;
            tracing::info!(%action, "restarting to put a privilege in effect");
            self.inner.restart.send_replace(true);
            return;
        }
        self.finish(action);
        self.refresh().await;
    }
}

/// What the facts are decided in, besides what the OS says.
#[derive(Debug, Clone)]
pub struct Context {
    /// The OS.
    pub os: HostOs,
    /// The Host listener.
    pub host: HostListener,
    /// The proxy range the firewall opens.
    pub proxy_ports: ProxyPorts,
    /// Whether pkexec already found no polkit agent.
    pub no_agent: bool,
    /// The actions in flight.
    pub pending: BTreeSet<Action>,
    /// What runs this alchemyd.
    pub supervisor: Supervisor,
    /// The autostart mode last asked for, if any.
    pub requested: Option<AutostartMode>,
}

/// The [`PrivilegeFacts`] from what the OS says and `context`.
///
/// The helper's trust is decided here from its metadata. A polkit agent
/// is assumed wherever pkexec is installed, until pkexec says otherwise:
/// there is no reliable way to ask polkit beforehand. The system service
/// is wanted when autostart was last asked for `boot`, or, before any
/// request, when it is installed.
pub fn privilege_facts(system: SystemFacts, context: &Context) -> PrivilegeFacts {
    let helper = system.helper.map(|reading| HelperFacts {
        trust: helper_trust(&reading.path, &reading.meta),
        path: reading.path,
    });
    PrivilegeFacts {
        os: context.os,
        alchemyd: system.alchemyd,
        uid: system.uid,
        helper,
        polkit_agent: system.pkexec && !context.no_agent,
        proxy_ports: context.proxy_ports,
        low_ports: LowPortFacts {
            host_listener: context.host,
            capable: system.capable,
            nosuid: system.nosuid,
            no_new_privs: system.no_new_privs,
            file_capable: system.file_capable,
            system_unit: context.supervisor == Supervisor::SystemService,
        },
        firewall: system.firewall,
        service: context.requested.map_or(
            ServiceFacts {
                installed: system.service_installed,
                wanted: system.service_installed,
            },
            |mode| mode.service_facts(system.service_installed),
        ),
        pending: context.pending.clone(),
    }
}

/// How the Host listener is doing, from what the routing task reports: a
/// listener that names the low-ports privilege was refused for lack of
/// permission.
pub fn host_listener(listener: &ProxyListener) -> HostListener {
    match listener.state {
        ListenerState::Off => HostListener::Off,
        ListenerState::Listening => HostListener::Listening,
        _ if listener.privilege == Some(PrivilegeAction::LowPorts) => HostListener::Denied,
        _ => HostListener::Failed,
    }
}

/// Every privilege, in display order, with how its last attempt ended.
pub fn privilege_list(
    facts: &PrivilegeFacts,
    last: &BTreeMap<Action, PrivilegeAttempt>,
) -> PrivilegeList {
    PrivilegeList {
        privileges: Action::ALL
            .into_iter()
            .filter_map(|action| {
                let now = states(facts).remove(&action)?;
                Some(privilege(action, &now, last.get(&action).cloned()))
            })
            .collect(),
    }
}

/// One privilege on the wire.
pub fn privilege(
    action: Action,
    now: &PrivilegeState,
    last: Option<PrivilegeAttempt>,
) -> Privilege {
    let (state, reason, message, command) = match now {
        PrivilegeState::NotNeeded => (WireState::NotNeeded, None, None, None),
        PrivilegeState::Needed => (WireState::Needed, None, None, None),
        PrivilegeState::Granted => (WireState::Granted, None, None, None),
        PrivilegeState::NeedsRestart => (WireState::NeedsRestart, None, None, None),
        PrivilegeState::Pending => (WireState::Pending, None, None, None),
        PrivilegeState::NeedsTerminal { command, reason } => (
            WireState::NeedsTerminal,
            Some(terminal_reason(reason)),
            Some(reason.to_string()),
            Some(helper_command(command)),
        ),
        PrivilegeState::Unsupported { reason } => (
            WireState::Unsupported,
            Some(unsupported_reason(reason)),
            Some(reason.to_string()),
            None,
        ),
    };
    Privilege {
        action: action.into(),
        state,
        reason,
        message,
        command,
        restarts: action.restarts_daemon(),
        last,
    }
}

fn terminal_reason(reason: &TerminalReason) -> PrivilegeReason {
    match reason {
        TerminalReason::UntrustedHelper { .. } => PrivilegeReason::UntrustedHelper,
        TerminalReason::NoPolkitAgent => PrivilegeReason::NoPolkitAgent,
        _ => PrivilegeReason::Other,
    }
}

/// The wire reason for `reason`.
pub fn unsupported_reason(reason: &UnsupportedReason) -> PrivilegeReason {
    match reason {
        UnsupportedReason::NoHelper => PrivilegeReason::NoHelper,
        UnsupportedReason::NotHelperSibling { .. } => PrivilegeReason::NotHelperSibling,
        UnsupportedReason::UnmanagedFirewall { .. } => PrivilegeReason::UnmanagedFirewall,
        UnsupportedReason::RunsAsRoot => PrivilegeReason::RunsAsRoot,
        UnsupportedReason::NosuidMount { .. } => PrivilegeReason::NosuidMount,
        UnsupportedReason::NoNewPrivileges => PrivilegeReason::NoNewPrivileges,
        _ => PrivilegeReason::Other,
    }
}

/// `command` on the wire: its argv and its shell line.
pub fn helper_command(command: &CommandLine) -> HelperCommand {
    HelperCommand {
        argv: command.argv().to_vec(),
        line: command.to_string(),
    }
}

/// How a grant or undo that `ended` that way, at `at`, is reported.
pub fn attempt(grant: bool, ended: &Ended, at: i64) -> PrivilegeAttempt {
    let (result, message) = match ended {
        Ended::Done { .. } => (AttemptResult::Done, None),
        Ended::Cancelled => (
            AttemptResult::Cancelled,
            Some("The system prompt was dismissed.".to_owned()),
        ),
        Ended::Denied(why) => (AttemptResult::Denied, Some(why.clone())),
        Ended::NoAgent => (
            AttemptResult::NeedsTerminal,
            Some(TerminalReason::NoPolkitAgent.to_string()),
        ),
        Ended::Failed(why) => (AttemptResult::Failed, Some(why.clone())),
    };
    PrivilegeAttempt {
        grant,
        result,
        message,
        at,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::path::PathBuf;

    use alchemy_api::network::ProxyReport;
    use alchemy_core::privilege::{
        Distrust, FileKind, FileMeta, Firewall, FirewallFacts, HelperPath, HelperTrust,
    };

    use super::*;
    use crate::routing::Routing;

    const HELPER: &str = "/usr/libexec/alchemy/alchemy-helper";
    const ALCHEMYD: &str = "/usr/libexec/alchemy/alchemyd";

    fn listener(state: ListenerState, privilege: Option<PrivilegeAction>) -> ProxyListener {
        ProxyListener {
            port: 80,
            app: None,
            state,
            message: None,
            fix: None,
            privilege,
        }
    }

    fn trusted_meta(helper: &HelperPath) -> BTreeMap<PathBuf, FileMeta> {
        helper
            .trust_paths()
            .into_iter()
            .enumerate()
            .map(|(i, path)| {
                let kind = if i == 0 {
                    FileKind::File
                } else {
                    FileKind::Dir
                };
                (
                    path,
                    FileMeta {
                        kind,
                        uid: 0,
                        mode: 0o755,
                    },
                )
            })
            .collect()
    }

    fn installed() -> SystemFacts {
        let path = HelperPath::new(HELPER).unwrap();
        SystemFacts {
            alchemyd: PathBuf::from(ALCHEMYD),
            uid: 1000,
            helper: Some(HelperReading {
                meta: trusted_meta(&path),
                path,
            }),
            pkexec: true,
            capable: false,
            nosuid: false,
            no_new_privs: false,
            file_capable: false,
            firewall: FirewallFacts {
                active: None,
                opened: false,
            },
            service_installed: false,
        }
    }

    #[test]
    fn host_listener_from_the_routing_report() {
        let cases = [
            (listener(ListenerState::Off, None), HostListener::Off),
            (
                listener(ListenerState::Listening, None),
                HostListener::Listening,
            ),
            (
                listener(ListenerState::Unavailable, Some(PrivilegeAction::LowPorts)),
                HostListener::Denied,
            ),
            (
                listener(ListenerState::Unavailable, None),
                HostListener::Failed,
            ),
            (listener(ListenerState::Other, None), HostListener::Failed),
        ];
        for (report, want) in cases {
            assert_eq!(host_listener(&report), want, "{report:?}");
        }
    }

    #[test]
    fn facts_decide_trust_and_the_polkit_agent() {
        let context = |no_agent| Context {
            os: HostOs::Linux,
            host: HostListener::Denied,
            proxy_ports: ProxyPorts::DEFAULT,
            no_agent,
            pending: BTreeSet::from([Action::Firewall]),
            supervisor: Supervisor::Manual,
            requested: None,
        };
        let facts = privilege_facts(installed(), &context(false));
        assert_eq!(facts.helper.as_ref().unwrap().trust, HelperTrust::Trusted);
        assert!(facts.polkit_agent);
        assert_eq!(facts.low_ports.host_listener, HostListener::Denied);
        assert!(!facts.low_ports.system_unit);
        assert_eq!(facts.pending, BTreeSet::from([Action::Firewall]));
        assert!(!facts.service.wanted);

        let unit = Context {
            supervisor: Supervisor::SystemService,
            ..context(false)
        };
        assert!(privilege_facts(installed(), &unit).low_ports.system_unit);

        let mut service = installed();
        service.service_installed = true;
        let cases = [
            (None, false, false),
            (None, true, true),
            (Some(AutostartMode::Boot), false, true),
            (Some(AutostartMode::Boot), true, true),
            (Some(AutostartMode::Login), true, false),
            (Some(AutostartMode::Off), false, false),
        ];
        for (requested, installed_now, wanted) in cases {
            let mut system = installed();
            system.service_installed = installed_now;
            let context = Context {
                requested,
                ..context(false)
            };
            let facts = privilege_facts(system, &context);
            assert_eq!(
                facts.service,
                ServiceFacts {
                    installed: installed_now,
                    wanted
                },
                "{requested:?} {installed_now}"
            );
        }
        assert!(privilege_facts(service, &context(false)).service.wanted);

        assert!(!privilege_facts(installed(), &context(true)).polkit_agent);
        let mut no_pkexec = installed();
        no_pkexec.pkexec = false;
        assert!(!privilege_facts(no_pkexec, &context(false)).polkit_agent);

        let mut dev = installed();
        if let Some(helper) = dev.helper.as_mut() {
            helper.meta.get_mut(&PathBuf::from(HELPER)).unwrap().uid = 1000;
        }
        assert!(matches!(
            privilege_facts(dev, &context(false)).helper.unwrap().trust,
            HelperTrust::Untrusted {
                why: Distrust::NotRootOwned { uid: 1000, .. }
            }
        ));
    }

    #[test]
    fn wire_privileges() {
        let helper = HelperPath::new(HELPER).unwrap();
        let sudo = alchemy_core::privilege::sudo_command(
            &helper,
            &alchemy_core::privilege::HelperOp::GrantLowPorts,
        );
        let last = PrivilegeAttempt {
            grant: true,
            result: AttemptResult::Cancelled,
            message: None,
            at: 1,
        };
        let terminal = privilege(
            Action::LowPorts,
            &PrivilegeState::NeedsTerminal {
                command: sudo,
                reason: TerminalReason::NoPolkitAgent,
            },
            Some(last.clone()),
        );
        assert_eq!(terminal.action, PrivilegeAction::LowPorts);
        assert_eq!(terminal.state, WireState::NeedsTerminal);
        assert_eq!(terminal.reason, Some(PrivilegeReason::NoPolkitAgent));
        assert_eq!(
            terminal.message.as_deref(),
            Some("No authentication agent is running.")
        );
        assert_eq!(
            terminal.command,
            Some(HelperCommand {
                argv: vec!["sudo".into(), HELPER.into(), "grant-low-ports".into()],
                line: format!("sudo {HELPER} grant-low-ports"),
            })
        );
        assert!(terminal.restarts);
        assert_eq!(terminal.last, Some(last));

        let unsupported = privilege(
            Action::Firewall,
            &PrivilegeState::Unsupported {
                reason: UnsupportedReason::UnmanagedFirewall {
                    proxy_ports: ProxyPorts::DEFAULT,
                },
            },
            None,
        );
        assert_eq!(unsupported.state, WireState::Unsupported);
        assert_eq!(unsupported.reason, Some(PrivilegeReason::UnmanagedFirewall));
        assert!(unsupported.message.unwrap().contains("8100-8999/tcp"));
        assert_eq!(unsupported.command, None);
        assert!(!unsupported.restarts);

        let plain = [
            (PrivilegeState::NotNeeded, WireState::NotNeeded),
            (PrivilegeState::Needed, WireState::Needed),
            (PrivilegeState::Granted, WireState::Granted),
            (PrivilegeState::NeedsRestart, WireState::NeedsRestart),
            (PrivilegeState::Pending, WireState::Pending),
        ];
        for (core, wire) in plain {
            let p = privilege(Action::SystemService, &core, None);
            assert_eq!(
                (p.state, p.reason, p.message, p.command),
                (wire, None, None, None)
            );
        }

        let reasons = [
            (UnsupportedReason::NoHelper, PrivilegeReason::NoHelper),
            (
                UnsupportedReason::NotHelperSibling {
                    alchemyd: PathBuf::from("/a"),
                    sibling: PathBuf::from("/b"),
                },
                PrivilegeReason::NotHelperSibling,
            ),
            (UnsupportedReason::RunsAsRoot, PrivilegeReason::RunsAsRoot),
            (
                UnsupportedReason::NosuidMount {
                    alchemyd: PathBuf::from("/a"),
                },
                PrivilegeReason::NosuidMount,
            ),
            (
                UnsupportedReason::NoNewPrivileges,
                PrivilegeReason::NoNewPrivileges,
            ),
        ];
        for (core, wire) in reasons {
            assert_eq!(unsupported_reason(&core), wire);
        }
    }

    #[test]
    fn attempts() {
        let cases = [
            (Ended::Done { changed: false }, AttemptResult::Done, None),
            (
                Ended::Cancelled,
                AttemptResult::Cancelled,
                Some("The system prompt was dismissed."),
            ),
            (
                Ended::Denied("no".into()),
                AttemptResult::Denied,
                Some("no"),
            ),
            (
                Ended::NoAgent,
                AttemptResult::NeedsTerminal,
                Some("No authentication agent is running."),
            ),
            (Ended::Failed("x".into()), AttemptResult::Failed, Some("x")),
        ];
        for (ended, result, message) in cases {
            let got = attempt(false, &ended, 7);
            assert_eq!(
                got,
                PrivilegeAttempt {
                    grant: false,
                    result,
                    message: message.map(Into::into),
                    at: 7,
                },
                "{ended:?}"
            );
        }
    }

    struct World {
        system: FakePrivilegeSystem,
        elevator: FakeElevator,
        state: State,
        privileges: Privileges,
    }

    fn world(facts: SystemFacts, host: ListenerState) -> World {
        let system = FakePrivilegeSystem::new(facts);
        let elevator = FakeElevator::new();
        let state = State::new();
        let privilege = (host == ListenerState::Unavailable).then_some(PrivilegeAction::LowPorts);
        state.set_routing(Routing {
            proxy: ProxyReport {
                host: listener(host, privilege),
                ..ProxyReport::default()
            },
            ..Routing::default()
        });
        let deps = PrivilegeDeps::new(Arc::new(system.clone()), Arc::new(elevator.clone()))
            .with_os(HostOs::Linux);
        let privileges = Privileges::new(deps, state.clone(), ProxyPorts::DEFAULT);
        World {
            system,
            elevator,
            state,
            privileges,
        }
    }

    impl World {
        fn get(&self, action: Action) -> Privilege {
            self.privileges.current(action)
        }

        async fn settled(&self, action: Action) -> Privilege {
            let mut watch = self.state.watch_privileges();
            let wanted = PrivilegeAction::from(action);
            tokio::time::timeout(
                Duration::from_secs(5),
                watch.wait_for(|list| {
                    list.privileges
                        .iter()
                        .any(|p| p.action == wanted && p.last.is_some())
                }),
            )
            .await
            .unwrap()
            .unwrap();
            self.get(action)
        }
    }

    #[tokio::test]
    async fn a_prompted_low_ports_grant_restarts_alchemyd() {
        let w = world(installed(), ListenerState::Unavailable);
        w.privileges.refresh().await;
        assert_eq!(w.get(Action::LowPorts).state, WireState::Needed);

        w.elevator.hold();
        let change = w.privileges.change(Action::LowPorts, true).await.unwrap();
        assert_eq!(change.outcome, ChangeOutcome::Pending);
        assert_eq!(change.privilege.state, WireState::Pending);
        assert!(matches!(
            w.privileges.change(Action::LowPorts, true).await,
            Err(ChangeError::Busy)
        ));
        assert!(!w.privileges.restart_requested());

        w.elevator.release();
        let after = w.settled(Action::LowPorts).await;
        assert_eq!(after.state, WireState::Pending, "pending until the re-exec");
        assert_eq!(after.last.unwrap().result, AttemptResult::Done);
        tokio::time::timeout(Duration::from_secs(5), w.privileges.restarting())
            .await
            .unwrap();
        let commands = w.elevator.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].argv(),
            [
                "/usr/bin/pkexec",
                "--disable-internal-agent",
                HELPER,
                "grant-low-ports"
            ]
        );
    }

    #[tokio::test]
    async fn a_dismissed_prompt_changes_nothing() {
        let w = world(installed(), ListenerState::Unavailable);
        w.elevator.answer(Ok(ElevatorOutput {
            code: Some(126),
            ..ElevatorOutput::default()
        }));
        let change = w.privileges.change(Action::LowPorts, true).await.unwrap();
        assert_eq!(change.outcome, ChangeOutcome::Pending);
        let after = w.settled(Action::LowPorts).await;
        assert_eq!(after.state, WireState::Needed);
        assert_eq!(after.last.unwrap().result, AttemptResult::Cancelled);
        assert!(!w.privileges.restart_requested());
    }

    #[tokio::test]
    async fn no_polkit_agent_turns_into_the_terminal() {
        let w = world(installed(), ListenerState::Unavailable);
        w.elevator.answer(Ok(ElevatorOutput {
            code: Some(127),
            stdout: String::new(),
            stderr: "Error executing command as another user: No authentication agent found.\n"
                .into(),
        }));
        w.privileges.change(Action::LowPorts, true).await.unwrap();
        let after = w.settled(Action::LowPorts).await;
        assert_eq!(after.state, WireState::NeedsTerminal);
        assert_eq!(after.reason, Some(PrivilegeReason::NoPolkitAgent));
        assert_eq!(
            after.command.unwrap().line,
            format!("sudo {HELPER} grant-low-ports")
        );
        assert_eq!(after.last.unwrap().result, AttemptResult::NeedsTerminal);

        let again = w.privileges.change(Action::LowPorts, true).await.unwrap();
        assert_eq!(again.outcome, ChangeOutcome::NeedsTerminal);
        assert_eq!(w.elevator.commands().len(), 1, "no second prompt");
    }

    #[tokio::test]
    async fn an_untrusted_helper_needs_the_terminal_and_runs_nothing() {
        let mut dev = installed();
        if let Some(helper) = dev.helper.as_mut() {
            helper.meta.get_mut(&PathBuf::from(HELPER)).unwrap().mode = 0o775;
        }
        let w = world(dev, ListenerState::Unavailable);
        let change = w.privileges.change(Action::LowPorts, true).await.unwrap();
        assert_eq!(change.outcome, ChangeOutcome::NeedsTerminal);
        assert_eq!(
            change.command.unwrap().argv,
            ["sudo", HELPER, "grant-low-ports"]
        );
        assert_eq!(
            change.message.as_deref(),
            Some(&*format!(
                "alchemy-helper isn't installed safely: {HELPER} is writable by its group."
            ))
        );
        assert_eq!(change.privilege.state, WireState::NeedsTerminal);
        assert_eq!(w.elevator.commands(), Vec::new());
    }

    #[tokio::test]
    async fn a_grant_from_the_terminal_only_restarts() {
        let mut facts = installed();
        facts.file_capable = true;
        let w = world(facts, ListenerState::Unavailable);
        let change = w.privileges.change(Action::LowPorts, true).await.unwrap();
        assert_eq!(change.outcome, ChangeOutcome::Restarting);
        assert_eq!(change.privilege.state, WireState::Pending);
        assert!(w.privileges.restart_requested());
        assert_eq!(w.elevator.commands(), Vec::new());
    }

    #[tokio::test]
    async fn unsupported_and_unchanged() {
        let mut facts = installed();
        facts.helper = None;
        let w = world(facts, ListenerState::Unavailable);
        let Err(ChangeError::Unsupported(reason)) =
            w.privileges.change(Action::LowPorts, true).await
        else {
            panic!("expected unsupported");
        };
        assert_eq!(reason, UnsupportedReason::NoHelper);

        let w = world(installed(), ListenerState::Listening);
        let change = w.privileges.change(Action::LowPorts, true).await.unwrap();
        assert_eq!(change.outcome, ChangeOutcome::Unchanged);
        assert_eq!(change.privilege.state, WireState::NotNeeded);
        let undo = w.privileges.change(Action::Firewall, false).await.unwrap();
        assert_eq!(undo.outcome, ChangeOutcome::Unchanged);
    }

    #[tokio::test]
    async fn a_firewall_grant_needs_no_restart() {
        let mut facts = installed();
        facts.firewall = FirewallFacts {
            active: Some(Firewall::Ufw),
            opened: false,
        };
        let w = world(facts, ListenerState::Listening);
        w.elevator.hold();
        w.privileges.change(Action::Firewall, true).await.unwrap();
        w.system.update(|f| f.firewall.opened = true);
        w.elevator.release();
        let after = w.settled(Action::Firewall).await;
        assert_eq!(after.state, WireState::Granted);
        assert_eq!(after.last.unwrap().result, AttemptResult::Done);
        assert!(!w.privileges.restart_requested());
        assert_eq!(
            w.elevator.commands()[0].args(),
            [
                "--disable-internal-agent",
                HELPER,
                "open-firewall",
                "--proxy-ports",
                "8100-8999"
            ]
        );
    }

    #[tokio::test]
    async fn the_task_follows_the_host_listener() {
        let w = world(installed(), ListenerState::Listening);
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(w.privileges.clone().run(stop_rx));
        let mut list = w.state.watch_privileges();
        let state = |list: &PrivilegeList| {
            list.privileges
                .iter()
                .find(|p| p.action == PrivilegeAction::LowPorts)
                .map(|p| p.state)
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            list.wait_for(|l| state(l) == Some(WireState::NotNeeded)),
        )
        .await
        .unwrap()
        .unwrap();
        w.state.set_routing(Routing {
            proxy: ProxyReport {
                host: listener(ListenerState::Unavailable, Some(PrivilegeAction::LowPorts)),
                ..ProxyReport::default()
            },
            ..Routing::default()
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            list.wait_for(|l| state(l) == Some(WireState::Needed)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            w.state
                .privileges()
                .privileges
                .iter()
                .map(|p| p.action)
                .collect::<Vec<_>>(),
            [
                PrivilegeAction::LowPorts,
                PrivilegeAction::Firewall,
                PrivilegeAction::SystemService
            ]
        );
        stop.send_replace(true);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_read_keeps_what_was_published() {
        let w = world(installed(), ListenerState::Listening);
        w.privileges.refresh().await;
        let before = w.state.privileges();
        w.system.set(Err(FactsError::CurrentExe("gone".into())));
        w.privileges.refresh().await;
        assert_eq!(w.state.privileges(), before);
        assert!(matches!(
            w.privileges.change(Action::LowPorts, true).await,
            Err(ChangeError::Facts(_))
        ));
    }
}
