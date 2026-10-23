//! Local IPC socket: bind, accept loop, and request dispatch.

use std::io;
use std::path::Path;

use alchemy_ipc::{
    DEFAULT_MAX_FRAME, ErrorCode, FrameDecoder, IpcError, PROTOCOL_VERSION, Request, Response,
    read_frame, read_message, write_message,
};
use interprocess::local_socket::tokio::Listener;
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::{State, secure_fs};

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

/// Accept connections until `shutdown` flips to `true`, answering each
/// from `state`.
///
/// Connections already open when `shutdown` flips finish their current
/// exchange; engine watch streams end at once.
pub async fn serve(listener: Listener, state: State, shutdown: watch::Receiver<bool>) {
    let mut stop = shutdown.clone();
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stop| *stop) => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    let (reader, writer) = stream.split();
                    tokio::spawn(handle_client(reader, writer, state.clone(), shutdown.clone()));
                }
                Err(e) => tracing::debug!(error = %e, "ipc accept failed"),
            },
        }
    }
}

async fn handle_client<R, W>(
    mut reader: R,
    mut writer: W,
    state: State,
    shutdown: watch::Receiver<bool>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut decoder = FrameDecoder::new();
    loop {
        let (resp, keep_open) = match read_message::<_, Request>(&mut reader, &mut decoder).await {
            Ok(Some(Request::WatchEngine)) => {
                return stream_engine(reader, decoder, writer, &state, shutdown).await;
            }
            Ok(Some(req)) => (dispatch(&state, &req), true),
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

/// Answer [`Request::WatchEngine`]: send the current engine status, then
/// one frame per change, until the client hangs up or `shutdown` flips.
///
/// The client sends nothing after subscribing, so the read half is only
/// watched for EOF. Without that, a client that disconnects while the
/// engine is idle would hold its subscription until the next change.
async fn stream_engine<R, W>(
    mut reader: R,
    mut decoder: FrameDecoder,
    mut writer: W,
    state: &State,
    mut shutdown: watch::Receiver<bool>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut engine = state.watch_engine();
    let hung_up =
        async move { while let Ok(Some(_)) = read_frame(&mut reader, &mut decoder).await {} };
    tokio::pin!(hung_up);
    loop {
        let frame = Response::Engine(engine.borrow_and_update().clone());
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => return,
            () = &mut hung_up => return,
            sent = write_message(&mut writer, &frame, DEFAULT_MAX_FRAME) => {
                if let Err(e) = sent {
                    tracing::debug!(error = %e, "ipc engine stream write failed");
                    return;
                }
            }
        }
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => return,
            () = &mut hung_up => return,
            changed = engine.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

/// Answer a single request.
///
/// [`Request::WatchEngine`] is a stream, not a single answer; the
/// connection handler serves it, and this answers it with an error.
pub fn dispatch(state: &State, req: &Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Version => Response::Version {
            daemon: alchemy_core::VERSION.to_owned(),
            protocol: PROTOCOL_VERSION,
        },
        Request::Engine => Response::Engine(state.engine()),
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
    use alchemy_core::engine::{EngineState, EngineStatus};
    use std::time::Duration;
    use tokio::io::{DuplexStream, duplex};
    use tokio::task::JoinHandle;

    const DEADLINE: Duration = Duration::from_secs(5);

    #[test]
    fn dispatch_answers_ping_and_version() {
        let state = State::new();
        assert_eq!(dispatch(&state, &Request::Ping), Response::Pong);
        assert_eq!(
            dispatch(&state, &Request::Version),
            Response::Version {
                daemon: alchemy_core::VERSION.to_owned(),
                protocol: PROTOCOL_VERSION,
            }
        );
    }

    #[test]
    fn dispatch_answers_engine_from_state() {
        let state = State::new();
        state.set_engine(EngineStatus::new(EngineState::Ready));
        assert_eq!(
            dispatch(&state, &Request::Engine),
            Response::Engine(EngineStatus::new(EngineState::Ready))
        );
    }

    #[test]
    fn dispatch_refuses_watch_engine_as_a_single_answer() {
        let resp = dispatch(&State::new(), &Request::WatchEngine);
        assert!(
            matches!(
                resp,
                Response::Error {
                    code: ErrorCode::BadRequest,
                    ..
                }
            ),
            "{resp:?}"
        );
    }

    fn spawn_client(
        state: &State,
        shutdown: watch::Receiver<bool>,
    ) -> (DuplexStream, JoinHandle<()>) {
        let (client, server) = duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let task = tokio::spawn(handle_client(reader, writer, state.clone(), shutdown));
        (client, task)
    }

    async fn next(client: &mut DuplexStream, decoder: &mut FrameDecoder) -> Option<Response> {
        tokio::time::timeout(DEADLINE, read_message(client, decoder))
            .await
            .unwrap()
            .unwrap()
    }

    async fn watch_engine(
        state: &State,
        shutdown: watch::Receiver<bool>,
    ) -> (DuplexStream, FrameDecoder, JoinHandle<()>) {
        let (mut client, task) = spawn_client(state, shutdown);
        write_message(&mut client, &Request::WatchEngine, DEFAULT_MAX_FRAME)
            .await
            .unwrap();
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            next(&mut client, &mut decoder).await,
            Some(Response::Engine(state.engine()))
        );
        (client, decoder, task)
    }

    #[tokio::test]
    async fn watch_engine_sends_current_status_then_each_change() {
        let state = State::new();
        let (_stop, stop_rx) = watch::channel(false);
        let (mut client, mut decoder, _task) = watch_engine(&state, stop_rx).await;
        for engine in [
            EngineState::Starting,
            EngineState::Ready,
            EngineState::NotRunning,
        ] {
            state.set_engine(EngineStatus::new(engine));
            assert_eq!(
                next(&mut client, &mut decoder).await,
                Some(Response::Engine(EngineStatus::new(engine)))
            );
        }
    }

    #[tokio::test]
    async fn watch_engine_stops_when_the_client_hangs_up() {
        let state = State::new();
        let (_stop, stop_rx) = watch::channel(false);
        let (client, _decoder, task) = watch_engine(&state, stop_rx).await;
        drop(client);
        tokio::time::timeout(DEADLINE, task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn watch_engine_stops_on_shutdown() {
        let state = State::new();
        let (stop, stop_rx) = watch::channel(false);
        let (mut client, mut decoder, task) = watch_engine(&state, stop_rx).await;
        stop.send_replace(true);
        tokio::time::timeout(DEADLINE, task).await.unwrap().unwrap();
        assert_eq!(next(&mut client, &mut decoder).await, None);
    }

    #[tokio::test]
    async fn engine_request_keeps_the_connection_open() {
        let state = State::new();
        let (_stop, stop_rx) = watch::channel(false);
        let (mut client, _task) = spawn_client(&state, stop_rx);
        let mut decoder = FrameDecoder::new();
        for req in [Request::Engine, Request::Ping] {
            write_message(&mut client, &req, DEFAULT_MAX_FRAME)
                .await
                .unwrap();
            assert_eq!(
                next(&mut client, &mut decoder).await,
                Some(dispatch(&state, &req))
            );
        }
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
