//! [`Daemon`] over HTTP/1.1 on alchemyd's Unix socket.

use std::collections::VecDeque;
use std::error::Error as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1;
use hyper::{Request, Response, header};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use super::{Daemon, EventStream, Method, Opened, Reply};
use crate::error::{ClientError, ProtocolError};
use crate::sse::{SseEvent, SseParser};

/// Upper bound on a single exchange so a wedged daemon cannot hang the CLI.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on a whole response body.
const MAX_BODY: usize = 16 << 20;

/// The `Host` header sent on the socket, where it names nothing.
const HOST: &str = "alchemyd";

/// Talks to alchemyd over HTTP on its Unix socket, one connection per
/// exchange.
pub(crate) struct HttpDaemon {
    socket: PathBuf,
}

impl HttpDaemon {
    /// A client for the daemon at `socket`. Nothing is opened until the
    /// first request.
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Response<Incoming>, ClientError> {
        let stream = UnixStream::connect(&self.socket).await.map_err(|e| {
            ClientError::DaemonUnreachable(format!("{}: {e}", self.socket.display()))
        })?;
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| from_hyper(&e))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder().uri(path).header(header::HOST, HOST);
        if method != Method::GET {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let body = Full::new(Bytes::copy_from_slice(body.unwrap_or_default()));
        let req = req
            .method(method)
            .body(body)
            .map_err(|e| ProtocolError::Http(e.to_string()))?;
        sender.send_request(req).await.map_err(|e| from_hyper(&e))
    }
}

impl Daemon for HttpDaemon {
    type Events = HttpEvents;

    fn socket(&self) -> &Path {
        &self.socket
    }

    async fn request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Reply, ClientError> {
        with_timeout(&self.socket, async {
            let resp = self.send(method, path, body).await?;
            reply(resp).await
        })
        .await
    }

    async fn events(&mut self, path: &str) -> Result<Opened<HttpEvents>, ClientError> {
        with_timeout(&self.socket, async {
            let resp = self.send(Method::GET, path, None).await?;
            if !resp.status().is_success() {
                return reply(resp).await.map(Opened::Refused);
            }
            Ok(Opened::Stream(HttpEvents {
                socket: self.socket.clone(),
                body: resp.into_body(),
                parser: SseParser::new(),
                ready: VecDeque::new(),
                started: false,
            }))
        })
        .await
    }
}

/// The events on an [`HttpDaemon`] stream's connection.
#[derive(Debug)]
pub(crate) struct HttpEvents {
    socket: PathBuf,
    body: Incoming,
    parser: SseParser,
    ready: VecDeque<SseEvent>,
    started: bool,
}

impl HttpEvents {
    async fn next_event(&mut self) -> Result<Option<SseEvent>, ClientError> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            let Some(frame) = self.body.frame().await else {
                return Ok(None);
            };
            if let Ok(data) = frame.map_err(|e| from_hyper(&e))?.into_data() {
                self.ready.extend(self.parser.push(&data)?);
            }
        }
    }
}

impl EventStream for HttpEvents {
    async fn next(&mut self) -> Result<Option<SseEvent>, ClientError> {
        if self.started {
            return self.next_event().await;
        }
        let socket = self.socket.clone();
        let first = with_timeout(&socket, self.next_event()).await?;
        self.started = true;
        Ok(first)
    }
}

async fn reply(resp: Response<Incoming>) -> Result<Reply, ClientError> {
    let status = resp.status().as_u16();
    let body = Limited::new(resp.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| match e.downcast::<hyper::Error>() {
            Ok(e) => from_hyper(&e),
            Err(e) => ProtocolError::Http(e.to_string()).into(),
        })?
        .to_bytes();
    Ok(Reply {
        status,
        body: body.to_vec(),
    })
}

/// Run `exchange` against the daemon at `socket`, giving up after
/// [`EXCHANGE_TIMEOUT`].
async fn with_timeout<T>(
    socket: &Path,
    exchange: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, exchange)
        .await
        .map_err(|_| ClientError::DaemonUnreachable(format!("{}: timed out", socket.display())))?
}

