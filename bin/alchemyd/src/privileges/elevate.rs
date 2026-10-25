//! Running the helper as root behind a system prompt: the [`Elevator`]
//! that spawns `pkexec` or `osascript`, and [`classify`], which reads how
//! it ended.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use alchemy_core::privilege::{CommandLine, HELPER, OSASCRIPT, PKEXEC};
use futures_util::future::{self, BoxFuture};
use serde::Deserialize;

/// Runs an elevated command, such as `pkexec --disable-internal-agent
/// <helper> grant-low-ports`, and reports how it ended.
///
/// Object safe, so the daemon can hold any implementation. The future
/// resolves only once the prompt is answered and the helper is done, and
/// dropping it ends the command.
pub trait Elevator: Send + Sync + fmt::Debug {
    /// Run `command` and collect its output.
    fn run<'a>(&'a self, command: &'a CommandLine) -> BoxFuture<'a, io::Result<ElevatorOutput>>;
}

/// How an elevated command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElevatorOutput {
    /// Its exit code, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// Its standard output.
    pub stdout: String,
    /// Its standard error.
    pub stderr: String,
}

/// How a grant or undo through a prompt ended, from [`classify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The helper ran and succeeded.
    Done {
        /// Whether it changed anything.
        changed: bool,
    },
    /// The user dismissed the prompt.
    Cancelled,
    /// The prompt refused, such as after a wrong password.
    Denied(String),
    /// No polkit agent answered, so the user has to use a terminal.
    NoAgent,
    /// The prompt or the helper failed.
    Failed(String),
}

/// The helper's one-line JSON result.
#[derive(Debug, Deserialize)]
struct ResultLine {
    result: String,
    operation: Option<String>,
    changed: Option<bool>,
    error: Option<String>,
}

/// How `output` of `command`, which ran the helper's `subcommand`, ended.
///
/// Following the #110 spike: `pkexec` exits 126 when its dialog is
/// dismissed and 127 when it can't authorize, with `No authentication
/// agent found.` when there is no agent; otherwise it exits with the
/// helper's code, which is never 126 or 127. `osascript` exits 1 with
/// `(-128)` when the dialog is cancelled; otherwise the fixed script
/// prints the helper's output with stderr folded in, then `exit=N`.
pub fn classify(command: &CommandLine, subcommand: &str, output: &ElevatorOutput) -> Ended {
    match command.program() {
        PKEXEC => match output.code {
            Some(127) if output.stderr.contains("No authentication agent found") => Ended::NoAgent,
            Some(127) => Ended::Denied(last_line(&output.stderr, "The system prompt refused.")),
            Some(126) => Ended::Cancelled,
            Some(_) => helper_result(subcommand, &output.stdout, &output.stderr),
            None => Ended::Failed("The system prompt was ended by a signal.".into()),
        },
        OSASCRIPT => match output.code {
            Some(0) => helper_result(subcommand, &output.stdout, &output.stdout),
            Some(_) if output.stderr.contains("(-128)") => Ended::Cancelled,
            Some(_) => Ended::Failed(last_line(&output.stderr, "The system prompt failed.")),
            None => Ended::Failed("The system prompt was ended by a signal.".into()),
        },
        program => Ended::Failed(format!("Alchemy doesn't know how to read {program}.")),
    }
}

fn helper_result(subcommand: &str, stdout: &str, messages: &str) -> Ended {
    let line = stdout
        .lines()
        .rev()
        .filter(|line| line.trim_start().starts_with('{'))
        .find_map(|line| serde_json::from_str::<ResultLine>(line.trim()).ok());
    let Some(line) = line else {
        return Ended::Failed(format!(
            "{HELPER} gave no result: {}",
            last_line(messages, "no output")
        ));
    };
    if line.operation.as_deref() != Some(subcommand) {
        return Ended::Failed(format!(
            "{HELPER} answered for {} instead of {subcommand}.",
            line.operation.as_deref().unwrap_or("nothing")
        ));
    }
    if let ("ok", Some(changed)) = (line.result.as_str(), line.changed) {
        return Ended::Done { changed };
    }
    let kind = line.error.unwrap_or_else(|| "unknown".into());
    let detail = messages
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("alchemy-helper: "))
        .map_or_else(String::new, |m| format!(": {m}"));
    Ended::Failed(format!("{HELPER} failed ({kind}){detail}"))
}

fn last_line(text: &str, fallback: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(|| fallback.to_owned(), str::to_owned)
}

