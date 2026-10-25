//! `alchemy machine autostart`: whether alchemyd starts on its own, at
//! login or at boot, and switching it.
//!
//! alchemyd owns its services and decides everything; this command asks
//! and follows. `boot` goes through the `system_service` privilege, so
//! switching to or from it follows the same path as `alchemy machine
//! privileges grant`: a system prompt to wait for, or a `sudo` command
//! run on the terminal, or printed without one. Switching from inside the
//! service being removed hands over to the other service, and this
//! alchemyd stops.

use std::io::Write;

use alchemy_api::ApiEvent;
use alchemy_api::autostart::{
    Autostart, AutostartAttempt, AutostartChange, AutostartMode, AutostartOutcome,
    AutostartRequest, LogHint, Supervisor,
};
use alchemy_api::privileges::{AttemptResult, HelperCommand};
use clap::builder::PossibleValue;
use serde::Serialize;

use super::privileges::{Handed, POLL, PROMPT_WAIT, is_away, run_handed};
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;
use crate::sudo::Sudo;

/// `alchemy machine autostart`, as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "machine autostart";

/// The route autostart is read from and switched at.
pub(crate) const PATH: &str = "/api/machine/autostart";

/// The most requests one switch sends: a prompt that finds no polkit
/// agent, then the terminal command, then the request that finishes, with
/// one to spare.
const MAX_ROUNDS: usize = 4;

/// Arguments to `alchemy machine autostart`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The mode to switch to; without one, show the current mode.
    pub mode: Option<ModeName>,
}

/// An autostart mode as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeName(pub AutostartMode);

impl clap::ValueEnum for ModeName {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            Self(AutostartMode::Off),
            Self(AutostartMode::Login),
            Self(AutostartMode::Boot),
        ]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.0.as_str()).help(about(self.0)))
    }
}

fn about(mode: AutostartMode) -> &'static str {
    match mode {
        AutostartMode::Off => "alchemyd starts only when you run it",
        AutostartMode::Login => "alchemyd starts when you log in, as a per-user service",
        AutostartMode::Boot => {
            "alchemyd starts when this computer starts, as a system service; needs your password"
        }
    }
}

/// The command name for `args`, as typed after `alchemy`, for errors.
pub(crate) fn name(args: &Args) -> &'static str {
    match args.mode {
        None => NAME,
        Some(ModeName(AutostartMode::Off)) => "machine autostart off",
        Some(ModeName(AutostartMode::Login)) => "machine autostart login",
        Some(ModeName(AutostartMode::Boot)) => "machine autostart boot",
    }
}

