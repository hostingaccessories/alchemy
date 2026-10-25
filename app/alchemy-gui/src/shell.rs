//! The shell's side of alchemyd: ensure a daemon at launch, follow its
//! event stream for the tray, notice when it goes, and run the tray's
//! actions through the API.
//!
//! Nothing here touches Tauri. The tray hears each new [`MenuModel`]
//! through [`Shell::on_change`], and the web UI reads [`Shell::launch`].

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use alchemy_api::ErrorResponse;
use alchemy_api::autostart::{
    Autostart, AutostartChange, AutostartMode, AutostartOutcome, AutostartRequest,
};
use alchemy_api::engine::{EngineStart, EngineStartResult};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::client::{ClientError, HttpDaemon, Method, Opened, Reply};
use crate::launch::{self, Installer, Launch, Timing, after_lost};
use crate::status::{Busy, MenuModel, Status, decode};

/// How often to try the event stream again while alchemyd is away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// While a daemon that was running is expected back.
    pub soon: Duration,
    /// Otherwise, in case someone starts alchemyd by other means.
    pub idle: Duration,
    /// How long a daemon that was running may stay away before the shell
    /// offers to start it again.
    pub gone_after: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            soon: Duration::from_secs(1),
            idle: Duration::from_secs(5),
            gone_after: launch::GONE_AFTER,
        }
    }
}

type Listener = Box<dyn Fn(&MenuModel) + Send + Sync>;

/// The shell's link to alchemyd, shared by the tray, the web UI's
/// commands, and the task that runs [`Shell::run`].
pub struct Shell<I> {
    client: HttpDaemon,
    installer: I,
    alchemyd: Result<PathBuf, String>,
    timing: Timing,
    retry: Retry,
    status: Mutex<Status>,
    shown: Mutex<Option<MenuModel>>,
    start: Notify,
    listener: OnceLock<Listener>,
}

