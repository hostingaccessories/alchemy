//! HTTP API and web UI.
//!
//! Routes under `/api` return JSON, except `/api/events`, which streams
//! [`ApiEvent`]s as server-sent events. On the TCP listener ([`router`])
//! everything else is served from the embedded web UI build with an
//! `index.html` fallback for client-side routing; on the Unix socket
//! ([`api_router`]) everything else is a JSON 404.
//!
//! Both routers apply the request checks in [`crate::guard`] before any
//! route sees a request, so a route added under `/api` is covered by
//! nesting it, with nothing to opt into.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use alchemy_api::{
    ApiEvent, EngineStatus, ErrorCode, ErrorResponse, HealthResponse, HealthStatus, MetricsRange,
    VersionResponse,
};
use axum::extract::rejection::QueryRejection;
use axum::extract::{self, FromRef, Query, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{Stream, StreamExt as _};
use serde::Deserialize;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;

use crate::State;
use crate::events::{self, Topic};
use crate::guard::{self, Refusal, TAURI_ORIGINS};
use crate::store::SharedStore;

/// How often `/api/events` sends an SSE comment while idle, so proxies and
/// clients do not give up on a quiet stream.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct Api {
    state: State,
    store: SharedStore,
    topics: Arc<[Topic]>,
    shutdown: watch::Receiver<bool>,
}

impl FromRef<Api> for State {
    fn from_ref(api: &Api) -> Self {
        api.state.clone()
    }
}

impl FromRef<Api> for SharedStore {
    fn from_ref(api: &Api) -> Self {
        api.store.clone()
    }
}

/// Build the API router: `/api` routes only, with no web UI and no CORS.
///
/// This is what the per-user Unix socket serves. Anything outside `/api`
/// is a JSON 404. Event streams end once `shutdown` becomes `true` (or its
/// sender is dropped), so open `/api/events` connections do not hold up
/// graceful shutdown. Only the socket's share of the [`crate::guard`]
/// checks applies; see [`socket_checks`].
pub fn api_router(state: State, store: SharedStore, shutdown: watch::Receiver<bool>) -> Router {
    socket_checks(
        Router::new()
            .nest(
                alchemy_api::API_PREFIX,
                api(state, store, events::topics(), shutdown),
            )
            .fallback(api_not_found),
    )
}

/// Build the full router for the TCP listener bound to `listen`: the
/// [`api_router`] routes, CORS for the desktop shell, the web UI, and every
/// [`crate::guard`] check.
pub fn router(
    state: State,
    store: SharedStore,
    listen: IpAddr,
    shutdown: watch::Receiver<bool>,
) -> Router {
    router_with_topics(state, store, listen, events::topics(), shutdown)
}

/// [`router`], with `/api/events` publishing `topics` instead of the
/// daemon's own [`events::topics`].
pub fn router_with_topics(
    state: State,
    store: SharedStore,
    listen: IpAddr,
    topics: Vec<Topic>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    tcp_checks(
        Router::new()
            .nest(alchemy_api::API_PREFIX, api(state, store, topics, shutdown))
            .fallback(crate::web::serve),
        listen,
    )
}

/// Wrap `routes` the way the TCP listener bound to `listen` serves them.
///
/// The `Host` check is outermost, so it covers CORS preflights too. The
/// origin and content-type checks sit inside CORS, so a refusal sent to the
/// desktop shell still carries the headers it needs to read the error.
pub fn tcp_checks(routes: Router, listen: IpAddr) -> Router {
    routes
        .layer(middleware::from_fn(require_browser_safe))
        .layer(cors())
        .layer(middleware::from_fn_with_state(listen, require_host))
}

/// Wrap `routes` the way the Unix socket serves them: the content-type
/// check alone, since no browser can reach the socket.
pub fn socket_checks(routes: Router) -> Router {
    routes.layer(middleware::from_fn(require_json))
}

/// The routes under `/api`, shared by [`api_router`] and [`router`].
fn api(
    state: State,
    store: SharedStore,
    topics: Vec<Topic>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/engine", get(engine))
        .route("/events", get(events))
        .route("/metrics", get(metrics))
        .route("/metrics/history", get(metrics_history))
        .method_not_allowed_fallback(api_method_not_allowed)
        .fallback(api_not_found)
        .with_state(Api {
            state,
            store,
            topics: topics.into(),
            shutdown,
        })
}

