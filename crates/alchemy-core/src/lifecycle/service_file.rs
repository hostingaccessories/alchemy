//! The per-user service files: a launch agent on macOS and a
//! `systemd --user` unit on Linux, both for `login` autostart.
//!
//! They are rendered from fixed templates, like `alchemy-helper`'s system
//! service, and the only values interpolated are alchemyd's absolute path
//! and, on macOS, the user's home for the log paths. A unit takes a path
//! verbatim only if systemd would not interpret any of it; a plist takes
//! any path without control characters, escaped as XML.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::route::HostOs;

/// The launch agent's label. The system service's launch daemon is
/// `computer.alchemy.alchemyd.<uid>`, so the two never collide.
pub const LAUNCH_AGENT_LABEL: &str = "computer.alchemy.alchemyd";

/// The `systemd --user` unit's name.
pub const SYSTEMD_USER_UNIT: &str = "alchemyd.service";

/// Where the macOS logs go, relative to the user's home. launchd does not
/// create it, so the daemon does before loading a plist.
pub const LOG_DIR: &str = "Library/Logs/Alchemy";

/// The log file's name in [`LOG_DIR`].
pub const LOG_FILE: &str = "alchemyd.log";

/// Where the system service's launch daemon logs on macOS. It is
/// root-owned all the way up: the root launchd opens the log, and a path
/// under the user's home could be a symlink the user planted (#112).
pub const SYSTEM_LOG_DIR: &str = "/Library/Logs/Alchemy";

/// The bundle ID of the desktop app, which the launch agent names as
/// responsible for it, so macOS can attribute it to Alchemy in Login
/// Items and Local Network privacy (#112).
pub const DESKTOP_BUNDLE_ID: &str = "computer.alchemy.desktop";

/// Why a service file could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ServiceFileError {
    /// The path is relative, and a service has no working directory to
    /// resolve it against.
    #[error("{} is not an absolute path", .path.display())]
    Relative {
        /// The path.
        path: PathBuf,
    },
    /// The path is not UTF-8 or has control characters.
    #[error("{} is not UTF-8 or has control characters", .path.display())]
    Unprintable {
        /// The path.
        path: PathBuf,
    },
    /// The path has characters a systemd unit would interpret, such as
    /// whitespace, quotes, `%`, or `$`.
    #[error("{} has characters a systemd unit would interpret", .path.display())]
    UnitUnsafe {
        /// The path.
        path: PathBuf,
    },
}

/// The launch agent's path for the user with `home`.
pub fn launch_agent_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

/// The `systemd --user` unit's path, under `$XDG_CONFIG_HOME` when it is
/// set to an absolute path and `~/.config` otherwise, as the XDG Base
/// Directory spec says and systemd's user search path follows.
pub fn systemd_user_unit_path(xdg_config_home: Option<&Path>, home: &Path) -> PathBuf {
    xdg_config_home
        .filter(|dir| dir.is_absolute())
        .map_or_else(|| home.join(".config"), Path::to_path_buf)
        .join("systemd/user")
        .join(SYSTEMD_USER_UNIT)
}

/// The per-user service file on `os` for the user with `home`: the
/// launch agent on macOS, the `systemd --user` unit on Linux.
pub fn user_service_path(os: HostOs, xdg_config_home: Option<&Path>, home: &Path) -> PathBuf {
    match os {
        HostOs::MacOs => launch_agent_path(home),
        HostOs::Linux => systemd_user_unit_path(xdg_config_home, home),
    }
}

/// The system service file `alchemy-helper install-service --uid <uid>`
/// writes on `os`: a systemd system unit on Linux, a launch daemon on
/// macOS. Its existence is what "installed" means.
pub fn system_service_path(os: HostOs, uid: u32) -> PathBuf {
    match os {
        HostOs::Linux => PathBuf::from(format!("/etc/systemd/system/alchemyd-{uid}.service")),
        HostOs::MacOs => PathBuf::from(format!(
            "/Library/LaunchDaemons/{LAUNCH_AGENT_LABEL}.{uid}.plist"
        )),
    }
}

/// The macOS log file for the user with `home`.
pub fn log_path(home: &Path) -> PathBuf {
    home.join(LOG_DIR).join(LOG_FILE)
}

