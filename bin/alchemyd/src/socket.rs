//! The per-user Unix socket the API is served on for the CLI and helper.
//!
//! The socket's only access control is filesystem permissions: see
//! [`bind`] for the rules.

use std::io;
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};

use tokio::net::UnixListener;

use crate::secure_fs;

/// Removes the socket file it was created for when dropped.
///
/// Only a socket is removed, so a file that replaced it is left alone.
#[derive(Debug)]
pub struct SocketFile {
    path: PathBuf,
}

impl SocketFile {
    /// Path of the socket file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let is_socket =
            std::fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.file_type().is_socket());
        if is_socket && let Err(e) = std::fs::remove_file(&self.path) {
            tracing::debug!(error = %e, socket = %self.path.display(), "socket removal failed");
        }
    }
}

/// Bind the API socket at `path`.
///
/// The default runtime directory is forced to `0o700`; a custom parent
/// directory is created if missing but otherwise left as configured. The
/// socket itself is always `0o600`. A stale socket from a crashed daemon
/// is replaced, but a live one (or a non-socket file) is never touched.
///
/// Must be called inside a Tokio runtime. Keep the returned [`SocketFile`]
/// alive for as long as the listener; dropping it removes the socket.
pub fn bind(path: &Path) -> io::Result<(UnixListener, SocketFile)> {
    if let Some(dir) = path.parent()
        && (dir == alchemy_api::runtime_dir() || !dir.exists())
    {
        secure_fs::create_private_dir(dir)?;
    }
    remove_stale_socket(path)?;
    let listener = UnixListener::bind(path)?;
    let file = SocketFile {
        path: path.to_path_buf(),
    };
    secure_fs::restrict_to_owner(path)?;
    Ok((listener, file))
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::net::UnixStream;

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        ));
    }
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another alchemyd is already listening on {}",
                path.display()
            ),
        ));
    }
    std::fs::remove_file(path)
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
    use std::os::unix::fs::PermissionsExt as _;

    fn sock_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ad")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn refuses_to_replace_a_regular_file() {
        let tmp = sock_dir();
        let path = tmp.path().join("alchemyd.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        let err = remove_stale_socket(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn refuses_to_steal_a_live_socket() {
        let tmp = sock_dir();
        let path = tmp.path().join("alchemyd.sock");
        let _live = bind(&path).unwrap();
        let err = bind(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn replaces_a_stale_socket() {
        let tmp = sock_dir();
        let path = tmp.path().join("alchemyd.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (_listener, file) = bind(&path).unwrap();
        assert_eq!(file.path(), path);
    }

    #[tokio::test]
    async fn socket_is_owner_only() {
        let tmp = sock_dir();
        let path = tmp.path().join("alchemyd.sock");
        let _bound = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn missing_parent_is_created_private() {
        let tmp = sock_dir();
        let dir = tmp.path().join("run");
        let _bound = bind(&dir.join("alchemyd.sock")).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[tokio::test]
    async fn dropping_the_socket_file_removes_it() {
        let tmp = sock_dir();
        let path = tmp.path().join("alchemyd.sock");
        let (listener, file) = bind(&path).unwrap();
        drop(listener);
        drop(file);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn dropping_leaves_a_file_that_replaced_the_socket() {
        let tmp = sock_dir();
        let path = tmp.path().join("alchemyd.sock");
        let (_listener, file) = bind(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"not a socket").unwrap();
        drop(file);
        assert!(path.exists());
    }
}
