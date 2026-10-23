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

#[cfg(test)]
impl<D: Daemon> Context<D, Vec<u8>> {
    /// A context for `command` that writes to a buffer.
    pub(crate) fn buffered(daemon: D, json: bool, command: &'static str) -> Self {
        Self::new(daemon, Output::new(Vec::new(), json), command)
    }

    /// Everything written to the output so far.
    pub(crate) fn written(&self) -> String {
        String::from_utf8_lossy(self.out.get_ref()).into_owned()
    }

    /// The daemon client, to inspect what was sent.
    pub(crate) fn daemon(&self) -> &D {
        &self.daemon
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
    use crate::daemon::fake::FakeDaemon;

    #[tokio::test]
    async fn handshakes_once_per_invocation() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .reply(Request::Ping, Response::Pong)
            .stream(Request::WatchEngine, [])
            .reply(Request::Ping, Response::Pong);
        let mut ctx = Context::buffered(fake, false, "test");
        assert_eq!(ctx.request(&Request::Ping).await.unwrap(), Response::Pong);
        let mut stream = ctx.stream(&Request::WatchEngine).await.unwrap();
        assert!(stream.next().await.unwrap().is_none());
        assert_eq!(ctx.request(&Request::Ping).await.unwrap(), Response::Pong);
        assert_eq!(
            ctx.daemon_info().await.unwrap(),
            &DaemonInfo {
                version: "1.2.3".into(),
                protocol: 1,
            }
        );
        assert_eq!(
            ctx.daemon().requests(),
            [
                Request::Version,
                Request::Ping,
                Request::WatchEngine,
                Request::Ping
            ]
        );
    }

    #[tokio::test]
    async fn error_responses_become_errors() {
        let internal = Response::Error {
            code: ErrorCode::Internal,
            message: "boom".into(),
        };
        let bad = Response::Error {
            code: ErrorCode::BadRequest,
            message: "unsupported or malformed request".into(),
        };
        let fake = FakeDaemon::new()
            .version("0.0.1", 1)
            .reply(Request::Ping, internal.clone())
            .reply(Request::Engine, bad.clone())
            .stream(Request::WatchEngine, [Ok(internal), Ok(bad)]);
        let mut ctx = Context::buffered(fake, false, "app up");

        let err = ctx.request(&Request::Ping).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::Daemon(m) if m == "boom"),
            "{err:?}"
        );

        let err = ctx.request(&Request::Engine).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::DaemonTooOld { daemon, command: "app up" } if daemon == "0.0.1"),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("alchemyd 0.0.1 is too old for `app up`; upgrade it"),
            "{err}"
        );

        let mut stream = ctx.stream(&Request::WatchEngine).await.unwrap();
        let err = stream.next().await.unwrap_err();
        assert!(matches!(err, ClientError::Daemon(_)), "{err:?}");
        let err = stream.next().await.unwrap_err();
        assert!(matches!(err, ClientError::DaemonTooOld { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_failed_handshake_sends_nothing_else() {
        let fake = FakeDaemon::new().fail(
            Request::Version,
            ClientError::DaemonUnreachable("/run/alchemyd.sock: refused".into()),
        );
        let mut ctx = Context::buffered(fake, false, "test");
        let err = ctx.request(&Request::Engine).await.unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");
        assert_eq!(ctx.daemon().requests(), [Request::Version]);
    }
}
