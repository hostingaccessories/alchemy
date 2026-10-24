//! `grant-low-ports` and `revoke-low-ports`: `cap_net_bind_service` on
//! the alchemyd next to the helper.
//!
//! The capability is written straight into the `security.capability`
//! extended attribute, so there is no dependency on `setcap`. The target
//! is never named on the command line: it is always the `alchemyd` in the
//! directory of the helper's own canonical executable.

use std::path::{Path, PathBuf};

use crate::system::{FileInfo, OpenError, System};
use crate::{HelperError, Outcome, Platform};

/// The extended attribute that holds file capabilities.
pub const CAPABILITY_XATTR: &str = "security.capability";

/// A v2 `vfs_cap_data` granting `cap_net_bind_service=ep`: what
/// `setcap cap_net_bind_service=+ep` writes. Little-endian `u32`s.
pub const NET_BIND_SERVICE_EP: [u8; 20] = [
    0x01, 0x00, 0x00, 0x02, // VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE
    0x00, 0x04, 0x00, 0x00, // permitted, caps 0-31: 1 << CAP_NET_BIND_SERVICE (10)
    0x00, 0x00, 0x00, 0x00, // inheritable, caps 0-31
    0x00, 0x00, 0x00, 0x00, // permitted, caps 32-63
    0x00, 0x00, 0x00, 0x00, // inheritable, caps 32-63
];

/// The daemon's file name, next to the helper.
pub const DAEMON: &str = "alchemyd";

const MACOS_WHY: &str = "macOS lets any user bind ports below 1024";

/// Set the capability, unless alchemyd already has exactly it.
pub(crate) fn grant<S: System>(system: &S, platform: Platform) -> Result<Outcome, HelperError> {
    let (path, file, info) = open_daemon(system, platform, "grant-low-ports")?;
    if info.nosuid {
        return Err(HelperError::Ineffective {
            path,
            why: "its filesystem is mounted nosuid, so the kernel would ignore the capability",
        });
    }
    let current = system
        .xattr(&file, CAPABILITY_XATTR)
        .map_err(|source| os("read the file capability of", &path, source))?;
    if current.as_deref() == Some(&NET_BIND_SERVICE_EP[..]) {
        return Ok(Outcome { changed: false });
    }
    system
        .set_xattr(&file, CAPABILITY_XATTR, &NET_BIND_SERVICE_EP)
        .map_err(|source| os("set the file capability on", &path, source))?;
    Ok(Outcome { changed: true })
}

/// Remove the file capability, if alchemyd has one.
pub(crate) fn revoke<S: System>(system: &S, platform: Platform) -> Result<Outcome, HelperError> {
    let (path, file, _) = open_daemon(system, platform, "revoke-low-ports")?;
    let changed = system
        .remove_xattr(&file, CAPABILITY_XATTR)
        .map_err(|source| os("remove the file capability from", &path, source))?;
    Ok(Outcome { changed })
}

/// Open the sibling alchemyd and check it is a regular file.
fn open_daemon<S: System>(
    system: &S,
    platform: Platform,
    operation: &'static str,
) -> Result<(PathBuf, S::File, FileInfo), HelperError> {
    if platform == Platform::MacOs {
        return Err(HelperError::UnsupportedPlatform {
            operation,
            platform,
            why: MACOS_WHY,
        });
    }
    let path = daemon_path(system)?;
    let file = match system.open_nofollow(&path) {
        Ok(file) => file,
        Err(OpenError::NotFound) => return Err(HelperError::NotFound(path)),
        Err(OpenError::Symlink) => {
            return Err(HelperError::Refused {
                path,
                why: "it is a symlink",
            });
        }
        Err(OpenError::Io(source)) => return Err(os("open", &path, source)),
    };
    let info = system
        .inspect(&file)
        .map_err(|source| os("inspect", &path, source))?;
    if !info.regular {
        return Err(HelperError::Refused {
            path,
            why: "it is not a regular file",
        });
    }
    Ok((path, file, info))
}

/// The `alchemyd` in the directory of the helper's canonical executable.
fn daemon_path<S: System>(system: &S) -> Result<PathBuf, HelperError> {
    let exe = system
        .current_exe()
        .map_err(|source| os("resolve", Path::new("alchemy-helper"), source))?;
    match exe.parent() {
        Some(dir) => Ok(dir.join(DAEMON)),
        None => Err(HelperError::Refused {
            path: exe,
            why: "the helper has no parent directory",
        }),
    }
}

fn os(action: &'static str, path: &Path, source: std::io::Error) -> HelperError {
    HelperError::Os {
        action,
        path: path.to_path_buf(),
        source,
    }
}
