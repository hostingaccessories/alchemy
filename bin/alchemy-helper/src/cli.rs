//! The command line: one subcommand per [`Operation`], strictly parsed.

use std::ffi::OsString;
use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use clap::{Parser, Subcommand};

use crate::firewall::ProxyPorts;

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
    /// Run the alchemyd next to this helper at boot as the given user.
    ///
    /// Installs and starts a systemd system unit on Linux or a
    /// launch daemon on macOS. alchemyd never runs as root.
    InstallService {
        /// The user alchemyd runs as: a decimal, nonzero UID.
        #[arg(long, value_parser = parse_uid)]
        uid: NonZeroU32,
    },
    /// Undo install-service: stop the service and delete it.
    RemoveService {
        /// The UID given to install-service.
        #[arg(long, value_parser = parse_uid)]
        uid: NonZeroU32,
    },
    /// Open 80/tcp, the proxy port range, and 5353/udp in the firewall.
    ///
    /// On Linux this allows the ufw app profile `alchemy` or adds the
    /// firewalld service `alchemy`, whichever is active. On macOS, when
    /// the application firewall is on, it allows the alchemyd next to
    /// this helper. Raw nftables and iptables rules are never edited.
    OpenFirewall {
        /// The proxy port range, such as 8100-8999. Must start at 1024 or
        /// above.
        #[arg(long, value_name = "START-END", value_parser = ProxyPorts::from_str)]
        proxy_ports: ProxyPorts,
    },
    /// Undo open-firewall: remove Alchemy's ufw profile, firewalld
    /// service, or macOS application firewall entry.
    CloseFirewall {
        /// The proxy port range open-firewall was given.
        #[arg(long, value_name = "START-END", value_parser = ProxyPorts::from_str)]
        proxy_ports: ProxyPorts,
    },
}

impl Operation {
    /// The subcommand, as it appears on the command line and in the
    /// result line.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::GrantLowPorts => "grant-low-ports",
            Self::RevokeLowPorts => "revoke-low-ports",
            Self::InstallService { .. } => "install-service",
            Self::RemoveService { .. } => "remove-service",
            Self::OpenFirewall { .. } => "open-firewall",
            Self::CloseFirewall { .. } => "close-firewall",
        }
    }
}

/// A UID as alchemyd writes it: plain decimal digits with no sign or
/// leading zero, and never root's.
fn parse_uid(value: &str) -> Result<NonZeroU32, String> {
    if value == "0" {
        return Err("the service never runs as root".to_owned());
    }
    let canonical = value.bytes().all(|b| b.is_ascii_digit()) && !value.starts_with('0');
    canonical
        .then(|| value.parse::<NonZeroU32>().ok())
        .flatten()
        .ok_or_else(|| "expected a decimal UID from 1 to 4294967295".to_owned())
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

    fn uid(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

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
            (
                &["alchemy-helper", "install-service", "--uid", "501"],
                Operation::InstallService { uid: uid(501) },
            ),
            (
                &["alchemy-helper", "remove-service", "--uid", "1000"],
                Operation::RemoveService { uid: uid(1000) },
            ),
            (
                &["alchemy-helper", "install-service", "--uid", "4294967295"],
                Operation::InstallService { uid: uid(u32::MAX) },
            ),
            (
                &[
                    "alchemy-helper",
                    "open-firewall",
                    "--proxy-ports",
                    "8100-8999",
                ],
                Operation::OpenFirewall {
                    proxy_ports: ProxyPorts::DEFAULT,
                },
            ),
            (
                &[
                    "alchemy-helper",
                    "close-firewall",
                    "--proxy-ports",
                    "8100-8999",
                ],
                Operation::CloseFirewall {
                    proxy_ports: ProxyPorts::DEFAULT,
                },
            ),
            (
                &[
                    "alchemy-helper",
                    "open-firewall",
                    "--proxy-ports",
                    "1024-1024",
                ],
                Operation::OpenFirewall {
                    proxy_ports: ProxyPorts::new(1024, 1024).unwrap(),
                },
            ),
            (
                &[
                    "alchemy-helper",
                    "open-firewall",
                    "--proxy-ports",
                    "9000-65535",
                ],
                Operation::OpenFirewall {
                    proxy_ports: ProxyPorts::new(9000, 65535).unwrap(),
                },
            ),
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
            &["alchemy-helper", "install-service"],
            &["alchemy-helper", "install-service", "--uid"],
            &["alchemy-helper", "install-service", "--uid", "0"],
            &["alchemy-helper", "install-service", "--uid", "00"],
            &["alchemy-helper", "install-service", "--uid", "0501"],
            &["alchemy-helper", "install-service", "--uid", "+501"],
            &["alchemy-helper", "install-service", "--uid", "-1"],
            &["alchemy-helper", "install-service", "--uid", " 501"],
            &["alchemy-helper", "install-service", "--uid", "501\n"],
            &["alchemy-helper", "install-service", "--uid", "0x1f5"],
            &["alchemy-helper", "install-service", "--uid", "4294967296"],
            &["alchemy-helper", "install-service", "--uid", "root"],
            &["alchemy-helper", "install-service", "--uid", ""],
            &[
                "alchemy-helper",
                "install-service",
                "--uid",
                "501",
                "--uid",
                "502",
            ],
            &["alchemy-helper", "install-service", "--uid", "501", "extra"],
            &["alchemy-helper", "remove-service", "--uid", "0"],
            &["alchemy-helper", "remove-service"],
        ];
        for argv in cases {
            let err = parse(*argv).unwrap_err();
            assert!(err.is_usage(), "{argv:?}");
        }
    }

    #[test]
    fn rejects_bad_proxy_ports() {
        let cases: &[&[&str]] = &[
            &["alchemy-helper", "open-firewall"],
            &["alchemy-helper", "close-firewall"],
            &["alchemy-helper", "open-firewall", "--proxy-ports"],
            &["alchemy-helper", "open-firewall", "--proxy-ports", "22-22"],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "1023-8999",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "8999-8100",
            ],
            &["alchemy-helper", "open-firewall", "--proxy-ports", "8100"],
            &["alchemy-helper", "open-firewall", "--proxy-ports", "8100-"],
            &["alchemy-helper", "open-firewall", "--proxy-ports", "-8999"],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "8100:8999",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "08100-8999",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "+8100-8999",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                " 8100-8999",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "8100-8999\n",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "8100-65536",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "8100-8999-9000",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports=8100-8999",
                "x",
            ],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "8100-8999",
                "--proxy-ports",
                "8100-8999",
            ],
            &["alchemy-helper", "open-firewall", "8100-8999"],
            &[
                "alchemy-helper",
                "open-firewall",
                "--proxy-ports",
                "8100-8999",
                "80",
            ],
            &["alchemy-helper", "check", "--proxy-ports", "8100-8999"],
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
            [
                "check",
                "grant-low-ports",
                "revoke-low-ports",
                "install-service",
                "remove-service",
                "open-firewall",
                "close-firewall",
            ]
        );
        for name in subcommands {
            let argv: &[&str] = match name {
                "install-service" | "remove-service" => &["alchemy-helper", name, "--uid", "501"],
                "open-firewall" | "close-firewall" => {
                    &["alchemy-helper", name, "--proxy-ports", "8100-8999"]
                }
                _ => &["alchemy-helper", name],
            };
            assert_eq!(parse(argv).unwrap().name(), name);
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
