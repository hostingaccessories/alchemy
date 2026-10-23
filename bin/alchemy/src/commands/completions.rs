//! `alchemy completions`: static shell completion scripts, generated from
//! the command tree.
//!
//! It never contacts alchemyd, so it works before the daemon is
//! installed. Hidden commands are left out.

use std::io::Write;

use clap_complete::Shell as Generator;

use crate::error::ClientError;

/// The shells `alchemy completions` writes scripts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    /// Bash.
    Bash,
    /// Zsh.
    Zsh,
    /// Fish.
    Fish,
}

impl Shell {
    fn generator(self) -> Generator {
        match self {
            Self::Bash => Generator::Bash,
            Self::Zsh => Generator::Zsh,
            Self::Fish => Generator::Fish,
        }
    }
}

/// Arguments to `alchemy completions`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// The shell to write completions for.
    #[arg(value_enum)]
    pub shell: Shell,
}

/// Run `alchemy completions`, writing the script to `out`.
///
/// The script is the result whatever `--json` says: there is no JSON
/// form of a shell script.
pub(crate) fn run<W: Write>(out: &mut W, args: &Args) -> Result<(), ClientError> {
    out.write_all(&script(args.shell))?;
    out.flush()?;
    Ok(())
}

/// The completion script for `shell`.
pub(crate) fn script(shell: Shell) -> Vec<u8> {
    let mut cmd = super::visible_tree();
    let name = cmd.get_name().to_owned();
    let mut script = Vec::new();
    clap_complete::generate(shell.generator(), &mut cmd, name, &mut script);
    script
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use clap::ValueEnum;

    use super::*;

    #[test]
    fn every_shell_names_the_visible_commands_only() {
        for shell in Shell::value_variants() {
            let script = String::from_utf8(script(*shell)).unwrap();
            assert!(!script.trim().is_empty(), "{shell:?}");
            for visible in ["status", "completions", "json", "socket", "watch"] {
                assert!(script.contains(visible), "{shell:?} lacks {visible}");
            }
            assert!(!script.contains("generate-man"), "{shell:?}");
        }
    }

    #[test]
    fn every_shell_completes_version_as_a_command() {
        let cases = [
            (Shell::Bash, "alchemy,version)"),
            (
                Shell::Zsh,
                "'version:Show the versions of alchemy and alchemyd",
            ),
            (Shell::Fish, "-a \"version\""),
        ];
        for (shell, command) in cases {
            let script = String::from_utf8(script(shell)).unwrap();
            assert!(script.contains(command), "{shell:?} lacks {command}");
        }
    }

    #[test]
    fn run_writes_the_script() {
        let mut out = Vec::new();
        run(&mut out, &Args { shell: Shell::Bash }).unwrap();
        assert_eq!(out, script(Shell::Bash));
    }
}
