//! `alchemy-helper`: the privileged one-shot.
//!
//! This crate is the security boundary. The binary is never setuid and
//! never long-running: callers elevate it explicitly (`sudo`, `pkexec`,
//! or macOS `osascript ... with administrator privileges`), it performs
//! one narrowly validated [`Operation`], and it exits. `main` refuses to
//! run unless the effective UID is already 0.
//!
//! The flow is [`parse`], a root check, [`run`] over a [`System`], and
//! [`report`], which prints one result line on stdout and returns the
//! [`Exit`] status. The result line is a single JSON object:
//!
//! ```text
//! {"result":"ok","operation":"grant-low-ports","changed":true}
//! {"result":"error","operation":"grant-low-ports","error":"unsupported"}
//! {"result":"error","operation":null,"error":"usage"}
//! ```
//!
//! A human-readable message for an error goes to stderr. Nothing is ever
//! passed through a shell, and the crate depends on no other Alchemy
//! crate.

mod cli;
mod command;
mod error;
mod firewall;
mod low_ports;
mod real;
mod service;
mod sibling;
mod system;

#[cfg(test)]
mod fake;

use std::fmt;
use std::io::Write;

pub use cli::{Operation, ParseError, parse};
pub use error::{Exit, HelperError};
pub use firewall::{MIN_PROXY_PORT, ProxyPorts, ProxyPortsError};
pub use low_ports::{CAPABILITY_XATTR, NET_BIND_SERVICE_EP};
pub use real::{COMMAND_PATH, RealSystem};
pub use sibling::DAEMON;
pub use system::{CommandOutput, FileInfo, FixedCommand, OpenError, System};

/// The operating system the helper runs on. Operations take it as a
/// value so both platforms' paths are testable on either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Linux.
    Linux,
    /// macOS.
    MacOs,
}

impl Platform {
    /// The platform this binary was built for.
    pub const fn current() -> Self {
        #[cfg(target_os = "linux")]
        let platform = Self::Linux;
        #[cfg(target_os = "macos")]
        let platform = Self::MacOs;
        platform
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Linux => "Linux",
            Self::MacOs => "macOS",
        })
    }
}

/// What a successful operation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the system changed. `false` when it was already in the
    /// requested state, so every operation is safe to repeat.
    pub changed: bool,
}

/// Perform `operation` through `system`. The caller has already checked
/// for root.
pub fn run<S: System>(
    operation: &Operation,
    system: &S,
    platform: Platform,
) -> Result<Outcome, HelperError> {
    match operation {
        Operation::Check => Ok(Outcome { changed: false }),
        Operation::GrantLowPorts => low_ports::grant(system, platform),
        Operation::RevokeLowPorts => low_ports::revoke(system, platform),
        Operation::InstallService { uid } => service::install(system, platform, *uid),
        Operation::RemoveService { uid } => service::remove(system, platform, *uid),
        Operation::OpenFirewall { proxy_ports } => firewall::open(system, platform, *proxy_ports),
        Operation::CloseFirewall { proxy_ports } => firewall::close(system, platform, *proxy_ports),
    }
}

/// The one-line JSON result for `operation` (`None` when the command
/// line did not parse), without a trailing newline.
///
/// Every value is a fixed token, so no escaping is needed. New fields
/// may be added; existing ones keep their meaning.
pub fn result_line(operation: Option<&Operation>, result: &Result<Outcome, HelperError>) -> String {
    let operation = operation.map_or_else(|| "null".to_owned(), |op| format!("\"{}\"", op.name()));
    match result {
        Ok(outcome) => format!(
            r#"{{"result":"ok","operation":{operation},"changed":{}}}"#,
            outcome.changed
        ),
        Err(e) => format!(
            r#"{{"result":"error","operation":{operation},"error":"{}"}}"#,
            e.kind()
        ),
    }
}

