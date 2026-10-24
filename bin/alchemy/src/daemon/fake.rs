//! A scripted [`Daemon`] for unit tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

use std::collections::VecDeque;
use std::future::{Future, ready};
use std::path::Path;

use serde::Serialize;

use super::{Daemon, EventStream, Method, Opened, Reply};
use crate::context::{EVENTS_PATH, VERSION_PATH};
use crate::error::ClientError;
use crate::sse::SseEvent;

/// The socket path a [`FakeDaemon`] reports.
pub(crate) const FAKE_SOCKET: &str = "/run/alchemyd.sock";

/// A raw event named `event` carrying `data`.
pub(crate) fn sse(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.to_owned(),
        data: data.to_owned(),
    }
}

enum Script {
    One(Result<Reply, ClientError>),
    Events(Result<Opened<VecDeque<Result<SseEvent, ClientError>>>, ClientError>),
}

/// Answers requests from a script, in order, and records every request as
/// `METHOD path`, and every body sent. A request that does not match the
/// next scripted one panics.
#[derive(Default)]
pub(crate) struct FakeDaemon {
    script: VecDeque<(String, Script)>,
    requests: Vec<String>,
    bodies: Vec<String>,
}

impl FakeDaemon {
    /// A daemon with an empty script.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Answer the version handshake as alchemyd `version` speaking API
    /// version `api`.
    pub(crate) fn version(self, version: &str, api: u32) -> Self {
        self.ok(
            Method::GET,
            VERSION_PATH,
            &serde_json::json!({ "version": version, "api": api }),
        )
    }

    /// Answer `method path` with 200 and `body` as JSON.
    pub(crate) fn ok(self, method: Method, path: &str, body: &impl Serialize) -> Self {
        self.reply(method, path, 200, serde_json::to_vec(body).unwrap())
    }

    /// Answer `method path` with an [`alchemy_api::ErrorResponse`].
    pub(crate) fn error(
        self,
        method: Method,
        path: &str,
        status: u16,
        code: &str,
        error: &str,
    ) -> Self {
        let body = serde_json::json!({ "error": error, "code": code });
        self.reply(method, path, status, serde_json::to_vec(&body).unwrap())
    }

    /// Answer `method path` with `status` and the raw `body`.
    pub(crate) fn reply(self, method: Method, path: &str, status: u16, body: Vec<u8>) -> Self {
        self.push(&method, path, Script::One(Ok(Reply { status, body })))
    }

    /// Fail `method path` with `err`, as a transport failure would.
    pub(crate) fn fail(self, method: Method, path: &str, err: ClientError) -> Self {
        self.push(&method, path, Script::One(Err(err)))
    }

    /// Open the event stream, send `items`, then end it.
    pub(crate) fn events(
        self,
        items: impl IntoIterator<Item = Result<SseEvent, ClientError>>,
    ) -> Self {
        let items = items.into_iter().collect();
        self.push(
            &Method::GET,
            EVENTS_PATH,
            Script::Events(Ok(Opened::Stream(items))),
        )
    }

    /// Refuse the event stream with `status` and the raw `body`.
    pub(crate) fn refuse_events(self, status: u16, body: Vec<u8>) -> Self {
        let refused = Opened::Refused(Reply { status, body });
        self.push(&Method::GET, EVENTS_PATH, Script::Events(Ok(refused)))
    }

    /// Fail opening the event stream with `err`.
    pub(crate) fn fail_events(self, err: ClientError) -> Self {
        self.push(&Method::GET, EVENTS_PATH, Script::Events(Err(err)))
    }

    /// Every request received so far, in order, as `METHOD path`.
    pub(crate) fn requests(&self) -> &[String] {
        &self.requests
    }

    /// Every request body received so far, in order, as text.
    pub(crate) fn bodies(&self) -> &[String] {
        &self.bodies
    }

    fn push(mut self, method: &Method, path: &str, script: Script) -> Self {
        self.script.push_back((format!("{method} {path}"), script));
        self
    }

    fn next(&mut self, method: &Method, path: &str) -> Script {
        let req = format!("{method} {path}");
        self.requests.push(req.clone());
        let Some((want, script)) = self.script.pop_front() else {
            panic!("unscripted request {req}");
        };
        assert_eq!(req, want, "requests out of script order");
        script
    }
}

impl Daemon for FakeDaemon {
    type Events = FakeEvents;

    fn socket(&self) -> &Path {
        Path::new(FAKE_SOCKET)
    }

    fn request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> impl Future<Output = Result<Reply, ClientError>> {
        if let Some(body) = body {
            self.bodies.push(String::from_utf8_lossy(body).into_owned());
        }
        ready(match self.next(&method, path) {
            Script::One(reply) => reply,
            Script::Events(_) => panic!("{method} {path} is scripted as events"),
        })
    }

    fn events(
        &mut self,
        path: &str,
    ) -> impl Future<Output = Result<Opened<FakeEvents>, ClientError>> {
        ready(match self.next(&Method::GET, path) {
            Script::Events(opened) => opened.map(|opened| match opened {
                Opened::Stream(items) => Opened::Stream(FakeEvents(items)),
                Opened::Refused(reply) => Opened::Refused(reply),
            }),
            Script::One(_) => panic!("{path} is scripted as a single reply"),
        })
    }
}

/// The scripted events on a [`FakeDaemon`] stream.
#[derive(Debug)]
pub(crate) struct FakeEvents(VecDeque<Result<SseEvent, ClientError>>);

impl EventStream for FakeEvents {
    fn next(&mut self) -> impl Future<Output = Result<Option<SseEvent>, ClientError>> {
        ready(self.0.pop_front().transpose())
    }
}
