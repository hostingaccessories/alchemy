//! The command line: one subcommand per [`Operation`], strictly parsed.

use std::ffi::OsString;
use std::fmt;

use clap::{Parser, Subcommand};

/// Alchemy privileged helper. Run it as root: it performs one operation
/// and exits.
#[derive(Debug, Parser)]
#[command(
    name = "alchemy-helper",
    version,
    about,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    operation: Operation,
}

/// One privileged operation: a subcommand and its validated arguments.
///
/// Every operation that changes the system has an undo with the same
/// arguments. Variant names are the subcommands in kebab-case, which
/// [`name`](Self::name) returns.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
#[non_exhaustive]
pub enum Operation {
    /// Verify the helper is running with root privileges and exit.
    Check,
    /// Let the alchemyd next to this helper bind ports below 1024 (Linux).
    ///
    /// Sets the file capability `cap_net_bind_service=ep` on it. The grant
    /// takes effect the next time alchemyd starts.
    GrantLowPorts,
    /// Undo grant-low-ports: remove the alchemyd file capability (Linux).
    RevokeLowPorts,
}

impl Operation {
    /// The subcommand, as it appears on the command line and in the
    /// result line.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::GrantLowPorts => "grant-low-ports",
            Self::RevokeLowPorts => "revoke-low-ports",
        }
    }
}

/// Why [`parse`] did not produce an [`Operation`].
#[derive(Debug)]
pub struct ParseError(clap::Error);

impl ParseError {
    /// Whether the command line was invalid. `false` means `--help` or
    /// `--version` was asked for, and the `Display` form is that text
    /// for stdout.
    pub fn is_usage(&self) -> bool {
        self.0.use_stderr()
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.render())
    }
}

impl std::error::Error for ParseError {}

/// Parse a full argv, starting with the program name, into an
/// [`Operation`].
///
/// This is the helper's whole input contract: alchemyd's contract test
/// feeds it every argv alchemyd builds. Unknown subcommands, unknown
/// flags, and extra arguments are all errors.
pub fn parse<I, T>(argv: I) -> Result<Operation, ParseError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::try_parse_from(argv)
        .map(|cli| cli.operation)
        .map_err(ParseError)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn parses_each_operation() {
        let cases = [
            (&["alchemy-helper", "check"][..], Operation::Check),
            (
                &["alchemy-helper", "grant-low-ports"],
                Operation::GrantLowPorts,
            ),
            (
                &["alchemy-helper", "revoke-low-ports"],
                Operation::RevokeLowPorts,
            ),
            (&["/usr/libexec/alchemy-helper", "check"], Operation::Check),
        ];
        for (argv, want) in cases {
            assert_eq!(parse(argv).unwrap(), want, "{argv:?}");
        }
    }

    #[test]
    fn rejects_anything_else() {
        let cases: &[&[&str]] = &[
            &["alchemy-helper"],
            &["alchemy-helper", "bogus"],
            &["alchemy-helper", "grant"],
            &["alchemy-helper", "Grant-Low-Ports"],
            &["alchemy-helper", "grant_low_ports"],
            &["alchemy-helper", "grant-low-ports", "/usr/bin/python3"],
            &["alchemy-helper", "grant-low-ports", "--path", "/bin/sh"],
            &["alchemy-helper", "revoke-low-ports", "extra"],
            &["alchemy-helper", "check", "--force"],
            &["alchemy-helper", "--verbose", "check"],
            &["alchemy-helper", "help"],
            &["alchemy-helper", "help", "check"],
            &["alchemy-helper", ""],
        ];
        for argv in cases {
            let err = parse(*argv).unwrap_err();
            assert!(err.is_usage(), "{argv:?}");
        }
    }

    #[test]
    fn help_and_version_are_not_usage_errors() {
        for flag in ["--help", "-h", "--version", "-V"] {
            let err = parse(["alchemy-helper", flag]).unwrap_err();
            assert!(!err.is_usage(), "{flag}");
            assert!(!err.to_string().is_empty(), "{flag}");
        }
    }

    #[test]
    fn names_match_the_subcommands() {
        let cli = Cli::command();
        let subcommands: Vec<&str> = cli.get_subcommands().map(clap::Command::get_name).collect();
        assert_eq!(
            subcommands,
            ["check", "grant-low-ports", "revoke-low-ports"]
        );
        for name in subcommands {
            assert_eq!(parse(["alchemy-helper", name]).unwrap().name(), name);
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