/// The real [`Elevator`]: spawns the command with no shell, stdin from
/// `/dev/null`, `LC_ALL=C` so pkexec's messages read the same everywhere,
/// a fixed `PATH`, and nothing else from alchemyd's environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemElevator;

impl Elevator for SystemElevator {
    fn run<'a>(&'a self, command: &'a CommandLine) -> BoxFuture<'a, io::Result<ElevatorOutput>> {
        Box::pin(async move {
            let output = tokio::process::Command::new(command.program())
                .args(command.args())
                .env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                .env("LC_ALL", "C")
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await?;
            Ok(ElevatorOutput {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }
}

/// An [`Elevator`] for tests: records each command and answers with the
/// next queued output, or, with none queued, as if the helper succeeded
/// and changed something. It can also be held until released, as if the
/// prompt were still up.
#[derive(Debug, Clone, Default)]
pub struct FakeElevator {
    inner: Arc<FakeInner>,
}

#[derive(Debug)]
struct FakeInner {
    commands: Mutex<Vec<CommandLine>>,
    answers: Mutex<VecDeque<io::Result<ElevatorOutput>>>,
    gate: tokio::sync::Semaphore,
    held: std::sync::atomic::AtomicBool,
}

impl Default for FakeInner {
    fn default() -> Self {
        Self {
            commands: Mutex::default(),
            answers: Mutex::default(),
            gate: tokio::sync::Semaphore::new(0),
            held: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl FakeElevator {
    /// A fake that answers at once.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer the next command with `answer`.
    pub fn answer(&self, answer: io::Result<ElevatorOutput>) {
        if let Ok(mut answers) = self.inner.answers.lock() {
            answers.push_back(answer);
        }
    }

    /// Hold every command until [`Self::release`], as if the prompt were
    /// up.
    pub fn hold(&self) {
        self.inner
            .held
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Let one held command finish.
    pub fn release(&self) {
        self.inner.gate.add_permits(1);
    }

    /// Every command run so far.
    pub fn commands(&self) -> Vec<CommandLine> {
        self.inner
            .commands
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    /// What the helper prints for a successful `subcommand`.
    pub fn ok(subcommand: &str, changed: bool) -> ElevatorOutput {
        ElevatorOutput {
            code: Some(0),
            stdout: format!(
                "{{\"result\":\"ok\",\"operation\":\"{subcommand}\",\"changed\":{changed}}}\n"
            ),
            stderr: String::new(),
        }
    }
}

impl Elevator for FakeElevator {
    fn run<'a>(&'a self, command: &'a CommandLine) -> BoxFuture<'a, io::Result<ElevatorOutput>> {
        if let Ok(mut commands) = self.inner.commands.lock() {
            commands.push(command.clone());
        }
        let queued = self
            .inner
            .answers
            .lock()
            .ok()
            .and_then(|mut answers| answers.pop_front());
        let answer = queued.unwrap_or_else(|| {
            let subcommand = command
                .argv()
                .iter()
                .skip_while(|arg| !arg.ends_with(HELPER))
                .nth(1)
                .cloned()
                .unwrap_or_default();
            Ok(Self::ok(&subcommand, true))
        });
        if !self.inner.held.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(future::ready(answer));
        }
        Box::pin(async move {
            if let Ok(permit) = self.inner.gate.acquire().await {
                permit.forget();
            }
            answer
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
    use alchemy_core::privilege::{Elevation, HelperOp, HelperPath, elevated_command};

    use super::*;

    fn out(code: Option<i32>, stdout: &str, stderr: &str) -> ElevatorOutput {
        ElevatorOutput {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn classifying_pkexec() {
        let helper = HelperPath::new("/usr/libexec/alchemy/alchemy-helper").unwrap();
        let pkexec = elevated_command(&Elevation::Pkexec, &helper, &HelperOp::GrantLowPorts);
        let ok = r#"{"result":"ok","operation":"grant-low-ports","changed":true}"#;
        let cases = [
            (out(Some(0), ok, ""), Ended::Done { changed: true }),
            (
                out(
                    Some(0),
                    r#"{"result":"ok","operation":"grant-low-ports","changed":false}"#,
                    "",
                ),
                Ended::Done { changed: false },
            ),
            (
                out(
                    Some(127),
                    "",
                    "Error executing command as another user: No authentication agent found.\n",
                ),
                Ended::NoAgent,
            ),
            (
                out(
                    Some(127),
                    "",
                    "polkit-agent-helper-1: pam_authenticate failed\nError executing command as another user: Not authorized\n",
                ),
                Ended::Denied("Error executing command as another user: Not authorized".into()),
            ),
            (out(Some(126), "", ""), Ended::Cancelled),
            (
                out(None, "", ""),
                Ended::Failed("The system prompt was ended by a signal.".into()),
            ),
            (
                out(
                    Some(69),
                    r#"{"result":"error","operation":"grant-low-ports","error":"unsupported"}"#,
                    "alchemy-helper: /opt/a/alchemyd is on a nosuid mount\n",
                ),
                Ended::Failed(
                    "alchemy-helper failed (unsupported): /opt/a/alchemyd is on a nosuid mount"
                        .into(),
                ),
            ),
            (
                out(
                    Some(0),
                    r#"{"result":"ok","operation":"revoke-low-ports","changed":true}"#,
                    "",
                ),
                Ended::Failed(
                    "alchemy-helper answered for revoke-low-ports instead of grant-low-ports."
                        .into(),
                ),
            ),
            (
                out(Some(64), "", "error: unrecognized subcommand\n"),
                Ended::Failed(
                    "alchemy-helper gave no result: error: unrecognized subcommand".into(),
                ),
            ),
        ];
        for (output, want) in cases {
            assert_eq!(
                classify(&pkexec, "grant-low-ports", &output),
                want,
                "{output:?}"
            );
        }
    }

    #[test]
    fn classifying_osascript() {
        let helper = HelperPath::new("/Library/PrivilegedHelperTools/alchemy-helper").unwrap();
        let op = HelperOp::OpenFirewall {
            proxy_ports: alchemy_core::privilege::ProxyPorts::DEFAULT,
        };
        let osascript = elevated_command(&Elevation::Osascript, &helper, &op);
        let cases = [
            (
                out(
                    Some(0),
                    "{\"result\":\"ok\",\"operation\":\"open-firewall\",\"changed\":true}\nexit=0\n\n",
                    "",
                ),
                Ended::Done { changed: true },
            ),
            (
                out(
                    Some(0),
                    "{\"result\":\"error\",\"operation\":\"open-firewall\",\"error\":\"os_error\"}\nalchemy-helper: socketfilterfw failed\nexit=71\n\n",
                    "",
                ),
                Ended::Failed("alchemy-helper failed (os_error): socketfilterfw failed".into()),
            ),
            (
                out(
                    Some(1),
                    "",
                    "6:22: execution error: User canceled. (-128)\n",
                ),
                Ended::Cancelled,
            ),
            (
                out(
                    Some(1),
                    "",
                    "0:10: execution error: No user interaction allowed. (-1713)\n",
                ),
                Ended::Failed("0:10: execution error: No user interaction allowed. (-1713)".into()),
            ),
            (
                out(Some(0), "exit=127\n\n", ""),
                Ended::Failed("alchemy-helper gave no result: exit=127".into()),
            ),
        ];
        for (output, want) in cases {
            assert_eq!(
                classify(&osascript, "open-firewall", &output),
                want,
                "{output:?}"
            );
        }
    }

    #[test]
    fn classifying_something_else() {
        let helper = HelperPath::new("/usr/libexec/alchemy/alchemy-helper").unwrap();
        let sudo = alchemy_core::privilege::sudo_command(&helper, &HelperOp::GrantLowPorts);
        assert_eq!(
            classify(&sudo, "grant-low-ports", &out(Some(0), "", "")),
            Ended::Failed("Alchemy doesn't know how to read sudo.".into())
        );
    }

    #[tokio::test]
    async fn the_fake_answers_like_the_helper() {
        let helper = HelperPath::new("/usr/libexec/alchemy/alchemy-helper").unwrap();
        let pkexec = elevated_command(&Elevation::Pkexec, &helper, &HelperOp::RevokeLowPorts);
        let fake = FakeElevator::new();
        let output = fake.run(&pkexec).await.unwrap();
        assert_eq!(
            classify(&pkexec, "revoke-low-ports", &output),
            Ended::Done { changed: true }
        );
        fake.answer(Ok(out(Some(126), "", "")));
        let output = fake.run(&pkexec).await.unwrap();
        assert_eq!(
            classify(&pkexec, "revoke-low-ports", &output),
            Ended::Cancelled
        );
        assert_eq!(fake.commands(), vec![pkexec.clone(), pkexec]);
    }
}
