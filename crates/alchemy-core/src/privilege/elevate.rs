//! How to run the helper as root: a system prompt when one is safe and
//! available, otherwise a `sudo` command for the user to run.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::helper::{Distrust, HelperPath, HelperTrust};
use super::op::HelperOp;
use crate::route::HostOs;

/// `osascript`, by absolute path.
pub const OSASCRIPT: &str = "/usr/bin/osascript";

/// `pkexec`, by absolute path.
pub const PKEXEC: &str = "/usr/bin/pkexec";

/// `sudo`, as the user would type it.
pub const SUDO: &str = "sudo";

/// The fixed `AppleScript`, one `-e` per line, that runs its arguments as
/// root behind the macOS administrator prompt.
///
/// The helper's path and arguments are passed to the `run` handler as
/// `argv`, never spliced into the script, and each is quoted with
/// `quoted form of` for the shell `do shell script` runs.
pub const OSASCRIPT_SCRIPT: [&str; 7] = [
    "on run argv",
    "set helperCommand to \"\"",
    "repeat with helperArg in argv",
    "set helperCommand to helperCommand & quoted form of (helperArg as text) & \" \"",
    "end repeat",
    "do shell script helperCommand with administrator privileges",
    "end run",
];

/// How the helper gets root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Elevation {
    /// The macOS administrator prompt, through `osascript`.
    Osascript,
    /// The polkit prompt, through `pkexec`.
    Pkexec,
    /// No safe prompt: the user runs a `sudo` command in a terminal.
    Terminal {
        /// Why there is no prompt.
        reason: TerminalReason,
    },
}

/// Why the user has to run the helper from a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TerminalReason {
    /// The helper could be swapped by a user, as with a dev build, so a
    /// password prompt in front of it isn't safe.
    UntrustedHelper {
        /// What could be swapped.
        why: Distrust,
    },
    /// Linux with no polkit agent to answer `pkexec`, such as over SSH or
    /// on a headless server.
    NoPolkitAgent,
}

impl fmt::Display for TerminalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedHelper { why } => {
                write!(f, "alchemy-helper isn't installed safely: {why}")
            }
            Self::NoPolkitAgent => f.write_str("No authentication agent is running."),
        }
    }
}

/// How to elevate on `os`, given the helper's `trust` and, on Linux,
/// whether a polkit agent answers.
///
/// An untrusted helper always needs the terminal, whatever the OS, so the
/// user sees exactly what runs as root.
pub fn elevation(os: HostOs, trust: &HelperTrust, polkit_agent: bool) -> Elevation {
    if let HelperTrust::Untrusted { why } = trust {
        return Elevation::Terminal {
            reason: TerminalReason::UntrustedHelper { why: why.clone() },
        };
    }
    match os {
        HostOs::MacOs => Elevation::Osascript,
        HostOs::Linux if polkit_agent => Elevation::Pkexec,
        HostOs::Linux => Elevation::Terminal {
            reason: TerminalReason::NoPolkitAgent,
        },
    }
}

/// A command as an argv, such as one that runs the helper as root.
///
/// Displays as one POSIX shell line, quoted only where needed, such as
/// `sudo /usr/libexec/alchemy/alchemy-helper grant-low-ports`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandLine {
    argv: Vec<String>,
}

impl CommandLine {
    /// Every element, the program first.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The program.
    pub fn program(&self) -> &str {
        self.argv.first().map_or("", String::as_str)
    }

    /// Everything after the program.
    pub fn args(&self) -> &[String] {
        self.argv.get(1..).unwrap_or_default()
    }
}

impl fmt::Display for CommandLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, arg) in self.argv.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write_quoted(f, arg)?;
        }
        Ok(())
    }
}

fn write_quoted(f: &mut fmt::Formatter<'_>, arg: &str) -> fmt::Result {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"%+,-./:=@_".contains(&b));
    if plain {
        return f.write_str(arg);
    }
    write!(f, "'{}'", arg.replace('\'', r"'\''"))
}

