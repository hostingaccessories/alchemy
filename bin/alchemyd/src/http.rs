//! HTTP API and web UI.
//!
//! Routes under `/api` return JSON, except `/api/events`, which streams
//! [`ApiEvent`]s as server-sent events. On the TCP listener ([`router`])
//! everything else is served from the embedded web UI build with an
//! `index.html` fallback for client-side routing; on the Unix socket
//! ([`api_router`]) everything else is a JSON 404.

use std::sync::Arc;
use std::time::Duration;

use alchemy_api::{
    ApiEvent, EngineStatus, ErrorCode, ErrorResponse, HealthResponse, HealthStatus, VersionResponse,
};
use axum::extract::{self, FromRef};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{Stream, StreamExt as _};
use tokio::sync::watch;
use tower_http::cors::CorsLayer;

use crate::State;
use crate::events::{self, Topic};

/// Origins the Tauri shell serves its bundled UI from (macOS and Linux
/// respectively). The browser UI is same-origin and needs no CORS.
const TAURI_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

/// How often `/api/events` sends an SSE comment while idle, so proxies and
/// clients do not give up on a quiet stream.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
struct Api {
    state: State,
    topics: Arc<[Topic]>,
    shutdown: watch::Receiver<bool>,
}

impl FromRef<Api> for State {
    fn from_ref(api: &Api) -> Self {
        api.state.clone()
    }
}

/// Build the API router: `/api` routes only, with no web UI and no CORS.
///
/// This is what the per-user Unix socket serves. Anything outside `/api`
/// is a JSON 404. Event streams end once `shutdown` becomes `true` (or its
/// sender is dropped), so open `/api/events` connections do not hold up
/// graceful shutdown.
pub fn api_router(state: State, shutdown: watch::Receiver<bool>) -> Router {
    Router::new()
        .nest(
            alchemy_api::API_PREFIX,
            api(state, events::topics(), shutdown),
        )
        .fallback(api_not_found)
}

/// Build the full router for the TCP listener: the [`api_router`] routes,
/// CORS for the desktop shell, and the web UI.
pub fn router(state: State, shutdown: watch::Receiver<bool>) -> Router {
    router_with_topics(state, events::topics(), shutdown)
}

/// [`router`], with `/api/events` publishing `topics` instead of the
/// daemon's own [`events::topics`].
pub fn router_with_topics(
    state: State,
    topics: Vec<Topic>,
    shutdown: watch::Receiver<bool>,
) -> Router {
    Router::new()
        .nest(alchemy_api::API_PREFIX, api(state, topics, shutdown))
        .fallback(crate::web::serve)
        .layer(cors())
}

/// The routes under `/api`, shared by [`api_router`] and [`router`].
fn api(state: State, topics: Vec<Topic>, shutdown: watch::Receiver<bool>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/engine", get(engine))
        .route("/events", get(events))
        .method_not_allowed_fallback(api_method_not_allowed)
        .fallback(api_not_found)
        .with_state(Api {
            state,
            topics: topics.into(),
            shutdown,
        })
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
    use tower::ServiceExt as _;

    /// The daemon's router with only the engine topic on `/api/events`, so
    /// these tests read engine frames alone as other topics are added.
    fn test_router(state: State) -> (Router, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        let engine = Topic::watch(State::watch_engine, ApiEvent::Engine);
        (router_with_topics(state, vec![engine], rx), tx)
    }

    async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
        get_json_from(State::new(), uri).await
    }

    async fn get_json_from(state: State, uri: &str) -> (StatusCode, serde_json::Value) {
        let (app, _tx) = test_router(state);
        let resp = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
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
        let mut req = Request::get("/api/events");
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
        let app = router_with_topics(State::new(), vec![topic(a_rx), topic(b_rx)], stop_rx);
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
        for uri in ["/api/health", "/api/version", "/api/engine", "/api/events"] {
            let (app, _stop) = test_router(State::new());
            let resp = app
                .oneshot(Request::post(uri).body(Body::empty()).unwrap())
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
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
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
            let resp = api_router(State::new(), rx)
                .oneshot(
                    Request::get(uri)
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
                        Request::get(uri)
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
}
