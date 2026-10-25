//! Running the `sudo alchemy-helper ...` command alchemyd hands over when
//! no system prompt is safe or available, for `alchemy machine
//! privileges`.
//!
//! The command comes from alchemyd over the user's own `0o600` socket.
//! It is run directly, never through a shell, with the terminal's stdin,
//! stdout, and stderr, so sudo can ask for the password.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// The program every terminal command must start with.
pub const SUDO: &str = "sudo";

/// Where a command runs `sudo`: the terminal in the binary, a script in
/// tests.
pub trait Sudo {
    /// Whether there is someone at a terminal to type sudo's password.
    /// Without one, the command is printed instead of run.
    fn is_interactive(&self) -> bool;

    /// Run `sudo` with `args` on the terminal and wait for it. An `Err`
    /// says why it couldn't run or didn't succeed.
    fn run(&mut self, args: &[String]) -> impl Future<Output = Result<(), String>>;
}

/// Runs `sudo` from a search path on the terminal.
#[derive(Debug, Clone, Default)]
pub struct TerminalSudo {
    search: Option<OsString>,
}

impl TerminalSudo {
    /// Find `sudo` in this process's `PATH`.
    pub fn from_env() -> Self {
        Self::searching(std::env::var_os("PATH"))
    }

    /// Find `sudo` in `search`, a `PATH`-style list of directories.
    pub fn searching(search: Option<OsString>) -> Self {
        Self { search }
    }
}

impl Sudo for TerminalSudo {
    /// Both stdin and stderr must be terminals: stdin to type the
    /// password on, and stderr so sudo's question is seen.
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    /// Waits on its own thread, so the async runtime keeps running.
    fn run(&mut self, args: &[String]) -> impl Future<Output = Result<(), String>> {
        let program = find_program(SUDO, self.search.as_deref(), Path::is_file);
        let args = args.to_vec();
        async move {
            let program = program.ok_or_else(|| format!("{SUDO} wasn't found on PATH"))?;
            let shown = program.display().to_string();
            let status = tokio::task::spawn_blocking(move || {
                std::process::Command::new(&program).args(&args).status()
            })
            .await
            .map_err(|e| format!("cannot wait for {shown}: {e}"))?;
            match status {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(format!("{SUDO} exited with {status}")),
                Err(e) => Err(format!("cannot run {shown}: {e}")),
            }
        }
    }
}

/// `name` in the first absolute directory in `path` that `exists` says
/// has it. Relative `PATH` entries are skipped, so nothing in the working
/// directory is run.
pub(crate) fn find_program(
    name: &str,
    path: Option<&OsStr>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| exists(candidate))
}

/// What a [`FakeSudo`] does when run, such as changing a fake machine as
/// the helper would.
#[cfg(any(test, feature = "test-command"))]
type Effect = Box<dyn FnMut(&[String]) -> Result<(), String>>;

/// A scripted [`Sudo`]: records each run and answers it with an effect.
#[cfg(any(test, feature = "test-command"))]
pub struct FakeSudo {
    interactive: bool,
    effect: Effect,
    ran: Vec<Vec<String>>,
}

#[cfg(any(test, feature = "test-command"))]
impl FakeSudo {
    /// A terminal where each run calls `effect` with sudo's arguments and
    /// answers with what it returns.
    pub fn tty(effect: impl FnMut(&[String]) -> Result<(), String> + 'static) -> Self {
        Self {
            interactive: true,
            effect: Box::new(effect),
            ran: Vec::new(),
        }
    }

    /// No terminal: nothing should be run.
    pub fn no_tty() -> Self {
        Self {
            interactive: false,
            effect: Box::new(|_| Err("no terminal".to_owned())),
            ran: Vec::new(),
        }
    }

    /// The arguments of every run so far, in order.
    pub fn ran(&self) -> &[Vec<String>] {
        &self.ran
    }
}

#[cfg(any(test, feature = "test-command"))]
impl std::fmt::Debug for FakeSudo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeSudo")
            .field("interactive", &self.interactive)
            .field("ran", &self.ran)
            .finish_non_exhaustive()
    }
}

#[cfg(any(test, feature = "test-command"))]
impl Sudo for FakeSudo {
    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn run(&mut self, args: &[String]) -> impl Future<Output = Result<(), String>> {
        self.ran.push(args.to_vec());
        std::future::ready((self.effect)(args))
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

    #[test]
    fn find_program_takes_the_first_absolute_match() {
        let cases: [(Option<&str>, &[&str], Option<&str>); 6] = [
            (None, &["/usr/bin/sudo"], None),
            (Some(""), &["/usr/bin/sudo"], None),
            (
                Some("/usr/local/bin:/usr/bin"),
                &["/usr/bin/sudo"],
                Some("/usr/bin/sudo"),
            ),
            (
                Some("/usr/local/bin:/usr/bin"),
                &["/usr/bin/sudo", "/usr/local/bin/sudo"],
                Some("/usr/local/bin/sudo"),
            ),
            (Some(".:bin:/usr/bin"), &["./sudo", "bin/sudo"], None),
            (Some("/opt/bin"), &[], None),
        ];
        for (path, present, want) in cases {
            let got = find_program("sudo", path.map(OsStr::new), |p| {
                present.iter().any(|q| Path::new(q) == p)
            });
            assert_eq!(got.as_deref(), want.map(Path::new), "{path:?} {present:?}");
        }
    }

    fn stand_in(dir: &Path, exit: u8) {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join(SUDO);
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nexit {exit}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn the_terminal_runs_sudo_with_its_arguments_and_no_shell() {
        let dir = tempfile::tempdir().unwrap();
        stand_in(dir.path(), 0);
        let mut sudo = TerminalSudo::searching(Some(dir.path().as_os_str().to_owned()));
        let args = [
            "/opt/alchemy helper".to_owned(),
            "$(id); echo hi".to_owned(),
        ];
        sudo.run(&args).await.unwrap();
        let seen = std::fs::read_to_string(dir.path().join("sudo.args")).unwrap();
        assert_eq!(seen, "/opt/alchemy helper\n$(id); echo hi\n");

        stand_in(dir.path(), 1);
        let err = sudo.run(&args).await.unwrap_err();
        assert!(err.starts_with("sudo exited with"), "{err}");

        let mut nowhere = TerminalSudo::searching(Some("relative/bin".into()));
        assert_eq!(
            nowhere.run(&args).await.unwrap_err(),
            "sudo wasn't found on PATH"
        );
    }

    #[tokio::test]
    async fn fake_records_runs_and_answers_with_its_effect() {
        let mut sudo = FakeSudo::tty(|args| {
            if args.first().is_some_and(|a| a == "fail") {
                Err("sudo exited with exit status: 1".into())
            } else {
                Ok(())
            }
        });
        assert!(sudo.is_interactive());
        assert_eq!(sudo.run(&["ok".into()]).await, Ok(()));
        assert!(sudo.run(&["fail".into()]).await.is_err());
        assert_eq!(sudo.ran(), [vec!["ok".to_owned()], vec!["fail".to_owned()]]);
        assert!(!FakeSudo::no_tty().is_interactive());
    }
}
