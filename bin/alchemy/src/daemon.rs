//! The CLI's view of alchemyd: a [`Daemon`] client that commands talk to
//! instead of the socket, so their logic can be tested against a fake.

mod ipc;

use std::path::Path;

use alchemy_ipc::{Request, Response};

use crate::error::ClientError;

pub(crate) use ipc::IpcDaemon;

/// A client of alchemyd.
///
/// Implementations return the daemon's [`Response`] as sent, including
/// [`Response::Error`]; [`crate::context::Context`] turns those into
/// [`ClientError`]s. They bound each request by a timeout so a wedged
/// daemon cannot hang the CLI.
pub(crate) trait Daemon {
    /// The responses that follow a streaming request.
    type Stream: ResponseStream;

    /// Where this client reaches the daemon.
    fn socket(&self) -> &Path;

    /// Send `req` and wait for its one response.
    async fn request(&mut self, req: &Request) -> Result<Response, ClientError>;

    /// Send `req`, which the daemon answers with a stream of responses.
    ///
    /// The stream takes over the client's connection; a later
    /// [`Daemon::request`] opens a new one.
    async fn stream(&mut self, req: &Request) -> Result<Self::Stream, ClientError>;
}

/// The responses to a streaming request, in the order the daemon sent
/// them.
pub(crate) trait ResponseStream {
    /// The next response, or `None` once the daemon ends the stream.
    ///
    /// Only the first response is bounded by a timeout: a stream may be
    /// quiet for as long as whatever it follows is.
    async fn next(&mut self) -> Result<Option<Response>, ClientError>;
}

/// What the daemon said about itself in the `Version` handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonInfo {
    /// The daemon's release version.
    pub(crate) version: String,
    /// The daemon's IPC protocol version.
    pub(crate) protocol: u32,
}