/// Print the result line to `out` and any error message to `err`, and
/// return the exit status. A failure to write the result line is
/// [`Exit::IoErr`], since the caller cannot learn the outcome.
pub fn report(
    operation: Option<&Operation>,
    result: &Result<Outcome, HelperError>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Exit {
    match result {
        Err(HelperError::Usage(message)) => {
            let _ = writeln!(err, "{}", message.trim_end());
        }
        Err(e) => {
            let _ = writeln!(err, "alchemy-helper: {e}");
        }
        Ok(_) => {}
    }
    let written = writeln!(out, "{}", result_line(operation, result)).and_then(|()| out.flush());
    match (written, result) {
        (Err(_), _) => Exit::IoErr,
        (Ok(()), Ok(_)) => Exit::Success,
        (Ok(()), Err(e)) => e.exit(),
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
    use std::io;
    use std::path::PathBuf;

    use super::*;
    use crate::fake::{Call, FakeFile, FakeSystem};

    const DAEMON_PATH: &str = "/opt/alchemy/bin/alchemyd";

    fn granted() -> FakeFile {
        FakeFile::regular().with_xattr(CAPABILITY_XATTR, &NET_BIND_SERVICE_EP)
    }

    struct Case {
        name: &'static str,
        operation: Operation,
        platform: Platform,
        daemon: Option<FakeFile>,
        fail: Option<Call>,
        want: Result<bool, &'static str>,
        want_xattr: Option<&'static [u8]>,
    }

    fn case(name: &'static str, operation: Operation) -> Case {
        Case {
            name,
            operation,
            platform: Platform::Linux,
            daemon: Some(FakeFile::regular()),
            fail: None,
            want: Ok(true),
            want_xattr: None,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn run_operations() {
        let grant = || case("", Operation::GrantLowPorts);
        let revoke = || case("", Operation::RevokeLowPorts);
        let cases = [
            Case {
                name: "check does nothing",
                want: Ok(false),
                ..case("", Operation::Check)
            },
            Case {
                name: "check on macOS",
                platform: Platform::MacOs,
                want: Ok(false),
                ..case("", Operation::Check)
            },
            Case {
                name: "grant sets the capability",
                want_xattr: Some(&NET_BIND_SERVICE_EP),
                ..grant()
            },
            Case {
                name: "grant again is unchanged",
                daemon: Some(granted()),
                want: Ok(false),
                want_xattr: Some(&NET_BIND_SERVICE_EP),
                ..grant()
            },
            Case {
                name: "grant replaces other capabilities",
                daemon: Some(
                    FakeFile::regular()
                        .with_xattr(CAPABILITY_XATTR, &[1, 0, 0, 2, 0xff, 0xff, 0xff, 0xff]),
                ),
                want_xattr: Some(&NET_BIND_SERVICE_EP),
                ..grant()
            },
            Case {
                name: "grant without alchemyd",
                daemon: None,
                want: Err("not_found"),
                ..grant()
            },
            Case {
                name: "grant refuses a symlink",
                daemon: Some(FakeFile::symlink()),
                want: Err("refused"),
                ..grant()
            },
            Case {
                name: "grant refuses a directory",
                daemon: Some(FakeFile::other()),
                want: Err("refused"),
                ..grant()
            },
            Case {
                name: "grant refuses a nosuid mount",
                daemon: Some(FakeFile::regular().nosuid()),
                want: Err("unsupported"),
                ..grant()
            },
            Case {
                name: "grant on macOS",
                platform: Platform::MacOs,
                want: Err("unsupported"),
                ..grant()
            },
            Case {
                name: "grant when the exe cannot be resolved",
                fail: Some(Call::CurrentExe),
                want: Err("os_error"),
                ..grant()
            },
            Case {
                name: "grant when open fails",
                fail: Some(Call::Open),
                want: Err("os_error"),
                ..grant()
            },
            Case {
                name: "grant when inspect fails",
                fail: Some(Call::Inspect),
                want: Err("os_error"),
                ..grant()
            },
            Case {
                name: "grant when reading fails",
                fail: Some(Call::Xattr),
                want: Err("os_error"),
                ..grant()
            },
            Case {
                name: "grant when writing fails",
                fail: Some(Call::SetXattr),
                want: Err("os_error"),
                ..grant()
            },
            Case {
                name: "revoke removes the capability",
                daemon: Some(granted()),
                ..revoke()
            },
            Case {
                name: "revoke again is unchanged",
                want: Ok(false),
                ..revoke()
            },
            Case {
                name: "revoke works on a nosuid mount",
                daemon: Some(granted().nosuid()),
                ..revoke()
            },
            Case {
                name: "revoke without alchemyd",
                daemon: None,
                want: Err("not_found"),
                ..revoke()
            },
            Case {
                name: "revoke refuses a symlink",
                daemon: Some(FakeFile::symlink()),
                want: Err("refused"),
                ..revoke()
            },
            Case {
                name: "revoke refuses a directory",
                daemon: Some(FakeFile::other()),
                want: Err("refused"),
                ..revoke()
            },
            Case {
                name: "revoke on macOS",
                platform: Platform::MacOs,
                daemon: Some(granted()),
                want: Err("unsupported"),
                want_xattr: Some(&NET_BIND_SERVICE_EP),
                ..revoke()
            },
            Case {
                name: "revoke when removing fails",
                daemon: Some(granted()),
                fail: Some(Call::RemoveXattr),
                want: Err("os_error"),
                want_xattr: Some(&NET_BIND_SERVICE_EP),
                ..revoke()
            },
        ];
        for c in cases {
            let mut sys = FakeSystem::new("/opt/alchemy/bin/alchemy-helper");
            if let Some(file) = c.daemon {
                sys = sys.with_file(DAEMON_PATH, file);
            }
            if let Some(call) = c.fail {
                sys = sys.failing(call);
            }
            let got = run(&c.operation, &sys, c.platform)
                .map(|o| o.changed)
                .map_err(|e| e.kind());
            assert_eq!(got, c.want, "{}", c.name);
            assert_eq!(
                sys.xattr_of(DAEMON_PATH, CAPABILITY_XATTR).as_deref(),
                c.want_xattr,
                "{}",
                c.name
            );
        }
    }

    #[test]
    fn grant_targets_only_the_sibling_of_the_canonical_exe() {
        let sys = FakeSystem::new("/opt/alchemy/bin/alchemy-helper")
            .with_file(DAEMON_PATH, FakeFile::regular())
            .with_file("/usr/local/bin/alchemyd", FakeFile::regular());
        run(&Operation::GrantLowPorts, &sys, Platform::Linux).unwrap();
        assert_eq!(sys.opened(), [PathBuf::from(DAEMON_PATH)]);
        assert_eq!(
            sys.xattr_of("/usr/local/bin/alchemyd", CAPABILITY_XATTR),
            None
        );
    }

    #[test]
    fn errors_name_the_target() {
        let sys = FakeSystem::new("/opt/alchemy/bin/alchemy-helper")
            .with_file(DAEMON_PATH, FakeFile::symlink());
        let err = run(&Operation::GrantLowPorts, &sys, Platform::Linux).unwrap_err();
        assert_eq!(
            err.to_string(),
            "/opt/alchemy/bin/alchemyd: refusing to change it: it is a symlink"
        );
        let err = run(&Operation::GrantLowPorts, &sys, Platform::MacOs).unwrap_err();
        assert_eq!(
            err.to_string(),
            "grant-low-ports is not supported on macOS: macOS lets any user bind ports below 1024"
        );
    }

    #[test]
    fn capability_bytes_match_setcap() {
        let words: Vec<u32> = NET_BIND_SERVICE_EP
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(words, [0x0200_0001, 1 << 10, 0, 0, 0]);
    }

    #[test]
    fn result_lines() {
        let check = Operation::Check;
        let grant = Operation::GrantLowPorts;
        let cases = [
            (
                Some(&check),
                Ok(Outcome { changed: false }),
                r#"{"result":"ok","operation":"check","changed":false}"#,
            ),
            (
                Some(&grant),
                Ok(Outcome { changed: true }),
                r#"{"result":"ok","operation":"grant-low-ports","changed":true}"#,
            ),
            (
                Some(&grant),
                Err(HelperError::NotFound(PathBuf::from(DAEMON_PATH))),
                r#"{"result":"error","operation":"grant-low-ports","error":"not_found"}"#,
            ),
            (
                Some(&check),
                Err(HelperError::NotRoot),
                r#"{"result":"error","operation":"check","error":"not_root"}"#,
            ),
            (
                None,
                Err(HelperError::Usage("bad".into())),
                r#"{"result":"error","operation":null,"error":"usage"}"#,
            ),
        ];
        for (operation, result, want) in cases {
            assert_eq!(result_line(operation, &result), want);
        }
    }

    #[test]
    fn report_writes_one_line_and_returns_the_exit() {
        let cases = [
            (Ok(Outcome { changed: true }), 0, ""),
            (
                Err(HelperError::NotRoot),
                77,
                "alchemy-helper: must be run as root\n",
            ),
            (
                Err(HelperError::Usage("error: bad\n\nUsage: x\n".into())),
                64,
                "error: bad\n\nUsage: x\n",
            ),
        ];
        for (result, code, want_err) in cases {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let exit = report(Some(&Operation::Check), &result, &mut out, &mut err);
            assert_eq!(exit.code(), code);
            let out = String::from_utf8(out).unwrap();
            assert_eq!(out.lines().count(), 1, "{out}");
            assert!(out.ends_with('\n'));
            assert_eq!(String::from_utf8(err).unwrap(), want_err);
        }
    }

    #[test]
    fn report_fails_when_stdout_does() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let exit = report(
            None,
            &Ok(Outcome { changed: false }),
            &mut Broken,
            &mut Vec::new(),
        );
        assert_eq!(exit, Exit::IoErr);
    }

    #[test]
    fn platform_current_matches_the_build() {
        assert_eq!(
            Platform::current() == Platform::Linux,
            cfg!(target_os = "linux")
        );
        assert_eq!(
            Platform::current() == Platform::MacOs,
            cfg!(target_os = "macos")
        );
    }
}
