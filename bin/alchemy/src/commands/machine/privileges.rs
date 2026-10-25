//! `alchemy machine privileges`: what needs root on this machine, and
//! granting or revoking it through alchemyd.
//!
//! alchemyd decides everything: each action's state, whether a system
//! prompt is safe, and the exact command when it isn't. This command asks
//! and follows. When alchemyd answers `needs_terminal`, it runs that
//! `sudo alchemy-helper ...` command on the terminal and asks again;
//! without a terminal it prints the command and fails. When alchemyd
//! restarts to put a change in effect, it waits for alchemyd to answer
//! again.

use std::io::Write;
use std::time::Duration;

use alchemy_api::ApiEvent;
use alchemy_api::privileges::{
    Action, AttemptResult, ChangeOutcome, HelperCommand, Privilege, PrivilegeAction,
    PrivilegeAttempt, PrivilegeChange, PrivilegeList, PrivilegeRequest, PrivilegeState,
};
use clap::builder::PossibleValue;
use serde::Serialize;

use crate::commands::usage::table;
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::{ClientError, ProtocolError};
use crate::sudo::{SUDO, Sudo};

/// `alchemy machine privileges`, as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "machine privileges";

/// `alchemy machine privileges grant`, for errors.
pub(crate) const GRANT_NAME: &str = "machine privileges grant";

/// `alchemy machine privileges revoke`, for errors.
pub(crate) const REVOKE_NAME: &str = "machine privileges revoke";

/// The route the privileges come from, and under which each changes.
pub(crate) const PATH: &str = "/api/machine/privileges";

/// How long to wait for a system prompt to be answered: alchemyd gives up
/// on one after five minutes.
const PROMPT_WAIT: Duration = Duration::from_secs(330);

/// How long to wait for alchemyd to answer again after it restarts.
const RESTART_WAIT: Duration = Duration::from_secs(60);

/// How often to ask whether alchemyd is back, or the action settled.
const POLL: Duration = Duration::from_millis(250);

/// The most requests one grant or revoke sends: a prompt that finds no
/// polkit agent, then the terminal command, then the restart, with one
/// to spare.
const MAX_ROUNDS: usize = 4;

/// Arguments to `alchemy machine privileges`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// Grant or revoke an action; without one, list them all.
    #[command(subcommand)]
    pub command: Option<Verb>,
}

/// The `alchemy machine privileges` verbs.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Verb {
    /// Grant an action, through one system password prompt.
    ///
    /// Where no prompt is safe or available, as over SSH or with a
    /// development build, alchemyd hands over a `sudo alchemy-helper ...`
    /// command, which this runs in the terminal before asking alchemyd
    /// again. Without a terminal, it prints the command and exits with
    /// status 3. Granting low-ports restarts alchemyd, which this waits
    /// for.
    Grant(ChangeArgs),
    /// Revoke an action, undoing what granting it did.
    ///
    /// It asks for a password the same way granting does.
    Revoke(ChangeArgs),
}

/// Arguments to `alchemy machine privileges grant|revoke`.
#[derive(Debug, Clone, clap::Args)]
pub struct ChangeArgs {
    /// The action.
    pub action: ActionName,
}

/// An action as typed: `low-ports`, `firewall`, or `system-service`, or
/// the snake-case name alchemyd's JSON uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionName(pub Action);

impl clap::ValueEnum for ActionName {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            Self(Action::LowPorts),
            Self(Action::Firewall),
            Self(Action::SystemService),
        ]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        let value = PossibleValue::new(self.0.cli_name()).help(about(self.0));
        Some(if self.0.as_str() == self.0.cli_name() {
            value
        } else {
            value.alias(self.0.as_str())
        })
    }
}

fn about(action: Action) -> &'static str {
    match action {
        Action::LowPorts => "let alchemyd listen on port 80 (Linux)",
        Action::Firewall => "open Alchemy's ports in the host firewall",
        Action::SystemService => "run alchemyd at boot as a system service",
    }
}

/// The command name for `args`, as typed after `alchemy`, for errors.
pub(crate) fn name(args: &Args) -> &'static str {
    match args.command {
        None => NAME,
        Some(Verb::Grant(_)) => GRANT_NAME,
        Some(Verb::Revoke(_)) => REVOKE_NAME,
    }
}

/// Run `alchemy machine privileges`, running any terminal command through
/// `sudo`.
pub(crate) async fn run<D: Daemon, W: Write, S: Sudo>(
    ctx: &mut Context<D, W>,
    sudo: &mut S,
    args: Args,
) -> Result<(), ClientError> {
    match args.command {
        None => {
            let list = ctx.get::<PrivilegeList>(PATH).await?;
            for line in list_lines(&list, ctx.json()) {
                ctx.out().line(line)?;
            }
            Ok(())
        }
        Some(Verb::Grant(args)) => change(ctx, sudo, args.action.0, true).await,
        Some(Verb::Revoke(args)) => change(ctx, sudo, args.action.0, false).await,
    }
}

/// What a grant or revoke did, on the way to the result.
#[derive(Debug, Default)]
struct Progress {
    ran: Option<HelperCommand>,
    prompted: bool,
    restarted: bool,
}

/// Grant (`grant` true) or revoke `action`, following alchemyd's answers
/// until the action settles.
async fn change<D: Daemon, W: Write, S: Sudo>(
    ctx: &mut Context<D, W>,
    sudo: &mut S,
    action: Action,
    grant: bool,
) -> Result<(), ClientError> {
    let path = format!("{PATH}/{}", action.cli_name());
    let request = PrivilegeRequest { grant };
    let mut progress = Progress::default();
    for _ in 0..MAX_ROUNDS {
        let answer: PrivilegeChange = ctx.post(&path, &request).await?;
        let settled = match answer.outcome {
            ChangeOutcome::Unchanged => answer.privilege,
            ChangeOutcome::NeedsTerminal => {
                let command = terminal_command(ctx, sudo, action, grant, &answer, &progress)?;
                if !ctx.json() {
                    ctx.out().progress(running_line(&answer, &command));
                }
                let args = sudo_args(&command)?;
                sudo.run(args).await.map_err(|why| {
                    ClientError::PrivilegeFailed(format!(
                        "`{}` didn't succeed: {why}; nothing changed",
                        command.line
                    ))
                })?;
                progress.ran = Some(command);
                continue;
            }
            ChangeOutcome::Pending => {
                progress.prompted = true;
                if !ctx.json() {
                    ctx.out()
                        .progress("Waiting for the system password prompt to be answered…");
                }
                let settled = settle(ctx, action, Wait::Prompt).await?;
                if needs_terminal_now(&settled) {
                    continue;
                }
                progress.restarted = restarted_after_prompt(&settled);
                settled
            }
            ChangeOutcome::Restarting => {
                progress.restarted = true;
                if !ctx.json() {
                    ctx.out()
                        .progress("Restarting alchemyd to put the change in effect…");
                }
                settle(ctx, action, Wait::Restart).await?
            }
            _ => {
                return Err(ClientError::Unexpected(format!(
                    "alchemyd answered {path} with an outcome this alchemy doesn't know"
                )));
            }
        };
        let result = verdict(action, grant, &settled, &progress)?;
        for line in change_lines(action, grant, result, &settled, &progress, ctx.json()) {
            ctx.out().line(line)?;
        }
        return Ok(());
    }
    Err(ClientError::PrivilegeFailed(format!(
        "{} {} didn't settle after {MAX_ROUNDS} requests to alchemyd",
        verbing(grant),
        action.cli_name()
    )))
}