/// Run `alchemy machine autostart`, running any terminal command through
/// `sudo`.
pub(crate) async fn run<D: Daemon, W: Write, S: Sudo>(
    ctx: &mut Context<D, W>,
    sudo: &mut S,
    args: Args,
) -> Result<(), ClientError> {
    let Some(ModeName(mode)) = args.mode else {
        let autostart = ctx.get::<Autostart>(PATH).await?;
        for line in show_lines(&autostart, ctx.json()) {
            ctx.out().line(line)?;
        }
        return Ok(());
    };
    let result = switch(ctx, sudo, mode).await?;
    for line in switch_lines(mode, &result, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// How a switch came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// It is now in the mode asked for.
    Changed,
    /// It already was.
    Unchanged,
    /// The alchemyd that answered stopped, and the other service's
    /// alchemyd, if any, takes over.
    HandedOver,
}

/// What a switch did, for the result.
#[derive(Debug)]
struct Switched {
    outcome: Outcome,
    ran: Option<HelperCommand>,
    autostart: Option<Autostart>,
}

/// Switch to `mode`, following alchemyd's answers until it settles.
async fn switch<D: Daemon, W: Write, S: Sudo>(
    ctx: &mut Context<D, W>,
    sudo: &mut S,
    mode: AutostartMode,
) -> Result<Switched, ClientError> {
    let request = AutostartRequest { mode };
    let mut ran: Option<HelperCommand> = None;
    let mut prompted = false;
    for _ in 0..MAX_ROUNDS {
        let answer: AutostartChange = ctx.put(PATH, &request).await?;
        let supervisor = answer.autostart.supervisor;
        let settled = match answer.outcome {
            AutostartOutcome::Done | AutostartOutcome::Unchanged => answer.autostart,
            AutostartOutcome::NeedsTerminal => {
                let handed = Handed {
                    command: answer.command.clone(),
                    why: answer.message.clone(),
                    retry: format!("{NAME} {mode}"),
                    check: NAME,
                    answer: serde_json::to_string(&answer).map_err(std::io::Error::other)?,
                };
                ran = Some(run_handed(ctx, sudo, handed, ran.as_ref()).await?);
                continue;
            }
            AutostartOutcome::Pending => {
                prompted = true;
                if !ctx.json() {
                    ctx.out()
                        .progress("Waiting for the system password prompt to be answered…");
                }
                let Some(settled) = settle(ctx, mode, supervisor).await? else {
                    return Ok(Switched {
                        outcome: Outcome::HandedOver,
                        ran,
                        autostart: None,
                    });
                };
                if needs_terminal_now(&settled, mode) {
                    continue;
                }
                settled
            }
            AutostartOutcome::Restarting => {
                return Ok(Switched {
                    outcome: Outcome::HandedOver,
                    ran,
                    autostart: Some(answer.autostart),
                });
            }
            _ => {
                return Err(ClientError::Unexpected(format!(
                    "alchemyd answered {PATH} with an outcome this alchemy doesn't know"
                )));
            }
        };
        let changed = answer.outcome == AutostartOutcome::Done || ran.is_some() || prompted;
        let outcome = match verdict(mode, &settled, prompted, changed)? {
            Outcome::Changed if hands_over(supervisor, mode) => Outcome::HandedOver,
            outcome => outcome,
        };
        return Ok(Switched {
            outcome,
            ran,
            autostart: Some(settled),
        });
    }
    Err(ClientError::AutostartFailed(format!(
        "switching autostart to {mode} didn't settle after {MAX_ROUNDS} requests to alchemyd"
    )))
}

/// Whether a prompt for `mode` ended by finding no way to show itself, so
/// the next request gets a terminal command instead.
fn needs_terminal_now(autostart: &Autostart, mode: AutostartMode) -> bool {
    autostart
        .last
        .as_ref()
        .is_some_and(|last| last.mode == mode && last.result == AttemptResult::NeedsTerminal)
}

/// Whether switching to `target` stops the alchemyd that `supervisor`
/// runs, so another takes over or none runs: the per-user service going
/// to `boot`, or the system service leaving it.
fn hands_over(supervisor: Supervisor, target: AutostartMode) -> bool {
    match supervisor {
        Supervisor::UserService => target == AutostartMode::Boot,
        Supervisor::SystemService => target != AutostartMode::Boot,
        Supervisor::Manual => false,
    }
}

/// Wait until no switch is pending, following `/api/events`, then asking
/// `GET` while alchemyd is away. `None` means alchemyd went away during a
/// switch that hands over, so it stopped as it should.
async fn settle<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    mode: AutostartMode,
    supervisor: Supervisor,
) -> Result<Option<Autostart>, ClientError> {
    tokio::time::timeout(PROMPT_WAIT, settle_now(ctx, mode, supervisor))
        .await
        .map_err(|_| {
            ClientError::AutostartFailed(format!(
                "the system prompt for autostart {mode} wasn't answered within {} minutes",
                PROMPT_WAIT.as_secs() / 60
            ))
        })?
}

