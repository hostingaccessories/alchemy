//! [`FakeLogSource`]: container logs held in memory, for tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use alchemy_api::logs::LogStream;
use alchemy_core::logs::split_timestamp;
use futures_util::StreamExt as _;
use futures_util::stream::{self, BoxStream};
use tokio::sync::mpsc;

use super::{LogChunk, LogError, LogRequest, LogSource};
use crate::engine::ApiError;

/// A [`LogSource`] over logs written by the test.
///
/// Each container's log is a list of frames, each normally one line with
/// its timestamp. A request gets the frames that match its `since` and
/// `tail`, as the engine would, and with `follow` and the container
/// running, every frame written after it until the container stops. A
/// container never written to or started is unknown, and reading it
/// fails as a removed container does. Cheap to clone: clones share the
/// logs.
#[derive(Debug, Clone, Default)]
pub struct FakeLogSource {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    containers: HashMap<String, Container>,
    requests: Vec<(String, LogRequest)>,
}

#[derive(Debug, Default)]
struct Container {
    frames: Vec<LogChunk>,
    running: bool,
    failure: Option<u16>,
    followers: Vec<mpsc::UnboundedSender<LogChunk>>,
}

impl FakeLogSource {
    /// A source that knows no containers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `line` written at `time`, such as
    /// `2024-05-06T07:08:09.000000001Z`, to container `id`'s `stream`,
    /// starting the container if it is unknown.
    pub fn write(&self, id: &str, stream: LogStream, time: &str, line: &str) {
        self.write_raw(id, stream, format!("{time} {line}\n").as_bytes());
    }

    /// Append raw `bytes` to container `id`'s `stream`, starting the
    /// container if it is unknown.
    pub fn write_raw(&self, id: &str, stream: LogStream, bytes: &[u8]) {
        let chunk = LogChunk {
            stream,
            bytes: bytes.to_vec(),
        };
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let container = inner
            .containers
            .entry(id.to_owned())
            .or_insert_with(|| Container {
                running: true,
                ..Container::default()
            });
        container
            .followers
            .retain(|follower| follower.send(chunk.clone()).is_ok());
        container.frames.push(chunk);
    }

    /// Mark container `id` running, so following it waits for new lines.
    pub fn start(&self, id: &str) {
        self.with(id, |container| container.running = true);
    }

    /// Mark container `id` stopped, ending every stream following it.
    pub fn stop(&self, id: &str) {
        self.with(id, |container| {
            container.running = false;
            container.followers.clear();
        });
    }

    /// Fail every later request for container `id` with HTTP `status`.
    pub fn fail(&self, id: &str, status: u16) {
        self.with(id, |container| container.failure = Some(status));
    }

    /// How many streams are following container `id` now.
    pub fn followers(&self, id: &str) -> usize {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner
            .containers
            .get(id)
            .map_or(0, |container| container.followers.len())
    }

    /// Every request so far, in order, by container ID.
    pub fn requests(&self) -> Vec<(String, LogRequest)> {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.requests.clone()
    }

    fn with(&self, id: &str, change: impl FnOnce(&mut Container)) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let container = inner.containers.entry(id.to_owned()).or_default();
        change(container);
    }
}

impl LogSource for FakeLogSource {
    fn logs(
        &self,
        _endpoint: &str,
        id: &str,
        request: LogRequest,
    ) -> BoxStream<'static, Result<LogChunk, LogError>> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.requests.push((id.to_owned(), request));
        let Some(container) = inner.containers.get_mut(id) else {
            return failed(404);
        };
        if let Some(status) = container.failure {
            return failed(status);
        }
        let since: Vec<LogChunk> = container
            .frames
            .iter()
            .filter(|frame| {
                let text = String::from_utf8_lossy(&frame.bytes);
                match (request.since, split_timestamp(&text).0) {
                    (Some(since), Some((_, at))) => at.secs() >= since,
                    _ => true,
                }
            })
            .cloned()
            .collect();
        let keep = request
            .tail
            .map_or(since.len(), |tail| since.len().min(tail as usize));
        let backlog = since
            .into_iter()
            .rev()
            .take(keep)
            .rev()
            .map(Ok)
            .collect::<Vec<_>>();
        let backlog = stream::iter(backlog);
        if !(request.follow && container.running) {
            return backlog.boxed();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        container.followers.push(tx);
        let live = stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (Ok(chunk), rx))
        });
        backlog.chain(live).boxed()
    }
}

fn failed(status: u16) -> BoxStream<'static, Result<LogChunk, LogError>> {
    let error = ApiError::Server {
        status,
        message: format!("fake engine error {status}"),
    };
    stream::once(std::future::ready(Err(LogError::Engine(error)))).boxed()
}
