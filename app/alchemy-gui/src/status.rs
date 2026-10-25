//! What the tray shows, as plain data: [`Status`] collects the shell's
//! launch state and what alchemyd's event stream says, and
//! [`Status::menu`] turns it into a [`MenuModel`].
//!
//! Every item is a menu entry, since Linux tray icons send no click
//! events (#112).

use alchemy_api::apps::StackStatus;
use alchemy_api::autostart::{Autostart, AutostartMode};
use alchemy_api::reconcile::{ReconcileOutcome, ReconcileStatus};
use alchemy_api::sse::SseEvent;
use alchemy_api::{AppList, EngineFlavor, EngineState, EngineStatus};

use crate::launch::Launch;

/// The longest notice the tray shows before cutting it short.
pub const NOTICE_MAX: usize = 120;

/// An event from `/api/events` that the tray uses. The rest are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonEvent {
    /// The `engine` event.
    Engine(EngineStatus),
    /// The `apps` event, reduced to how many apps run.
    Apps {
        /// Apps with at least one service running.
        running: usize,
    },
    /// The `autostart` event.
    Autostart(Autostart),
    /// The `reconcile` event, reduced to how many apps didn't come back.
    Reconcile {
        /// Apps whose start failed.
        failed: usize,
        /// Why they were being started, such as "was running before the
        /// computer slept".
        reason: String,
    },
}

/// The tray's event from a raw SSE event, or `None` for one it doesn't
/// use or can't read.
pub fn decode(raw: &SseEvent) -> Option<DaemonEvent> {
    let data = raw.data.as_bytes();
    match raw.event.as_str() {
        "engine" => serde_json::from_slice(data).ok().map(DaemonEvent::Engine),
        "apps" => serde_json::from_slice::<AppList>(data)
            .ok()
            .map(|list| DaemonEvent::Apps {
                running: list
                    .apps
                    .iter()
                    .filter(|app| matches!(app.status, StackStatus::Running | StackStatus::Partial))
                    .count(),
            }),
        "autostart" => serde_json::from_slice(data)
            .ok()
            .map(DaemonEvent::Autostart),
        "reconcile" => serde_json::from_slice::<ReconcileStatus>(data)
            .ok()
            .map(|status| match status.last {
                Some(last) => DaemonEvent::Reconcile {
                    failed: last
                        .apps
                        .iter()
                        .filter(|app| app.outcome == ReconcileOutcome::Failed)
                        .count(),
                    reason: last.reason,
                },
                None => DaemonEvent::Reconcile {
                    failed: 0,
                    reason: String::new(),
                },
            }),
        _ => None,
    }
}

/// An action the tray started and hasn't heard back about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// Asking alchemyd to start Docker Desktop.
    StartingDocker,
    /// Switching Start at login.
    SwitchingLogin,
}

/// Everything the tray knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// How the shell stands with alchemyd.
    pub launch: Launch,
    /// The last `engine` event while connected.
    pub engine: Option<EngineStatus>,
    /// How many apps run, from the last `apps` event.
    pub apps_running: Option<usize>,
    /// The last `autostart` event.
    pub autostart: Option<Autostart>,
    /// Whether the event stream refused the token.
    pub signed_out: bool,
    /// What the last tray action said, when it needs saying.
    pub notice: Option<String>,
    /// Apps that didn't come back, from the last reconcile.
    pub reconcile_notice: Option<String>,
    /// An action in flight.
    pub busy: Option<Busy>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            launch: Launch::Checking,
            engine: None,
            apps_running: None,
            autostart: None,
            signed_out: false,
            notice: None,
            reconcile_notice: None,
            busy: None,
        }
    }
}

impl Status {
    /// Move to `launch`. Leaving `Running` forgets what the stream said,
    /// since none of it holds while alchemyd is away.
    pub fn set_launch(&mut self, launch: Launch) {
        if !launch.is_running() {
            self.engine = None;
            self.apps_running = None;
            self.autostart = None;
            self.signed_out = false;
            self.reconcile_notice = None;
        }
        self.launch = launch;
    }

