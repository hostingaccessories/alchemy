//! One-shot request/response exchange with the daemon.

use std::path::Path;
use std::time::Duration;

use alchemy_ipc::{
    DEFAULT_MAX_FRAME, FrameDecoder, Request, Response, read_message, write_message,
};
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};

use crate::error::ClientError;

/// Upper bound on a single exchange so a wedged daemon cannot hang the CLI.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect to the daemon at `socket`, send `req`, and read one response.
pub async fn exchange_at(socket: &Path, req: &Request) -> Result<Response, ClientError> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, exchange_inner(socket, req))
        .await
        .map_err(|_| ClientError::DaemonUnreachable(format!("{}: timed out", socket.display())))?
}

async fn exchange_inner(socket: &Path, req: &Request) -> Result<Response, ClientError> {
    let unreachable =
        |e: std::io::Error| ClientError::DaemonUnreachable(format!("{}: {e}", socket.display()));
    let name = socket
        .to_fs_name::<GenericFilePath>()
        .map_err(unreachable)?;
    let stream = Stream::connect(name).await.map_err(unreachable)?;
    let (mut reader, mut writer) = stream.split();
    write_message(&mut writer, req, DEFAULT_MAX_FRAME).await?;
    let mut decoder = FrameDecoder::new();
    read_message::<_, Response>(&mut reader, &mut decoder)
        .await?
        .ok_or(ClientError::ConnectionClosed)
}