/// A daemon that closes the connection before answering has gone away
/// mid-request; an I/O failure keeps only its kind; anything else is HTTP
/// this client does not understand, such as an older alchemyd that
/// predates HTTP on the socket.
fn from_hyper(err: &hyper::Error) -> ClientError {
    if err.is_incomplete_message() {
        return ClientError::ConnectionClosed;
    }
    let mut source = err.source();
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return ProtocolError::Io { kind: io.kind() }.into();
        }
        source = cause.source();
    }
    ProtocolError::Http(err.to_string()).into()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::convert::Infallible;

    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::UnixListener;

    use super::*;

    struct Server {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("d.sock")
    }

    fn serve(app: Router) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Server {
            _dir: dir,
            socket,
            task,
        }
    }

    /// Answers every connection with `bytes`, read after the request head,
    /// then closes it.
    fn raw(bytes: &'static [u8]) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(bytes).await;
            }
        });
        Server {
            _dir: dir,
            socket,
            task,
        }
    }

    fn sse(body: &'static [&'static str]) -> axum::response::Response {
        let chunks = body
            .iter()
            .map(|chunk| Ok::<_, Infallible>(Bytes::from_static(chunk.as_bytes())));
        axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .unwrap()
    }

    fn app() -> Router {
        Router::new()
            .route(
                "/api/version",
                get(|| async { r#"{"version":"1.2.3","api":1}"# }),
            )
            .route(
                "/api/missing",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        r#"{"error":"not found","code":"not_found"}"#,
                    )
                }),
            )
            .route(
                "/api/events",
                get(|| async {
                    sse(&[
                        ": hello\n\nevent: engine\nda",
                        "ta: {\"state\":\"ready\"}\n\nevent: engine\r\n",
                        "data: 2\r\n\r\n",
                    ])
                }),
            )
            .route("/api/quiet", get(|| async { sse(&[]) }))
    }

    #[tokio::test]
    async fn requests_return_status_and_body() {
        let server = serve(app());
        let mut daemon = HttpDaemon::new(server.socket.clone());
        let cases = [
            ("/api/version", 200, r#"{"version":"1.2.3","api":1}"#),
            (
                "/api/missing",
                404,
                r#"{"error":"not found","code":"not_found"}"#,
            ),
            ("/nope", 404, ""),
        ];
        for (path, status, body) in cases {
            let reply = daemon.request(Method::GET, path, None).await.unwrap();
            assert_eq!(reply.status, status, "{path}");
            assert_eq!(String::from_utf8(reply.body).unwrap(), body, "{path}");
        }
        assert_eq!(daemon.socket(), server.socket);
    }

    #[tokio::test]
    async fn mutating_requests_send_json_with_their_body() {
        use axum::http::HeaderMap;
        let echo = |method: &'static str| {
            move |headers: HeaderMap, body: String| async move {
                let content_type = headers
                    .get(header::CONTENT_TYPE)
                    .map_or("none", |v| v.to_str().unwrap())
                    .to_owned();
                format!("{method} {content_type} {body}")
            }
        };
        let app = Router::new().route("/api/thing", get(echo("GET")).put(echo("PUT")));
        let server = serve(app);
        let mut daemon = HttpDaemon::new(server.socket.clone());
        let cases = [
            (Method::GET, None, "GET none "),
            (
                Method::PUT,
                Some(&b"{\"a\":1}"[..]),
                "PUT application/json {\"a\":1}",
            ),
            (Method::PUT, None, "PUT application/json "),
        ];
        for (method, body, want) in cases {
            let reply = daemon
                .request(method.clone(), "/api/thing", body)
                .await
                .unwrap();
            assert_eq!(reply.status, 200, "{method}");
            assert_eq!(String::from_utf8(reply.body).unwrap(), want, "{method}");
        }
    }

    #[tokio::test]
    async fn events_arrive_parsed_across_chunks() {
        let server = serve(app());
        let mut daemon = HttpDaemon::new(server.socket.clone());
        let Opened::Stream(mut events) = daemon.events("/api/events").await.unwrap() else {
            panic!("refused");
        };
        let mut got = Vec::new();
        while let Some(event) = events.next().await.unwrap() {
            got.push((event.event, event.data));
        }
        assert_eq!(
            got,
            [
                ("engine".to_owned(), r#"{"state":"ready"}"#.to_owned()),
                ("engine".to_owned(), "2".to_owned()),
            ]
        );

        let Opened::Stream(mut quiet) = daemon.events("/api/quiet").await.unwrap() else {
            panic!("refused");
        };
        assert_eq!(quiet.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_refused_stream_returns_its_reply() {
        let server = serve(app());
        let mut daemon = HttpDaemon::new(server.socket.clone());
        let Opened::Refused(reply) = daemon.events("/api/missing").await.unwrap() else {
            panic!("opened");
        };
        assert_eq!(reply.status, 404);
        assert_eq!(
            reply.body,
            br#"{"error":"not found","code":"not_found"}"#.to_vec()
        );
    }

    #[tokio::test]
    async fn transport_failures_map_to_client_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut missing = HttpDaemon::new(tmp.path().join("nope.sock"));
        let err = missing
            .request(Method::GET, "/api/version", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");
        let err = missing.events("/api/events").await.unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");

        let silent = raw(b"");
        let err = HttpDaemon::new(silent.socket.clone())
            .request(Method::GET, "/api/version", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::ConnectionClosed), "{err:?}");
        assert_eq!(err.exit_code(), 76);

        let framed = raw(b"\x00\x00\x00\x02{}");
        let err = HttpDaemon::new(framed.socket.clone())
            .request(Method::GET, "/api/version", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Protocol(ProtocolError::Http(_))),
            "{err:?}"
        );
        assert_eq!(err.kind(), "protocol");
        assert_eq!(err.exit_code(), 76);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_daemon_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let listener = UnixListener::bind(&socket).unwrap();
        let task = tokio::spawn(async move {
            let mut open = Vec::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                open.push(stream);
            }
        });
        let mut daemon = HttpDaemon::new(socket);
        let err = daemon
            .request(Method::GET, "/api/version", None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ClientError::DaemonUnreachable(m) if m.ends_with(": timed out")),
            "{err:?}"
        );
        let err = daemon.events("/api/events").await.unwrap_err();
        assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");
        task.abort();
    }
}
