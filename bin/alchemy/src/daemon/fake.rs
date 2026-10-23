//! A scripted [`Daemon`] for unit tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::future::{Future, ready};
use std::path::Path;

use alchemy_ipc::{Request, Response};

use super::{Daemon, ResponseStream};
use crate::error::ClientError;

/// The socket path a [`FakeDaemon`] reports.
pub(crate) const FAKE_SOCKET: &str = "/run/alchemyd.sock";

enum Reply {
    One(Result<Response, ClientError>),
    Stream(VecDeque<Result<Response, ClientError>>),
}

/// Answers requests from a script, in order, and records every request.
/// A request that does not match the next scripted one panics.
#[derive(Default)]
pub(crate) struct FakeDaemon {
    script: VecDeque<(Request, Reply)>,
    requests: Vec<Request>,
}

impl FakeDaemon {
    /// A daemon with an empty script.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Answer the `Version` handshake as alchemyd `daemon` speaking
    /// `protocol`.
    pub(crate) fn version(self, daemon: &str, protocol: u32) -> Self {
        self.reply(
            Request::Version,
            Response::Version {
                daemon: daemon.to_owned(),
                protocol,
            },
        )
    }

    /// Answer `req` with `resp`.
    pub(crate) fn reply(self, req: Request, resp: Response) -> Self {
        self.push(req, Reply::One(Ok(resp)))
    }

    /// Fail `req` with `err`, as a transport failure would.
    pub(crate) fn fail(self, req: Request, err: ClientError) -> Self {
        self.push(req, Reply::One(Err(err)))
    }

    /// Answer the streaming `req` with `items`, then end the stream.
    pub(crate) fn stream(
        self,
        req: Request,
        items: impl IntoIterator<Item = Result<Response, ClientError>>,
    ) -> Self {
        self.push(req, Reply::Stream(items.into_iter().collect()))
    }

    /// Every request received so far, in order.
    pub(crate) fn requests(&self) -> &[Request] {
        &self.requests
    }

    fn push(mut self, req: Request, reply: Reply) -> Self {
        self.script.push_back((req, reply));
        self
    }

    fn next(&mut self, req: &Request) -> Reply {
        self.requests.push(req.clone());
        let Some((want, reply)) = self.script.pop_front() else {
            panic!("unscripted request {req:?}");
        };
        assert_eq!(*req, want, "requests out of script order");
        reply
    }
}

impl Daemon for FakeDaemon {
    type Stream = FakeStream;

    fn socket(&self) -> &Path {
        Path::new(FAKE_SOCKET)
    }

    fn request(&mut self, req: &Request) -> impl Future<Output = Result<Response, ClientError>> {
        ready(match self.next(req) {
            Reply::One(resp) => resp,
            Reply::Stream(_) => panic!("{req:?} is scripted as a stream"),
        })
    }

    fn stream(&mut self, req: &Request) -> impl Future<Output = Result<FakeStream, ClientError>> {
        ready(match self.next(req) {
            Reply::Stream(items) => Ok(FakeStream(items)),
            Reply::One(Err(err)) => Err(err),
            Reply::One(Ok(_)) => panic!("{req:?} is scripted as a single response"),
        })
    }
}

/// The scripted responses to a [`FakeDaemon`] stream.
pub(crate) struct FakeStream(VecDeque<Result<Response, ClientError>>);

impl ResponseStream for FakeStream {
    fn next(&mut self) -> impl Future<Output = Result<Option<Response>, ClientError>> {
        ready(self.0.pop_front().transpose())
    }
}
