//! Where the state database lives, and the lock that keeps one daemon on
//! it.
//!
//! The state directory itself is resolved in `alchemy-api`, so the
//! desktop shell finds the TCP token in the same place.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

pub use alchemy_api::{STATE_DIR_ENV, default_state_dir};

use super::StoreError;
use crate::secure_fs;

/// File name of the SQLite database inside the state directory.
pub const DB_FILE: &str = "alchemy.db";

/// File name of the lock file inside the state directory.
pub const LOCK_FILE: &str = "alchemyd.lock";

/// An exclusive lock on a state directory, held until dropped.
///
/// The lock is an `flock` on [`LOCK_FILE`], so the OS releases it when the
/// daemon exits, however it exits; a file left behind locks nothing.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
    path: PathBuf,
}

impl StateLock {
    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Lock `dir` for this daemon, creating it `0o700` and its lock file
/// `0o600` if missing.
///
/// Fails at once with [`StoreError::InUse`], without waiting, while
/// another daemon holds the lock, so two never share a database.
pub fn lock_state_dir(dir: &Path) -> Result<StateLock, StoreError> {
    let path = dir.join(LOCK_FILE);
    secure_fs::create_private_dir(dir).map_err(|source| StoreError::Path {
        path: dir.to_owned(),
        source,
    })?;
    secure_fs::create_private_file(&path).map_err(|source| StoreError::Path {
        path: path.clone(),
        source,
    })?;
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|source| StoreError::Path {
            path: path.clone(),
            source,
        })?;
    match file.try_lock() {
        Ok(()) => Ok(StateLock { _file: file, path }),
        Err(TryLockError::WouldBlock) => Err(StoreError::InUse {
            path: dir.to_owned(),
        }),
        Err(TryLockError::Error(source)) => Err(StoreError::Path { path, source }),
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
    fn a_second_lock_fails_until_the_first_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let first = lock_state_dir(&dir).unwrap();
        assert_eq!(first.path(), dir.join(LOCK_FILE));

        let err = lock_state_dir(&dir).unwrap_err();
        assert!(matches!(&err, StoreError::InUse { path } if *path == dir));
        assert_eq!(
            err.to_string(),
            format!(
                "another alchemyd is already using the state directory {}; stop it first",
                dir.display()
            )
        );

        drop(first);
        let again = lock_state_dir(&dir).unwrap();
        drop(again);
        assert!(dir.join(LOCK_FILE).exists());
        lock_state_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn creates_a_private_dir_and_lock_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let _lock = lock_state_dir(&dir).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(LOCK_FILE)), 0o600);
    }
}
