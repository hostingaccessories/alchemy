//! What a command runs with: the daemon client, the output sink, and the
//! global flags.

use std::io::Write;
use std::path::Path;

use alchemy_api::autostart::Autostart;
use alchemy_api::sse::SseEvent;
use alchemy_api::{
    ApiEvent, AppList, EngineStatus, ErrorCode, ErrorResponse, MetricsResponse, OperationList,
    PrivilegeList, ReconcileStatus, VersionResponse, Wake,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::daemon::{Daemon, DaemonInfo, EventStream, Method, Opened, Reply};
use crate::error::{ClientError, ProtocolError};
use crate::output::Output;

/// The route that reports the daemon's version, for the handshake.
pub(crate) const VERSION_PATH: &str = "/api/version";

/// The route that streams [`ApiEvent`]s.
pub(crate) const EVENTS_PATH: &str = "/api/events";

/// Everything a command's `run` needs. Commands reach the daemon only
/// through this.
///
/// The first request performs the version handshake; its answer is kept
/// for the rest of the invocation, so a command never asks for
/// [`VERSION_PATH`] itself.
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

    /// `GET` `path`, such as `/api/engine`, and decode its JSON body. An
    /// error status comes back as a [`ClientError`].
    pub(crate) async fn get<T: DeserializeOwned>(&mut self, path: &str) -> Result<T, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        let reply = self.daemon.request(Method::GET, path, None).await?;
        accept(&reply, &daemon, self.command)?;
        decode(&reply.body)
    }

    /// `PUT` `body` as JSON to `path` and decode the JSON answer, as
    /// [`Self::get`] does.
    pub(crate) async fn put<B: Serialize, T: DeserializeOwned>(
        &mut self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        let body = serde_json::to_vec(body).map_err(|e| ProtocolError::Http(e.to_string()))?;
        let reply = self.daemon.request(Method::PUT, path, Some(&body)).await?;
        accept(&reply, &daemon, self.command)?;
        decode(&reply.body)
    }

    /// `POST` `body` as JSON to `path` and decode the JSON answer, as
    /// [`Self::get`] does.
    pub(crate) async fn post<B: Serialize, T: DeserializeOwned>(
        &mut self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        let body = serde_json::to_vec(body).map_err(|e| ProtocolError::Http(e.to_string()))?;
        let reply = self.daemon.request(Method::POST, path, Some(&body)).await?;
        accept(&reply, &daemon, self.command)?;
        decode(&reply.body)
    }

    /// Send `body` as JSON to `path` with `method` and decode the JSON
    /// answer. A 409 `conflict` or 422 `invalid` comes back as
    /// [`Sent::Refused`] for the command to act on; any other error status
    /// is a [`ClientError`], as with [`Self::get`].
    pub(crate) async fn send<B: Serialize, T: DeserializeOwned>(
        &mut self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<Sent<T>, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        let body = serde_json::to_vec(body).map_err(|e| ProtocolError::Http(e.to_string()))?;
        let reply = self.daemon.request(method, path, Some(&body)).await?;
        if matches!(reply.status, 409 | 422) {
            let refusal = error_reply(&reply)?;
            if matches!(refusal.code, ErrorCode::Conflict | ErrorCode::Invalid) {
                return Ok(Sent::Refused(refusal));
            }
        }
        accept(&reply, &daemon, self.command)?;
        decode(&reply.body).map(Sent::Done)
    }

    /// `POST` to `path` with no body and decode the JSON answer. Any error
    /// the daemon answers with comes back as [`Sent::Refused`] for the
    /// command to word, except a sign that the daemon predates the route,
    /// which is a [`ClientError`] as with [`Self::get`].
    pub(crate) async fn post_empty<T: DeserializeOwned>(
        &mut self,
        path: &str,
    ) -> Result<Sent<T>, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        let reply = self.daemon.request(Method::POST, path, None).await?;
        if (400..600).contains(&reply.status) {
            let refusal = error_reply(&reply)?;
            if !matches!(
                refusal.code,
                ErrorCode::NotFound | ErrorCode::MethodNotAllowed
            ) {
                return Ok(Sent::Refused(refusal));
            }
        }
        accept(&reply, &daemon, self.command)?;
        decode(&reply.body).map(Sent::Done)
    }

    /// Follow [`EVENTS_PATH`]. A refusal comes back as a [`ClientError`].
    pub(crate) async fn events(&mut self) -> Result<Subscription<D::Events>, ClientError> {
        self.stream(EVENTS_PATH)
            .await
            .map(|inner| Subscription { inner })
    }

    /// Follow the event stream at `path`, such as an app's logs, as raw
    /// events. A refusal comes back as a [`ClientError`], as with
    /// [`Self::get`].
    pub(crate) async fn stream(&mut self, path: &str) -> Result<D::Events, ClientError> {
        let daemon = self.daemon_info().await?.version.clone();
        match self.daemon.events(path).await? {
            Opened::Stream(inner) => Ok(inner),
            Opened::Refused(reply) => match accept(&reply, &daemon, self.command) {
                Ok(()) => Err(ClientError::Unexpected(status_text(reply.status))),
                Err(err) => Err(err),
            },
        }
    }
}

