//! `install-service` and `remove-service`: run the alchemyd next to the
//! helper at boot, as the user with the given UID.
//!
//! Linux gets a systemd system unit with a numeric `User=`, so the
//! service can only ever run as that UID. macOS gets a launch daemon with
//! `UserName`, after checking the name resolves back to the same UID.
//! Both are rendered from fixed templates, and the only values
//! interpolated are the UID, the GID, the user's home directory (macOS),
//! and the sibling alchemyd's canonical path, each validated first.
//!
//! The user's name, group, and home come from the user database as
//! `getent passwd` (Linux) or `id -P` (macOS) report them, since the
//! helper may run with an empty environment and no `SUDO_UID`.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use crate::sibling::{self, os};
use crate::system::{CommandOutput, FixedCommand, OpenError, System};
use crate::{HelperError, Outcome, Platform};

const SYSTEMCTL: &str = "/usr/bin/systemctl";
const GETENT: &str = "/usr/bin/getent";
const LAUNCHCTL: &str = "/bin/launchctl";
const ID: &str = "/usr/bin/id";

/// Exists only when systemd is the init system (`sd_booted(3)`).
const SYSTEMD_BOOTED: &str = "/run/systemd/system";
const SYSTEMD_UNIT_DIR: &str = "/etc/systemd/system";
const LAUNCH_DAEMONS_DIR: &str = "/Library/LaunchDaemons";
const LAUNCHD_LABEL_PREFIX: &str = "computer.alchemy.alchemyd";
const SERVICE_FILE_MODE: u32 = 0o644;