async fn require_host(
    extract::State(listen): extract::State<IpAddr>,
    req: Request,
    next: Next,
) -> Response {
    match guard::check_host(req.headers(), listen) {
        Ok(()) => next.run(req).await,
        Err(refusal) => refuse(refusal),
    }
}

async fn require_browser_safe(req: Request, next: Next) -> Response {
    match guard::check_origin(req.method(), req.headers())
        .and_then(|()| guard::check_content_type(req.method(), req.headers()))
    {
        Ok(()) => next.run(req).await,
        Err(refusal) => refuse(refusal),
    }
}

async fn require_json(req: Request, next: Next) -> Response {
    match guard::check_content_type(req.method(), req.headers()) {
        Ok(()) => next.run(req).await,
        Err(refusal) => refuse(refusal),
    }
}

fn refuse(refusal: Refusal) -> Response {
    tracing::debug!(?refusal, "refused request");
    api_error(refusal.status(), refusal.code(), refusal.message())
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(TAURI_ORIGINS.map(HeaderValue::from_static))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE])
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: HealthStatus::Ok,
    })
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: alchemy_core::VERSION.to_owned(),
        api: alchemy_api::API_VERSION,
    })
}

async fn engine(extract::State(state): extract::State<State>) -> Json<EngineStatus> {
    Json(state.engine())
}

/// `?project=NAME` on the metrics routes.
#[derive(Debug, Deserialize)]
struct MetricsQuery {
    project: Option<String>,
}

/// `?range=24h&project=NAME` on `/api/metrics/history`.
#[derive(Debug, Deserialize)]
struct HistoryQuery {
    range: Option<String>,
    project: Option<String>,
}

async fn metrics(
    extract::State(state): extract::State<State>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Response {
    match query {
        Ok(Query(query)) => {
            Json(crate::metrics::current(&state, query.project.as_deref())).into_response()
        }
        Err(rejection) => bad_query(&rejection),
    }
}

async fn metrics_history(
    extract::State(store): extract::State<SharedStore>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(rejection) => return bad_query(&rejection),
    };
    let range = match query.range.as_deref().map(str::parse::<MetricsRange>) {
        None => MetricsRange::Hour,
        Some(Ok(range)) => range,
        Some(Err(error)) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &error.to_string(),
            );
        }
    };
    match crate::metrics::history(&store, range, query.project, unix_now()).await {
        Ok(history) => Json(history).into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot read metrics history");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "cannot read metrics history from the state database",
            )
        }
    }
}

/// The wall clock in Unix seconds, for the end of a history range.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

fn bad_query(rejection: &QueryRejection) -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        ErrorCode::BadRequest,
        &format!("invalid query: {}", rejection.body_text()),
    )
}

async fn events(
    extract::State(api): extract::State<Api>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    Sse::new(event_stream(api)).keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
}

/// Every topic's current value, then one event per change, until shutdown.
fn event_stream(api: Api) -> impl Stream<Item = Result<Event, axum::Error>> {
    events::stream(&api.state, &api.topics, api.shutdown).map(|event| sse_event(&event))
}

fn sse_event(event: &ApiEvent) -> Result<Event, axum::Error> {
    Event::default().event(event.name()).json_data(event.data())
}

async fn api_not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, ErrorCode::NotFound, "not found")
}

async fn api_method_not_allowed() -> Response {
    api_error(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorCode::MethodNotAllowed,
        "method not allowed",
    )
}

