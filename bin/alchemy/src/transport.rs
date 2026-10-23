//! Request/response exchanges and streams with the daemon.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use alchemy_ipc::{
    DEFAULT_MAX_FRAME, FrameDecoder, Request, Response, read_message, write_message,
};
use interprocess::local_socket::tokio::{RecvHalf, SendHalf, Stream};
use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};

use crate::error::ClientError;

/// Upper bound on a single exchange so a wedged daemon cannot hang the CLI.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect to the daemon at `socket`, send `req`, and read one response.
pub async fn exchange_at(socket: &Path, req: &Request) -> Result<Response, ClientError> {
    with_timeout(socket, async {
        Connection::connect(socket).await?.request(req).await
    })
    .await
}

/// Run `exchange` against the daemon at `socket`, giving up after
/// [`EXCHANGE_TIMEOUT`].
pub(crate) async fn with_timeout<T>(
    socket: &Path,
    exchange: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, exchange)
        .await
        .map_err(|_| ClientError::DaemonUnreachable(format!("{}: timed out", socket.display())))?
}

/// One open connection to the daemon. Requests on it are answered in
/// order; none of its methods time out on their own.
pub(crate) struct Connection {
    reader: RecvHalf,
    writer: SendHalf,
    decoder: FrameDecoder,
}

impl Connection {
    pub(crate) async fn connect(socket: &Path) -> Result<Self, ClientError> {
        let unreachable = |e: std::io::Error| {
            ClientError::DaemonUnreachable(format!("{}: {e}", socket.display()))
        };
        let name = socket
            .to_fs_name::<GenericFilePath>()
            .map_err(unreachable)?;
        let stream = Stream::connect(name).await.map_err(unreachable)?;
        let (reader, writer) = stream.split();
        Ok(Self {
            reader,
            writer,
            decoder: FrameDecoder::new(),
        })
    }

    pub(crate) async fn send(&mut self, req: &Request) -> Result<(), ClientError> {
        Ok(write_message(&mut self.writer, req, DEFAULT_MAX_FRAME).await?)
    }

    /// The next response, or `None` once the daemon closes the connection.
    pub(crate) async fn recv(&mut self) -> Result<Option<Response>, ClientError> {
        Ok(read_message(&mut self.reader, &mut self.decoder).await?)
    }

    pub(crate) async fn request(&mut self, req: &Request) -> Result<Response, ClientError> {
        self.send(req).await?;
        self.recv().await?.ok_or(ClientError::ConnectionClosed)
    }
}