/// The command to run for a `needs_terminal` answer, or the error when it
/// can't be run here: no terminal, a command already run, or none given.
/// Without a terminal, the command is printed on stdout first.
fn terminal_command<D: Daemon, W: Write, S: Sudo>(
    ctx: &mut Context<D, W>,
    sudo: &S,
    action: Action,
    grant: bool,
    answer: &PrivilegeChange,
    progress: &Progress,
) -> Result<HelperCommand, ClientError> {
    let command = answer
        .command
        .clone()
        .or_else(|| answer.privilege.command.clone())
        .ok_or_else(|| {
            ClientError::Unexpected("alchemyd asked for a terminal but gave no command".into())
        })?;
    if let Some(ran) = &progress.ran {
        return Err(ClientError::PrivilegeFailed(format!(
            "alchemyd still asks for `{}` after `{}` ran; check `alchemy machine privileges`",
            command.line, ran.line
        )));
    }
    if sudo.is_interactive() {
        return Ok(command);
    }
    let shown = if ctx.json() {
        serde_json::to_string(answer).map_err(std::io::Error::other)?
    } else {
        command.line.clone()
    };
    ctx.out().line(shown)?;
    Err(ClientError::NeedsTerminal {
        command: format!(
            "{} {}",
            if grant { GRANT_NAME } else { REVOKE_NAME },
            action.cli_name()
        ),
        why: answer
            .message
            .clone()
            .or_else(|| answer.privilege.message.clone())
            .unwrap_or_else(|| "alchemyd can't show a system prompt here.".to_owned()),
        line: command.line,
    })
}

/// `command`'s arguments after `sudo`. alchemyd only ever hands over a
/// `sudo` command; anything else is refused rather than run.
pub(crate) fn sudo_args(command: &HelperCommand) -> Result<&[String], ClientError> {
    match command.argv.split_first() {
        Some((program, args)) if program == SUDO && !args.is_empty() => Ok(args),
        _ => Err(ClientError::PrivilegeFailed(format!(
            "alchemyd asked to run `{}`, which isn't a {SUDO} command, so it wasn't run",
            command.line
        ))),
    }
}

/// The progress line before running `command`: why there is no prompt,
/// and what runs.
fn running_line(answer: &PrivilegeChange, command: &HelperCommand) -> String {
    match answer.message.as_deref() {
        Some(why) => format!("{why}\nRunning: {}", command.line),
        None => format!("Running: {}", command.line),
    }
}

/// Whether a prompt ended by finding no way to show itself, so the next
/// request gets a terminal command instead.
fn needs_terminal_now(privilege: &Privilege) -> bool {
    privilege.state == PrivilegeState::NeedsTerminal
        && privilege
            .last
            .as_ref()
            .is_some_and(|last| last.result == AttemptResult::NeedsTerminal)
}

/// Whether alchemyd restarted after a prompt: only an action that
/// restarts it can, and a restart forgets how the prompt ended.
fn restarted_after_prompt(privilege: &Privilege) -> bool {
    privilege.restarts && privilege.last.is_none()
}

/// What a wait is for, which sets how long it may take and what giving
/// up means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    Prompt,
    Restart,
}

/// Wait until `action` is no longer `pending`, following `/api/events`
/// while alchemyd sends them, then asking `GET` until it answers again,
/// as after a restart.
async fn settle<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    action: Action,
    wait: Wait,
) -> Result<Privilege, ClientError> {
    let limit = match wait {
        Wait::Prompt => PROMPT_WAIT,
        Wait::Restart => RESTART_WAIT,
    };
    let socket = ctx.socket().display().to_string();
    tokio::time::timeout(limit, settle_now(ctx, action))
        .await
        .map_err(|_| match wait {
            Wait::Prompt => ClientError::PrivilegeFailed(format!(
                "the system prompt for {} wasn't answered within {} minutes",
                action.cli_name(),
                PROMPT_WAIT.as_secs() / 60
            )),
            Wait::Restart => ClientError::DaemonUnreachable(format!(
                "{socket}: alchemyd didn't answer within {} seconds of restarting",
                RESTART_WAIT.as_secs()
            )),
        })?
}