    /// Take in one event from the stream.
    pub fn apply(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::Engine(engine) => {
                if matches!(engine.state, EngineState::Starting | EngineState::Ready) {
                    self.busy = self.busy.filter(|b| *b != Busy::StartingDocker);
                }
                self.engine = Some(engine);
            }
            DaemonEvent::Apps { running } => self.apps_running = Some(running),
            DaemonEvent::Autostart(autostart) => self.autostart = Some(autostart),
            DaemonEvent::Reconcile { failed, reason } => {
                self.reconcile_notice = (failed > 0).then(|| {
                    let apps = if failed == 1 { "1 app" } else { "apps" };
                    let count = if failed == 1 {
                        String::new()
                    } else {
                        format!("{failed} ")
                    };
                    let reason = if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" ({reason})")
                    };
                    format!("{count}{apps} didn't come back{reason}. Open Alchemy for details.")
                });
            }
        }
    }

    /// The menu for this status.
    pub fn menu(&self) -> MenuModel {
        let notice = self
            .notice
            .clone()
            .or_else(|| launch_message(&self.launch))
            .or_else(|| self.reconcile_notice.clone())
            .map(|n| shorten(&n, NOTICE_MAX));
        MenuModel {
            status: self.status_line(),
            notice,
            start_daemon: self.launch.may_start(),
            start_docker: self
                .docker_down()
                .then_some(self.busy != Some(Busy::StartingDocker)),
            login: self.login_item(),
        }
    }

    fn status_line(&self) -> String {
        match &self.launch {
            Launch::Checking => "Connecting to alchemyd…".to_owned(),
            Launch::Starting => "Starting Alchemy…".to_owned(),
            Launch::Lost => "alchemyd isn't answering, reconnecting…".to_owned(),
            Launch::Stopped | Launch::Failed { .. } | Launch::Unavailable { .. } => {
                "alchemyd isn't running".to_owned()
            }
            Launch::Running if self.signed_out => "alchemyd running · can't sign in".to_owned(),
            Launch::Running => {
                let mut parts = vec!["alchemyd running".to_owned()];
                if let Some(engine) = &self.engine {
                    parts.push(docker_words(engine.state).to_owned());
                    if engine.state == EngineState::Ready
                        && let Some(n) = self.apps_running
                    {
                        parts.push(match n {
                            1 => "1 app running".to_owned(),
                            n => format!("{n} apps running"),
                        });
                    }
                }
                parts.join(" · ")
            }
        }
    }

    /// Docker Desktop, or a Docker that may be it, isn't running.
    fn docker_down(&self) -> bool {
        self.launch.is_running()
            && self.engine.as_ref().is_some_and(|e| {
                e.state == EngineState::NotRunning
                    && matches!(e.flavor, None | Some(EngineFlavor::DockerDesktop))
            })
    }

    fn login_item(&self) -> LoginItem {
        let Some(autostart) = self.autostart.as_ref().filter(|_| self.launch.is_running()) else {
            return LoginItem {
                checked: false,
                enabled: false,
                note: None,
            };
        };
        let unavailable = autostart
            .unavailable
            .iter()
            .find(|u| u.mode == AutostartMode::Login)
            .map(|u| u.message.clone());
        let switchable = match autostart.mode {
            AutostartMode::Off => {
                unavailable.is_none() && autostart.available_modes.contains(&AutostartMode::Login)
            }
            AutostartMode::Login => autostart.available_modes.contains(&AutostartMode::Off),
            AutostartMode::Boot => false,
        };
        LoginItem {
            checked: autostart.mode != AutostartMode::Off,
            enabled: switchable && autostart.pending.is_none() && self.busy.is_none(),
            note: if autostart.mode == AutostartMode::Off {
                unavailable.map(|m| shorten(&m, NOTICE_MAX))
            } else {
                None
            },
        }
    }
}

/// What the launch state needs said, beyond the status line.
fn launch_message(launch: &Launch) -> Option<String> {
    match launch {
        Launch::Failed { message } | Launch::Unavailable { message } => Some(message.clone()),
        Launch::Stopped => Some("alchemyd stopped and didn't come back.".to_owned()),
        _ => None,
    }
}

fn docker_words(state: EngineState) -> &'static str {
    match state {
        EngineState::Ready => "Docker ready",
        EngineState::Starting => "Docker starting",
        EngineState::NotRunning => "Docker stopped",
        EngineState::NotInstalled => "Docker not installed",
        _ => "Docker unavailable",
    }
}