/// The command that runs `op` with `helper` through `elevation`.
///
/// For [`Elevation::Terminal`] it is [`sudo_command`].
pub fn elevated_command(elevation: &Elevation, helper: &HelperPath, op: &HelperOp) -> CommandLine {
    let mut argv: Vec<String> = match elevation {
        Elevation::Osascript => std::iter::once(OSASCRIPT)
            .chain(OSASCRIPT_SCRIPT.iter().flat_map(|line| ["-e", line]))
            .map(str::to_owned)
            .collect(),
        Elevation::Pkexec => vec![PKEXEC.to_owned()],
        Elevation::Terminal { .. } => vec![SUDO.to_owned()],
    };
    argv.push(helper.as_str().to_owned());
    argv.extend(op.args());
    CommandLine { argv }
}

/// `sudo <helper> <op>`: what the user runs in a terminal, and what the
/// CLI runs itself when it has a TTY.
pub fn sudo_command(helper: &HelperPath, op: &HelperOp) -> CommandLine {
    elevated_command(
        &Elevation::Terminal {
            reason: TerminalReason::NoPolkitAgent,
        },
        helper,
        op,
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
    use std::path::PathBuf;

    use super::*;
    use crate::privilege::{ProxyPorts, ServiceUid};

    fn untrusted() -> HelperTrust {
        HelperTrust::Untrusted {
            why: Distrust::NotRootOwned {
                path: PathBuf::from("/home/me/target/debug/alchemy-helper"),
                uid: 1000,
            },
        }
    }

    #[test]
    fn choosing_elevation() {
        let no_agent = Elevation::Terminal {
            reason: TerminalReason::NoPolkitAgent,
        };
        let dev_build = Elevation::Terminal {
            reason: TerminalReason::UntrustedHelper {
                why: Distrust::NotRootOwned {
                    path: PathBuf::from("/home/me/target/debug/alchemy-helper"),
                    uid: 1000,
                },
            },
        };
        let cases = [
            (
                HostOs::MacOs,
                HelperTrust::Trusted,
                false,
                Elevation::Osascript,
            ),
            (
                HostOs::MacOs,
                HelperTrust::Trusted,
                true,
                Elevation::Osascript,
            ),
            (HostOs::MacOs, untrusted(), false, dev_build.clone()),
            (HostOs::Linux, HelperTrust::Trusted, true, Elevation::Pkexec),
            (HostOs::Linux, HelperTrust::Trusted, false, no_agent),
            (HostOs::Linux, untrusted(), true, dev_build.clone()),
            (HostOs::Linux, untrusted(), false, dev_build),
        ];
        for (os, trust, agent, want) in cases {
            assert_eq!(
                elevation(os, &trust, agent),
                want,
                "{os:?} {trust:?} {agent}"
            );
        }
    }

    #[test]
    fn commands() {
        let helper = HelperPath::new("/usr/libexec/alchemy/alchemy-helper").unwrap();
        let spaced = HelperPath::new("/opt/Alchemy's Tools/alchemy-helper").unwrap();
        let open = HelperOp::OpenFirewall {
            proxy_ports: ProxyPorts::DEFAULT,
        };
        let install = HelperOp::InstallService {
            uid: ServiceUid::new(1000).unwrap(),
        };
        let mut osascript: Vec<&str> = vec![OSASCRIPT];
        for line in OSASCRIPT_SCRIPT {
            osascript.extend(["-e", line]);
        }
        osascript.extend([
            "/usr/libexec/alchemy/alchemy-helper",
            "open-firewall",
            "--proxy-ports",
            "8100-8999",
        ]);
        let cases: Vec<(Elevation, &HelperPath, HelperOp, Vec<&str>)> = vec![
            (Elevation::Osascript, &helper, open, osascript),
            (
                Elevation::Pkexec,
                &helper,
                HelperOp::GrantLowPorts,
                vec![
                    PKEXEC,
                    "/usr/libexec/alchemy/alchemy-helper",
                    "grant-low-ports",
                ],
            ),
            (
                Elevation::Terminal {
                    reason: TerminalReason::NoPolkitAgent,
                },
                &helper,
                install,
                vec![
                    "sudo",
                    "/usr/libexec/alchemy/alchemy-helper",
                    "install-service",
                    "--uid",
                    "1000",
                ],
            ),
            (
                Elevation::Terminal {
                    reason: TerminalReason::UntrustedHelper {
                        why: Distrust::Unchecked {
                            path: PathBuf::from("/opt"),
                        },
                    },
                },
                &spaced,
                HelperOp::RevokeLowPorts,
                vec![
                    "sudo",
                    "/opt/Alchemy's Tools/alchemy-helper",
                    "revoke-low-ports",
                ],
            ),
        ];
        for (elevation, helper, op, want) in cases {
            let got = elevated_command(&elevation, helper, &op);
            assert_eq!(got.argv(), want, "{elevation:?} {op:?}");
            assert_eq!(got.program(), want[0]);
            assert_eq!(got.args(), &want[1..]);
        }
        assert_eq!(
            sudo_command(&helper, &HelperOp::GrantLowPorts).argv(),
            [
                "sudo",
                "/usr/libexec/alchemy/alchemy-helper",
                "grant-low-ports"
            ]
        );
    }

    #[test]
    fn osascript_script_takes_argv_only() {
        assert_eq!(OSASCRIPT_SCRIPT.first(), Some(&"on run argv"));
        assert_eq!(OSASCRIPT_SCRIPT.last(), Some(&"end run"));
        assert!(
            OSASCRIPT_SCRIPT
                .iter()
                .any(|l| l.contains("quoted form of (helperArg as text)"))
        );
        assert!(
            OSASCRIPT_SCRIPT
                .iter()
                .any(|l| l.ends_with("with administrator privileges"))
        );
    }

    #[test]
    fn shell_lines() {
        let line = |argv: &[&str]| {
            CommandLine {
                argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            }
            .to_string()
        };
        let cases: [(&[&str], &str); 6] = [
            (
                &[
                    "sudo",
                    "/usr/libexec/alchemy/alchemy-helper",
                    "grant-low-ports",
                ],
                "sudo /usr/libexec/alchemy/alchemy-helper grant-low-ports",
            ),
            (
                &[
                    "sudo",
                    "/opt/x/alchemy-helper",
                    "open-firewall",
                    "--proxy-ports",
                    "8100-8999",
                ],
                "sudo /opt/x/alchemy-helper open-firewall --proxy-ports 8100-8999",
            ),
            (
                &["sudo", "/opt/Alchemy Tools/alchemy-helper"],
                "sudo '/opt/Alchemy Tools/alchemy-helper'",
            ),
            (
                &["sudo", "/opt/it's/alchemy-helper"],
                r"sudo '/opt/it'\''s/alchemy-helper'",
            ),
            (&["echo", "$HOME", "a;b"], "echo '$HOME' 'a;b'"),
            (&["echo", ""], "echo ''"),
        ];
        for (argv, want) in cases {
            assert_eq!(line(argv), want);
        }
        assert_eq!(CommandLine { argv: vec![] }.program(), "");
        assert_eq!(CommandLine { argv: vec![] }.args(), <&[String]>::default());
    }

    #[test]
    fn wire() {
        let helper = HelperPath::new("/usr/libexec/alchemy/alchemy-helper").unwrap();
        assert_eq!(
            serde_json::to_value(sudo_command(&helper, &HelperOp::GrantLowPorts)).unwrap(),
            serde_json::json!({
                "argv": ["sudo", "/usr/libexec/alchemy/alchemy-helper", "grant-low-ports"],
            })
        );
        assert_eq!(
            serde_json::to_value(Elevation::Pkexec).unwrap(),
            serde_json::json!({"method": "pkexec"})
        );
        assert_eq!(
            serde_json::to_value(Elevation::Terminal {
                reason: TerminalReason::NoPolkitAgent
            })
            .unwrap(),
            serde_json::json!({"method": "terminal", "reason": {"kind": "no_polkit_agent"}})
        );
        assert_eq!(
            TerminalReason::NoPolkitAgent.to_string(),
            "No authentication agent is running."
        );
        assert_eq!(
            TerminalReason::UntrustedHelper {
                why: Distrust::GroupWritable {
                    path: PathBuf::from("/Applications"),
                },
            }
            .to_string(),
            "alchemy-helper isn't installed safely: /Applications is writable by its group."
        );
    }
}