async fn settle_now<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    action: Action,
) -> Result<Privilege, ClientError> {
    if let Ok(mut events) = ctx.events().await {
        while let Ok(Some(event)) = events.next().await {
            let ApiEvent::Privileges(list) = event else {
                continue;
            };
            let privilege = find(list, action)?;
            if privilege.state != PrivilegeState::Pending {
                return Ok(privilege);
            }
        }
    }
    loop {
        match ctx.get::<PrivilegeList>(PATH).await {
            Ok(list) => {
                let privilege = find(list, action)?;
                if privilege.state != PrivilegeState::Pending {
                    return Ok(privilege);
                }
            }
            Err(err) if is_away(&err) => {}
            Err(err) => return Err(err),
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Whether `err` means alchemyd is away for now, as while it restarts.
fn is_away(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::DaemonUnreachable(_)
            | ClientError::ConnectionClosed
            | ClientError::Disconnected
            | ClientError::Protocol(ProtocolError::Io { .. })
    )
}

fn find(list: PrivilegeList, action: Action) -> Result<Privilege, ClientError> {
    let wanted = PrivilegeAction::from(action);
    list.privileges
        .into_iter()
        .find(|p| p.action == wanted)
        .ok_or_else(|| ClientError::Unexpected(format!("alchemyd didn't list {action}")))
}

/// How a grant or revoke came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// It is now granted.
    Granted,
    /// It is now revoked.
    Revoked,
    /// It already was that way, or isn't needed.
    Unchanged,
}

/// Whether `privilege`, settled, is what was asked for, given what the
/// command did on the way: a prompt that didn't succeed, or an action
/// left short of the goal, is an error.
fn verdict(
    action: Action,
    grant: bool,
    privilege: &Privilege,
    progress: &Progress,
) -> Result<Outcome, ClientError> {
    let name = action.cli_name();
    if progress.prompted
        && let Some(last) = &privilege.last
        && last.result != AttemptResult::Done
    {
        return Err(ClientError::PrivilegeFailed(failed_attempt(name, last)));
    }
    let reached = if grant {
        matches!(
            privilege.state,
            PrivilegeState::Granted | PrivilegeState::NotNeeded
        )
    } else {
        !matches!(
            privilege.state,
            PrivilegeState::Granted | PrivilegeState::NeedsRestart | PrivilegeState::Pending
        )
    };
    if !reached {
        let why = privilege
            .message
            .as_deref()
            .map_or_else(String::new, |m| format!(": {m}"));
        return Err(ClientError::PrivilegeFailed(format!(
            "{} {name} didn't take effect (it is {}){why}",
            verbing(grant),
            state_word(privilege.state)
        )));
    }
    let changed = progress.ran.is_some() || progress.prompted || progress.restarted;
    Ok(match (changed, grant) {
        (false, _) => Outcome::Unchanged,
        (true, true) => Outcome::Granted,
        (true, false) => Outcome::Revoked,
    })
}

fn failed_attempt(name: &str, last: &PrivilegeAttempt) -> String {
    let verbing = verbing(last.grant);
    let detail = last
        .message
        .as_deref()
        .map_or_else(String::new, |m| format!(": {m}"));
    match last.result {
        AttemptResult::Cancelled => {
            format!("{verbing} {name} was cancelled at the system prompt; nothing changed")
        }
        AttemptResult::Denied => {
            format!("the system prompt refused {verbing} {name}{detail}; nothing changed")
        }
        AttemptResult::NeedsTerminal => {
            format!("{verbing} {name} needs a terminal{detail}")
        }
        AttemptResult::Failed => format!("{verbing} {name} failed{detail}"),
        _ => format!("{verbing} {name} ended in a way this alchemy doesn't know{detail}"),
    }
}

fn verbing(grant: bool) -> &'static str {
    if grant { "granting" } else { "revoking" }
}

/// The `--json` result of a grant or revoke.
#[derive(Debug, Serialize)]
struct ChangeReport<'a> {
    action: PrivilegeAction,
    grant: bool,
    result: Outcome,
    ran: Option<&'a HelperCommand>,
    restarted: bool,
    privilege: &'a Privilege,
}

/// What a grant or revoke prints on success: one sentence, or with
/// `json` the [`ChangeReport`].
fn change_lines(
    action: Action,
    grant: bool,
    result: Outcome,
    privilege: &Privilege,
    progress: &Progress,
    json: bool,
) -> Vec<String> {
    if json {
        let report = ChangeReport {
            action: action.into(),
            grant,
            result,
            ran: progress.ran.as_ref(),
            restarted: progress.restarted,
            privilege,
        };
        return vec![serde_json::to_string(&report).unwrap_or_default()];
    }
    let name = action.cli_name();
    let restarted = if progress.restarted {
        "; alchemyd restarted to put it in effect"
    } else {
        ""
    };
    let line = match result {
        Outcome::Granted => format!("Granted {name}{restarted}"),
        Outcome::Revoked => format!("Revoked {name}{restarted}"),
        Outcome::Unchanged => match (grant, privilege.state) {
            (true, PrivilegeState::Granted) => format!("{name} is already granted"),
            (_, PrivilegeState::NotNeeded) => format!("{name} isn't needed on this machine"),
            (false, _) => format!("{name} isn't granted"),
            (true, state) => format!("{name} is {}", state_word(state)),
        },
    };
    vec![line]
}

/// What `alchemy machine privileges` prints: a table of actions and their
/// states, then a note for each that needs something, or the response as
/// one JSON object.
pub(crate) fn list_lines(list: &PrivilegeList, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::to_string(list).unwrap_or_default()];
    }
    if list.privileges.is_empty() {
        return vec!["alchemyd reports no privileges".to_owned()];
    }
    let header = ["ACTION", "STATE"].map(str::to_owned).to_vec();
    let rows: Vec<Vec<String>> = std::iter::once(header)
        .chain(list.privileges.iter().map(|p| {
            vec![
                action_name(p.action).to_owned(),
                state_word(p.state).to_owned(),
            ]
        }))
        .collect();
    let mut lines = table(&rows);
    let notes: Vec<String> = list.privileges.iter().flat_map(notes).collect();
    if !notes.is_empty() {
        lines.push(String::new());
        lines.extend(notes);
    }
    lines
}

/// What to say about `privilege` under the table: how to grant it, why it
/// can't be, and how the last attempt failed.
fn notes(privilege: &Privilege) -> Vec<String> {
    let name = action_name(privilege.action);
    let grant = format!("`alchemy machine privileges grant {name}`");
    let message = privilege.message.as_deref();
    let mut notes = match privilege.state {
        PrivilegeState::Needed => vec![format!("{name}: not allowed yet; {grant} allows it")],
        PrivilegeState::NeedsTerminal => {
            let mut lines = vec![format!(
                "{name}: {}",
                message.unwrap_or("alchemyd can't show a system prompt for it.")
            )];
            if let Some(command) = &privilege.command {
                lines.push(format!("  {grant} runs `{}`", command.line));
            }
            lines
        }
        PrivilegeState::NeedsRestart => vec![format!(
            "{name}: allowed, but alchemyd must restart to use it; {grant} restarts it"
        )],
        PrivilegeState::Unsupported => vec![format!(
            "{name}: {}",
            message.unwrap_or("Alchemy can't change it on this machine.")
        )],
        _ => Vec::new(),
    };
    if let Some(last) = &privilege.last
        && last.result != AttemptResult::Done
        && privilege.state != PrivilegeState::Pending
    {
        notes.push(format!("{name}: {}", last_words(last)));
    }
    notes
}

