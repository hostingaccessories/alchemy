//! The shell's own client of alchemyd, over HTTP/1.1 on its TCP listener.
//!
//! The web UI talks to alchemyd by itself; this client is only for what the
//! shell does natively: checking that a daemon answers, following
//! `/api/events` for the tray, and the tray's few actions. Requests carry
//! the TCP token as `Authorization: Bearer`, read again for each one, and
//! a JSON content type on everything but `GET`, as the daemon's guard
//! requires. No `Origin` is sent, so the guard treats the shell as a
//! client that is not a browser.

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use alchemy_api::sse::{SseEvent, SseParser};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1;
use hyper::{Request, Response, header};
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tokio::net::TcpStream;

pub use hyper::Method;

use crate::launch::Probe;

/// Where alchemyd listens by default, `alchemyd::DEFAULT_LISTEN`. The web
/// UI's `DEFAULT_DAEMON_URL` names the same address.
pub const DAEMON_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7240);

/// Upper bound on one request and its answer.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the health probe, so a launch decides quickly.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an open event stream may stay silent before it counts as
/// gone. alchemyd sends a keep-alive every 15 seconds, so this is three
/// missed ones, such as after the computer slept with the stream open.
pub const STREAM_IDLE: Duration = Duration::from_secs(45);

/// Upper bound on a whole response body.
const MAX_BODY: usize = 16 << 20;

/// Reads the TCP token for each request, or says why it can't.
pub type TokenSource = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// Why an exchange with alchemyd failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClientError {
    /// Nothing accepted the connection, or it broke before an answer.
    #[error("alchemyd isn't answering: {0}")]
    Unreachable(String),
    /// The daemon accepted but did not answer in time.
    #[error("alchemyd didn't answer in time")]
    TimedOut,
    /// The token could not be read.
    #[error("{0}")]
    Token(String),
    /// The answer was not HTTP this client understands.
    #[error("alchemyd sent something Alchemy doesn't understand: {0}")]
    Protocol(String),
}

/// A complete HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The HTTP status code.
    pub status: u16,
    /// The body, usually JSON.
    pub body: Vec<u8>,
}

impl Reply {
    /// Whether the status is a 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The outcome of opening the event stream.
#[derive(Debug)]
pub enum Opened {
    /// The daemon accepted and is streaming.
    Stream(Events),
    /// The daemon answered with a status other than success, such as a
    /// 401 when the token is stale.
    Refused(Reply),
}

/// Talks to alchemyd on its TCP listener, one connection per exchange.
#[derive(Clone)]
pub struct HttpDaemon {
    addr: SocketAddr,
    token: TokenSource,
}

impl fmt::Debug for HttpDaemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpDaemon")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl HttpDaemon {
    /// A client for the daemon at `addr`, signing in with what `token`
    /// reads. Nothing is opened until the first request.
    pub fn new(addr: SocketAddr, token: TokenSource) -> Self {
        Self { addr, token }
    }

    /// Send `method` to `path`, such as `/api/engine/start`, with `body`
    /// as its JSON body if given, and return the status and the body.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Reply, ClientError> {
        let token = (self.token)().map_err(ClientError::Token)?;
        within(EXCHANGE_TIMEOUT, async {
            let resp = self.send(method, path, body, Some(&token)).await?;
            reply(resp).await
        })
        .await
    }

    /// Open `GET /api/events`. Only the answer's head is bounded by a
    /// timeout; [`Events::next`] bounds each wait after that.
    pub async fn events(&self) -> Result<Opened, ClientError> {
        let token = (self.token)().map_err(ClientError::Token)?;
        within(EXCHANGE_TIMEOUT, async {
            let resp = self
                .send(Method::GET, "/api/events", None, Some(&token))
                .await?;
            if !resp.status().is_success() {
                return reply(resp).await.map(Opened::Refused);
            }
            Ok(Opened::Stream(Events {
                body: resp.into_body(),
                parser: SseParser::new(),
                ready: VecDeque::new(),
                idle: STREAM_IDLE,
            }))
        })
        .await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        token: Option<&str>,
    ) -> Result<Response<Incoming>, ClientError> {
        let stream = TcpStream::connect(self.addr)
            .await
            .map_err(|e| ClientError::Unreachable(format!("{}: {e}", self.addr)))?;
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| from_hyper(&e))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder()
            .uri(path)
            .header(header::HOST, self.addr.to_string());
        if method != Method::GET {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = Full::new(Bytes::copy_from_slice(body.unwrap_or_default()));
        let req = req
            .method(method)
            .body(body)
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        sender.send_request(req).await.map_err(|e| from_hyper(&e))
    }
}

impl Probe for HttpDaemon {
    /// Whether `GET /api/health`, which needs no token, answers with
    /// success.
    async fn answers(&self) -> bool {
        within(PROBE_TIMEOUT, async {
            let resp = self.send(Method::GET, "/api/health", None, None).await?;
            Ok(resp.status().is_success())
        })
        .await
        .unwrap_or(false)
    }
}

/// The events on an open stream, in the order the daemon sent them.
#[derive(Debug)]
pub struct Events {
    body: Incoming,
    parser: SseParser,
    ready: VecDeque<SseEvent>,
    idle: Duration,
}

impl Events {
    /// Give up on the stream after `idle` without a byte, instead of
    /// [`STREAM_IDLE`].
    #[must_use]
    pub fn with_idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// The next event, or `None` once the daemon ends the stream. A
    /// stream silent for longer than its idle limit is
    /// [`ClientError::TimedOut`].
    pub async fn next(&mut self) -> Result<Option<SseEvent>, ClientError> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            let frame = tokio::time::timeout(self.idle, self.body.frame())
                .await
                .map_err(|_| ClientError::TimedOut)?;
            let Some(frame) = frame else {
                return Ok(None);
            };
            if let Ok(data) = frame.map_err(|e| from_hyper(&e))?.into_data() {
                let events = self
                    .parser
                    .push(&data)
                    .map_err(|e| ClientError::Protocol(e.to_string()))?;
                self.ready.extend(events);
            }
        }
    }
}

async fn reply(resp: Response<Incoming>) -> Result<Reply, ClientError> {
    let status = resp.status().as_u16();
    let body = Limited::new(resp.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| ClientError::Protocol(e.to_string()))?
        .to_bytes();
    Ok(Reply {
        status,
        body: body.to_vec(),
    })
}

async fn within<T>(
    limit: Duration,
    exchange: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::time::timeout(limit, exchange)
        .await
        .map_err(|_| ClientError::TimedOut)?
}

fn from_hyper(err: &hyper::Error) -> ClientError {
    if err.is_incomplete_message() || err.is_closed() || err.is_canceled() {
        return ClientError::Unreachable("the connection closed".to_owned());
    }
    ClientError::Protocol(err.to_string())
}