/// The user a service runs as, from the user database.
#[derive(Debug, Clone, PartialEq, Eq)]
struct User {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

/// Install, enable, and start the service, replacing an older
/// definition. Unchanged when it is already installed and running.
pub(crate) fn install<S: System>(
    system: &S,
    platform: Platform,
    uid: NonZeroU32,
) -> Result<Outcome, HelperError> {
    match platform {
        Platform::Linux => install_systemd(system, uid),
        Platform::MacOs => install_launchd(system, uid),
    }
}

/// Stop the service and delete its definition. Unchanged when it is not
/// installed.
pub(crate) fn remove<S: System>(
    system: &S,
    platform: Platform,
    uid: NonZeroU32,
) -> Result<Outcome, HelperError> {
    match platform {
        Platform::Linux => remove_systemd(system, uid),
        Platform::MacOs => remove_launchd(system, uid),
    }
}

fn unit_name(uid: NonZeroU32) -> String {
    format!("alchemyd-{uid}.service")
}

fn unit_path(uid: NonZeroU32) -> PathBuf {
    Path::new(SYSTEMD_UNIT_DIR).join(unit_name(uid))
}

fn launchd_label(uid: NonZeroU32) -> String {
    format!("{LAUNCHD_LABEL_PREFIX}.{uid}")
}

fn plist_path(uid: NonZeroU32) -> PathBuf {
    Path::new(LAUNCH_DAEMONS_DIR).join(format!("{}.plist", launchd_label(uid)))
}

fn install_systemd<S: System>(system: &S, uid: NonZeroU32) -> Result<Outcome, HelperError> {
    require_systemd(system, "install-service")?;
    let (daemon, _, _) = sibling::open(system)?;
    let user = lookup_user(system, Platform::Linux, uid)?;
    let unit = render_unit(uid, user.gid, &daemon)?;
    let wrote = replace(system, &unit_path(uid), unit.as_bytes())?;
    let name = unit_name(uid);
    if wrote {
        must(system, SYSTEMCTL, &["daemon-reload"])?;
    }
    let enabled = query(system, SYSTEMCTL, &["is-enabled", "--quiet", &name])?;
    if !enabled {
        must(system, SYSTEMCTL, &["enable", &name])?;
    }
    let active = query(system, SYSTEMCTL, &["is-active", "--quiet", &name])?;
    if !active {
        must(system, SYSTEMCTL, &["start", &name])?;
    } else if wrote {
        must(system, SYSTEMCTL, &["restart", &name])?;
    }
    Ok(Outcome {
        changed: wrote || !enabled || !active,
    })
}

fn remove_systemd<S: System>(system: &S, uid: NonZeroU32) -> Result<Outcome, HelperError> {
    require_systemd(system, "remove-service")?;
    let path = unit_path(uid);
    let installed = system
        .read_file(&path)
        .map_err(|source| os("read", &path, source))?
        .is_some();
    if !installed {
        return Ok(Outcome { changed: false });
    }
    let name = unit_name(uid);
    must(system, SYSTEMCTL, &["stop", &name])?;
    must(system, SYSTEMCTL, &["disable", &name])?;
    system
        .remove_file(&path)
        .map_err(|source| os("remove", &path, source))?;
    must(system, SYSTEMCTL, &["daemon-reload"])?;
    Ok(Outcome { changed: true })
}

fn install_launchd<S: System>(system: &S, uid: NonZeroU32) -> Result<Outcome, HelperError> {
    let (daemon, _, _) = sibling::open(system)?;
    let user = lookup_user(system, Platform::MacOs, uid)?;
    confirm_name(system, &user)?;
    let plist = render_plist(uid, &user, &daemon)?;
    let path = plist_path(uid);
    let wrote = replace(system, &path, plist.as_bytes())?;
    let target = format!("system/{}", launchd_label(uid));
    let loaded = query(system, LAUNCHCTL, &["print", &target])?;
    if loaded && wrote {
        must(system, LAUNCHCTL, &["bootout", &target])?;
    }
    if !loaded || wrote {
        let plist_arg = path.to_string_lossy();
        must(system, LAUNCHCTL, &["enable", &target])?;
        must(system, LAUNCHCTL, &["bootstrap", "system", &plist_arg])?;
    }
    Ok(Outcome {
        changed: wrote || !loaded,
    })
}

fn remove_launchd<S: System>(system: &S, uid: NonZeroU32) -> Result<Outcome, HelperError> {
    let target = format!("system/{}", launchd_label(uid));
    let loaded = query(system, LAUNCHCTL, &["print", &target])?;
    if loaded {
        must(system, LAUNCHCTL, &["bootout", &target])?;
    }
    let path = plist_path(uid);
    let existed = system
        .remove_file(&path)
        .map_err(|source| os("remove", &path, source))?;
    Ok(Outcome {
        changed: loaded || existed,
    })
}

fn require_systemd<S: System>(system: &S, operation: &'static str) -> Result<(), HelperError> {
    match system.open_nofollow(Path::new(SYSTEMD_BOOTED)) {
        Ok(_) => Ok(()),
        Err(OpenError::NotFound | OpenError::Symlink) => Err(HelperError::UnsupportedPlatform {
            operation,
            platform: Platform::Linux,
            why: "systemd is not the init system",
        }),
        Err(OpenError::Io(source)) => Err(os("open", Path::new(SYSTEMD_BOOTED), source)),
    }
}

/// Write `contents` to `path` unless it already holds exactly that.
/// Returns whether it wrote.
fn replace<S: System>(system: &S, path: &Path, contents: &[u8]) -> Result<bool, HelperError> {
    let current = system
        .read_file(path)
        .map_err(|source| os("read", path, source))?;
    if current.as_deref() == Some(contents) {
        return Ok(false);
    }
    system
        .write_file(path, contents, SERVICE_FILE_MODE)
        .map_err(|source| os("write", path, source))?;
    Ok(true)
}

/// Look `uid` up in the user database.
fn lookup_user<S: System>(
    system: &S,
    platform: Platform,
    uid: NonZeroU32,
) -> Result<User, HelperError> {
    let uid_arg = uid.to_string();
    let (program, args, not_found) = match platform {
        Platform::Linux => (GETENT, ["passwd", uid_arg.as_str()], 2),
        Platform::MacOs => (ID, ["-P", uid_arg.as_str()], 1),
    };
    let output = exec(system, program, &args)?;
    if output.code == Some(not_found) {
        return Err(HelperError::NoSuchUser(uid.get()));
    }
    if !output.success() {
        return Err(failed(program, &args, &output));
    }
    let user = String::from_utf8(output.stdout)
        .ok()
        .and_then(|text| parse_passwd(&text, platform))
        .ok_or(HelperError::RefusedUser {
            uid: uid.get(),
            why: "its user database entry is malformed",
        })?;
    if user.uid != uid.get() {
        return Err(HelperError::RefusedUser {
            uid: uid.get(),
            why: "the user database returned an entry for another UID",
        });
    }
    Ok(user)
}

/// Parse the first line of `getent passwd` output (Linux: seven fields)
/// or `id -P` output (macOS: ten fields, the BSD `master.passwd` form).
fn parse_passwd(text: &str, platform: Platform) -> Option<User> {
    let line = text.lines().next()?;
    let fields: Vec<&str> = line.split(':').collect();
    let (count, home) = match platform {
        Platform::Linux => (7, 5),
        Platform::MacOs => (10, 8),
    };
    if fields.len() != count {
        return None;
    }
    let name = (*fields.first()?).to_owned();
    if name.is_empty() || name.starts_with('-') || name.chars().any(char::is_control) {
        return None;
    }
    Some(User {
        name,
        uid: fields.get(2)?.parse().ok()?,
        gid: fields.get(3)?.parse().ok()?,
        home: (*fields.get(home)?).to_owned(),
    })
}

/// launchd resolves `UserName` itself, so check the name maps back to
/// the requested UID and not, say, to root through a duplicate entry.
fn confirm_name<S: System>(system: &S, user: &User) -> Result<(), HelperError> {
    let output = must(system, ID, &["-u", &user.name])?;
    let resolved = String::from_utf8_lossy(&output.stdout);
    if resolved.trim_end() == user.uid.to_string() {
        Ok(())
    } else {
        Err(HelperError::RefusedUser {
            uid: user.uid,
            why: "its name resolves to another UID",
        })
    }
}

/// Whether `path` can go in a systemd `ExecStart=` verbatim: no
/// whitespace, quotes, backslashes, `%` specifiers, or `$` variables.
fn unit_safe(path: &str) -> bool {
    path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._+-@".contains(&b))
}

/// The systemd unit for `uid`.
///
/// Per #110: `User=`/`Group=` are numeric so they cannot resolve to
/// another account; there is no `%U` (it is root's UID in a system unit)
/// and no `PrivateTmp`; the socket goes in a `RuntimeDirectory` because
/// the unit has no `XDG_RUNTIME_DIR`; and the ambient capability lets
/// alchemyd bind port 80 even under `NoNewPrivileges`, where a file
/// capability is ignored.
fn render_unit(uid: NonZeroU32, gid: u32, daemon: &Path) -> Result<String, HelperError> {
    let exec = daemon
        .to_str()
        .filter(|p| unit_safe(p))
        .ok_or_else(|| HelperError::Refused {
            path: daemon.to_path_buf(),
            why: "its path has characters a systemd unit would interpret",
        })?;
    Ok(format!(
        "\
# Installed by alchemy-helper install-service --uid {uid}.
# Remove it with alchemy-helper remove-service --uid {uid}.
[Unit]
Description=Alchemy daemon for UID {uid}
Wants=network-online.target
After=network-online.target docker.service

[Service]
Type=exec
User={uid}
Group={gid}
ExecStart={exec}
Environment=ALCHEMY_SOCKET=/run/alchemy-{uid}/alchemyd.sock
RuntimeDirectory=alchemy-{uid}
RuntimeDirectoryMode=0700
AmbientCapabilities=CAP_NET_BIND_SERVICE
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
"
    ))
}

/// Escape `value` for XML character data or an attribute.
fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The launch daemon property list for `uid`.
///
/// launchd sets no `TMPDIR` and may set no `HOME` for a `UserName`
/// daemon (#110), so `HOME` is explicit; with no `TMPDIR`, Rust's temp
/// dir falls back to the user's `confstr` temp dir, where the CLI looks
/// for the socket.
fn render_plist(uid: NonZeroU32, user: &User, daemon: &Path) -> Result<String, HelperError> {
    let program = daemon
        .to_str()
        .filter(|p| !p.chars().any(char::is_control))
        .ok_or_else(|| HelperError::Refused {
            path: daemon.to_path_buf(),
            why: "its path is not UTF-8 or has control characters",
        })?;
    if !user.home.starts_with('/') || user.home.chars().any(char::is_control) {
        return Err(HelperError::RefusedUser {
            uid: uid.get(),
            why: "its home directory is not an absolute path",
        });
    }
    Ok(format!(
        "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<!-- Installed by alchemy-helper install-service for UID {uid}. Undo it with remove-service. -->
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>{label}</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>{program}</string>
\t</array>
\t<key>UserName</key>
\t<string>{name}</string>
\t<key>EnvironmentVariables</key>
\t<dict>
\t\t<key>HOME</key>
\t\t<string>{home}</string>
\t</dict>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
</dict>
</plist>
",
        label = launchd_label(uid),
        program = xml_escape(program),
        name = xml_escape(&user.name),
        home = xml_escape(&user.home),
    ))
}

fn command(program: &'static str, args: &[&str]) -> Result<FixedCommand, HelperError> {
    FixedCommand::new(program, args.iter().map(|&a| a.to_owned()).collect()).ok_or_else(|| {
        os(
            "run",
            Path::new(program),
            std::io::Error::from(std::io::ErrorKind::InvalidInput),
        )
    })
}

/// Run a fixed command, whatever its exit status.
fn exec<S: System>(
    system: &S,
    program: &'static str,
    args: &[&str],
) -> Result<CommandOutput, HelperError> {
    system
        .run(&command(program, args)?)
        .map_err(|source| os("run", Path::new(program), source))
}

/// Run a fixed command and report whether it exited 0.
fn query<S: System>(system: &S, program: &'static str, args: &[&str]) -> Result<bool, HelperError> {
    Ok(exec(system, program, args)?.success())
}

/// Run a fixed command that must exit 0.
fn must<S: System>(
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

fn failed(program: &str, args: &[&str], output: &CommandOutput) -> HelperError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    HelperError::CommandFailed {
        command: std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" "),
        status: output
            .code
            .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
        stderr: if stderr.is_empty() {
            "no output".to_owned()
        } else {
            stderr
        },
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
    use crate::fake::{Call, FakeFile, FakeSystem};

    const HELPER: &str = "/opt/alchemy/bin/alchemy-helper";
    const DAEMON_PATH: &str = "/opt/alchemy/bin/alchemyd";
    const UNIT: &str = "/etc/systemd/system/alchemyd-1000.service";
    const PLIST: &str = "/Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist";
    const GETENT_LINE: &str = "u:x:1000:1000:U,,,:/home/u:/bin/bash\n";
    const ID_LINE: &str = "jo:********:501:20::0:0:Jo Doe:/Users/jo:/bin/zsh\n";

    const WANT_UNIT: &str = "\
# Installed by alchemy-helper install-service --uid 1000.
# Remove it with alchemy-helper remove-service --uid 1000.
[Unit]
Description=Alchemy daemon for UID 1000
Wants=network-online.target
After=network-online.target docker.service

[Service]
Type=exec
User=1000
Group=1000
ExecStart=/opt/alchemy/bin/alchemyd
Environment=ALCHEMY_SOCKET=/run/alchemy-1000/alchemyd.sock
RuntimeDirectory=alchemy-1000
RuntimeDirectoryMode=0700
AmbientCapabilities=CAP_NET_BIND_SERVICE
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
";

    const WANT_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Installed by alchemy-helper install-service for UID 501. Undo it with remove-service. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>computer.alchemy.alchemyd.501</string>
	<key>ProgramArguments</key>
	<array>
		<string>/opt/alchemy/bin/alchemyd</string>
	</array>
	<key>UserName</key>
	<string>jo</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>HOME</key>
		<string>/Users/jo</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
</dict>
</plist>
"#;

    fn uid(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    fn file(contents: &str) -> FakeFile {
        FakeFile::with_contents(contents)
    }

    /// A Linux host booted with systemd, alchemyd next to the helper, and
    /// user 1000 in the user database.
    fn linux() -> FakeSystem {
        FakeSystem::new(HELPER)
            .with_file(SYSTEMD_BOOTED, FakeFile::other())
            .with_file(DAEMON_PATH, FakeFile::regular())
            .responding(&[GETENT, "passwd", "1000"], 0, GETENT_LINE)
    }

    /// `linux()` where the unit is not enabled or running.
    fn linux_stopped() -> FakeSystem {
        linux()
            .responding(
                &[SYSTEMCTL, "is-enabled", "--quiet", "alchemyd-1000.service"],
                1,
                "",
            )
            .responding(
                &[SYSTEMCTL, "is-active", "--quiet", "alchemyd-1000.service"],
                3,
                "",
            )
    }

    /// A macOS host with alchemyd next to the helper and user 501.
    fn macos() -> FakeSystem {
        FakeSystem::new(HELPER)
            .with_file(DAEMON_PATH, FakeFile::regular())
            .responding(&[ID, "-P", "501"], 0, ID_LINE)
            .responding(&[ID, "-u", "jo"], 0, "501\n")
    }

    /// `macos()` where the daemon is not loaded.
    fn macos_unloaded() -> FakeSystem {
        macos().responding(
            &[LAUNCHCTL, "print", "system/computer.alchemy.alchemyd.501"],
            113,
            "",
        )
    }

    fn argv(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    struct Case {
        name: &'static str,
        system: FakeSystem,
        platform: Platform,
        install: bool,
        uid: u32,
        want: Result<bool, &'static str>,
        commands: &'static [&'static str],
        path: &'static str,
        contents: Option<&'static str>,
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn install_and_remove() {
        let cases = [
            Case {
                name: "linux install from scratch",
                system: linux_stopped(),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Ok(true),
                commands: &[
                    "/usr/bin/getent passwd 1000",
                    "/usr/bin/systemctl daemon-reload",
                    "/usr/bin/systemctl is-enabled --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl enable alchemyd-1000.service",
                    "/usr/bin/systemctl is-active --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl start alchemyd-1000.service",
                ],
                path: UNIT,
                contents: Some(WANT_UNIT),
            },
            Case {
                name: "linux install again is unchanged",
                system: linux().with_file(UNIT, file(WANT_UNIT)),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Ok(false),
                commands: &[
                    "/usr/bin/getent passwd 1000",
                    "/usr/bin/systemctl is-enabled --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl is-active --quiet alchemyd-1000.service",
                ],
                path: UNIT,
                contents: Some(WANT_UNIT),
            },
            Case {
                name: "linux install over an old unit restarts it",
                system: linux().with_file(UNIT, file("old")),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Ok(true),
                commands: &[
                    "/usr/bin/getent passwd 1000",
                    "/usr/bin/systemctl daemon-reload",
                    "/usr/bin/systemctl is-enabled --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl is-active --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl restart alchemyd-1000.service",
                ],
                path: UNIT,
                contents: Some(WANT_UNIT),
            },
            Case {
                name: "linux install starts a stopped, unchanged unit",
                system: linux().with_file(UNIT, file(WANT_UNIT)).responding(
                    &[SYSTEMCTL, "is-active", "--quiet", "alchemyd-1000.service"],
                    3,
                    "",
                ),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Ok(true),
                commands: &[
                    "/usr/bin/getent passwd 1000",
                    "/usr/bin/systemctl is-enabled --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl is-active --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl start alchemyd-1000.service",
                ],
                path: UNIT,
                contents: Some(WANT_UNIT),
            },
            Case {
                name: "linux install for an unknown user",
                system: linux().responding(&[GETENT, "passwd", "4242"], 2, ""),
                platform: Platform::Linux,
                install: true,
                uid: 4242,
                want: Err("no_such_user"),
                commands: &["/usr/bin/getent passwd 4242"],
                path: "/etc/systemd/system/alchemyd-4242.service",
                contents: None,
            },
            Case {
                name: "linux install when getent fails",
                system: linux().responding(&[GETENT, "passwd", "1000"], 1, ""),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("command_failed"),
                commands: &["/usr/bin/getent passwd 1000"],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux install when getent answers for another uid",
                system: linux().responding(
                    &[GETENT, "passwd", "1000"],
                    0,
                    "root:x:0:0:root:/root:/bin/sh\n",
                ),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("refused"),
                commands: &["/usr/bin/getent passwd 1000"],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux install with a malformed entry",
                system: linux().responding(&[GETENT, "passwd", "1000"], 0, "u:x:1000\n"),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("refused"),
                commands: &["/usr/bin/getent passwd 1000"],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux install without systemd",
                system: FakeSystem::new(HELPER).with_file(DAEMON_PATH, FakeFile::regular()),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("unsupported"),
                commands: &[],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux install without alchemyd",
                system: FakeSystem::new(HELPER).with_file(SYSTEMD_BOOTED, FakeFile::other()),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("not_found"),
                commands: &[],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux install refuses a symlinked alchemyd",
                system: linux().with_file(DAEMON_PATH, FakeFile::symlink()),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("refused"),
                commands: &[],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux install when enabling fails",
                system: linux_stopped().responding(
                    &[SYSTEMCTL, "enable", "alchemyd-1000.service"],
                    1,
                    "",
                ),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("command_failed"),
                commands: &[
                    "/usr/bin/getent passwd 1000",
                    "/usr/bin/systemctl daemon-reload",
                    "/usr/bin/systemctl is-enabled --quiet alchemyd-1000.service",
                    "/usr/bin/systemctl enable alchemyd-1000.service",
                ],
                path: UNIT,
                contents: Some(WANT_UNIT),
            },
            Case {
                name: "linux install when writing fails",
                system: linux_stopped().failing(Call::WriteFile),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("os_error"),
                commands: &["/usr/bin/getent passwd 1000"],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux install when commands cannot run",
                system: linux().failing(Call::Run),
                platform: Platform::Linux,
                install: true,
                uid: 1000,
                want: Err("os_error"),
                commands: &[],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux remove",
                system: linux().with_file(UNIT, file(WANT_UNIT)),
                platform: Platform::Linux,
                install: false,
                uid: 1000,
                want: Ok(true),
                commands: &[
                    "/usr/bin/systemctl stop alchemyd-1000.service",
                    "/usr/bin/systemctl disable alchemyd-1000.service",
                    "/usr/bin/systemctl daemon-reload",
                ],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux remove again is unchanged",
                system: linux(),
                platform: Platform::Linux,
                install: false,
                uid: 1000,
                want: Ok(false),
                commands: &[],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "linux remove for a deleted user still works",
                system: linux()
                    .with_file("/etc/systemd/system/alchemyd-4242.service", file("x"))
                    .responding(&[GETENT, "passwd", "4242"], 2, ""),
                platform: Platform::Linux,
                install: false,
                uid: 4242,
                want: Ok(true),
                commands: &[
                    "/usr/bin/systemctl stop alchemyd-4242.service",
                    "/usr/bin/systemctl disable alchemyd-4242.service",
                    "/usr/bin/systemctl daemon-reload",
                ],
                path: "/etc/systemd/system/alchemyd-4242.service",
                contents: None,
            },
            Case {
                name: "linux remove keeps the unit when stopping fails",
                system: linux().with_file(UNIT, file(WANT_UNIT)).responding(
                    &[SYSTEMCTL, "stop", "alchemyd-1000.service"],
                    1,
                    "",
                ),
                platform: Platform::Linux,
                install: false,
                uid: 1000,
                want: Err("command_failed"),
                commands: &["/usr/bin/systemctl stop alchemyd-1000.service"],
                path: UNIT,
                contents: Some(WANT_UNIT),
            },
            Case {
                name: "linux remove without systemd",
                system: FakeSystem::new(HELPER),
                platform: Platform::Linux,
                install: false,
                uid: 1000,
                want: Err("unsupported"),
                commands: &[],
                path: UNIT,
                contents: None,
            },
            Case {
                name: "macos install from scratch",
                system: macos_unloaded(),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Ok(true),
                commands: &[
                    "/usr/bin/id -P 501",
                    "/usr/bin/id -u jo",
                    "/bin/launchctl print system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl enable system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl bootstrap system /Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist",
                ],
                path: PLIST,
                contents: Some(WANT_PLIST),
            },
            Case {
                name: "macos install again is unchanged",
                system: macos().with_file(PLIST, file(WANT_PLIST)),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Ok(false),
                commands: &[
                    "/usr/bin/id -P 501",
                    "/usr/bin/id -u jo",
                    "/bin/launchctl print system/computer.alchemy.alchemyd.501",
                ],
                path: PLIST,
                contents: Some(WANT_PLIST),
            },
            Case {
                name: "macos install over an old plist reloads it",
                system: macos().with_file(PLIST, file("old")),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Ok(true),
                commands: &[
                    "/usr/bin/id -P 501",
                    "/usr/bin/id -u jo",
                    "/bin/launchctl print system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl bootout system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl enable system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl bootstrap system /Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist",
                ],
                path: PLIST,
                contents: Some(WANT_PLIST),
            },
            Case {
                name: "macos install loads an unchanged, unloaded plist",
                system: macos_unloaded().with_file(PLIST, file(WANT_PLIST)),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Ok(true),
                commands: &[
                    "/usr/bin/id -P 501",
                    "/usr/bin/id -u jo",
                    "/bin/launchctl print system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl enable system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl bootstrap system /Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist",
                ],
                path: PLIST,
                contents: Some(WANT_PLIST),
            },
            Case {
                name: "macos install for an unknown user",
                system: macos().responding(&[ID, "-P", "777"], 1, ""),
                platform: Platform::MacOs,
                install: true,
                uid: 777,
                want: Err("no_such_user"),
                commands: &["/usr/bin/id -P 777"],
                path: "/Library/LaunchDaemons/computer.alchemy.alchemyd.777.plist",
                contents: None,
            },
            Case {
                name: "macos install when the name resolves elsewhere",
                system: macos().responding(&[ID, "-u", "jo"], 0, "0\n"),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Err("refused"),
                commands: &["/usr/bin/id -P 501", "/usr/bin/id -u jo"],
                path: PLIST,
                contents: None,
            },
            Case {
                name: "macos install with a relative home",
                system: macos().responding(
                    &[ID, "-P", "501"],
                    0,
                    "jo:*:501:20::0:0:Jo:Users/jo:/bin/zsh\n",
                ),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Err("refused"),
                commands: &["/usr/bin/id -P 501", "/usr/bin/id -u jo"],
                path: PLIST,
                contents: None,
            },
            Case {
                name: "macos install when bootstrap fails",
                system: macos_unloaded().responding(
                    &[
                        LAUNCHCTL,
                        "bootstrap",
                        "system",
                        "/Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist",
                    ],
                    5,
                    "",
                ),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Err("command_failed"),
                commands: &[
                    "/usr/bin/id -P 501",
                    "/usr/bin/id -u jo",
                    "/bin/launchctl print system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl enable system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl bootstrap system /Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist",
                ],
                path: PLIST,
                contents: Some(WANT_PLIST),
            },
            Case {
                name: "macos install without alchemyd",
                system: FakeSystem::new(HELPER),
                platform: Platform::MacOs,
                install: true,
                uid: 501,
                want: Err("not_found"),
                commands: &[],
                path: PLIST,
                contents: None,
            },
            Case {
                name: "macos remove",
                system: macos().with_file(PLIST, file(WANT_PLIST)),
                platform: Platform::MacOs,
                install: false,
                uid: 501,
                want: Ok(true),
                commands: &[
                    "/bin/launchctl print system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl bootout system/computer.alchemy.alchemyd.501",
                ],
                path: PLIST,
                contents: None,
            },
            Case {
                name: "macos remove again is unchanged",
                system: macos_unloaded(),
                platform: Platform::MacOs,
                install: false,
                uid: 501,
                want: Ok(false),
                commands: &["/bin/launchctl print system/computer.alchemy.alchemyd.501"],
                path: PLIST,
                contents: None,
            },
            Case {
                name: "macos remove deletes an unloaded plist",
                system: macos_unloaded().with_file(PLIST, file(WANT_PLIST)),
                platform: Platform::MacOs,
                install: false,
                uid: 501,
                want: Ok(true),
                commands: &["/bin/launchctl print system/computer.alchemy.alchemyd.501"],
                path: PLIST,
                contents: None,
            },
            Case {
                name: "macos remove keeps the plist when bootout fails",
                system: macos().with_file(PLIST, file(WANT_PLIST)).responding(
                    &[LAUNCHCTL, "bootout", "system/computer.alchemy.alchemyd.501"],
                    1,
                    "",
                ),
                platform: Platform::MacOs,
                install: false,
                uid: 501,
                want: Err("command_failed"),
                commands: &[
                    "/bin/launchctl print system/computer.alchemy.alchemyd.501",
                    "/bin/launchctl bootout system/computer.alchemy.alchemyd.501",
                ],
                path: PLIST,
                contents: Some(WANT_PLIST),
            },
        ];
        for c in cases {
            let got = if c.install {
                install(&c.system, c.platform, uid(c.uid))
            } else {
                remove(&c.system, c.platform, uid(c.uid))
            }
            .map(|o| o.changed)
            .map_err(|e| e.kind());
            assert_eq!(got, c.want, "{}", c.name);
            let want: Vec<Vec<String>> = c.commands.iter().map(|l| argv(l)).collect();
            assert_eq!(c.system.argvs(), want, "{}", c.name);
            let file = c.system.file(c.path);
            assert_eq!(
                file.as_ref()
                    .map(|f| String::from_utf8(f.contents.clone()).unwrap())
                    .as_deref(),
                c.contents,
                "{}",
                c.name
            );
            if c.want.is_ok()
                && let Some(f) = file
            {
                assert_eq!(f.mode, 0o644, "{}", c.name);
            }
        }
    }

    #[test]
    fn errors_explain_themselves() {
        let sys =
            linux_stopped().responding(&[SYSTEMCTL, "enable", "alchemyd-1000.service"], 1, "");
        let err = install(&sys, Platform::Linux, uid(1000)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "`/usr/bin/systemctl enable alchemyd-1000.service` failed (exit 1): \
             /usr/bin/systemctl enable alchemyd-1000.service failed"
        );
        let err = install(&FakeSystem::new(HELPER), Platform::Linux, uid(1000)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "install-service is not supported on Linux: systemd is not the init system"
        );
        let sys = linux().responding(&[GETENT, "passwd", "4242"], 2, "");
        let err = install(&sys, Platform::Linux, uid(4242)).unwrap_err();
        assert_eq!(err.to_string(), "no user has uid 4242");
    }

    #[test]
    fn unit_paths_must_be_literal() {
        let cases = [
            ("/opt/alchemy/bin/alchemyd", true),
            ("/usr/lib/alchemy-1.2+git/alchemyd", true),
            ("/home/u/.cargo/bin/alchemyd", true),
            ("/home/u/my apps/alchemyd", false),
            ("/home/u/100%/alchemyd", false),
            ("/home/$USER/alchemyd", false),
            ("/home/u/\"x\"/alchemyd", false),
            ("/home/u/a\\b/alchemyd", false),
            ("/home/u/a;b/alchemyd", false),
            ("/home/u/a\nExecStartPre=/bin/sh/alchemyd", false),
            ("/home/ü/alchemyd", false),
            ("relative/alchemyd", false),
        ];
        for (path, ok) in cases {
            assert_eq!(unit_safe(path), ok, "{path:?}");
            assert_eq!(
                render_unit(uid(1000), 1000, Path::new(path)).is_ok(),
                ok,
                "{path:?}"
            );
        }
    }

    #[test]
    fn plist_values_are_escaped_or_refused() {
        let user = |name: &str, home: &str| User {
            name: name.to_owned(),
            uid: 501,
            gid: 20,
            home: home.to_owned(),
        };
        let plist = render_plist(
            uid(501),
            &user("a&b", "/Users/<x> \"y\" 'z'"),
            Path::new("/Applications/Alchemy & Co.app/Contents/MacOS/alchemyd"),
        )
        .unwrap();
        assert!(plist.contains("<string>a&amp;b</string>"), "{plist}");
        assert!(
            plist.contains("<string>/Users/&lt;x&gt; &quot;y&quot; &apos;z&apos;</string>"),
            "{plist}"
        );
        assert!(
            plist.contains(
                "<string>/Applications/Alchemy &amp; Co.app/Contents/MacOS/alchemyd</string>"
            ),
            "{plist}"
        );

        let refused = [
            (user("jo", "/Users/jo"), "/opt/a\nb/alchemyd"),
            (user("jo", "/Users/jo\n"), DAEMON_PATH),
            (user("jo", "Users/jo"), DAEMON_PATH),
            (user("jo", ""), DAEMON_PATH),
        ];
        for (user, daemon) in refused {
            assert_eq!(
                render_plist(uid(501), &user, Path::new(daemon))
                    .map_err(|e| e.kind())
                    .unwrap_err(),
                "refused",
                "{user:?} {daemon:?}"
            );
        }
    }

    #[test]
    fn parses_passwd_entries() {
        let u = |name: &str, uid, gid, home: &str| {
            Some(User {
                name: name.to_owned(),
                uid,
                gid,
                home: home.to_owned(),
            })
        };
        let cases = [
            (Platform::Linux, GETENT_LINE, u("u", 1000, 1000, "/home/u")),
            (
                Platform::Linux,
                "svc:x:1001:1001::/nonexistent:/usr/sbin/nologin",
                u("svc", 1001, 1001, "/nonexistent"),
            ),
            (Platform::MacOs, ID_LINE, u("jo", 501, 20, "/Users/jo")),
            (Platform::Linux, ID_LINE, None),
            (Platform::MacOs, GETENT_LINE, None),
            (Platform::Linux, "", None),
            (Platform::Linux, ":x:1000:1000::/home/u:/bin/sh", None),
            (Platform::Linux, "-u:x:1000:1000::/home/u:/bin/sh", None),
            (Platform::Linux, "u\t:x:1000:1000::/home/u:/bin/sh", None),
            (Platform::Linux, "u:x:abc:1000::/home/u:/bin/sh", None),
            (Platform::Linux, "u:x:1000:-1::/home/u:/bin/sh", None),
        ];
        for (platform, text, want) in cases {
            assert_eq!(parse_passwd(text, platform), want, "{platform} {text:?}");
        }
    }

    #[test]
    fn rendered_unit_and_plist_match_the_templates() {
        assert_eq!(
            render_unit(uid(1000), 1000, Path::new(DAEMON_PATH)).unwrap(),
            WANT_UNIT
        );
        let user = parse_passwd(ID_LINE, Platform::MacOs).unwrap();
        assert_eq!(
            render_plist(uid(501), &user, Path::new(DAEMON_PATH)).unwrap(),
            WANT_PLIST
        );
    }
}