fn last_words(last: &PrivilegeAttempt) -> String {
    let what = if last.grant {
        "the last grant"
    } else {
        "the last revoke"
    };
    let detail = last
        .message
        .as_deref()
        .map_or_else(String::new, |m| format!(": {m}"));
    match last.result {
        AttemptResult::Cancelled => format!("{what} was cancelled"),
        AttemptResult::Denied => format!("{what} was refused{detail}"),
        AttemptResult::NeedsTerminal => format!("{what} needed a terminal{detail}"),
        AttemptResult::Failed => format!("{what} failed{detail}"),
        _ => format!("{what} ended in a way this alchemy doesn't know{detail}"),
    }
}

/// An action's name as typed on the command line.
pub(crate) fn action_name(action: PrivilegeAction) -> &'static str {
    action.action().map_or("unknown", Action::cli_name)
}

/// A state as words, such as `needs terminal`.
pub(crate) fn state_word(state: PrivilegeState) -> &'static str {
    match state {
        PrivilegeState::NotNeeded => "not needed",
        PrivilegeState::Needed => "needed",
        PrivilegeState::Granted => "granted",
        PrivilegeState::NeedsRestart => "needs restart",
        PrivilegeState::Pending => "pending",
        PrivilegeState::NeedsTerminal => "needs terminal",
        PrivilegeState::Unsupported => "unsupported",
        _ => "unknown",
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
    use alchemy_api::privileges::PrivilegeReason;
    use clap::Parser as _;

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::{FakeDaemon, sse};
    use crate::sudo::FakeSudo;
    use crate::{Cli, Command};

    const HELPER: &str = "/home/u/target/debug/alchemy-helper";
    const UNTRUSTED: &str = "alchemy-helper isn't installed safely: /home/u/target/debug/alchemy-helper is owned by UID 1000, not root.";

    fn privilege(action: PrivilegeAction, state: PrivilegeState) -> Privilege {
        Privilege {
            action,
            state,
            reason: None,
            message: None,
            command: None,
            restarts: action == PrivilegeAction::LowPorts,
            last: None,
        }
    }

    fn command(subcommand: &str) -> HelperCommand {
        HelperCommand {
            argv: vec![SUDO.into(), HELPER.into(), subcommand.into()],
            line: format!("{SUDO} {HELPER} {subcommand}"),
        }
    }

    fn terminal(action: PrivilegeAction, subcommand: &str) -> Privilege {
        Privilege {
            reason: Some(PrivilegeReason::UntrustedHelper),
            message: Some(UNTRUSTED.into()),
            command: Some(command(subcommand)),
            ..privilege(action, PrivilegeState::NeedsTerminal)
        }
    }

    fn attempt(grant: bool, result: AttemptResult, message: Option<&str>) -> PrivilegeAttempt {
        PrivilegeAttempt {
            grant,
            result,
            message: message.map(Into::into),
            at: 1_700_000_000,
        }
    }

    fn with_last(privilege: Privilege, last: PrivilegeAttempt) -> Privilege {
        Privilege {
            last: Some(last),
            ..privilege
        }
    }

    fn list_of(changed: &Privilege) -> PrivilegeList {
        let mut privileges = vec![
            privilege(PrivilegeAction::LowPorts, PrivilegeState::NotNeeded),
            privilege(PrivilegeAction::Firewall, PrivilegeState::NotNeeded),
            privilege(PrivilegeAction::SystemService, PrivilegeState::NotNeeded),
        ];
        for p in &mut privileges {
            if p.action == changed.action {
                *p = changed.clone();
            }
        }
        PrivilegeList { privileges }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn event(changed: &Privilege) -> Result<crate::sse::SseEvent, ClientError> {
        Ok(sse(
            "privileges",
            &serde_json::to_string(&list_of(changed)).unwrap(),
        ))
    }

    fn answer(outcome: ChangeOutcome, privilege: Privilege) -> PrivilegeChange {
        let (command, message) = if outcome == ChangeOutcome::NeedsTerminal {
            (privilege.command.clone(), privilege.message.clone())
        } else {
            (None, None)
        };
        PrivilegeChange {
            outcome,
            command,
            message,
            privilege,
        }
    }

    fn post(fake: FakeDaemon, action: &str, status: u16, change: &PrivilegeChange) -> FakeDaemon {
        fake.reply(
            Method::POST,
            &format!("{PATH}/{action}"),
            status,
            serde_json::to_vec(change).unwrap(),
        )
    }

    fn args(verb: &str, action: Action) -> Args {
        let change = ChangeArgs {
            action: ActionName(action),
        };
        Args {
            command: Some(match verb {
                "grant" => Verb::Grant(change),
                _ => Verb::Revoke(change),
            }),
        }
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

    fn parse(args: &[&str]) -> Result<Args, String> {
        let cli = Cli::try_parse_from(args).map_err(|e| e.kind().to_string())?;
        match cli.command {
            Command::Machine(machine) => match machine.command {
                crate::commands::machine::Verb::Privileges(args) => Ok(args),
                other => Err(format!("{other:?}")),
            },
            other => Err(format!("{other:?}")),
        }
    }

    /// A parsed command line: no verb, or a verb and its action.
    type Parsed = Option<(&'static str, Action)>;

    #[test]
    fn the_command_line_takes_either_spelling() {
        let cases: [(&[&str], Result<Parsed, ()>); 8] = [
            (&["alchemy", "machine", "privileges"], Ok(None)),
            (
                &["alchemy", "machine", "privileges", "grant", "low-ports"],
                Ok(Some(("grant", Action::LowPorts))),
            ),
            (
                &["alchemy", "machine", "privileges", "grant", "low_ports"],
                Ok(Some(("grant", Action::LowPorts))),
            ),
            (
                &["alchemy", "machine", "privileges", "revoke", "firewall"],
                Ok(Some(("revoke", Action::Firewall))),
            ),
            (
                &[
                    "alchemy",
                    "machine",
                    "privileges",
                    "revoke",
                    "system_service",
                ],
                Ok(Some(("revoke", Action::SystemService))),
            ),
            (
                &["alchemy", "machine", "privileges", "grant", "docker-group"],
                Err(()),
            ),
            (&["alchemy", "machine", "privileges", "grant"], Err(())),
            (
                &["alchemy", "machine", "privileges", "undo", "firewall"],
                Err(()),
            ),
        ];
        for (argv, want) in cases {
            let got = parse(argv).map(|args| {
                args.command.map(|verb| match verb {
                    Verb::Grant(a) => ("grant", a.action.0),
                    Verb::Revoke(a) => ("revoke", a.action.0),
                })
            });
            assert_eq!(got.map_err(|_| ()), want, "{argv:?}");
        }
    }

    #[test]
    fn names_follow_the_verb() {
        assert_eq!(name(&Args::default()), NAME);
        assert_eq!(name(&args("grant", Action::LowPorts)), GRANT_NAME);
        assert_eq!(name(&args("revoke", Action::LowPorts)), REVOKE_NAME);
    }

    #[test]
    fn only_a_sudo_command_is_run() {
        let make = |argv: &[&str]| HelperCommand {
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            line: argv.join(" "),
        };
        let cases: [(&[&str], Option<&[&str]>); 5] = [
            (
                &["sudo", HELPER, "grant-low-ports"],
                Some(&[HELPER, "grant-low-ports"]),
            ),
            (&["sudo"], None),
            (&[], None),
            (&["/usr/bin/sudo", HELPER, "grant-low-ports"], None),
            (&["sh", "-c", "sudo rm -rf /"], None),
        ];
        for (argv, want) in cases {
            let command = make(argv);
            let got = sudo_args(&command);
            if let Some(want) = want {
                assert_eq!(got.unwrap(), want, "{argv:?}");
            } else {
                let err = got.unwrap_err();
                assert_eq!(err.kind(), "privilege_failed", "{argv:?}");
                assert!(err.to_string().contains("isn't a sudo command"), "{err}");
            }
        }
    }

    #[test]
    fn the_list_is_a_table_then_notes() {
        let list = PrivilegeList {
            privileges: vec![
                terminal(PrivilegeAction::LowPorts, "grant-low-ports"),
                with_last(
                    privilege(PrivilegeAction::Firewall, PrivilegeState::Needed),
                    attempt(true, AttemptResult::Cancelled, None),
                ),
                privilege(PrivilegeAction::SystemService, PrivilegeState::NotNeeded),
                privilege(PrivilegeAction::Other, PrivilegeState::Other),
            ],
        };
        assert_eq!(
            list_lines(&list, false),
            [
                "ACTION          STATE".to_owned(),
                "low-ports       needs terminal".to_owned(),
                "firewall        needed".to_owned(),
                "system-service  not needed".to_owned(),
                "unknown         unknown".to_owned(),
                String::new(),
                format!("low-ports: {UNTRUSTED}"),
                format!(
                    "  `alchemy machine privileges grant low-ports` runs `sudo {HELPER} grant-low-ports`"
                ),
                "firewall: not allowed yet; `alchemy machine privileges grant firewall` allows it"
                    .to_owned(),
                "firewall: the last grant was cancelled".to_owned(),
            ]
        );
    }

    #[test]
    fn notes_cover_every_state_and_failed_attempt() {
        let unsupported = Privilege {
            message: Some("The firewall is raw nftables.".into()),
            ..privilege(PrivilegeAction::Firewall, PrivilegeState::Unsupported)
        };
        let cases: Vec<(Privilege, Vec<&str>)> = vec![
            (
                privilege(PrivilegeAction::LowPorts, PrivilegeState::Granted),
                vec![],
            ),
            (
                privilege(PrivilegeAction::LowPorts, PrivilegeState::NeedsRestart),
                vec![
                    "low-ports: allowed, but alchemyd must restart to use it; `alchemy machine privileges grant low-ports` restarts it",
                ],
            ),
            (unsupported, vec!["firewall: The firewall is raw nftables."]),
            (
                privilege(PrivilegeAction::Firewall, PrivilegeState::Unsupported),
                vec!["firewall: Alchemy can't change it on this machine."],
            ),
            (
                with_last(
                    privilege(PrivilegeAction::Firewall, PrivilegeState::Pending),
                    attempt(true, AttemptResult::Failed, Some("boom")),
                ),
                vec![],
            ),
            (
                with_last(
                    privilege(PrivilegeAction::Firewall, PrivilegeState::Granted),
                    attempt(false, AttemptResult::Denied, Some("wrong password")),
                ),
                vec!["firewall: the last revoke was refused: wrong password"],
            ),
            (
                with_last(
                    privilege(PrivilegeAction::Firewall, PrivilegeState::Granted),
                    attempt(true, AttemptResult::Done, None),
                ),
                vec![],
            ),
            (
                with_last(
                    privilege(PrivilegeAction::Firewall, PrivilegeState::Needed),
                    attempt(
                        true,
                        AttemptResult::Failed,
                        Some("alchemy-helper: no_firewall"),
                    ),
                ),
                vec![
                    "firewall: not allowed yet; `alchemy machine privileges grant firewall` allows it",
                    "firewall: the last grant failed: alchemy-helper: no_firewall",
                ],
            ),
        ];
        for (privilege, want) in cases {
            assert_eq!(notes(&privilege), want, "{privilege:?}");
        }
    }

    #[test]
    fn the_list_as_json_is_the_response() {
        let list = list_of(&terminal(PrivilegeAction::LowPorts, "grant-low-ports"));
        let lines = list_lines(&list, true);
        assert_eq!(lines.len(), 1);
        let back: PrivilegeList = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(back, list);
        assert_eq!(
            list_lines(&PrivilegeList::default(), false),
            ["alchemyd reports no privileges"]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn verdicts_follow_the_goal_and_the_last_attempt() {
        let ran = Progress {
            ran: Some(command("open-firewall")),
            ..Progress::default()
        };
        let prompted = Progress {
            prompted: true,
            ..Progress::default()
        };
        let quiet = Progress::default();
        let fw = |state| privilege(PrivilegeAction::Firewall, state);
        let done = |p, grant, result, message| with_last(p, attempt(grant, result, message));
        let cases: Vec<(bool, Privilege, &Progress, Result<Outcome, &str>)> = vec![
            (
                true,
                fw(PrivilegeState::Granted),
                &quiet,
                Ok(Outcome::Unchanged),
            ),
            (
                true,
                fw(PrivilegeState::NotNeeded),
                &quiet,
                Ok(Outcome::Unchanged),
            ),
            (
                true,
                fw(PrivilegeState::Granted),
                &ran,
                Ok(Outcome::Granted),
            ),
            (
                false,
                fw(PrivilegeState::Needed),
                &ran,
                Ok(Outcome::Revoked),
            ),
            (
                false,
                fw(PrivilegeState::NotNeeded),
                &quiet,
                Ok(Outcome::Unchanged),
            ),
            (
                true,
                done(fw(PrivilegeState::Granted), true, AttemptResult::Done, None),
                &prompted,
                Ok(Outcome::Granted),
            ),
            (
                true,
                done(
                    fw(PrivilegeState::Needed),
                    true,
                    AttemptResult::Cancelled,
                    None,
                ),
                &prompted,
                Err("granting firewall was cancelled at the system prompt; nothing changed"),
            ),
            (
                false,
                done(
                    fw(PrivilegeState::Granted),
                    false,
                    AttemptResult::Denied,
                    None,
                ),
                &prompted,
                Err("the system prompt refused revoking firewall; nothing changed"),
            ),
            (
                true,
                done(
                    fw(PrivilegeState::Needed),
                    true,
                    AttemptResult::Failed,
                    Some("alchemy-helper: no_firewall"),
                ),
                &prompted,
                Err("granting firewall failed: alchemy-helper: no_firewall"),
            ),
            (
                true,
                done(fw(PrivilegeState::Needed), true, AttemptResult::Other, None),
                &prompted,
                Err("granting firewall ended in a way this alchemy doesn't know"),
            ),
            (
                true,
                done(
                    fw(PrivilegeState::Needed),
                    true,
                    AttemptResult::Failed,
                    None,
                ),
                &quiet,
                Err("granting firewall didn't take effect (it is needed)"),
            ),
            (
                true,
                Privilege {
                    message: Some("Run it again.".into()),
                    ..fw(PrivilegeState::NeedsRestart)
                },
                &ran,
                Err("granting firewall didn't take effect (it is needs restart): Run it again."),
            ),
            (
                false,
                fw(PrivilegeState::Granted),
                &ran,
                Err("revoking firewall didn't take effect (it is granted)"),
            ),
        ];
        for (grant, privilege, progress, want) in cases {
            let got = verdict(Action::Firewall, grant, &privilege, progress);
            match want {
                Ok(want) => assert_eq!(got.unwrap(), want, "{privilege:?}"),
                Err(want) => {
                    let err = got.unwrap_err();
                    assert_eq!(err.kind(), "privilege_failed");
                    assert_eq!(err.to_string(), want, "{privilege:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn listing_asks_for_the_privileges() {
        let list = list_of(&privilege(
            PrivilegeAction::Firewall,
            PrivilegeState::Granted,
        ));
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &list);
        let ran = run_on(fake, &mut FakeSudo::no_tty(), false, Args::default()).await;
        ran.result.unwrap();
        assert!(ran.out.contains("firewall        granted"), "{}", ran.out);
        assert_eq!(
            ran.requests,
            ["GET /api/version", "GET /api/machine/privileges"]
        );
    }

    #[tokio::test]
    async fn an_old_daemon_is_too_old() {
        let fake = FakeDaemon::new().version("0.0.1", 1).error(
            Method::POST,
            "/api/machine/privileges/low-ports",
            404,
            "not_found",
            "not found",
        );
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            args("grant", Action::LowPorts),
        )
        .await;
        let err = ran.result.unwrap_err();
        assert_eq!(err.exit_code(), 76, "{err}");
        assert!(
            err.to_string().contains("machine privileges grant"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn refusals_pass_through() {
        let cases = [
            (
                409,
                "conflict",
                "A grant of firewall is already in progress.",
            ),
            (
                422,
                "unsupported",
                "Alchemy can't find alchemy-helper next to alchemyd.",
            ),
        ];
        for (status, code, message) in cases {
            let fake = FakeDaemon::new().version("1.2.3", 1).error(
                Method::POST,
                "/api/machine/privileges/firewall",
                status,
                code,
                message,
            );
            let ran = run_on(
                fake,
                &mut FakeSudo::no_tty(),
                false,
                args("grant", Action::Firewall),
            )
            .await;
            let err = ran.result.unwrap_err();
            assert_eq!(err.exit_code(), 1, "{err}");
            assert_eq!(err.to_string(), format!("alchemyd: {message}"));
            assert_eq!(ran.out, "");
        }
    }

    #[tokio::test]
    async fn nothing_to_do_says_why() {
        let cases = [
            (
                "grant",
                PrivilegeState::Granted,
                "firewall is already granted",
            ),
            (
                "grant",
                PrivilegeState::NotNeeded,
                "firewall isn't needed on this machine",
            ),
            ("revoke", PrivilegeState::Needed, "firewall isn't granted"),
            (
                "revoke",
                PrivilegeState::NotNeeded,
                "firewall isn't needed on this machine",
            ),
        ];
        for (verb, state, want) in cases {
            let fake = post(
                FakeDaemon::new().version("1.2.3", 1),
                "firewall",
                200,
                &answer(
                    ChangeOutcome::Unchanged,
                    privilege(PrivilegeAction::Firewall, state),
                ),
            );
            let ran = run_on(
                fake,
                &mut FakeSudo::no_tty(),
                false,
                args(verb, Action::Firewall),
            )
            .await;
            ran.result.unwrap();
            assert_eq!(ran.out, format!("{want}\n"), "{verb} {state:?}");
            let grant = verb == "grant";
            assert_eq!(ran.bodies, [format!("{{\"grant\":{grant}}}")]);
        }
    }

    #[tokio::test]
    async fn a_terminal_command_runs_through_sudo_then_asks_again() {
        let fw = terminal(PrivilegeAction::Firewall, "open-firewall");
        let fake = FakeDaemon::new().version("1.2.3", 1);
        let fake = post(
            fake,
            "firewall",
            200,
            &answer(ChangeOutcome::NeedsTerminal, fw),
        );
        let fake = post(
            fake,
            "firewall",
            200,
            &answer(
                ChangeOutcome::Unchanged,
                privilege(PrivilegeAction::Firewall, PrivilegeState::Granted),
            ),
        );
        let mut sudo = FakeSudo::tty(|_| Ok(()));
        let ran = run_on(fake, &mut sudo, false, args("grant", Action::Firewall)).await;
        ran.result.unwrap();
        assert_eq!(ran.out, "Granted firewall\n");
        assert_eq!(
            ran.sudo,
            [vec![HELPER.to_owned(), "open-firewall".to_owned()]]
        );
        assert_eq!(
            ran.progress,
            format!("{UNTRUSTED}\nRunning: sudo {HELPER} open-firewall\n")
        );
        assert_eq!(
            ran.requests,
            [
                "GET /api/version",
                "POST /api/machine/privileges/firewall",
                "POST /api/machine/privileges/firewall",
            ]
        );
    }

    #[tokio::test]
    async fn without_a_terminal_the_command_is_printed() {
        for json in [false, true] {
            let low = terminal(PrivilegeAction::LowPorts, "grant-low-ports");
            let change = answer(ChangeOutcome::NeedsTerminal, low);
            let fake = post(
                FakeDaemon::new().version("1.2.3", 1),
                "low-ports",
                200,
                &change,
            );
            let mut sudo = FakeSudo::no_tty();
            let ran = run_on(fake, &mut sudo, json, args("grant", Action::LowPorts)).await;
            let err = ran.result.unwrap_err();
            assert_eq!(err.exit_code(), 3);
            assert_eq!(err.kind(), "needs_terminal");
            assert_eq!(
                err.to_string(),
                format!(
                    "`machine privileges grant low-ports` needs sudo in a terminal: {UNTRUSTED}"
                )
            );
            assert_eq!(
                err.hint().unwrap(),
                format!(
                    "run `sudo {HELPER} grant-low-ports` in a terminal, then run `alchemy machine privileges grant low-ports` again"
                )
            );
            if json {
                let back: PrivilegeChange = serde_json::from_str(ran.out.trim_end()).unwrap();
                assert_eq!(back, change);
            } else {
                assert_eq!(ran.out, format!("sudo {HELPER} grant-low-ports\n"));
            }
            assert_eq!(ran.sudo, Vec::<Vec<String>>::new());
        }
    }

    #[tokio::test]
    async fn a_failed_sudo_changes_nothing() {
        let fw = terminal(PrivilegeAction::Firewall, "close-firewall");
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "firewall",
            200,
            &answer(ChangeOutcome::NeedsTerminal, fw),
        );
        let mut sudo = FakeSudo::tty(|_| Err("sudo exited with exit status: 1".into()));
        let ran = run_on(fake, &mut sudo, false, args("revoke", Action::Firewall)).await;
        let err = ran.result.unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            err.to_string(),
            format!(
                "`sudo {HELPER} close-firewall` didn't succeed: sudo exited with exit status: 1; nothing changed"
            )
        );
        assert_eq!(ran.requests.len(), 2);
    }

    #[tokio::test]
    async fn asking_for_the_terminal_twice_stops() {
        let fw = terminal(PrivilegeAction::Firewall, "open-firewall");
        let fake = FakeDaemon::new().version("1.2.3", 1);
        let fake = post(
            fake,
            "firewall",
            200,
            &answer(ChangeOutcome::NeedsTerminal, fw.clone()),
        );
        let fake = post(
            fake,
            "firewall",
            200,
            &answer(ChangeOutcome::NeedsTerminal, fw),
        );
        let mut sudo = FakeSudo::tty(|_| Ok(()));
        let ran = run_on(fake, &mut sudo, false, args("grant", Action::Firewall)).await;
        let err = ran.result.unwrap_err();
        assert_eq!(err.kind(), "privilege_failed");
        assert!(err.to_string().contains("still asks for"), "{err}");
        assert_eq!(ran.sudo.len(), 1);
    }

    #[tokio::test]
    async fn a_command_that_isnt_sudo_is_not_run() {
        let mut fw = terminal(PrivilegeAction::Firewall, "open-firewall");
        fw.command = Some(HelperCommand {
            argv: vec!["sh".into(), "-c".into(), "id".into()],
            line: "sh -c id".into(),
        });
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "firewall",
            200,
            &answer(ChangeOutcome::NeedsTerminal, fw),
        );
        let mut sudo = FakeSudo::tty(|_| Ok(()));
        let ran = run_on(fake, &mut sudo, false, args("grant", Action::Firewall)).await;
        assert_eq!(ran.result.unwrap_err().kind(), "privilege_failed");
        assert_eq!(ran.sudo, Vec::<Vec<String>>::new());
    }

    #[tokio::test]
    async fn a_prompt_is_followed_on_the_event_stream() {
        let pending = privilege(PrivilegeAction::Firewall, PrivilegeState::Pending);
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "firewall",
            202,
            &answer(ChangeOutcome::Pending, pending.clone()),
        )
        .events([
            event(&pending),
            Ok(sse("future", "{}")),
            event(&with_last(
                privilege(PrivilegeAction::Firewall, PrivilegeState::Granted),
                attempt(true, AttemptResult::Done, None),
            )),
        ]);
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            args("grant", Action::Firewall),
        )
        .await;
        ran.result.unwrap();
        assert_eq!(ran.out, "Granted firewall\n");
        assert_eq!(
            ran.progress,
            "Waiting for the system password prompt to be answered…\n"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_prompt_fails_after_polling_when_the_stream_ends() {
        let pending = privilege(PrivilegeAction::Firewall, PrivilegeState::Pending);
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "firewall",
            202,
            &answer(ChangeOutcome::Pending, pending.clone()),
        )
        .events([event(&pending)])
        .ok(Method::GET, PATH, &list_of(&pending))
        .ok(
            Method::GET,
            PATH,
            &list_of(&with_last(
                privilege(PrivilegeAction::Firewall, PrivilegeState::Needed),
                attempt(true, AttemptResult::Cancelled, None),
            )),
        );
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            true,
            args("grant", Action::Firewall),
        )
        .await;
        let err = ran.result.unwrap_err();
        assert_eq!(
            err.to_string(),
            "granting firewall was cancelled at the system prompt; nothing changed"
        );
        assert_eq!(ran.out, "");
        assert_eq!(ran.progress, "", "no progress with --json");
    }

    #[tokio::test]
    async fn a_prompt_with_no_agent_falls_back_to_the_terminal() {
        let pending = privilege(PrivilegeAction::Firewall, PrivilegeState::Pending);
        let no_agent = with_last(
            Privilege {
                reason: Some(PrivilegeReason::NoPolkitAgent),
                ..terminal(PrivilegeAction::Firewall, "open-firewall")
            },
            attempt(
                true,
                AttemptResult::NeedsTerminal,
                Some("No authentication agent found."),
            ),
        );
        let fake = FakeDaemon::new().version("1.2.3", 1);
        let fake = post(
            fake,
            "firewall",
            202,
            &answer(ChangeOutcome::Pending, pending),
        )
        .events([event(&no_agent)]);
        let fake = post(
            fake,
            "firewall",
            200,
            &answer(ChangeOutcome::NeedsTerminal, no_agent),
        );
        let fake = post(
            fake,
            "firewall",
            200,
            &answer(
                ChangeOutcome::Unchanged,
                privilege(PrivilegeAction::Firewall, PrivilegeState::Granted),
            ),
        );
        let mut sudo = FakeSudo::tty(|_| Ok(()));
        let ran = run_on(fake, &mut sudo, false, args("grant", Action::Firewall)).await;
        ran.result.unwrap();
        assert_eq!(ran.out, "Granted firewall\n");
        assert_eq!(ran.sudo.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn low_ports_wait_for_alchemyd_to_restart() {
        let low = terminal(PrivilegeAction::LowPorts, "grant-low-ports");
        let pending = privilege(PrivilegeAction::LowPorts, PrivilegeState::Pending);
        let granted = privilege(PrivilegeAction::LowPorts, PrivilegeState::Granted);
        let fake = FakeDaemon::new().version("1.2.3", 1);
        let fake = post(
            fake,
            "low-ports",
            200,
            &answer(ChangeOutcome::NeedsTerminal, low),
        );
        let fake = post(
            fake,
            "low-ports",
            202,
            &answer(ChangeOutcome::Restarting, pending.clone()),
        )
        .fail_events(ClientError::DaemonUnreachable(
            "/run/alchemyd.sock: refused".into(),
        ))
        .fail(
            Method::GET,
            PATH,
            ClientError::DaemonUnreachable("/run/alchemyd.sock: no such file".into()),
        )
        .fail(Method::GET, PATH, ClientError::ConnectionClosed)
        .ok(Method::GET, PATH, &list_of(&pending))
        .ok(Method::GET, PATH, &list_of(&granted));
        let mut sudo = FakeSudo::tty(|_| Ok(()));
        let ran = run_on(fake, &mut sudo, true, args("grant", Action::LowPorts)).await;
        ran.result.unwrap();
        let report: serde_json::Value = serde_json::from_str(ran.out.trim_end()).unwrap();
        assert_eq!(
            report,
            serde_json::json!({
                "action": "low_ports",
                "grant": true,
                "result": "granted",
                "ran": command("grant-low-ports"),
                "restarted": true,
                "privilege": granted,
            })
        );
        assert_eq!(
            ran.sudo,
            [vec![HELPER.to_owned(), "grant-low-ports".to_owned()]]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn restart_text_says_alchemyd_restarted() {
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "low-ports",
            202,
            &answer(
                ChangeOutcome::Restarting,
                privilege(PrivilegeAction::LowPorts, PrivilegeState::Pending),
            ),
        )
        .events([])
        .ok(
            Method::GET,
            PATH,
            &list_of(&privilege(
                PrivilegeAction::LowPorts,
                PrivilegeState::Needed,
            )),
        );
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            args("revoke", Action::LowPorts),
        )
        .await;
        ran.result.unwrap();
        assert_eq!(
            ran.out,
            "Revoked low-ports; alchemyd restarted to put it in effect\n"
        );
        assert_eq!(
            ran.progress,
            "Restarting alchemyd to put the change in effect…\n"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompted_low_ports_grant_follows_the_restart() {
        let pending = privilege(PrivilegeAction::LowPorts, PrivilegeState::Pending);
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "low-ports",
            202,
            &answer(ChangeOutcome::Pending, pending.clone()),
        )
        .events([event(&with_last(
            pending.clone(),
            attempt(true, AttemptResult::Done, None),
        ))])
        .fail(
            Method::GET,
            PATH,
            ClientError::DaemonUnreachable("gone".into()),
        )
        .ok(
            Method::GET,
            PATH,
            &list_of(&privilege(
                PrivilegeAction::LowPorts,
                PrivilegeState::Granted,
            )),
        );
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            args("grant", Action::LowPorts),
        )
        .await;
        ran.result.unwrap();
        assert_eq!(
            ran.out,
            "Granted low-ports; alchemyd restarted to put it in effect\n"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_that_never_answers_gives_up() {
        let mut fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "low-ports",
            202,
            &answer(
                ChangeOutcome::Restarting,
                privilege(PrivilegeAction::LowPorts, PrivilegeState::Pending),
            ),
        )
        .fail_events(ClientError::DaemonUnreachable("refused".into()));
        let polls = RESTART_WAIT.as_millis() / POLL.as_millis() + 10;
        for _ in 0..polls {
            fake = fake.fail(
                Method::GET,
                PATH,
                ClientError::DaemonUnreachable("refused".into()),
            );
        }
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            args("grant", Action::LowPorts),
        )
        .await;
        let err = ran.result.unwrap_err();
        assert_eq!(err.exit_code(), 69, "{err}");
        assert!(
            err.to_string().contains("within 60 seconds of restarting"),
            "{err}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn other_errors_while_waiting_stop_the_wait() {
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "low-ports",
            202,
            &answer(
                ChangeOutcome::Restarting,
                privilege(PrivilegeAction::LowPorts, PrivilegeState::Pending),
            ),
        )
        .events([])
        .error(
            Method::GET,
            PATH,
            500,
            "internal",
            "cannot read the privileges",
        );
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            args("grant", Action::LowPorts),
        )
        .await;
        let err = ran.result.unwrap_err();
        assert_eq!(err.to_string(), "alchemyd: cannot read the privileges");
    }

    #[tokio::test]
    async fn an_unknown_outcome_is_unexpected() {
        let fake = post(
            FakeDaemon::new().version("1.2.3", 1),
            "firewall",
            200,
            &answer(
                ChangeOutcome::Other,
                privilege(PrivilegeAction::Firewall, PrivilegeState::Granted),
            ),
        );
        let ran = run_on(
            fake,
            &mut FakeSudo::no_tty(),
            false,
            args("grant", Action::Firewall),
        )
        .await;
        assert_eq!(ran.result.unwrap_err().kind(), "unexpected_response");
    }
}
