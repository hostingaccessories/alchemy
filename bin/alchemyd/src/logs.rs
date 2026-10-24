//! App logs, read from the Engine API and served as
//! `GET /api/apps/{name}/logs`.
//!
//! A [`LogSource`] reads one container's raw log ([`SystemLogs`] through
//! `bollard`, or [`FakeLogSource`] in tests). [`stream()`] reads every
//! container of an app, or of one of its services, through it: the last
//! lines of each, merged in time order with
//! [`alchemy_core::logs::merge_tail`], then, when following, new lines
//! from each as they arrive. A container that stops is followed again,
//! without repeating a line, once it runs again; one that appears in the
//! app later, such as after `docker compose up` recreates it, is
//! followed from its first line.
//!
//! Each request is bounded: its tail holds at most
//! [`MAX_TAIL_BYTES`](alchemy_core::logs::MAX_TAIL_BYTES) of the newest
//! lines, and at most [`MAX_STREAMS`] streams are open at once across
//! both listeners, counted in [`State::log_streams`]. A request past that
//! is refused rather than queued.
//!
//! The containers come from the app's entry in [`State::apps`], so they
//! are the ones the app list shows, matched by Compose project.

mod fake;
mod follow;

pub use fake::FakeLogSource;
pub use follow::{LogQuery, LogsRefusal, stream};

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use alchemy_api::logs::LogStream;
use futures_util::StreamExt as _;
use futures_util::stream::{self, BoxStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::engine::{ApiError, BollardApi, Endpoint, EndpointError};

#[cfg(doc)]
use crate::State;

/// What to read of one container's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRequest {
    /// Keep the stream open for new lines until the container stops.
    pub follow: bool,
    /// Only the last this many lines, or every line when `None`.
    pub tail: Option<u32>,
    /// Only lines from this Unix second on, or from the start when `None`.
    pub since: Option<i64>,
}

/// Bytes a container wrote to one of its outputs, with Docker's
/// timestamp before every line. A chunk may hold several lines or part
/// of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChunk {
    /// The output written to.
    pub stream: LogStream,
    /// The raw bytes, which need not be UTF-8.
    pub bytes: Vec<u8>,
}

/// Why a container's log could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LogError {
    /// The engine endpoint is not one Alchemy can talk to.
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    /// The engine refused or failed the request.
    #[error(transparent)]
    Engine(#[from] ApiError),
}

impl LogError {
    /// Whether the container no longer exists, as when it was removed
    /// after the app list was read.
    pub fn is_gone(&self) -> bool {
        matches!(self, Self::Engine(ApiError::Server { status: 404, .. }))
    }
}

/// Reads container logs from the engine.
pub trait LogSource: fmt::Debug + Send + Sync {
    /// The log of container `id` on the engine at `endpoint`, such as
    /// `unix:///var/run/docker.sock`, as [`LogChunk`]s in the order they
    /// were written.
    ///
    /// The stream ends when the lines asked for are sent or, with
    /// [`LogRequest::follow`], when the container stops. It yields an
    /// error when the log cannot be read at all, and ends after it.
    fn logs(
        &self,
        endpoint: &str,
        id: &str,
        request: LogRequest,
    ) -> BoxStream<'static, Result<LogChunk, LogError>>;
}

/// A [`LogSource`] shared by every request.
pub type SharedLogSource = Arc<dyn LogSource>;

/// The most log streams alchemyd serves at once, across both listeners,
/// followed or not. Each holds up to
/// [`MAX_TAIL_BYTES`](alchemy_core::logs::MAX_TAIL_BYTES) per container
/// while its tail is read, and a connection to the engine per container
/// while it follows.
pub const MAX_STREAMS: usize = 16;

/// Counts the open log streams against a limit. Clones share the count.
#[derive(Debug, Clone)]
pub struct LogStreams {
    permits: Arc<Semaphore>,
}

impl LogStreams {
    /// At most `max` streams at once.
    pub fn new(max: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max)),
        }
    }

    /// A place for one more stream, or `None` while every place is taken.
    pub fn try_open(&self) -> Option<StreamPermit> {
        let permit = self.permits.clone().try_acquire_owned().ok()?;
        Some(StreamPermit { _permit: permit })
    }
}

impl Default for LogStreams {
    /// At most [`MAX_STREAMS`].
    fn default() -> Self {
        Self::new(MAX_STREAMS)
    }
}

/// One open stream's place in its [`LogStreams`], given back when dropped.
#[derive(Debug)]
pub struct StreamPermit {
    _permit: OwnedSemaphorePermit,
}

/// The real [`LogSource`]: the Engine API through `bollard`. Cheap to
/// clone: clones share the client.
#[derive(Clone, Default)]
pub struct SystemLogs {
    engine: Arc<Mutex<Option<(String, BollardApi)>>>,
}

impl SystemLogs {
    /// A source that connects on its first request.
    pub fn new() -> Self {
        Self::default()
    }

    fn engine_for(&self, endpoint: &str) -> Result<BollardApi, LogError> {
        let mut cached = self.engine.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_endpoint, api)) = cached.as_ref()
            && cached_endpoint == endpoint
        {
            return Ok(api.clone());
        }
        let api = BollardApi::connect(&Endpoint::parse(endpoint)?, BollardApi::DEFAULT_TIMEOUT)?;
        *cached = Some((endpoint.to_owned(), api.clone()));
        Ok(api)
    }
}

impl fmt::Debug for SystemLogs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemLogs").finish_non_exhaustive()
    }
}

impl LogSource for SystemLogs {
    fn logs(
        &self,
        endpoint: &str,
        id: &str,
        request: LogRequest,
    ) -> BoxStream<'static, Result<LogChunk, LogError>> {
        match self.engine_for(endpoint) {
            Ok(api) => api
                .logs(id, &request)
                .map(|item| item.map_err(LogError::from))
                .boxed(),
            Err(error) => stream::once(std::future::ready(Err(error))).boxed(),
        }
    }
}