/// `text` cut to at most `max` characters, ending in `…` when cut.
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The tray menu, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    /// The status line: alchemyd, Docker, apps running.
    pub status: String,
    /// A second, greyed line saying what went wrong, if anything.
    pub notice: Option<String>,
    /// Whether to offer Start Alchemy.
    pub start_daemon: bool,
    /// Whether to offer Start Docker Desktop, and if so whether it is
    /// enabled.
    pub start_docker: Option<bool>,
    /// Start at login.
    pub login: LoginItem,
}

/// The Start at login item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginItem {
    /// Whether alchemyd starts on its own (at login or at boot).
    pub checked: bool,
    /// Whether clicking it switches.
    pub enabled: bool,
    /// Why it can't be turned on, shown under it.
    pub note: Option<String>,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_api::autostart::{LogHint, Supervisor, UnavailableMode};

    use super::*;

    fn engine(state: EngineState, flavor: Option<EngineFlavor>) -> EngineStatus {
        let mut e = EngineStatus::new(state);
        e.flavor = flavor;
        e
    }

    fn autostart(mode: AutostartMode, available: &[AutostartMode]) -> Autostart {
        Autostart {
            mode,
            available_modes: available.to_vec(),
            unavailable: Vec::new(),
            log_hint: LogHint::Terminal,
            supervisor: Supervisor::Manual,
            pending: None,
            last: None,
        }
    }

    fn running() -> Status {
        let mut s = Status::default();
        s.set_launch(Launch::Running);
        s
    }

    #[test]
    fn status_line_table() {
        let ready = Some(engine(
            EngineState::Ready,
            Some(EngineFlavor::DockerDesktop),
        ));
        let stopped = Some(engine(EngineState::NotRunning, None));
        let cases = [
            (Launch::Checking, None, None, "Connecting to alchemyd…"),
            (Launch::Starting, None, None, "Starting Alchemy…"),
            (Launch::Running, None, None, "alchemyd running"),
            (
                Launch::Running,
                ready.clone(),
                Some(3),
                "alchemyd running · Docker ready · 3 apps running",
            ),
            (
                Launch::Running,
                ready.clone(),
                Some(1),
                "alchemyd running · Docker ready · 1 app running",
            ),
            (
                Launch::Running,
                stopped.clone(),
                Some(2),
                "alchemyd running · Docker stopped",
            ),
            (
                Launch::Lost,
                None,
                None,
                "alchemyd isn't answering, reconnecting…",
            ),
            (Launch::Stopped, None, None, "alchemyd isn't running"),
        ];
        for (launch, engine, apps, want) in cases {
            let status = Status {
                launch,
                engine,
                apps_running: apps,
                ..Status::default()
            };
            assert_eq!(status.menu().status, want);
        }
    }

    #[test]
    fn start_docker_only_when_docker_desktop_is_down() {
        let cases = [
            (EngineState::NotRunning, None, Some(true)),
            (
                EngineState::NotRunning,
                Some(EngineFlavor::DockerDesktop),
                Some(true),
            ),
            (
                EngineState::NotRunning,
                Some(EngineFlavor::DockerEngine),
                None,
            ),
            (EngineState::Ready, Some(EngineFlavor::DockerDesktop), None),
            (EngineState::NotInstalled, None, None),
        ];
        for (state, flavor, want) in cases {
            let mut s = running();
            s.apply(DaemonEvent::Engine(engine(state, flavor)));
            assert_eq!(s.menu().start_docker, want, "{state:?} {flavor:?}");
        }
        let mut s = running();
        s.apply(DaemonEvent::Engine(engine(EngineState::NotRunning, None)));
        s.busy = Some(Busy::StartingDocker);
        assert_eq!(s.menu().start_docker, Some(false));
        s.apply(DaemonEvent::Engine(engine(EngineState::Starting, None)));
        assert_eq!(s.busy, None);
        assert_eq!(s.menu().start_docker, None);
    }

    #[test]
    fn login_item_table() {
        use AutostartMode::{Boot, Login, Off};
        let cases = [
            (autostart(Off, &[Off, Login, Boot]), true, false, true),
            (autostart(Login, &[Off, Login, Boot]), true, true, true),
            (autostart(Boot, &[Off, Login, Boot]), true, true, false),
            (autostart(Off, &[Off]), true, false, false),
            (autostart(Off, &[Off, Login]), false, false, false),
        ];
        for (auto, connected, checked, enabled) in cases {
            let mut s = running();
            s.apply(DaemonEvent::Autostart(auto.clone()));
            if !connected {
                s.set_launch(Launch::Lost);
            }
            let item = s.menu().login;
            assert_eq!((item.checked, item.enabled), (checked, enabled), "{auto:?}");
        }
    }

    #[test]
    fn unavailable_login_is_disabled_with_its_reason() {
        let mut auto = autostart(AutostartMode::Off, &[AutostartMode::Off]);
        auto.unavailable = vec![UnavailableMode {
            mode: AutostartMode::Login,
            message: "Install the .deb package.".to_owned(),
        }];
        let mut s = running();
        s.apply(DaemonEvent::Autostart(auto));
        let item = s.menu().login;
        assert!(!item.enabled);
        assert_eq!(item.note.as_deref(), Some("Install the .deb package."));
    }

    #[test]
    fn login_is_disabled_while_switching() {
        let mut s = running();
        s.apply(DaemonEvent::Autostart(autostart(
            AutostartMode::Off,
            &[AutostartMode::Off, AutostartMode::Login],
        )));
        assert!(s.menu().login.enabled);
        s.busy = Some(Busy::SwitchingLogin);
        assert!(!s.menu().login.enabled);
    }

    #[test]
    fn notice_and_start_daemon_follow_the_launch() {
        let cases = [
            (Launch::Running, None, false),
            (
                Launch::Stopped,
                Some("alchemyd stopped and didn't come back."),
                true,
            ),
            (
                Launch::Failed {
                    message: "boom".to_owned(),
                },
                Some("boom"),
                true,
            ),
            (
                Launch::Unavailable {
                    message: "AppImage".to_owned(),
                },
                Some("AppImage"),
                false,
            ),
        ];
        for (launch, notice, start) in cases {
            let mut s = Status::default();
            s.set_launch(launch.clone());
            let menu = s.menu();
            assert_eq!(menu.notice.as_deref(), notice, "{launch:?}");
            assert_eq!(menu.start_daemon, start, "{launch:?}");
        }
    }

    #[test]
    fn leaving_running_forgets_the_stream() {
        let mut s = running();
        s.apply(DaemonEvent::Engine(engine(EngineState::Ready, None)));
        s.apply(DaemonEvent::Apps { running: 2 });
        s.set_launch(Launch::Lost);
        assert_eq!(s.engine, None);
        assert_eq!(s.apps_running, None);
    }

    #[test]
    fn reconcile_failures_become_a_notice() {
        let mut s = running();
        s.apply(DaemonEvent::Reconcile {
            failed: 2,
            reason: "was running before the computer slept".to_owned(),
        });
        assert_eq!(
            s.menu().notice.as_deref(),
            Some(
                "2 apps didn't come back (was running before the computer slept). Open Alchemy for details."
            )
        );
        s.notice = Some("Docker Desktop isn't installed".to_owned());
        assert_eq!(
            s.menu().notice.as_deref(),
            Some("Docker Desktop isn't installed")
        );
        s.notice = None;
        s.apply(DaemonEvent::Reconcile {
            failed: 0,
            reason: String::new(),
        });
        assert_eq!(s.menu().notice, None);
    }

    #[test]
    fn decode_table() {
        let raw = |event: &str, data: &str| SseEvent {
            event: event.to_owned(),
            data: data.to_owned(),
        };
        assert_eq!(
            decode(&raw("engine", r#"{"state":"ready"}"#)),
            Some(DaemonEvent::Engine(EngineStatus::new(EngineState::Ready)))
        );
        assert_eq!(
            decode(&raw("reconcile", r#"{"last":null}"#)),
            Some(DaemonEvent::Reconcile {
                failed: 0,
                reason: String::new()
            })
        );
        assert_eq!(decode(&raw("engine", "not json")), None);
        assert_eq!(decode(&raw("metrics", "{}")), None);
        assert_eq!(decode(&raw("woke", "{}")), None);
    }

    #[test]
    fn shorten_table() {
        let cases = [("abc", 3, "abc"), ("abcd", 3, "ab…"), ("", 3, "")];
        for (text, max, want) in cases {
            assert_eq!(shorten(text, max), want);
        }
    }
}