async fn settle_now<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    mode: AutostartMode,
    supervisor: Supervisor,
) -> Result<Option<Autostart>, ClientError> {
    if let Ok(mut events) = ctx.events().await {
        while let Ok(Some(event)) = events.next().await {
            if let ApiEvent::Autostart(autostart) = event
                && autostart.pending.is_none()
            {
                return Ok(Some(autostart));
            }
        }
    }
    if hands_over(supervisor, mode) {
        return Ok(None);
    }
    loop {
        match ctx.get::<Autostart>(PATH).await {
            Ok(autostart) if autostart.pending.is_none() => return Ok(Some(autostart)),
            Ok(_) => {}
            Err(err) if is_away(&err) => {}
            Err(err) => return Err(err),
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Whether `autostart`, settled, is in `mode`: a prompt that didn't
/// succeed, or a mode left short of the goal, is an error.
fn verdict(
    mode: AutostartMode,
    autostart: &Autostart,
    prompted: bool,
    changed: bool,
) -> Result<Outcome, ClientError> {
    if prompted
        && let Some(last) = &autostart.last
        && last.mode == mode
        && last.result != AttemptResult::Done
    {
        return Err(ClientError::AutostartFailed(failed_attempt(last)));
    }
    if autostart.mode != mode {
        let why = autostart
            .last
            .as_ref()
            .filter(|last| last.mode == mode)
            .and_then(|last| last.message.as_deref())
            .map_or_else(String::new, |m| format!(": {m}"));
        return Err(ClientError::AutostartFailed(format!(
            "autostart is still {}, not {mode}{why}",
            autostart.mode
        )));
    }
    Ok(if changed {
        Outcome::Changed
    } else {
        Outcome::Unchanged
    })
}

fn failed_attempt(last: &AutostartAttempt) -> String {
    let mode = last.mode;
    let detail = last
        .message
        .as_deref()
        .map_or_else(String::new, |m| format!(": {m}"));
    match last.result {
        AttemptResult::Cancelled => {
            format!(
                "switching autostart to {mode} was cancelled at the system prompt; nothing changed"
            )
        }
        AttemptResult::Denied => {
            format!(
                "the system prompt refused switching autostart to {mode}{detail}; nothing changed"
            )
        }
        AttemptResult::NeedsTerminal => {
            format!("switching autostart to {mode} needs a terminal{detail}")
        }
        AttemptResult::Failed => format!("switching autostart to {mode} failed{detail}"),
        _ => format!(
            "switching autostart to {mode} ended in a way this alchemy doesn't know{detail}"
        ),
    }
}

/// The `--json` result of a switch.
#[derive(Debug, Serialize)]
struct SwitchReport<'a> {
    mode: AutostartMode,
    result: Outcome,
    ran: Option<&'a HelperCommand>,
    autostart: Option<&'a Autostart>,
}

/// What a switch to `mode` prints on success: one sentence, or with
/// `json` the [`SwitchReport`].
fn switch_lines(mode: AutostartMode, switched: &Switched, json: bool) -> Vec<String> {
    if json {
        let report = SwitchReport {
            mode,
            result: switched.outcome,
            ran: switched.ran.as_ref(),
            autostart: switched.autostart.as_ref(),
        };
        return vec![serde_json::to_string(&report).unwrap_or_default()];
    }
    let line = match switched.outcome {
        Outcome::Changed => format!("Autostart is now {mode}: {}", about(mode)),
        Outcome::Unchanged => format!("Autostart is already {mode}"),
        Outcome::HandedOver => match mode {
            AutostartMode::Boot => "Autostart is now boot: this alchemyd is handing over to the system service, which starts alchemyd when this computer starts".to_owned(),
            AutostartMode::Login => "Autostart is now login: this alchemyd is handing over to the per-user service, which starts alchemyd when you log in".to_owned(),
            AutostartMode::Off => "Autostart is now off: alchemyd stopped with its service; run `alchemyd` to start it again".to_owned(),
        },
    };
    vec![line]
}

/// What `alchemy machine autostart` prints: the mode, what else it can
/// be, what runs alchemyd, where it logs, and a switch in flight or the
/// last that failed; or the response as one JSON object.
pub(crate) fn show_lines(autostart: &Autostart, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::to_string(autostart).unwrap_or_default()];
    }
    let modes: Vec<&str> = autostart
        .available_modes
        .iter()
        .map(|mode| mode.as_str())
        .collect();
    let mut lines = vec![
        format!("Autostart: {}", mode_text(autostart.mode)),
        format!("Can be: {}", modes.join(", ")),
        format!("Running as: {}", supervisor_text(autostart.supervisor)),
        format!(
            "Logs: {}",
            logs_text(&autostart.log_hint, autostart.supervisor)
        ),
    ];
    lines.extend(
        autostart
            .unavailable
            .iter()
            .map(|u| format!("{} isn't available here: {}", u.mode, u.message)),
    );
    if let Some(pending) = autostart.pending {
        lines.push(format!(
            "Switching to {pending}: waiting for the system password prompt"
        ));
    } else if let Some(last) = &autostart.last
        && last.result != AttemptResult::Done
    {
        lines.push(format!("The last switch: {}", failed_attempt(last)));
    }
    lines
}

