//! Running a [`FixedCommand`] and turning how it ended into a result.

use std::path::Path;

use crate::HelperError;
use crate::sibling::os;
use crate::system::{CommandOutput, FixedCommand, System};

/// `program args` as a [`FixedCommand`]. `program` must be absolute.
pub(crate) fn command(program: &'static str, args: &[&str]) -> Result<FixedCommand, HelperError> {
    FixedCommand::new(program, args.iter().map(|&a| a.to_owned()).collect()).ok_or_else(|| {
        os(
            "run",
            Path::new(program),
            std::io::Error::from(std::io::ErrorKind::InvalidInput),
        )
    })
}

/// Run a fixed command, whatever its exit status.
pub(crate) fn exec<S: System>(
    system: &S,
    program: &'static str,
    args: &[&str],
) -> Result<CommandOutput, HelperError> {
    system
        .run(&command(program, args)?)
        .map_err(|source| os("run", Path::new(program), source))
}

/// Run a fixed command that must exit 0.
pub(crate) fn must<S: System>(
    system: &S,
    program: &'static str,
    args: &[&str],
) -> Result<CommandOutput, HelperError> {
    let output = exec(system, program, args)?;
    if output.success() {
        Ok(output)
    } else {
        Err(failed(program, args, &output))
    }
}

/// A [`HelperError::CommandFailed`] for `program args` and how it ended.
/// The message is its stderr, or its stdout when stderr is empty, since
/// some tools print their errors there.
pub(crate) fn failed(program: &str, args: &[&str], output: &CommandOutput) -> HelperError {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
    let stderr = [text(&output.stderr), text(&output.stdout)]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| "no output".to_owned());
    HelperError::CommandFailed {
        command: std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" "),
        status: output
            .code
            .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
        stderr,
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
    fn failures_say_what_ran_and_how_it_ended() {
        let out = |code, stdout: &str, stderr: &str| CommandOutput {
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let cases = [
            (
                out(Some(1), "", "ERROR: no\n"),
                "`/usr/sbin/ufw allow alchemy` failed (exit 1): ERROR: no",
            ),
            (
                out(Some(0), "Status: unknown\n", ""),
                "`/usr/sbin/ufw allow alchemy` failed (exit 0): Status: unknown",
            ),
            (
                out(None, "", ""),
                "`/usr/sbin/ufw allow alchemy` failed (killed by a signal): no output",
            ),
        ];
        for (output, want) in cases {
            let err = failed("/usr/sbin/ufw", &["allow", "alchemy"], &output);
            assert_eq!(err.to_string(), want);
            assert_eq!(err.kind(), "command_failed");
        }
    }

    #[test]
    fn programs_must_be_absolute() {
        assert!(command("/bin/true", &[]).is_ok());
        assert_eq!(command("true", &[]).unwrap_err().kind(), "os_error");
    }
}
