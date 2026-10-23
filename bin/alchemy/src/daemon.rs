//! The CLI's view of alchemyd: a [`Daemon`] client that commands talk to
//! instead of the socket, so their logic can be tested against a fake.
//!
//! The trait works at the HTTP level: a status and a body, or raw
//! server-sent events. [`crate::context::Context`] decodes them into API
//! types and errors.

#[cfg(test)]
pub(crate) mod fake;
mod http;

use std::path::Path;

pub(crate) use hyper::Method;

use crate::error::ClientError;
use crate::sse::SseEvent;

pub(crate) use http::HttpDaemon;

/// A client of alchemyd.
///
/// Implementations return the daemon's answer as sent, error statuses
/// included; only a failure to exchange it at all is an `Err`. They bound
/// each exchange by a timeout so a wedged daemon cannot hang the CLI.
pub(crate) trait Daemon {
    /// The events that follow a [`Daemon::events`] request.
    type Events: EventStream;

    /// Where this client reaches the daemon.
    fn socket(&self) -> &Path;

    /// Send `method` to `path`, such as `/api/engine`, with no body, and
    /// return the status and the whole body.
    async fn request(&mut self, method: Method, path: &str) -> Result<Reply, ClientError>;

    /// `GET` the server-sent event stream at `path`. A status other than
    /// success comes back as [`Opened::Refused`] with its body.
    async fn events(&mut self, path: &str) -> Result<Opened<Self::Events>, ClientError>;
}

/// The events on an open stream, in the order the daemon sent them.
pub(crate) trait EventStream {
    /// The next event, or `None` once the daemon ends the stream.
    ///
    /// Only the first event is bounded by a timeout: a stream may be quiet
    /// for as long as whatever it follows is.
    async fn next(&mut self) -> Result<Option<SseEvent>, ClientError>;
}

/// A complete HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    /// The HTTP status code.
    pub(crate) status: u16,
    /// The body, usually JSON.
    pub(crate) body: Vec<u8>,
}

/// The outcome of opening an event stream.
#[derive(Debug)]
pub(crate) enum Opened<S> {
    /// The daemon accepted and is streaming.
    Stream(S),
    /// The daemon answered with a status other than success.
    Refused(Reply),
}

/// What the daemon said about itself on `GET /api/version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonInfo {
    /// The daemon's release version.
    pub(crate) version: String,
    /// The daemon's API version.
    pub(crate) api: u32,
}