/// A mode with what it means, such as `login (alchemyd starts when you
/// log in, ...)`.
pub(crate) fn mode_text(mode: AutostartMode) -> String {
    format!("{mode} ({})", about(mode))
}

fn supervisor_text(supervisor: Supervisor) -> &'static str {
    match supervisor {
        Supervisor::Manual => "started by hand, not by a service",
        Supervisor::UserService => "the per-user service",
        Supervisor::SystemService => "the system service",
    }
}

/// Where the alchemyd that `supervisor` runs logs: the terminal it was
/// started from when nothing supervises it, and `hint` otherwise.
pub(crate) fn logs_text(hint: &LogHint, supervisor: Supervisor) -> String {
    match (supervisor, hint) {
        (Supervisor::Manual, _) | (_, LogHint::Terminal) => {
            "the terminal alchemyd was started from".to_owned()
        }
        (_, LogHint::File { path }) => path.display().to_string(),
        (_, LogHint::Journal { command }) => command.clone(),
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
    use alchemy_api::autostart::UnavailableMode;
    use alchemy_api::sse::SseEvent;
    use clap::Parser as _;

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::{FakeDaemon, sse};
    use crate::sudo::{FakeSudo, SUDO};
    use crate::{Cli, Command};

    const HELPER: &str = "/home/u/target/debug/alchemy-helper";
    const JOURNAL: &str = "journalctl --user -u alchemyd.service";

    fn autostart(mode: AutostartMode, supervisor: Supervisor) -> Autostart {
        Autostart {
            mode,
            available_modes: AutostartMode::ALL.to_vec(),
            log_hint: match mode {
                AutostartMode::Off => LogHint::Terminal,
                AutostartMode::Login => LogHint::Journal {
                    command: JOURNAL.into(),
                },
                AutostartMode::Boot => LogHint::Journal {
                    command: "journalctl -u alchemyd-1000.service".into(),
                },
            },
            supervisor,
            pending: None,
            last: None,
            unavailable: Vec::new(),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn attempt(mode: AutostartMode, result: AttemptResult) -> Option<AutostartAttempt> {
        Some(AutostartAttempt {
            mode,
            result,
            message: None,
            at: 1_700_000_000,
        })
    }

    fn command() -> HelperCommand {
        HelperCommand {
            argv: vec![
                SUDO.into(),
                HELPER.into(),
                "install-service".into(),
                "--uid".into(),
                "1000".into(),
            ],
            line: format!("{SUDO} {HELPER} install-service --uid 1000"),
        }
    }

    fn change(outcome: AutostartOutcome, autostart: Autostart) -> AutostartChange {
        let terminal = outcome == AutostartOutcome::NeedsTerminal;
        AutostartChange {
            outcome,
            command: terminal.then(command),
            message: terminal.then(|| "alchemy-helper isn't installed safely.".to_owned()),
            autostart,
        }
    }

    fn put(fake: FakeDaemon, status: u16, change: &AutostartChange) -> FakeDaemon {
        fake.reply(
            Method::PUT,
            PATH,
            status,
            serde_json::to_vec(change).unwrap(),
        )
    }

    #[allow(clippy::unnecessary_wraps)]
    fn event(autostart: &Autostart) -> Result<SseEvent, ClientError> {
        Ok(sse("autostart", &serde_json::to_string(autostart).unwrap()))
    }

    struct Ran {
        result: Result<(), ClientError>,
        out: String,
        progress: String,
        requests: Vec<String>,
        bodies: Vec<String>,
        sudo: Vec<Vec<String>>,
    }

    async fn run_on(fake: FakeDaemon, sudo: &mut FakeSudo, json: bool, args: Args) -> Ran {
        let mut ctx = Context::buffered(fake, json, name(&args));
        let result = run(&mut ctx, sudo, args).await;
        Ran {
            result,
            out: ctx.written(),
            progress: ctx.progress_written(),
            requests: ctx.daemon().requests().to_vec(),
            bodies: ctx.daemon().bodies().to_vec(),
            sudo: sudo.ran().to_vec(),
        }
    }

    fn daemon() -> FakeDaemon {
        FakeDaemon::new().version("1.2.3", 1)
    }

    fn to(mode: AutostartMode) -> Args {
        Args {
            mode: Some(ModeName(mode)),
        }
    }

    fn parse(args: &[&str]) -> Result<Args, String> {
        let cli = Cli::try_parse_from(args).map_err(|e| e.kind().to_string())?;
        match cli.command {
            Command::Machine(machine) => match machine.command {
                crate::commands::machine::Verb::Autostart(args) => Ok(args),
                other => Err(format!("{other:?}")),
            },
            other => Err(format!("{other:?}")),
        }
    }

    #[test]
    fn the_command_line_takes_a_mode_or_none() {
        type Parsed = Result<Option<AutostartMode>, ()>;
        let cases: [(&[&str], Parsed); 5] = [
            (&["alchemy", "machine", "autostart"], Ok(None)),
            (
                &["alchemy", "machine", "autostart", "off"],
                Ok(Some(AutostartMode::Off)),
            ),
            (
                &["alchemy", "machine", "autostart", "login"],
                Ok(Some(AutostartMode::Login)),
            ),
            (
                &["alchemy", "machine", "autostart", "boot"],
                Ok(Some(AutostartMode::Boot)),
            ),
            (&["alchemy", "machine", "autostart", "sometimes"], Err(())),
        ];
        for (argv, want) in cases {
            let got = parse(argv).map(|args| args.mode.map(|m| m.0));
            assert_eq!(got.map_err(|_| ()), want, "{argv:?}");
        }
        assert_eq!(name(&Args::default()), NAME);
        assert_eq!(name(&to(AutostartMode::Boot)), "machine autostart boot");
    }

    #[test]
    fn show_has_the_mode_what_runs_alchemyd_and_the_logs() {
        let mut now = autostart(AutostartMode::Login, Supervisor::UserService);
        now.available_modes = vec![AutostartMode::Off, AutostartMode::Login];
        now.unavailable = vec![UnavailableMode {
            mode: AutostartMode::Boot,
            message: "Alchemy can't find alchemy-helper next to alchemyd.".into(),
        }];
        now.last = attempt(AutostartMode::Boot, AttemptResult::Cancelled);
        assert_eq!(
            show_lines(&now, false),
            [
                "Autostart: login (alchemyd starts when you log in, as a per-user service)",
                "Can be: off, login",
                "Running as: the per-user service",
                "Logs: journalctl --user -u alchemyd.service",
                "boot isn't available here: Alchemy can't find alchemy-helper next to alchemyd.",
                "The last switch: switching autostart to boot was cancelled at the system prompt; nothing changed",
            ]
        );
        let mut off = autostart(AutostartMode::Off, Supervisor::Manual);
        off.pending = Some(AutostartMode::Boot);
        assert_eq!(
            show_lines(&off, false),
            [
                "Autostart: off (alchemyd starts only when you run it)",
                "Can be: off, login, boot",
                "Running as: started by hand, not by a service",
                "Logs: the terminal alchemyd was started from",
                "Switching to boot: waiting for the system password prompt",
            ]
        );
        let json: Autostart = serde_json::from_str(&show_lines(&now, true)[0]).unwrap();
        assert_eq!(json, now);
    }

    #[test]
    fn logs_follow_what_runs_alchemyd() {
        let terminal = "the terminal alchemyd was started from";
        let mac = "/Users/jo/Library/Logs/Alchemy/alchemyd.log";
        let file = LogHint::File { path: mac.into() };
        let journal = LogHint::Journal {
            command: "journalctl -u alchemyd-1000.service".into(),
        };
        let cases = [
            (Supervisor::UserService, file.clone(), mac),
            (Supervisor::Manual, file, terminal),
            (
                Supervisor::SystemService,
                journal,
                "journalctl -u alchemyd-1000.service",
            ),
            (Supervisor::UserService, LogHint::Terminal, terminal),
        ];
        for (supervisor, hint, want) in cases {
            assert_eq!(logs_text(&hint, supervisor), want, "{supervisor:?}");
        }
    }

    #[test]
    fn only_leaving_a_running_service_hands_over() {
        use AutostartMode::{Boot, Login, Off};
        let cases = [
            (Supervisor::Manual, [false, false, false]),
            (Supervisor::UserService, [false, false, true]),
            (Supervisor::SystemService, [true, true, false]),
        ];
        for (supervisor, want) in cases {
            let got = [Off, Login, Boot].map(|mode| hands_over(supervisor, mode));
            assert_eq!(got, want, "{supervisor:?}");
        }
    }

    #[tokio::test]
    async fn show_reads_autostart() {
        let now = autostart(AutostartMode::Off, Supervisor::Manual);
        let ran = run_on(
            daemon().ok(Method::GET, PATH, &now),
            &mut FakeSudo::no_tty(),
            false,
            Args::default(),
        )
        .await;
        ran.result.unwrap();
        assert!(ran.out.starts_with("Autostart: off "), "{}", ran.out);
        assert_eq!(
            ran.requests,
            ["GET /api/version", "GET /api/machine/autostart"]
        );
    }

    #[tokio::test]
    async fn login_switches_at_once() {
        let login = autostart(AutostartMode::Login, Supervisor::Manual);
        let cases = [
            (
                AutostartOutcome::Done,
                "Autostart is now login: alchemyd starts when you log in, as a per-user service\n",
            ),
            (AutostartOutcome::Unchanged, "Autostart is already login\n"),
        ];
        for (outcome, want) in cases {
            let fake = put(daemon(), 200, &change(outcome, login.clone()));
            let ran = run_on(
                fake,
                &mut FakeSudo::no_tty(),
                false,
                to(AutostartMode::Login),
            )
            .await;
            ran.result.unwrap();
            assert_eq!(ran.out, want);
            assert_eq!(ran.bodies, [r#"{"mode":"login"}"#]);
        }

        let fake = put(daemon(), 200, &change(AutostartOutcome::Done, login));
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            true,
            to(AutostartMode::Login),
        )
        .await;
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(json["mode"], "login");
        assert_eq!(json["result"], "changed");
        assert_eq!(json["ran"], serde_json::Value::Null);
        assert_eq!(json["autostart"]["mode"], "login");
    }

    #[tokio::test]
    async fn boot_waits_for_the_prompt() {
        let login = autostart(AutostartMode::Login, Supervisor::Manual);
        let mut pending = login.clone();
        pending.pending = Some(AutostartMode::Boot);
        let mut booted = autostart(AutostartMode::Boot, Supervisor::Manual);
        booted.last = attempt(AutostartMode::Boot, AttemptResult::Done);
        let fake = put(
            daemon(),
            202,
            &change(AutostartOutcome::Pending, pending.clone()),
        )
        .events([event(&pending), event(&booted)]);
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            to(AutostartMode::Boot),
        )
        .await;
        ran.result.unwrap();
        assert_eq!(
            ran.out,
            "Autostart is now boot: alchemyd starts when this computer starts, as a system service; needs your password\n"
        );
        assert_eq!(
            ran.progress,
            "Waiting for the system password prompt to be answered…\n"
        );
        assert_eq!(
            ran.requests,
            [
                "GET /api/version",
                "PUT /api/machine/autostart",
                "GET /api/events"
            ]
        );
    }

    #[tokio::test]
    async fn a_dismissed_prompt_is_an_error() {
        let login = autostart(AutostartMode::Login, Supervisor::Manual);
        let mut pending = login.clone();
        pending.pending = Some(AutostartMode::Boot);
        let mut cancelled = login;
        cancelled.last = attempt(AutostartMode::Boot, AttemptResult::Cancelled);
        let fake = put(daemon(), 202, &change(AutostartOutcome::Pending, pending))
            .events([event(&cancelled)]);
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            to(AutostartMode::Boot),
        )
        .await;
        let err = ran.result.unwrap_err();
        assert_eq!(err.kind(), "autostart_failed");
        assert_eq!(
            err.to_string(),
            "switching autostart to boot was cancelled at the system prompt; nothing changed"
        );
        assert_eq!(ran.out, "");
    }

    #[tokio::test]
    async fn a_terminal_command_runs_through_sudo_then_asks_again() {
        let off = autostart(AutostartMode::Off, Supervisor::Manual);
        let boot = autostart(AutostartMode::Boot, Supervisor::Manual);
        let fake = put(daemon(), 200, &change(AutostartOutcome::NeedsTerminal, off));
        let fake = put(fake, 200, &change(AutostartOutcome::Unchanged, boot));
        let mut sudo = FakeSudo::tty(|_| Ok(()));
        let ran = run_on(fake, &mut sudo, false, to(AutostartMode::Boot)).await;
        ran.result.unwrap();
        assert!(
            ran.out.starts_with("Autostart is now boot: "),
            "{}",
            ran.out
        );
        assert_eq!(
            ran.sudo,
            [vec![
                HELPER.to_owned(),
                "install-service".into(),
                "--uid".into(),
                "1000".into()
            ]]
        );
        assert_eq!(
            ran.progress,
            format!(
                "alchemy-helper isn't installed safely.\nRunning: {}\n",
                command().line
            )
        );
        assert_eq!(ran.bodies.len(), 2);
    }

    #[tokio::test]
    async fn without_a_terminal_the_command_is_printed() {
        let off = autostart(AutostartMode::Off, Supervisor::Manual);
        for json in [false, true] {
            let answer = change(AutostartOutcome::NeedsTerminal, off.clone());
            let fake = put(daemon(), 200, &answer);
            let ran = run_on(fake, &mut FakeSudo::no_tty(), json, to(AutostartMode::Boot)).await;
            let err = ran.result.unwrap_err();
            assert_eq!(err.kind(), "needs_terminal");
            assert_eq!(err.exit_code(), 3);
            assert_eq!(
                err.hint().unwrap(),
                format!(
                    "run `{}` in a terminal, then run `alchemy machine autostart boot` again",
                    command().line
                )
            );
            if json {
                let printed: AutostartChange = serde_json::from_str(&ran.out).unwrap();
                assert_eq!(printed, answer);
            } else {
                assert_eq!(ran.out, format!("{}\n", command().line));
            }
            assert_eq!(ran.sudo.len(), 0);
        }
    }

    #[tokio::test]
    async fn a_prompt_that_finds_no_agent_falls_back_to_the_terminal() {
        let off = autostart(AutostartMode::Off, Supervisor::Manual);
        let mut pending = off.clone();
        pending.pending = Some(AutostartMode::Boot);
        let mut no_agent = off;
        no_agent.last = attempt(AutostartMode::Boot, AttemptResult::NeedsTerminal);
        let fake = put(daemon(), 202, &change(AutostartOutcome::Pending, pending))
            .events([event(&no_agent)]);
        let fake = put(
            fake,
            200,
            &change(AutostartOutcome::NeedsTerminal, no_agent),
        );
        let fake = put(
            fake,
            200,
            &change(
                AutostartOutcome::Unchanged,
                autostart(AutostartMode::Boot, Supervisor::Manual),
            ),
        );
        let mut sudo = FakeSudo::tty(|_| Ok(()));
        let ran = run_on(fake, &mut sudo, false, to(AutostartMode::Boot)).await;
        ran.result.unwrap();
        assert_eq!(ran.sudo.len(), 1);
        assert!(
            ran.out.starts_with("Autostart is now boot: "),
            "{}",
            ran.out
        );
    }

    #[tokio::test]
    async fn leaving_the_service_that_runs_alchemyd_hands_over() {
        let login = autostart(AutostartMode::Login, Supervisor::UserService);
        let mut pending = login.clone();
        pending.pending = Some(AutostartMode::Boot);
        let gone = put(
            daemon(),
            202,
            &change(AutostartOutcome::Pending, pending.clone()),
        )
        .events([event(&pending), Err(ClientError::Disconnected)]);
        let restarting = put(daemon(), 202, &change(AutostartOutcome::Restarting, login));
        for fake in [gone, restarting] {
            let ran = run_on(
                fake,
                &mut FakeSudo::no_tty(),
                false,
                to(AutostartMode::Boot),
            )
            .await;
            ran.result.unwrap();
            assert_eq!(
                ran.out,
                "Autostart is now boot: this alchemyd is handing over to the system service, which starts alchemyd when this computer starts\n"
            );
        }
    }

    #[tokio::test]
    async fn refusals_come_from_alchemyd() {
        let fake = daemon().error(
            Method::PUT,
            PATH,
            422,
            "unsupported",
            "No systemd --user manager is running for this user.",
        );
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            to(AutostartMode::Login),
        )
        .await;
        let err = ran.result.unwrap_err();
        assert_eq!(
            err.to_string(),
            "alchemyd: No systemd --user manager is running for this user."
        );
    }
}