impl<I: Installer + Send + Sync + 'static> Shell<I> {
    /// A shell that talks to alchemyd through `client` and, when none
    /// answers, installs `alchemyd` with `installer`.
    pub fn new(
        client: HttpDaemon,
        installer: I,
        alchemyd: Result<PathBuf, String>,
        timing: Timing,
        retry: Retry,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            installer,
            alchemyd,
            timing,
            retry,
            status: Mutex::new(Status::default()),
            shown: Mutex::new(None),
            start: Notify::new(),
            listener: OnceLock::new(),
        })
    }

    /// Call `listener` with the menu now and each time it changes. Only
    /// the first listener set is kept.
    pub fn on_change(&self, listener: impl Fn(&MenuModel) + Send + Sync + 'static) {
        let _ = self.listener.set(Box::new(listener));
        let menu = self.menu();
        *lock(&self.shown) = Some(menu.clone());
        if let Some(listener) = self.listener.get() {
            listener(&menu);
        }
    }

    /// How the shell stands with alchemyd.
    pub fn launch(&self) -> Launch {
        lock(&self.status).launch.clone()
    }

    /// The tray menu as it stands.
    pub fn menu(&self) -> MenuModel {
        lock(&self.status).menu()
    }

    /// Ask [`Shell::run`] to start alchemyd again, if it has stopped or
    /// failed to start. Does nothing otherwise.
    pub fn request_start(&self) {
        if self.launch().may_start() {
            self.start.notify_one();
        }
    }

    /// Ensure a daemon, then follow it for as long as the app runs.
    pub async fn run(self: Arc<Self>) {
        self.ensure().await;
        let mut away_since: Option<Instant> = None;
        loop {
            let delay = match self.follow().await {
                Followed::Ended { connected } => {
                    if connected {
                        away_since = None;
                    }
                    let since = *away_since.get_or_insert_with(Instant::now);
                    self.update(|s| {
                        let next = after_lost(&s.launch, since.elapsed(), self.retry.gone_after);
                        s.set_launch(next);
                    });
                    self.delay()
                }
                Followed::SignedOut => {
                    away_since = None;
                    self.retry.idle
                }
            };
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = self.start.notified() => {
                    if self.launch().may_start() {
                        self.ensure().await;
                        away_since = None;
                    }
                }
            }
        }
    }

    /// Ask alchemyd to start Docker Desktop.
    pub async fn start_docker(&self) {
        self.update(|s| {
            s.busy = Some(Busy::StartingDocker);
            s.notice = None;
        });
        let result = self
            .client
            .request(Method::POST, "/api/engine/start", Some(b"{}"))
            .await;
        let notice = engine_start_notice(&result);
        self.update(|s| {
            s.busy = None;
            s.notice = notice;
        });
    }

    /// Turn Start at login on, or off if alchemyd starts on its own now.
    pub async fn toggle_login(&self) {
        let Some(current) = lock(&self.status).autostart.as_ref().map(|a| a.mode) else {
            return;
        };
        let mode = if current == AutostartMode::Off {
            AutostartMode::Login
        } else {
            AutostartMode::Off
        };
        self.update(|s| {
            s.busy = Some(Busy::SwitchingLogin);
            s.notice = None;
        });
        let body = serde_json::to_vec(&AutostartRequest { mode }).unwrap_or_default();
        let result = self
            .client
            .request(Method::PUT, "/api/machine/autostart", Some(&body))
            .await;
        let (autostart, notice) = autostart_notice(&result);
        self.update(|s| {
            s.busy = None;
            s.notice = notice;
            if let Some(autostart) = autostart {
                s.autostart = Some(autostart);
            }
        });
    }

    async fn ensure(&self) {
        let end = launch::ensure(
            &self.client,
            &self.installer,
            self.alchemyd.clone(),
            self.timing,
            |step| self.update(|s| s.set_launch(step.clone())),
        )
        .await;
        self.update(|s| {
            s.notice = None;
            s.set_launch(end);
        });
    }

    /// Follow `/api/events` until it ends. Marks alchemyd running while
    /// the stream is open.
    async fn follow(&self) -> Followed {
        let mut events = match self.client.events().await {
            Ok(Opened::Stream(events)) => events,
            Ok(Opened::Refused(reply)) if reply.status == 401 => {
                self.update(|s| {
                    s.set_launch(Launch::Running);
                    s.signed_out = true;
                });
                return Followed::SignedOut;
            }
            Ok(Opened::Refused(_)) | Err(_) => return Followed::Ended { connected: false },
        };
        self.update(|s| {
            s.set_launch(Launch::Running);
            s.signed_out = false;
        });
        while let Ok(Some(raw)) = events.next().await {
            if let Some(event) = decode(&raw) {
                self.update(|s| s.apply(event));
            }
        }
        Followed::Ended { connected: true }
    }

    fn delay(&self) -> Duration {
        match self.launch() {
            Launch::Lost => self.retry.soon,
            _ => self.retry.idle,
        }
    }

    fn update(&self, change: impl FnOnce(&mut Status)) {
        let menu = {
            let mut status = lock(&self.status);
            change(&mut status);
            status.menu()
        };
        let mut shown = lock(&self.shown);
        if shown.as_ref() == Some(&menu) {
            return;
        }
        *shown = Some(menu.clone());
        drop(shown);
        if let Some(listener) = self.listener.get() {
            listener(&menu);
        }
    }
}