/// The system service's macOS log file for the user with `uid`, as
/// `alchemy-helper install-service` writes it.
pub fn system_log_path(uid: u32) -> PathBuf {
    Path::new(SYSTEM_LOG_DIR).join(format!("alchemyd.{uid}.log"))
}

/// Why alchemyd's path can't back a service: it is gone once the app
/// that holds it quits (#112).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransientInstall {
    /// alchemyd runs from inside an `AppImage`, a FUSE mount that exists
    /// only while the `AppImage` runs.
    AppImage,
    /// macOS App Translocation runs the app from a random read-only copy,
    /// because it was opened where it was downloaded.
    Translocated,
}

impl fmt::Display for TransientInstall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AppImage => {
                "Alchemy runs from an AppImage, which a service can't start. Install the .deb package to start Alchemy on its own."
            }
            Self::Translocated => {
                "macOS is running Alchemy from a temporary copy. Move Alchemy to the Applications folder first."
            }
        })
    }
}

/// Whether the alchemyd at `alchemyd`, its canonical path, is somewhere
/// a service can't point at. `appimage` is whether the `AppImage` runtime's
/// `APPIMAGE` variable is set.
pub fn transient_install(alchemyd: &Path, appimage: bool) -> Option<TransientInstall> {
    let mut components = alchemyd.components().filter_map(|c| match c {
        Component::Normal(name) => name.to_str(),
        _ => None,
    });
    let first_two = (components.next(), components.next());
    if appimage || matches!(first_two, (Some("tmp"), Some(mount)) if mount.starts_with(".mount_")) {
        return Some(TransientInstall::AppImage);
    }
    alchemyd
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
        .then_some(TransientInstall::Translocated)
}

/// An absolute UTF-8 path with no control characters.
fn printable(path: &Path) -> Result<&str, ServiceFileError> {
    if !path.is_absolute() {
        return Err(ServiceFileError::Relative {
            path: path.to_path_buf(),
        });
    }
    path.to_str()
        .filter(|p| !p.chars().any(char::is_control))
        .ok_or_else(|| ServiceFileError::Unprintable {
            path: path.to_path_buf(),
        })
}

