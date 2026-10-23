//! Local IPC socket: bind, accept loop, and request dispatch.

use std::io;
use std::path::Path;

use alchemy_ipc::{
    DEFAULT_MAX_FRAME, ErrorCode, FrameDecoder, IpcError, PROTOCOL_VERSION, Request, Response,
    read_message, write_message,
};
use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use tokio::sync::watch;

use crate::secure_fs;

/// Bind the IPC socket at `path`.
///
/// The default runtime directory is forced to `0o700`; a custom parent
/// directory is created if missing but otherwise left as configured. The
/// socket itself is always `0o600`. A stale socket from a crashed daemon
/// is replaced, but a live one (or a non-socket file) is never touched.
pub fn bind(path: &Path) -> io::Result<Listener> {
    if let Some(dir) = path.parent()
        && (dir == alchemy_ipc::runtime_dir() || !dir.exists())
    {
        secure_fs::create_private_dir(dir)?;
    }
    remove_stale_socket(path)?;
    let name = path.to_fs_name::<GenericFilePath>()?;
    let listener = ListenerOptions::new().name(name).create_tokio()?;
    secure_fs::restrict_to_owner(path)?;
    Ok(listener)
}

#[cfg(unix)]
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
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

#[cfg(not(unix))]
fn remove_stale_socket(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Accept connections until `shutdown` flips to `true`.
pub async fn serve(listener: Listener, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    tokio::spawn(handle_client(stream));
                }
                Err(e) => tracing::debug!(error = %e, "ipc accept failed"),
            },
        }
    }
}

async fn handle_client(stream: Stream) {
    let (mut reader, mut writer) = stream.split();
    let mut decoder = FrameDecoder::new();
    loop {
        let (resp, keep_open) = match read_message::<_, Request>(&mut reader, &mut decoder).await {
            Ok(Some(req)) => (dispatch(&req), true),
            Ok(None) => return,
            Err(IpcError::Decode(e)) => (
                Response::Error {
                    code: ErrorCode::BadRequest,
                    message: format!("unsupported or malformed request: {e}"),
                },
                false,
            ),
            Err(e) => {
                tracing::debug!(error = %e, "ipc read failed");
                return;
            }
        };
        if let Err(e) = write_message(&mut writer, &resp, DEFAULT_MAX_FRAME).await {
            tracing::debug!(error = %e, "ipc write failed");
            return;
        }
        if !keep_open {
            return;
        }
    }
}

/// Answer a single request.
pub fn dispatch(req: &Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Version => Response::Version {
            daemon: alchemy_core::VERSION.to_owned(),
            protocol: PROTOCOL_VERSION,
        },
        _ => Response::Error {
            code: ErrorCode::BadRequest,
            message: "request not supported by this daemon".to_owned(),
        },
    }
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

    #[test]
    fn dispatch_answers_ping_and_version() {
        assert_eq!(dispatch(&Request::Ping), Response::Pong);
        assert_eq!(
            dispatch(&Request::Version),
            Response::Version {
                daemon: alchemy_core::VERSION.to_owned(),
                protocol: PROTOCOL_VERSION,
            }
        );
    }

    #[test]
    fn refuses_to_replace_a_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("alchemyd.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        let err = remove_stale_socket(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn refuses_to_steal_a_live_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("alchemyd.sock");
        let _live = bind(&path).unwrap();
        let err = bind(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("alchemyd.sock");
        let _listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
