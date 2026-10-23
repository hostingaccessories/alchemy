//! What a command runs with: the daemon client, the output sink, and the
//! global flags.

use std::io::Write;
use std::path::Path;

use alchemy_ipc::{ErrorCode, Request, Response};

use crate::daemon::{Daemon, DaemonInfo, ResponseStream};
use crate::error::ClientError;
use crate::output::Output;

/// Everything a command's `run` needs. Commands reach the daemon only
/// through this.
///
/// The first request performs the `Version` handshake; its answer is
/// kept for the rest of the invocation, so a command never needs to
/// send `Version` itself.
pub(crate) struct Context<D, W> {
    daemon: D,
    out: Output<W>,
    command: &'static str,
    info: Option<DaemonInfo>,
}

impl<D: Daemon, W: Write> Context<D, W> {
    /// A context for one invocation of `command`, named as typed after
    /// `alchemy` (`status`, `app up`) for errors.
    pub(crate) fn new(daemon: D, out: Output<W>, command: &'static str) -> Self {
        Self {
            daemon,
            out,
            command,
            info: None,
        }
    }

    /// Whether `--json` was given.
    pub(crate) fn json(&self) -> bool {
        self.out.json()
    }

    /// Where the command's results go.
    pub(crate) fn out(&mut self) -> &mut Output<W> {
        &mut self.out
    }

    /// Where the daemon is reached.
    pub(crate) fn socket(&self) -> &Path {
        self.daemon.socket()
    }

    /// The daemon's version, from the handshake, performing it if no
    /// request has yet.
    pub(crate) async fn daemon_info(&mut self) -> Result<&DaemonInfo, ClientError> {
        let info = match self.info.take() {
            Some(info) => info,
            None => handshake(&mut self.daemon).await?,
        };
        Ok(self.info.insert(info))
    }

    /// Send `req` and return its response. An error response comes back
    /// as a [`ClientError`].
    pub(crate) async fn request(&mut self, req: &Request) -> Result<Response, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        let resp = self.daemon.request(req).await?;
        accept(resp, &daemon, self.command)
    }

    /// Send a streaming `req` and return its responses. Error responses
    /// come back as [`ClientError`]s.
    pub(crate) async fn stream(
        &mut self,
        req: &Request,
    ) -> Result<Subscription<D::Stream>, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        let inner = self.daemon.stream(req).await?;
        Ok(Subscription {
            inner,
            daemon,
            command: self.command,
        })
    }
}

/// The responses to a request sent with [`Context::stream`].
pub(crate) struct Subscription<S> {
    inner: S,
    daemon: String,
    command: &'static str,
}

impl<S: ResponseStream> Subscription<S> {
    /// The next response, or `None` once the daemon ends the stream.
    pub(crate) async fn next(&mut self) -> Result<Option<Response>, ClientError> {
        match self.inner.next().await? {
            Some(resp) => accept(resp, &self.daemon, self.command).map(Some),
            None => Ok(None),
        }
    }
}

async fn handshake<D: Daemon>(daemon: &mut D) -> Result<DaemonInfo, ClientError> {
    match daemon.request(&Request::Version).await? {
        Response::Version { daemon, protocol } => Ok(DaemonInfo {
            version: daemon,
            protocol,
        }),
        other => Err(ClientError::from_response(other)),
    }
}

/// Turn an error response into a [`ClientError`]. alchemyd answers a
/// request it does not know with `BadRequest`, so that means the daemon
/// predates it.
fn accept(resp: Response, daemon: &str, command: &'static str) -> Result<Response, ClientError> {
    match resp {
        Response::Error {
            code: ErrorCode::BadRequest,
            ..
        } => Err(ClientError::DaemonTooOld {
            daemon: daemon.to_owned(),
            command,
        }),
        Response::Error { .. } => Err(ClientError::from_response(resp)),
        resp => Ok(resp),
    }
}