enum Followed {
    /// The stream ended, or never opened.
    Ended {
        /// Whether it was open first.
        connected: bool,
    },
    /// alchemyd answered but refused the token.
    SignedOut,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What to say after asking alchemyd to start Docker Desktop.
pub fn engine_start_notice(result: &Result<Reply, ClientError>) -> Option<String> {
    let reply = match result {
        Ok(reply) if reply.is_success() => reply,
        Ok(reply) => return Some(failure(reply)),
        Err(e) => return Some(e.to_string()),
    };
    match serde_json::from_slice::<EngineStart>(&reply.body) {
        Ok(EngineStart {
            result: EngineStartResult::Unsupported,
            message,
            command,
        }) => Some(match command {
            Some(command) => format!("{message} Run `{command}` in a terminal."),
            None => message,
        }),
        Ok(_) => None,
        Err(e) => Some(ClientError::Protocol(e.to_string()).to_string()),
    }
}

/// Autostart as it stands after a switch, and what to say about it.
pub fn autostart_notice(
    result: &Result<Reply, ClientError>,
) -> (Option<Autostart>, Option<String>) {
    let reply = match result {
        Ok(reply) if reply.is_success() => reply,
        Ok(reply) => return (None, Some(failure(reply))),
        Err(e) => return (None, Some(e.to_string())),
    };
    match serde_json::from_slice::<AutostartChange>(&reply.body) {
        Ok(change) => {
            let notice = (change.outcome == AutostartOutcome::NeedsTerminal).then(|| {
                let why = change
                    .message
                    .clone()
                    .unwrap_or_else(|| "Alchemy can't ask for your password here.".to_owned());
                match &change.command {
                    Some(command) => format!("{why} Run `{}` in a terminal.", command.line),
                    None => why,
                }
            });
            (Some(change.autostart), notice)
        }
        Err(e) => (None, Some(ClientError::Protocol(e.to_string()).to_string())),
    }
}

/// The daemon's own message from an error reply.
fn failure(reply: &Reply) -> String {
    serde_json::from_slice::<ErrorResponse>(&reply.body).map_or_else(
        |_| format!("alchemyd answered HTTP {}", reply.status),
        |e| e.error,
    )
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

    #[allow(clippy::unnecessary_wraps)]
    fn reply(status: u16, body: &str) -> Result<Reply, ClientError> {
        Ok(Reply {
            status,
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn engine_start_notice_table() {
        let cases = [
            (
                reply(
                    202,
                    r#"{"result":"starting","message":"Starting","command":null}"#,
                ),
                None,
            ),
            (
                reply(
                    200,
                    r#"{"result":"already_running","message":"Up","command":null}"#,
                ),
                None,
            ),
            (
                reply(
                    200,
                    r#"{"result":"unsupported","message":"Alchemy can't start Docker Engine.","command":"sudo systemctl start docker"}"#,
                ),
                Some(
                    "Alchemy can't start Docker Engine. Run `sudo systemctl start docker` in a terminal.",
                ),
            ),
            (
                reply(
                    422,
                    r#"{"error":"Docker Desktop isn't installed","code":"unsupported"}"#,
                ),
                Some("Docker Desktop isn't installed"),
            ),
            (reply(500, "oops"), Some("alchemyd answered HTTP 500")),
            (
                Err(ClientError::TimedOut),
                Some("alchemyd didn't answer in time"),
            ),
        ];
        for (result, want) in cases {
            assert_eq!(engine_start_notice(&result).as_deref(), want, "{result:?}");
        }
    }

    #[test]
    fn autostart_notice_table() {
        let autostart = r#"{"mode":"login","available_modes":["off","login"],"log_hint":{"kind":"terminal"},"supervisor":"manual","pending":null,"last":null}"#;
        let done = format!(
            r#"{{"outcome":"done","command":null,"message":null,"autostart":{autostart}}}"#
        );
        let (got, notice) = autostart_notice(&reply(200, &done));
        assert_eq!(got.unwrap().mode, AutostartMode::Login);
        assert_eq!(notice, None);

        let terminal = format!(
            r#"{{"outcome":"needs_terminal","command":{{"argv":["sudo","x"],"line":"sudo x"}},"message":"No prompt.","autostart":{autostart}}}"#
        );
        let (_, notice) = autostart_notice(&reply(200, &terminal));
        assert_eq!(
            notice.as_deref(),
            Some("No prompt. Run `sudo x` in a terminal.")
        );

        let (got, notice) = autostart_notice(&reply(
            409,
            r#"{"error":"a switch is in flight","code":"conflict"}"#,
        ));
        assert_eq!(got, None);
        assert_eq!(notice.as_deref(), Some("a switch is in flight"));
    }
}
