//! The `alchemyd` next to the helper: the only daemon binary any
//! operation acts on. Its path is never an argument; it is always derived
//! from the helper's own canonical executable.

use std::path::{Path, PathBuf};

use crate::HelperError;
use crate::system::{FileInfo, OpenError, System};

/// The daemon's file name, next to the helper.
pub const DAEMON: &str = "alchemyd";

/// Open the sibling alchemyd without following a symlink and check it is
/// a regular file. Returns its path, the open file, and what it is.
pub(crate) fn open<S: System>(system: &S) -> Result<(PathBuf, S::File, FileInfo), HelperError> {
    let path = path(system)?;
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
pub(crate) fn path<S: System>(system: &S) -> Result<PathBuf, HelperError> {
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

/// A failed system call on `path`.
pub(crate) fn os(action: &'static str, path: &Path, source: std::io::Error) -> HelperError {
    HelperError::Os {
        action,
        path: path.to_path_buf(),
        source,
    }
}