/// Decode the JSON `data` of a raw event as `T`.
pub(crate) fn decode_data<T: DeserializeOwned>(raw: &SseEvent) -> Result<T, ClientError> {
    decode(raw.data.as_bytes())
}

/// What [`Context::send`] came to.
#[derive(Debug)]
pub(crate) enum Sent<T> {
    /// The daemon did it and answered with this.
    Done(T),
    /// The daemon refused it, as a `conflict` or as `invalid` for
    /// [`Context::send`].
    Refused(ErrorResponse),
}

/// The events from [`Context::events`], decoded.
pub(crate) struct Subscription<S> {
    inner: S,
}

impl<S: EventStream> Subscription<S> {
    /// The next event this alchemy knows, or `None` once the daemon ends
    /// the stream. Events it does not know, from a newer daemon, are
    /// skipped.
    pub(crate) async fn next(&mut self) -> Result<Option<ApiEvent>, ClientError> {
        while let Some(raw) = self.inner.next().await? {
            if let Some(event) = decode_event(&raw)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

/// An [`ApiEvent`] from its SSE name and data, or `None` for a name this
/// build does not know.
pub(crate) fn decode_event(raw: &SseEvent) -> Result<Option<ApiEvent>, ClientError> {
    match raw.event.as_str() {
        "engine" => decode::<EngineStatus>(raw.data.as_bytes()).map(|e| Some(ApiEvent::Engine(e))),
        "metrics" => {
            decode::<MetricsResponse>(raw.data.as_bytes()).map(|m| Some(ApiEvent::Metrics(m)))
        }
        "apps" => decode::<AppList>(raw.data.as_bytes()).map(|a| Some(ApiEvent::Apps(a))),
        "operations" => {
            decode::<OperationList>(raw.data.as_bytes()).map(|o| Some(ApiEvent::Operations(o)))
        }
        "privileges" => {
            decode::<PrivilegeList>(raw.data.as_bytes()).map(|p| Some(ApiEvent::Privileges(p)))
        }
        "reconcile" => {
            decode::<ReconcileStatus>(raw.data.as_bytes()).map(|r| Some(ApiEvent::Reconcile(r)))
        }
        "woke" => decode::<Wake>(raw.data.as_bytes()).map(|w| Some(ApiEvent::Woke(w))),
        "autostart" => {
            decode::<Autostart>(raw.data.as_bytes()).map(|a| Some(ApiEvent::Autostart(a)))
        }
        _ => Ok(None),
    }
}

async fn handshake<D: Daemon>(daemon: &mut D) -> Result<DaemonInfo, ClientError> {
    let reply = daemon.request(Method::GET, VERSION_PATH, None).await?;
    if !is_success(reply.status) {
        let err = error_reply(&reply)?;
        return Err(ClientError::Daemon(err.error));
    }
    let VersionResponse { version, api } = decode(&reply.body)?;
    Ok(DaemonInfo { version, api })
}

/// Check `reply`'s status. A `not_found` or `method_not_allowed` error
/// means the daemon predates the route, so it is too old for `command`;
/// any other error is the daemon's to report.
fn accept(reply: &Reply, daemon: &str, command: &'static str) -> Result<(), ClientError> {
    if is_success(reply.status) {
        return Ok(());
    }
    let err = error_reply(reply)?;
    match (reply.status, err.code) {
        (404, ErrorCode::NotFound) | (405, ErrorCode::MethodNotAllowed) => {
            Err(ClientError::DaemonTooOld {
                daemon: daemon.to_owned(),
                command,
            })
        }
        _ => Err(ClientError::Daemon(err.error)),
    }
}

/// The [`ErrorResponse`] in a reply that is not a success. A status that
/// is not an error either is unexpected; an undecodable body is a
/// protocol error.
fn error_reply(reply: &Reply) -> Result<ErrorResponse, ClientError> {
    if !(400..600).contains(&reply.status) {
        return Err(ClientError::Unexpected(status_text(reply.status)));
    }
    decode(&reply.body)
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

fn status_text(status: u16) -> String {
    format!("HTTP status {status}")
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, ClientError> {
    serde_json::from_slice(body).map_err(|e| ProtocolError::Decode(e).into())
}

#[cfg(test)]
impl<D: Daemon> Context<D, Vec<u8>> {
    /// A context for `command` that writes to a buffer.
    pub(crate) fn buffered(daemon: D, json: bool, command: &'static str) -> Self {
        Self::new(
            daemon,
            Output::new(Vec::new(), json, crate::output::Color::Off),
            command,
        )
    }

    /// Everything written to the output so far.
    pub(crate) fn written(&self) -> String {
        String::from_utf8_lossy(self.out.get_ref()).into_owned()
    }

    /// Everything written as progress, to stderr, so far.
    pub(crate) fn progress_written(&self) -> String {
        self.out.progress_written()
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
    use alchemy_api::autostart::{AutostartMode, LogHint, Supervisor};
    use alchemy_api::{EngineState, HealthResponse, HealthStatus};

    use super::*;
    use crate::daemon::fake::{FakeDaemon, sse};

    fn engine(state: EngineState) -> EngineStatus {
        EngineStatus::new(state)
    }

    #[tokio::test]
    async fn handshakes_once_per_invocation() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(
                Method::GET,
                "/api/health",
                &serde_json::json!({ "status": "ok" }),
            )
            .events([])
            .ok(
                Method::GET,
                "/api/health",
                &serde_json::json!({ "status": "ok" }),
            );
        let mut ctx = Context::buffered(fake, false, "test");
        let ok = HealthResponse {
            status: HealthStatus::Ok,
        };
        assert_eq!(ctx.get::<HealthResponse>("/api/health").await.unwrap(), ok);
        let mut stream = ctx.events().await.unwrap();
        assert!(stream.next().await.unwrap().is_none());
        assert_eq!(ctx.get::<HealthResponse>("/api/health").await.unwrap(), ok);
        assert_eq!(
            ctx.daemon_info().await.unwrap(),
            &DaemonInfo {
                version: "1.2.3".into(),
                api: 1,
            }
        );
        assert_eq!(
            ctx.daemon().requests(),
            [
                "GET /api/version",
                "GET /api/health",
                "GET /api/events",
                "GET /api/health"
            ]
        );
    }

    fn summarize(result: Result<EngineStatus, ClientError>) -> String {
        match result {
            Ok(engine) => format!("ok {:?}", engine.state),
            Err(err) => format!("{} {}: {err}", err.exit_code(), err.kind()),
        }
    }

    #[tokio::test]
    async fn replies_map_to_values_and_errors() {
        let ready = serde_json::to_vec(&engine(EngineState::Ready)).unwrap();
        let error = |code: &str, msg: &str| {
            serde_json::to_vec(&serde_json::json!({ "error": msg, "code": code })).unwrap()
        };
        let cases: Vec<(u16, Vec<u8>, &str)> = vec![
            (200, ready.clone(), "ok Ready"),
            (204, ready, "ok Ready"),
            (
                404,
                error("not_found", "not found"),
                "76 daemon_too_old: alchemyd 0.0.1 is too old for `app up`",
            ),
            (
                405,
                error("method_not_allowed", "method not allowed"),
                "76 daemon_too_old: alchemyd 0.0.1 is too old for `app up`",
            ),
            (
                404,
                error("unknown", "no such app"),
                "1 daemon: alchemyd: no such app",
            ),
            (
                404,
                br#"{"error":"not found"}"#.to_vec(),
                "1 daemon: alchemyd: not found",
            ),
            (405, error("not_found", "odd"), "1 daemon: alchemyd: odd"),
            (
                500,
                error("internal", "engine monitor stopped"),
                "1 daemon: alchemyd: engine monitor stopped",
            ),
            (
                400,
                error("some_new_code", "bad"),
                "1 daemon: alchemyd: bad",
            ),
            (
                500,
                b"oops".to_vec(),
                "76 protocol: cannot talk to alchemyd: decode: expected value at line 1 column 1",
            ),
            (
                200,
                b"{}".to_vec(),
                "76 protocol: cannot talk to alchemyd: decode: missing field `state` at line 1 column 2",
            ),
            (
                302,
                Vec::new(),
                "76 unexpected_response: unexpected response from alchemyd: HTTP status 302",
            ),
        ];
        for (status, body, want) in cases {
            let fake = FakeDaemon::new().version("0.0.1", 1).reply(
                Method::GET,
                "/api/engine",
                status,
                body.clone(),
            );
            let mut ctx = Context::buffered(fake, false, "app up");
            let got = summarize(ctx.get::<EngineStatus>("/api/engine").await);
            assert_eq!(got, want, "{status} {}", String::from_utf8_lossy(&body));

            if (200..300).contains(&status) {
                continue;
            }
            let fake = FakeDaemon::new()
                .version("0.0.1", 1)
                .refuse_events(status, body.clone());
            let mut ctx = Context::buffered(fake, false, "app up");
            let got = match ctx.events().await {
                Ok(_) => "opened".to_owned(),
                Err(err) => format!("{} {}: {err}", err.exit_code(), err.kind()),
            };
            assert_eq!(got, want, "events {status}");
        }
    }

    #[tokio::test]
    async fn the_handshake_reports_its_own_failures() {
        let cases: Vec<(FakeDaemon, &str)> = vec![
            (
                FakeDaemon::new().fail(
                    Method::GET,
                    VERSION_PATH,
                    ClientError::DaemonUnreachable("/run/alchemyd.sock: refused".into()),
                ),
                "69 daemon_unreachable",
            ),
            (
                FakeDaemon::new().reply(
                    Method::GET,
                    VERSION_PATH,
                    404,
                    br#"{"error":"not found","code":"not_found"}"#.to_vec(),
                ),
                "1 daemon",
            ),
            (
                FakeDaemon::new().reply(
                    Method::GET,
                    VERSION_PATH,
                    200,
                    br#"{"version":"0.0.1"}"#.to_vec(),
                ),
                "76 protocol",
            ),
        ];
        for (fake, want) in cases {
            let mut ctx = Context::buffered(fake, false, "test");
            let err = ctx.get::<EngineStatus>("/api/engine").await.unwrap_err();
            assert_eq!(format!("{} {}", err.exit_code(), err.kind()), want, "{err}");
            assert_eq!(ctx.daemon().requests(), ["GET /api/version"]);
        }
    }

    #[tokio::test]
    async fn subscriptions_decode_known_events_and_skip_the_rest() {
        let ready = serde_json::to_string(&engine(EngineState::Ready)).unwrap();
        let fake = FakeDaemon::new().version("1.2.3", 1).events([
            Ok(sse("future", "{}")),
            Ok(sse("engine", &ready)),
            Ok(sse("message", "x")),
            Ok(sse("engine", "nope")),
        ]);
        let mut ctx = Context::buffered(fake, false, "status");
        let mut events = ctx.events().await.unwrap();
        assert_eq!(
            events.next().await.unwrap(),
            Some(ApiEvent::Engine(engine(EngineState::Ready)))
        );
        let err = events.next().await.unwrap_err();
        assert_eq!(err.kind(), "protocol", "{err}");
        assert_eq!(events.next().await.unwrap(), None);
    }

    #[test]
    fn decode_event_knows_every_api_event_name() {
        let events = [
            ApiEvent::Engine(engine(EngineState::Starting)),
            ApiEvent::Metrics(MetricsResponse {
                interval: 60,
                ..MetricsResponse::default()
            }),
            ApiEvent::Apps(AppList::default()),
            ApiEvent::Operations(OperationList::default()),
            ApiEvent::Privileges(PrivilegeList::default()),
            ApiEvent::Reconcile(ReconcileStatus::default()),
            ApiEvent::Woke(Wake {
                slept_for: 600,
                at: 1_700_000_000,
            }),
            ApiEvent::Autostart(Autostart {
                mode: AutostartMode::Login,
                available_modes: AutostartMode::ALL.to_vec(),
                log_hint: LogHint::Terminal,
                supervisor: Supervisor::Manual,
                pending: None,
                last: None,
                unavailable: Vec::new(),
            }),
        ];
        for event in events {
            let raw = SseEvent {
                event: event.name().to_owned(),
                data: serde_json::to_string(&event.data()).unwrap(),
            };
            assert_eq!(decode_event(&raw).unwrap(), Some(event));
        }
    }
}
