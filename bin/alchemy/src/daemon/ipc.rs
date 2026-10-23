//! [`Daemon`] over the local IPC socket.

use std::path::{Path, PathBuf};

use alchemy_ipc::{Request, Response};

use super::{Daemon, ResponseStream};
use crate::error::ClientError;
use crate::transport::{Connection, with_timeout};

/// Talks to alchemyd over its IPC socket, on one connection that it
/// opens on the first request and reuses after.
///
/// alchemyd closes a connection after answering with an error, so the
/// client drops it then and reconnects on the next request.
pub(crate) struct IpcDaemon {
    socket: PathBuf,
    conn: Option<Connection>,
}

impl IpcDaemon {
    /// A client for the daemon at `socket`. Nothing is opened until the
    /// first request.
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self { socket, conn: None }
    }

    async fn connection(
        socket: &Path,
        conn: &mut Option<Connection>,
    ) -> Result<Connection, ClientError> {
        match conn.take() {
            Some(conn) => Ok(conn),
            None => Connection::connect(socket).await,
        }
    }
}

impl Daemon for IpcDaemon {
    type Stream = IpcStream;

    fn socket(&self) -> &Path {
        &self.socket
    }

    async fn request(&mut self, req: &Request) -> Result<Response, ClientError> {
        let Self { socket, conn } = self;
        with_timeout(socket, async {
            let mut open = Self::connection(socket, conn).await?;
            let resp = open.request(req).await?;
            if !matches!(resp, Response::Error { .. }) {
                *conn = Some(open);
            }
            Ok(resp)
        })
        .await
    }

    async fn stream(&mut self, req: &Request) -> Result<IpcStream, ClientError> {
        let Self { socket, conn } = self;
        let conn = with_timeout(socket, async {
            let mut open = Self::connection(socket, conn).await?;
            open.send(req).await?;
            Ok(open)
        })
        .await?;
        Ok(IpcStream {
            socket: socket.clone(),
            conn,
            started: false,
        })
    }
}

/// The responses to a streaming request on an [`IpcDaemon`]'s connection.
pub(crate) struct IpcStream {
    socket: PathBuf,
    conn: Connection,
    started: bool,
}

impl ResponseStream for IpcStream {
    async fn next(&mut self) -> Result<Option<Response>, ClientError> {
        if self.started {
            return self.conn.recv().await;
        }
        let first = with_timeout(&self.socket, self.conn.recv()).await?;
        self.started = true;
        first.map(Some).ok_or(ClientError::ConnectionClosed)
    }
}