fn api_error(status: StatusCode, code: ErrorCode, error: &str) -> Response {
    (status, Json(ErrorResponse::new(code, error))).into_response()
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
    use alchemy_api::EngineState;
    use axum::body::{Body, BodyDataStream, to_bytes};
    use axum::http::Request;
    use axum::http::request::Builder;
    use axum::routing::post;
    use tower::ServiceExt as _;

    const LISTEN: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const HOST: &str = "127.0.0.1:7240";

    /// A request as a same-origin browser page sends it.
    fn req(method: Method, uri: &str) -> Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, HOST)
    }

    fn memory() -> SharedStore {
        std::sync::Arc::new(crate::store::MemoryStore::new())
    }

    /// The daemon's router with only the engine topic on `/api/events`, so
    /// these tests read engine frames alone as other topics are added.
    fn test_router(state: State) -> (Router, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        let engine = Topic::watch(State::watch_engine, ApiEvent::Engine);
        (
            router_with_topics(state, memory(), LISTEN, vec![engine], rx),
            tx,
        )
    }

    async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
        get_json_from(State::new(), uri).await
    }

    async fn get_json_from(state: State, uri: &str) -> (StatusCode, serde_json::Value) {
        let (app, _tx) = test_router(state);
        let resp = app
            .oneshot(req(Method::GET, uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// Reads SSE frames, each ending in a blank line, from a response body.
    struct Frames {
        body: BodyDataStream,
        buf: String,
    }

    impl Frames {
        fn new(resp: Response) -> Self {
            Self {
                body: resp.into_body().into_data_stream(),
                buf: String::new(),
            }
        }

        async fn next(&mut self) -> Option<String> {
            loop {
                if let Some(end) = self.buf.find("\n\n") {
                    let frame = self.buf[..end].to_owned();
                    self.buf.drain(..end + 2);
                    return Some(frame);
                }
                let chunk = self.body.next().await?.unwrap();
                self.buf.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        }

        async fn next_engine(&mut self) -> EngineStatus {
            let frame = self.next().await.expect("stream ended");
            let data = frame
                .strip_prefix("event: engine\ndata: ")
                .unwrap_or_else(|| panic!("not an engine event: {frame:?}"));
            serde_json::from_str(data).unwrap()
        }
    }

    async fn open_events(app: Router, origin: Option<&str>) -> Response {
        let mut req = req(Method::GET, "/api/events");
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn health_is_ok() {
        let (status, body) = get_json("/api/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "status": "ok" }));
    }

    #[tokio::test]
    async fn version_reports_crate_version() {
        let (status, body) = get_json("/api/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["version"], alchemy_core::VERSION);
        assert_eq!(body["api"], alchemy_api::API_VERSION);
    }

    #[tokio::test]
    async fn engine_reports_current_status() {
        let state = State::new();
        let mut ready = EngineStatus::new(EngineState::Ready);
        ready.engine_version = Some("29.8.1".into());
        state.set_engine(ready.clone());

        let (status, body) = get_json_from(state, "/api/engine").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::to_value(&ready).unwrap());
    }

    #[tokio::test]
    async fn metrics_report_the_latest_sample_or_one_project() {
        let (status, body) = get_json("/api/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "at": null, "interval": 60, "containers_in_vm": false,
                "machine": null, "projects": [], "containers": [],
            })
        );

        let state = State::new();
        let containers: Vec<alchemy_api::ContainerUsage> = ["web", "db"]
            .map(|project| alchemy_api::ContainerUsage {
                id: project.into(),
                name: format!("{project}-1"),
                project: Some(project.into()),
                ..Default::default()
            })
            .into();
        state.set_metrics(alchemy_api::MetricsResponse {
            at: Some(120),
            interval: 60,
            containers_in_vm: false,
            machine: Some(alchemy_api::MachineUsage::default()),
            projects: alchemy_core::metrics::projects_of(&containers),
            containers,
        });
        let (status, body) = get_json_from(state.clone(), "/api/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["at"], 120);
        assert_eq!(body["projects"].as_array().unwrap().len(), 2);
        assert!(body["machine"].is_object());

        let (status, body) = get_json_from(state, "/api/metrics?project=web").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["machine"], serde_json::Value::Null);
        assert_eq!(body["projects"][0]["name"], "web");
        assert_eq!(body["containers"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn metrics_history_defaults_to_an_hour_and_refuses_unknown_ranges() {
        let cases = [
            ("/api/metrics/history", StatusCode::OK, "1h"),
            ("/api/metrics/history?range=24h", StatusCode::OK, "24h"),
            (
                "/api/metrics/history?range=30d&project=web",
                StatusCode::OK,
                "30d",
            ),
            ("/api/metrics/history?range=2h", StatusCode::BAD_REQUEST, ""),
            ("/api/metrics/history?range=", StatusCode::BAD_REQUEST, ""),
        ];
        for (uri, want, range) in cases {
            let (status, body) = get_json(uri).await;
            assert_eq!(status, want, "{uri}");
            if want == StatusCode::OK {
                assert_eq!(body["range"], range, "{uri}");
                assert_eq!(body["points"], serde_json::json!([]), "{uri}");
            } else {
                assert_eq!(body["code"], "bad_request", "{uri}");
                assert!(
                    body["error"].as_str().unwrap().contains("unknown range"),
                    "{uri}: {body}"
                );
            }
        }
    }

    #[tokio::test]
    async fn events_send_engine_on_connect_and_on_change() {
        let state = State::new();
        state.set_engine(EngineStatus::new(EngineState::Starting));
        let (app, stop) = test_router(state.clone());

        let resp = open_events(app, None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let mut frames = Frames::new(resp);
        assert_eq!(frames.next_engine().await.state, EngineState::Starting);

        state.set_engine(EngineStatus::new(EngineState::Ready));
        assert_eq!(frames.next_engine().await.state, EngineState::Ready);

        state.set_engine(EngineStatus::new(EngineState::NotRunning));
        assert_eq!(frames.next_engine().await.state, EngineState::NotRunning);

        stop.send_replace(true);
        assert_eq!(frames.next().await, None);
    }

    #[tokio::test]
    async fn events_merge_every_topic() {
        let topic = |rx: watch::Receiver<EngineState>| {
            Topic::watch(
                move |_| rx.clone(),
                |s| ApiEvent::Engine(EngineStatus::new(s)),
            )
        };
        let (a, a_rx) = watch::channel(EngineState::Starting);
        let (b, b_rx) = watch::channel(EngineState::Ready);
        let (stop, stop_rx) = watch::channel(false);
        let app = router_with_topics(
            State::new(),
            memory(),
            LISTEN,
            vec![topic(a_rx), topic(b_rx)],
            stop_rx,
        );
        let mut frames = Frames::new(open_events(app, None).await);

        let mut initial = [
            frames.next_engine().await.state,
            frames.next_engine().await.state,
        ];
        initial.sort_by_key(|s| format!("{s:?}"));
        assert_eq!(initial, [EngineState::Ready, EngineState::Starting]);

        a.send_replace(EngineState::NotRunning);
        assert_eq!(frames.next_engine().await.state, EngineState::NotRunning);

        b.send_replace(EngineState::Unsupported);
        assert_eq!(frames.next_engine().await.state, EngineState::Unsupported);

        stop.send_replace(true);
        assert_eq!(frames.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn events_keep_alive_while_idle() {
        let state = State::new();
        let (app, _stop) = test_router(state.clone());
        let mut frames = Frames::new(open_events(app, None).await);
        frames.next_engine().await;

        let start = tokio::time::Instant::now();
        assert_eq!(frames.next().await.as_deref(), Some(":"));
        assert!(start.elapsed() >= KEEP_ALIVE, "{:?}", start.elapsed());
    }

    #[tokio::test]
    async fn events_end_when_sender_is_dropped() {
        let state = State::new();
        let (app, stop) = test_router(state.clone());
        let mut frames = Frames::new(open_events(app, None).await);
        frames.next_engine().await;
        drop(stop);
        assert_eq!(frames.next().await, None);
        drop(state);
    }

    #[tokio::test]
    async fn tauri_origin_can_open_events() {
        for origin in TAURI_ORIGINS {
            let (app, _stop) = test_router(State::new());
            let resp = open_events(app, Some(origin)).await;
            assert_eq!(resp.status(), StatusCode::OK, "{origin}");
            assert_eq!(
                resp.headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .unwrap(),
                origin
            );
            let mut frames = Frames::new(resp);
            assert_eq!(frames.next_engine().await.state, EngineState::NotRunning);
        }
    }

    #[tokio::test]
    async fn unknown_api_route_is_json_404() {
        let (status, body) = get_json("/api/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            serde_json::json!({ "error": "not found", "code": "not_found" })
        );
    }

    #[tokio::test]
    async fn wrong_method_on_api_route_is_json_405() {
        for uri in [
            "/api/health",
            "/api/version",
            "/api/engine",
            "/api/events",
            "/api/metrics",
            "/api/metrics/history",
        ] {
            let (app, _stop) = test_router(State::new());
            let resp = app
                .oneshot(
                    req(Method::POST, uri)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{uri}");
            assert_eq!(
                resp.headers().get(header::ALLOW).unwrap(),
                "GET,HEAD",
                "{uri}"
            );
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({ "error": "method not allowed", "code": "method_not_allowed" }),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn root_serves_ui_or_reports_it_unbuilt() {
        let (app, _stop) = test_router(State::new());
        let resp = app
            .oneshot(req(Method::GET, "/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            matches!(
                resp.status(),
                StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
            ),
            "{}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn api_router_answers_api_routes_only() {
        for (uri, status) in [
            ("/api/health", StatusCode::OK),
            ("/api/version", StatusCode::OK),
            ("/api/engine", StatusCode::OK),
            ("/api/nope", StatusCode::NOT_FOUND),
            ("/", StatusCode::NOT_FOUND),
            ("/machines", StatusCode::NOT_FOUND),
            ("/theme.js", StatusCode::NOT_FOUND),
        ] {
            let (_stop, rx) = watch::channel(false);
            let resp = api_router(State::new(), memory(), rx)
                .oneshot(
                    req(Method::GET, uri)
                        .header(header::ORIGIN, "tauri://localhost")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), status, "{uri}");
            assert!(
                resp.headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .is_none(),
                "{uri}"
            );
            if status == StatusCode::NOT_FOUND {
                let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                    serde_json::json!({ "error": "not found", "code": "not_found" }),
                    "{uri}"
                );
            }
        }
    }

    #[tokio::test]
    async fn cors_allows_tauri_origin_only() {
        for (origin, allowed) in [
            ("tauri://localhost", true),
            ("http://tauri.localhost", true),
            ("https://evil.example", false),
        ] {
            for uri in ["/api/health", "/api/engine", "/api/events"] {
                let (app, _stop) = test_router(State::new());
                let resp = app
                    .oneshot(
                        req(Method::GET, uri)
                            .header(header::ORIGIN, origin)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let got = resp.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN);
                assert_eq!(got.is_some(), allowed, "{origin} {uri}");
            }
        }
    }

    /// A stand-in for the first mutating route, until M4 adds real ones.
    fn things() -> Router {
        Router::new().route("/api/things", post(|| async { StatusCode::NO_CONTENT }))
    }

    /// A `POST`-style request with only the headers that are `Some`.
    struct Probe<'a> {
        method: Method,
        host: Option<&'a str>,
        origin: Option<&'a str>,
        content_type: Option<&'a str>,
    }

    impl Probe<'_> {
        fn post(
            host: Option<&'static str>,
            origin: Option<&'static str>,
            content_type: Option<&'static str>,
        ) -> Probe<'static> {
            Probe {
                method: Method::POST,
                host,
                origin,
                content_type,
            }
        }

        fn request(&self, uri: &str) -> Request<Body> {
            let mut builder = Request::builder().method(self.method.clone()).uri(uri);
            for (name, value) in [
                (header::HOST, self.host),
                (header::ORIGIN, self.origin),
                (header::CONTENT_TYPE, self.content_type),
            ] {
                if let Some(value) = value {
                    builder = builder.header(name, value);
                }
            }
            builder.body(Body::from("{}")).unwrap()
        }
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, Option<String>, Vec<u8>) {
        let resp = app.oneshot(request).await.unwrap();
        let status = resp.status();
        let acao = resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, acao, body.to_vec())
    }

    fn error_code(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice::<serde_json::Value>(body).unwrap()["code"].clone()
    }

    const JSON: Option<&str> = Some("application/json");

    #[tokio::test]
    async fn tcp_checks_guard_mutating_routes() {
        let forbidden = Some(StatusCode::FORBIDDEN);
        let unsupported = Some(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let cases = [
            (
                "same-origin",
                Some(HOST),
                Some("http://127.0.0.1:7240"),
                JSON,
                None,
            ),
            ("no origin", Some(HOST), None, JSON, None),
            ("tauri", Some(HOST), Some("tauri://localhost"), JSON, None),
            (
                "tauri linux",
                Some(HOST),
                Some("http://tauri.localhost"),
                JSON,
                None,
            ),
            (
                "vite dev proxy",
                Some("localhost:5173"),
                Some("http://localhost:5173"),
                Some("application/json; charset=utf-8"),
                None,
            ),
            (
                "cross-origin form",
                Some(HOST),
                Some("https://evil.example"),
                Some("application/x-www-form-urlencoded"),
                forbidden,
            ),
            (
                "cross-origin text/plain",
                Some(HOST),
                Some("https://evil.example"),
                Some("text/plain"),
                forbidden,
            ),
            (
                "cross-origin JSON",
                Some(HOST),
                Some("https://evil.example"),
                JSON,
                forbidden,
            ),
            ("opaque origin", Some(HOST), Some("null"), JSON, forbidden),
            (
                "text/plain",
                Some(HOST),
                None,
                Some("text/plain"),
                unsupported,
            ),
            (
                "same-origin text/plain",
                Some(HOST),
                Some("http://127.0.0.1:7240"),
                Some("text/plain"),
                unsupported,
            ),
            ("no content type", Some(HOST), None, None, unsupported),
            (
                "rebinding host",
                Some("evil.example:7240"),
                Some("http://evil.example:7240"),
                JSON,
                forbidden,
            ),
            ("no host", None, None, JSON, forbidden),
        ];
        for (name, host, origin, content_type, refused) in cases {
            let probe = Probe::post(host, origin, content_type);
            let app = tcp_checks(things(), LISTEN);
            let (status, _, body) = send(app, probe.request("/api/things")).await;
            match refused {
                None => assert_eq!(status, StatusCode::NO_CONTENT, "{name}"),
                Some(want) => {
                    assert_eq!(status, want, "{name}");
                    let code = if want == StatusCode::FORBIDDEN {
                        "forbidden"
                    } else {
                        "unsupported_media_type"
                    };
                    assert_eq!(error_code(&body), code, "{name}");
                }
            }
        }
    }

    #[tokio::test]
    async fn host_check_covers_every_request() {
        for (method, uri) in [
            (Method::GET, "/api/health"),
            (Method::GET, "/api/events"),
            (Method::GET, "/"),
            (Method::OPTIONS, "/api/health"),
        ] {
            let probe = Probe {
                method: method.clone(),
                host: Some("evil.example:7240"),
                origin: Some("tauri://localhost"),
                content_type: None,
            };
            let (app, _stop) = test_router(State::new());
            let (status, acao, body) = send(app, probe.request(uri)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
            assert_eq!(acao, None, "{method} {uri}");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({ "error": Refusal::Host.message(), "code": "forbidden" }),
                "{method} {uri}"
            );
        }
    }

    #[tokio::test]
    async fn tauri_preflight_passes_and_refusals_stay_readable() {
        let preflight = req(Method::OPTIONS, "/api/things")
            .header(header::ORIGIN, "tauri://localhost")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
            .body(Body::empty())
            .unwrap();
        let (status, acao, _) = send(tcp_checks(things(), LISTEN), preflight).await;
        assert!(status.is_success(), "{status}");
        assert_eq!(acao.as_deref(), Some("tauri://localhost"));

        let probe = Probe::post(Some(HOST), Some("tauri://localhost"), Some("text/plain"));
        let app = tcp_checks(things(), LISTEN);
        let (status, acao, body) = send(app, probe.request("/api/things")).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(acao.as_deref(), Some("tauri://localhost"));
        assert_eq!(error_code(&body), "unsupported_media_type");
    }

    #[tokio::test]
    async fn tcp_checks_accept_the_configured_listen_address() {
        let lan = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        for (host, origin, want) in [
            (
                "192.168.1.5:7240",
                "http://192.168.1.5:7240",
                StatusCode::NO_CONTENT,
            ),
            (
                "localhost:7240",
                "http://localhost:7240",
                StatusCode::NO_CONTENT,
            ),
            (
                "10.0.0.9:7240",
                "http://10.0.0.9:7240",
                StatusCode::FORBIDDEN,
            ),
            ("nas.lan:7240", "http://nas.lan:7240", StatusCode::FORBIDDEN),
        ] {
            let probe = Probe::post(Some(host), Some(origin), JSON);
            let app = tcp_checks(things(), lan);
            let (status, _, _) = send(app, probe.request("/api/things")).await;
            assert_eq!(status, want, "{host}");
        }
    }

    #[tokio::test]
    async fn socket_checks_content_type_only() {
        let unsupported = StatusCode::UNSUPPORTED_MEDIA_TYPE;
        for (host, origin, content_type, want) in [
            (Some("alchemyd"), None, JSON, StatusCode::NO_CONTENT),
            (None, None, JSON, StatusCode::NO_CONTENT),
            (
                Some("evil.example"),
                Some("https://evil.example"),
                JSON,
                StatusCode::NO_CONTENT,
            ),
            (Some("alchemyd"), None, Some("text/plain"), unsupported),
            (Some("alchemyd"), None, None, unsupported),
        ] {
            let probe = Probe::post(host, origin, content_type);
            let (status, _, _) = send(socket_checks(things()), probe.request("/api/things")).await;
            assert_eq!(status, want, "{host:?} {origin:?} {content_type:?}");
        }
    }

    #[tokio::test]
    async fn api_router_answers_the_cli_host() {
        let (_stop, rx) = watch::channel(false);
        let probe = Probe {
            method: Method::GET,
            host: Some("alchemyd"),
            origin: None,
            content_type: None,
        };
        let app = api_router(State::new(), memory(), rx);
        let (status, _, _) = send(app, probe.request("/api/health")).await;
        assert_eq!(status, StatusCode::OK);
    }
}