/// Whether `path` can go in a systemd `ExecStart=` verbatim: no
/// whitespace, quotes, backslashes, `%` specifiers, or `$` variables.
/// The same rule as the helper's.
fn unit_safe(path: &str) -> bool {
    path.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._+-@".contains(&b))
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

/// The launch agent property list for the alchemyd at `alchemyd`, logging
/// to [`log_path`] under `home`.
///
/// `KeepAlive` with `SuccessfulExit` false restarts alchemyd after a crash
/// but not after a clean exit, such as a mode switch handing over to the
/// system service.
pub fn launch_agent_plist(alchemyd: &Path, home: &Path) -> Result<String, ServiceFileError> {
    let program = printable(alchemyd)?;
    printable(home)?;
    let log = log_path(home);
    let log = printable(&log)?;
    Ok(format!(
        "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<!-- Installed by alchemyd service install. Undo it with alchemyd service remove. -->
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>{LAUNCH_AGENT_LABEL}</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>{program}</string>
\t</array>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
\t<key>StandardOutPath</key>
\t<string>{log}</string>
\t<key>StandardErrorPath</key>
\t<string>{log}</string>
\t<key>AssociatedBundleIdentifiers</key>
\t<array>
\t\t<string>{DESKTOP_BUNDLE_ID}</string>
\t</array>
</dict>
</plist>
",
        program = xml_escape(program),
        log = xml_escape(log),
    ))
}

/// The `systemd --user` unit for the alchemyd at `alchemyd`. It logs to
/// the journal.
///
/// A user unit has `XDG_RUNTIME_DIR`, so alchemyd's default socket path
/// works and no `Environment=` is needed. `Restart=on-failure` restarts
/// it after a crash but not after a clean exit.
pub fn systemd_user_unit(alchemyd: &Path) -> Result<String, ServiceFileError> {
    let exec = printable(alchemyd)?;
    if !unit_safe(exec) {
        return Err(ServiceFileError::UnitUnsafe {
            path: alchemyd.to_path_buf(),
        });
    }
    Ok(format!(
        "\
# Installed by alchemyd service install.
# Remove it with alchemyd service remove.
[Unit]
Description=Alchemy daemon

[Service]
Type=exec
ExecStart={exec}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"
    ))
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

    const WANT_PLIST: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<!-- Installed by alchemyd service install. Undo it with alchemyd service remove. -->
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>computer.alchemy.alchemyd</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/Applications/Alchemy.app/Contents/MacOS/alchemyd</string>
\t</array>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
\t<key>StandardOutPath</key>
\t<string>/Users/jo/Library/Logs/Alchemy/alchemyd.log</string>
\t<key>StandardErrorPath</key>
\t<string>/Users/jo/Library/Logs/Alchemy/alchemyd.log</string>
\t<key>AssociatedBundleIdentifiers</key>
\t<array>
\t\t<string>computer.alchemy.desktop</string>
\t</array>
</dict>
</plist>
";

    const WANT_UNIT: &str = "\
# Installed by alchemyd service install.
# Remove it with alchemyd service remove.
[Unit]
Description=Alchemy daemon

[Service]
Type=exec
ExecStart=/home/u/.local/bin/alchemyd
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
";

    #[test]
    fn renders_the_launch_agent() {
        let plist = launch_agent_plist(
            Path::new("/Applications/Alchemy.app/Contents/MacOS/alchemyd"),
            Path::new("/Users/jo"),
        )
        .unwrap();
        assert_eq!(plist, WANT_PLIST);
    }

    #[test]
    fn renders_the_user_unit() {
        let unit = systemd_user_unit(Path::new("/home/u/.local/bin/alchemyd")).unwrap();
        assert_eq!(unit, WANT_UNIT);
    }

    #[test]
    fn plist_escapes_xml() {
        let cases = [
            ("/Users/jo/My Apps/alchemyd", "/Users/jo/My Apps/alchemyd"),
            ("/opt/a&b/alchemyd", "/opt/a&amp;b/alchemyd"),
            ("/opt/<x>/alchemyd", "/opt/&lt;x&gt;/alchemyd"),
            ("/opt/\"q'/alchemyd", "/opt/&quot;q&apos;/alchemyd"),
            ("/opt/%h$HOME/alchemyd", "/opt/%h$HOME/alchemyd"),
        ];
        for (path, want) in cases {
            let plist = launch_agent_plist(Path::new(path), Path::new("/Users/jo")).unwrap();
            assert!(
                plist.contains(&format!("\t\t<string>{want}</string>\n")),
                "{path}: {plist}"
            );
        }
        let plist =
            launch_agent_plist(Path::new("/opt/alchemyd"), Path::new("/Users/a&b")).unwrap();
        assert!(
            plist.contains("<string>/Users/a&amp;b/Library/Logs/Alchemy/alchemyd.log</string>"),
            "{plist}"
        );
    }

    #[test]
    fn plist_refuses_bad_paths() {
        let cases = [
            (
                "alchemyd",
                "/Users/jo",
                ServiceFileError::Relative {
                    path: "alchemyd".into(),
                },
            ),
            (
                "./bin/alchemyd",
                "/Users/jo",
                ServiceFileError::Relative {
                    path: "./bin/alchemyd".into(),
                },
            ),
            (
                "/opt/alchemyd",
                "Users/jo",
                ServiceFileError::Relative {
                    path: "Users/jo".into(),
                },
            ),
            (
                "/opt/a\nb/alchemyd",
                "/Users/jo",
                ServiceFileError::Unprintable {
                    path: "/opt/a\nb/alchemyd".into(),
                },
            ),
            (
                "/opt/alchemyd",
                "/Users/j\to",
                ServiceFileError::Unprintable {
                    path: "/Users/j\to".into(),
                },
            ),
            (
                "/opt/\u{7f}/alchemyd",
                "/Users/jo",
                ServiceFileError::Unprintable {
                    path: "/opt/\u{7f}/alchemyd".into(),
                },
            ),
        ];
        for (daemon, home, want) in cases {
            assert_eq!(
                launch_agent_plist(Path::new(daemon), Path::new(home)),
                Err(want),
                "{daemon:?} {home:?}"
            );
        }
    }

    #[test]
    fn unit_refuses_bad_paths() {
        let unsafe_paths = [
            "/home/u/My Apps/alchemyd",
            "/home/u/%h/alchemyd",
            "/home/u/$HOME/alchemyd",
            "/home/u/\"q\"/alchemyd",
            "/home/u/back\\slash/alchemyd",
            "/home/u/semi;colon/alchemyd",
            "/home/ü/alchemyd",
        ];
        for path in unsafe_paths {
            assert_eq!(
                systemd_user_unit(Path::new(path)),
                Err(ServiceFileError::UnitUnsafe { path: path.into() }),
                "{path}"
            );
        }
        let cases = [
            (
                "alchemyd",
                ServiceFileError::Relative {
                    path: "alchemyd".into(),
                },
            ),
            (
                "/home/u/a\nb",
                ServiceFileError::Unprintable {
                    path: "/home/u/a\nb".into(),
                },
            ),
        ];
        for (path, want) in cases {
            assert_eq!(systemd_user_unit(Path::new(path)), Err(want), "{path:?}");
        }
        for ok in [
            "/usr/bin/alchemyd",
            "/home/u/.cargo/bin/alchemyd",
            "/opt/alchemy-1.2+3/bin/alchemyd",
            "/home/u@x/bin/alchemyd",
        ] {
            assert!(systemd_user_unit(Path::new(ok)).is_ok(), "{ok}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(OsStr::from_bytes(b"/opt/\xff/alchemyd"));
        let want = Err(ServiceFileError::Unprintable { path: path.into() });
        assert_eq!(systemd_user_unit(path), want);
        assert_eq!(launch_agent_plist(path, Path::new("/Users/jo")), want);
    }

    #[test]
    fn transient_installs() {
        let cases = [
            ("/usr/bin/alchemyd", false, None),
            (
                "/Applications/Alchemy.app/Contents/MacOS/alchemyd",
                false,
                None,
            ),
            (
                "/tmp/.mount_AlchemXYZ/usr/bin/alchemyd",
                false,
                Some(TransientInstall::AppImage),
            ),
            ("/usr/bin/alchemyd", true, Some(TransientInstall::AppImage)),
            ("/home/u/tmp/.mount_x/alchemyd", false, None),
            (
                "/private/var/folders/xy/T/AppTranslocation/1234/d/Alchemy.app/Contents/MacOS/alchemyd",
                false,
                Some(TransientInstall::Translocated),
            ),
            ("/opt/AppTranslocationNot/alchemyd", false, None),
        ];
        for (path, appimage, want) in cases {
            assert_eq!(
                transient_install(Path::new(path), appimage),
                want,
                "{path} {appimage}"
            );
        }
        assert!(
            TransientInstall::Translocated
                .to_string()
                .contains("Applications")
        );
        assert!(TransientInstall::AppImage.to_string().contains(".deb"));
        assert_eq!(
            system_log_path(501),
            PathBuf::from("/Library/Logs/Alchemy/alchemyd.501.log")
        );
    }

    #[test]
    fn service_file_paths() {
        assert_eq!(
            launch_agent_path(Path::new("/Users/jo")),
            PathBuf::from("/Users/jo/Library/LaunchAgents/computer.alchemy.alchemyd.plist")
        );
        assert_eq!(
            log_path(Path::new("/Users/jo")),
            PathBuf::from("/Users/jo/Library/Logs/Alchemy/alchemyd.log")
        );
        let home = Path::new("/home/u");
        let cases = [
            (None, "/home/u/.config/systemd/user/alchemyd.service"),
            (Some(""), "/home/u/.config/systemd/user/alchemyd.service"),
            (
                Some("relative/config"),
                "/home/u/.config/systemd/user/alchemyd.service",
            ),
            (
                Some("/xdg/config"),
                "/xdg/config/systemd/user/alchemyd.service",
            ),
        ];
        for (xdg, want) in cases {
            assert_eq!(
                systemd_user_unit_path(xdg.map(Path::new), home),
                PathBuf::from(want),
                "{xdg:?}"
            );
        }
    }

    #[test]
    fn service_paths_per_os() {
        let xdg = Some(Path::new("/xdg/config"));
        let cases = [
            (
                HostOs::MacOs,
                "/Users/jo/Library/LaunchAgents/computer.alchemy.alchemyd.plist",
                "/Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist",
            ),
            (
                HostOs::Linux,
                "/xdg/config/systemd/user/alchemyd.service",
                "/etc/systemd/system/alchemyd-501.service",
            ),
        ];
        for (os, user, system) in cases {
            assert_eq!(
                user_service_path(os, xdg, Path::new("/Users/jo")),
                PathBuf::from(user),
                "{os:?}"
            );
            assert_eq!(
                system_service_path(os, 501),
                PathBuf::from(system),
                "{os:?}"
            );
        }
    }
}
