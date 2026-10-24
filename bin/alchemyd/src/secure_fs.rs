//! Filesystem hardening for daemon-owned paths.
//!
//! The API socket's only access control is filesystem permissions, so the
//! runtime directory is forced to `0o700` and the socket to `0o600`. The
//! state directory and database get the same modes. If another user owns
//! a pre-existing path the `chmod` fails and the daemon refuses to start
//! rather than trust it.

use std::io;
use std::path::Path;

/// Create `path` (and parents) and force its mode to `0o700`.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_mode(path, 0o700)
}

/// Force `path`'s mode to `0o600`.
pub fn restrict_to_owner(path: &Path) -> io::Result<()> {
    set_mode(path, 0o600)
}

/// Create the regular file `path` if it is missing, never truncating it,
/// and force its mode to `0o600`.
///
/// A new file is created `0o600` rather than tightened afterwards, so it is
/// never readable by others in between.
pub fn create_private_file(path: &Path) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?;
    restrict_to_owner(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn create_private_dir_tightens_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        create_private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn create_private_file_keeps_contents_and_tightens_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let new = tmp.path().join("new");
        create_private_file(&new).unwrap();
        assert_eq!(mode(&new), 0o600);

        let existing = tmp.path().join("existing");
        std::fs::write(&existing, b"keep").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
        create_private_file(&existing).unwrap();
        assert_eq!(mode(&existing), 0o600);
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep");
    }

    #[test]
    fn restrict_to_owner_is_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        restrict_to_owner(&file).unwrap();
        assert_eq!(mode(&file), 0o600);
    }
}
