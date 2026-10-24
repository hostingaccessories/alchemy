//! HTTP API and web UI.
//!
//! Routes under `/api` return JSON, except `/api/events`, which streams
//! [`ApiEvent`]s as server-sent events, and `/api/apps/{name}/logs`,
//! which streams an app's [`LogEvent`]s. The app routes are described in
//! [`alchemy_api::apps`], and the lifecycle operation routes, whose
//! `/api/operations/{id}/events` streams too, in
//! [`alchemy_api::operations`]. On the TCP listener ([`router`])
//! everything else is served from the embedded web UI build with an
//! `index.html` fallback for client-side routing; on the Unix socket
//! ([`api_router`]) everything else is a JSON 404.
//!
//! Both routers apply the request checks in [`crate::guard`] before any
//! route sees a request, so a route added under `/api` is covered by
//! nesting it, with nothing to opt into. The TCP listener then requires
//! signing in, as [`crate::auth`] describes, for every `/api` route but
//! health and version; it also serves `/login`, which redeems a launch
//! code that only the socket's `POST /api/session/launch` issues.
//!
//! Creating apps and editing their files needs an [`AppEditor`], which
//! [`with_editor`] adds to either router; without one those routes answer
//! 503.

mod app_files;

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alchemy_api::apps::{App, AppList, AppSettings, AppSettingsUpdate};
use alchemy_api::logs::LogEvent;
use alchemy_api::operations::{
    Action, ActionRequest, OperationEvent, OperationList, OperationResponse,
};
use alchemy_api::session::{SessionAuth, SessionResponse};
use alchemy_api::{
    ApiEvent, EngineStatus, ErrorCode, ErrorResponse, HealthResponse, HealthStatus, MetricsRange,
    VersionResponse,
};
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{self, FromRef, Query, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::stream::{self, BoxStream};
use futures_util::{Stream, StreamExt as _};
use serde::Deserialize;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;

use crate::State;
use crate::apps::{self, AppEditor, settings};
use crate::auth::{LAUNCH_PATH, LOGIN_PATH};
use crate::events::{self, Topic};
use crate::guard::{self, Refusal, TAURI_ORIGINS};
use crate::logs::{self, LogQuery, LogsRefusal, SharedLogSource};
use crate::metrics::Scope;
use crate::operations::{CancelError, Operations, StartError};
use crate::session::Auth;
use crate::store::SharedStore;

/// How often `/api/events` sends an SSE comment while idle, so proxies and
/// clients do not give up on a quiet stream.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// How many operations `GET /api/operations` returns at most.
pub const RECENT_OPERATIONS: usize = 50;

#[derive(Debug, Clone)]
struct Api {
    state: State,
    store: SharedStore,
    topics: Arc<[Topic]>,
    shutdown: watch::Receiver<bool>,
    logs: SharedLogSource,
    operations: Operations,
    door: Door,
}

/// Which front door [`api`] is serving, for the one route that differs.
#[derive(Debug, Clone)]
enum Door {
    /// The Unix socket, which issues launch codes from its [`Auth`].
    Socket(Auth),
    /// The TCP listener, which refuses to.
    Tcp,
}

/// What the TCP listener adds to the API: the address it is bound to, for
/// the `Host` check, and its sign-in state.
#[derive(Debug, Clone)]
pub struct Tcp {
    /// The address the listener is bound to.
    pub listen: IpAddr,
    /// The token, launch codes, and sessions it accepts.
    pub auth: Auth,
}

impl FromRef<Api> for Door {
    fn from_ref(api: &Api) -> Self {
        api.door.clone()
    }
}

impl FromRef<Api> for Operations {
    fn from_ref(api: &Api) -> Self {
        api.operations.clone()
    }
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
/// checks applies; see [`socket_checks`]. App logs are read through
/// `logs`, and lifecycle actions run through `operations`. Launch codes
/// for the TCP listener are issued from `auth`.
pub fn api_router(
    state: State,
    store: SharedStore,
    shutdown: watch::Receiver<bool>,
    logs: SharedLogSource,
    operations: Operations,
    auth: Auth,
) -> Router {
    socket_checks(
        Router::new()
            .nest(
                alchemy_api::API_PREFIX,
                api(
                    state,
                    store,
                    events::topics(),
                    shutdown,
                    logs,
                    operations,
                    Door::Socket(auth),
                ),
            )
            .fallback(api_not_found),
    )
}

/// Build the full router for the TCP listener `tcp`: the [`api_router`]
/// routes, CORS for the desktop shell, the web UI, `/login`, every
/// [`crate::guard`] check, and signing in.
pub fn router(
    state: State,
    store: SharedStore,
    tcp: Tcp,
    shutdown: watch::Receiver<bool>,
    logs: SharedLogSource,
    operations: Operations,
) -> Router {
    router_with_topics(
        state,
        store,
        tcp,
        events::topics(),
        shutdown,
        logs,
        operations,
    )
}

/// [`router`], with `/api/events` publishing `topics` instead of the
/// daemon's own [`events::topics`].
pub fn router_with_topics(
    state: State,
    store: SharedStore,
    tcp: Tcp,
    topics: Vec<Topic>,
    shutdown: watch::Receiver<bool>,
    logs: SharedLogSource,
    operations: Operations,
) -> Router {
    let login = Router::new()
        .route(LOGIN_PATH, get(login))
        .with_state(tcp.auth.clone());
    tcp_checks(
        Router::new()
            .nest(
                alchemy_api::API_PREFIX,
                api(state, store, topics, shutdown, logs, operations, Door::Tcp),
            )
            .merge(login)
            .fallback(crate::web::serve),
        tcp,
    )
}

/// Wrap `routes` the way the TCP listener `tcp` serves them.
///
/// The `Host` check is outermost, so it covers CORS preflights too. The
/// origin and content-type checks sit inside CORS, so a refusal sent to the
/// desktop shell still carries the headers it needs to read the error.
/// Signing in is checked last, after every refusal that needs no
/// credential, and inside CORS for the same reason.
pub fn tcp_checks(routes: Router, tcp: Tcp) -> Router {
    routes
        .layer(middleware::from_fn_with_state(tcp.auth, require_auth))
        .layer(middleware::from_fn(require_browser_safe))
        .layer(cors())
        .layer(middleware::from_fn_with_state(tcp.listen, require_host))
}

/// Give `router`'s app file routes `editor` to create and edit apps with.
pub fn with_editor(router: Router, editor: Arc<AppEditor>) -> Router {
    router.layer(Extension(editor))
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
    logs: SharedLogSource,
    operations: Operations,
    door: Door,
) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/session", get(session))
        .route(LAUNCH_PATH, post(launch))
        .route("/engine", get(engine))
        .route("/events", get(events))
        .route("/metrics", get(metrics))
        .route("/metrics/history", get(metrics_history))
        .route(
            "/apps",
            get(list_apps)
                .post(app_files::create)
                .layer(app_files::body_limit()),
        )
        .route("/apps/{name}", get(show_app))
        .route("/apps/{name}/logs", get(app_logs))
        .route("/apps/{name}/actions", post(start_action))
        .route("/apps/{name}/plan", get(plan_action))
        .route("/operations", get(list_operations))
        .route("/operations/{id}", get(show_operation))
        .route("/operations/{id}/cancel", post(cancel_operation))
        .route("/operations/{id}/events", get(operation_events))
        .route(
            "/apps/{name}/files/{file}",
            get(app_files::read)
                .put(app_files::write)
                .layer(app_files::body_limit()),
        )
        .route("/settings/apps", get(app_settings).put(set_app_settings))
        .method_not_allowed_fallback(api_method_not_allowed)
        .fallback(api_not_found)
        .with_state(Api {
            state,
            store,
            topics: topics.into(),
            shutdown,
            logs,
            operations,
            door,
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
    match guard::check_fetch_site(req.uri().path(), req.headers())
        .and_then(|()| guard::check_origin(req.method(), req.headers()))
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

async fn require_auth(
    extract::State(auth): extract::State<Auth>,
    mut req: Request,
    next: Next,
) -> Response {
    if crate::auth::is_public(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    match auth.authenticate(req.headers()) {
        Some(how) => {
            req.extensions_mut().insert(how);
            next.run(req).await
        }
        None => unauthorized(),
    }
}

/// 401, with the `WWW-Authenticate` challenge RFC 9110 asks for.
fn unauthorized() -> Response {
    tracing::debug!("refused a request that is not signed in");
    let mut resp = api_error(
        StatusCode::UNAUTHORIZED,
        ErrorCode::Unauthorized,
        "not signed in: run `alchemy open` to open Alchemy in your browser",
    );
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"alchemyd\""),
    );
    resp
}

fn refuse(refusal: Refusal) -> Response {
    tracing::debug!(?refusal, "refused request");
    api_error(refusal.status(), refusal.code(), refusal.message())
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(TAURI_ORIGINS.map(HeaderValue::from_static))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
}

/// `GET /api/session`. A request that reached it over TCP was let in by
/// [`require_auth`], which says how; one without that came over the
/// socket.
async fn session(how: Option<Extension<SessionAuth>>) -> Json<SessionResponse> {
    Json(SessionResponse {
        auth: how.map_or(SessionAuth::Socket, |Extension(how)| how),
    })
}

/// `POST /api/session/launch`, which only the socket answers: a launch
/// code is a way in to the TCP listener for whoever can open the socket.
async fn launch(extract::State(door): extract::State<Door>) -> Response {
    let Door::Socket(auth) = door else {
        return api_error(
            StatusCode::FORBIDDEN,
            ErrorCode::Forbidden,
            "launch codes are issued only on the alchemyd socket: run `alchemy open`",
        );
    };
    match auth.launch() {
        Ok(launch) => ([(header::CACHE_CONTROL, "no-store")], Json(launch)).into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot make a launch code");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "cannot make a launch code",
            )
        }
    }
}

/// `?code=` on `/login`.
#[derive(Debug, Deserialize)]
struct LoginQuery {
    code: Option<String>,
}

/// `GET /login?code=CODE` on the TCP listener: redeem a launch code, set
/// the session cookie, and send the browser to the web UI. Nothing it
/// answers is cached, and the code never leaves in a `Referer`.
async fn login(
    extract::State(auth): extract::State<Auth>,
    query: Result<Query<LoginQuery>, QueryRejection>,
) -> Response {
    let code = query.ok().and_then(|Query(q)| q.code).unwrap_or_default();
    let private = [
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
    ];
    match auth.login(&code) {
        Ok(Some(cookie)) => (
            StatusCode::SEE_OTHER,
            private,
            [
                (header::LOCATION, "/".to_owned()),
                (header::SET_COOKIE, cookie),
            ],
        )
            .into_response(),
        Ok(None) => (
            StatusCode::UNAUTHORIZED,
            private,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            LOGIN_REFUSED,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot start a session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                private,
                "cannot start a session",
            )
                .into_response()
        }
    }
}

/// What `/login` shows for a code that is unknown, used, or expired.
const LOGIN_REFUSED: &str = concat!(
    "<!doctype html>\n<html lang=\"en\">\n<meta charset=\"utf-8\">\n",
    "<title>Alchemy</title>\n<h1>This sign-in link no longer works</h1>\n",
    "<p>A link from <code>alchemy open</code> works once, within a minute. ",
    "Run <code>alchemy open</code> again for a new one.</p>\n</html>\n",
);

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

/// `?app=NAME` or `?project=NAME` on the metrics routes.
#[derive(Debug, Deserialize)]
struct MetricsQuery {
    app: Option<String>,
    project: Option<String>,
}

/// `?range=24h&app=NAME` or `?range=24h&project=NAME` on
/// `/api/metrics/history`.
#[derive(Debug, Deserialize)]
struct HistoryQuery {
    range: Option<String>,
    app: Option<String>,
    project: Option<String>,
}

/// Why a metrics route refused its `app` or `project`.
#[derive(Debug)]
enum ScopeRefusal {
    Both,
    UnknownApp(String),
}

impl IntoResponse for ScopeRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::Both => api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                "give app or project, not both",
            ),
            Self::UnknownApp(name) => api_error(
                StatusCode::NOT_FOUND,
                ErrorCode::AppNotFound,
                &format!("no app is named {name:?}"),
            ),
        }
    }
}

/// Whose figures `app` or `project` asks for: an app by its folder name,
/// through the Compose project it resolves to, or a project by name.
fn metrics_scope(
    state: &State,
    app: Option<String>,
    project: Option<String>,
) -> Result<Scope, ScopeRefusal> {
    match (app, project) {
        (Some(_), Some(_)) => Err(ScopeRefusal::Both),
        (None, None) => Ok(Scope::All),
        (None, Some(project)) => Ok(Scope::Project(project)),
        (Some(name), None) => match apps::find(&state.apps(), &name) {
            Some(app) => Ok(app
                .project
                .clone()
                .map_or(Scope::Unresolved, Scope::Project)),
            None => Err(ScopeRefusal::UnknownApp(name)),
        },
    }
}

async fn metrics(
    extract::State(state): extract::State<State>,
    query: Result<Query<MetricsQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(&rejection),
    };
    match metrics_scope(&state, query.app, query.project) {
        Ok(scope) => Json(crate::metrics::current(&state, &scope)).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn metrics_history(
    extract::State(api): extract::State<Api>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(rejection) => return bad_query(&rejection),
    };
    let scope = match metrics_scope(&api.state, query.app, query.project) {
        Ok(scope) => scope,
        Err(refusal) => return refusal.into_response(),
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
    match crate::metrics::history(&api.store, range, scope, unix_now()).await {
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

async fn list_apps(extract::State(state): extract::State<State>) -> Json<AppList> {
    Json(state.apps())
}

async fn show_app(
    extract::State(state): extract::State<State>,
    name: Result<extract::Path<String>, PathRejection>,
) -> Response {
    let extract::Path(name) = match name {
        Ok(name) => name,
        Err(rejection) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &rejection.body_text(),
            );
        }
    };
    match apps::find(&state.apps(), &name) {
        Some(app) => Json::<&App>(app).into_response(),
        None => api_error(
            StatusCode::NOT_FOUND,
            ErrorCode::AppNotFound,
            &format!("no app is named {name:?}"),
        ),
    }
}

/// `?service=web&tail=200&follow=true` on `/api/apps/{name}/logs`.
#[derive(Debug, Deserialize)]
struct LogsQuery {
    service: Option<String>,
    tail: Option<String>,
    follow: Option<String>,
}

/// The app's logs as server-sent events, until `done`, the client goes
/// away, or shutdown.
async fn app_logs(
    extract::State(api): extract::State<Api>,
    name: Result<extract::Path<String>, PathRejection>,
    query: Result<Query<LogsQuery>, QueryRejection>,
) -> Response {
    let extract::Path(name) = match name {
        Ok(name) => name,
        Err(rejection) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &rejection.body_text(),
            );
        }
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return bad_query(&rejection),
    };
    let query = match LogQuery::new(
        &api.state.apps(),
        &name,
        query.service.as_deref(),
        query.tail.as_deref(),
        query.follow.as_deref(),
    ) {
        Ok(query) => query,
        Err(refusal @ LogsRefusal::AppNotFound(_)) => {
            return api_error(
                StatusCode::NOT_FOUND,
                ErrorCode::AppNotFound,
                &refusal.to_string(),
            );
        }
        Err(refusal) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &refusal.to_string(),
            );
        }
    };
    let events = match logs::stream(api.logs.clone(), api.state.clone(), query) {
        Ok(events) => events,
        Err(refusal) => {
            tracing::debug!(%refusal, "refused a log stream");
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Busy,
                &refusal.to_string(),
            );
        }
    };
    let mut shutdown = api.shutdown.clone();
    let events = events
        .map(|event| log_event(&event))
        .take_until(async move {
            let _ = shutdown.wait_for(|stop| *stop).await;
        });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

fn log_event(event: &LogEvent) -> Result<Event, axum::Error> {
    Event::default().event(event.name()).json_data(event.data())
}

fn bad_path(rejection: &PathRejection) -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        ErrorCode::BadRequest,
        &rejection.body_text(),
    )
}

fn start_refused(error: &StartError) -> Response {
    let (status, code) = match error {
        StartError::AppNotFound { .. } => (StatusCode::NOT_FOUND, ErrorCode::AppNotFound),
        StartError::Project { .. } => (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal),
        _ => (StatusCode::CONFLICT, ErrorCode::Conflict),
    };
    api_error(status, code, &error.to_string())
}

fn operation_not_found(id: &str) -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        ErrorCode::OperationNotFound,
        &format!("no operation has id {id:?}"),
    )
}

fn history_unreadable(error: &crate::store::StoreError) -> Response {
    tracing::warn!(%error, "cannot read operation history");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::Internal,
        "cannot read operation history from the state database",
    )
}

/// Start an action on an app: 202 with the running operation.
async fn start_action(
    extract::State(operations): extract::State<Operations>,
    name: Result<extract::Path<String>, PathRejection>,
    body: Bytes,
) -> Response {
    let extract::Path(name) = match name {
        Ok(name) => name,
        Err(rejection) => return bad_path(&rejection),
    };
    let request: ActionRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &format!("invalid action: {err}"),
            );
        }
    };
    match operations.start(&name, request.action, request.force).await {
        Ok(operation) => {
            (StatusCode::ACCEPTED, Json(OperationResponse { operation })).into_response()
        }
        Err(error) => start_refused(&error),
    }
}

/// `?action=up` on `/api/apps/{name}/plan`.
#[derive(Debug, Deserialize)]
struct PlanQuery {
    action: Option<String>,
}

async fn plan_action(
    extract::State(operations): extract::State<Operations>,
    name: Result<extract::Path<String>, PathRejection>,
    query: Result<Query<PlanQuery>, QueryRejection>,
) -> Response {
    let extract::Path(name) = match name {
        Ok(name) => name,
        Err(rejection) => return bad_path(&rejection),
    };
    let action = match query {
        Ok(Query(PlanQuery {
            action: Some(action),
        })) => action.parse::<Action>(),
        Ok(Query(PlanQuery { action: None })) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                "missing ?action=",
            );
        }
        Err(rejection) => return bad_query(&rejection),
    };
    match action {
        Ok(action) => match operations.plan(&name, action) {
            Ok(plan) => Json(plan).into_response(),
            Err(error) => start_refused(&error),
        },
        Err(error) => api_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::BadRequest,
            &error.to_string(),
        ),
    }
}

/// `?app=NAME` on `/api/operations`.
#[derive(Debug, Deserialize)]
struct OperationsQuery {
    app: Option<String>,
}

async fn list_operations(
    extract::State(operations): extract::State<Operations>,
    query: Result<Query<OperationsQuery>, QueryRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(rejection) => return bad_query(&rejection),
    };
    match operations
        .recent(query.app.as_deref(), RECENT_OPERATIONS)
        .await
    {
        Ok(operations) => Json(OperationList { operations }).into_response(),
        Err(error) => history_unreadable(&error),
    }
}

async fn show_operation(
    extract::State(operations): extract::State<Operations>,
    id: Result<extract::Path<String>, PathRejection>,
) -> Response {
    let extract::Path(id) = match id {
        Ok(id) => id,
        Err(rejection) => return bad_path(&rejection),
    };
    match operations.get(&id).await {
        Ok(Some(operation)) => Json(operation).into_response(),
        Ok(None) => operation_not_found(&id),
        Err(error) => history_unreadable(&error),
    }
}

async fn cancel_operation(
    extract::State(operations): extract::State<Operations>,
    id: Result<extract::Path<String>, PathRejection>,
) -> Response {
    let extract::Path(id) = match id {
        Ok(id) => id,
        Err(rejection) => return bad_path(&rejection),
    };
    match operations.cancel(&id) {
        Ok(operation) => {
            (StatusCode::ACCEPTED, Json(OperationResponse { operation })).into_response()
        }
        Err(error @ CancelError::Finished { .. }) => api_error(
            StatusCode::CONFLICT,
            ErrorCode::Conflict,
            &error.to_string(),
        ),
        Err(CancelError::NotFound { .. }) => operation_not_found(&id),
    }
}

/// Follow one operation: its events while it is in memory, or, for one
/// only the store remembers, the operation and `done` at once.
async fn operation_events(
    extract::State(operations): extract::State<Operations>,
    id: Result<extract::Path<String>, PathRejection>,
) -> Response {
    let extract::Path(id) = match id {
        Ok(id) => id,
        Err(rejection) => return bad_path(&rejection),
    };
    let events: BoxStream<'static, OperationEvent> = match operations.follow(&id) {
        Some(follower) => stream::unfold(follower, |mut follower| async move {
            follower.next().await.map(|event| (event, follower))
        })
        .boxed(),
        None => match operations.get(&id).await {
            Ok(Some(operation)) => stream::iter([
                OperationEvent::Operation(operation.clone()),
                OperationEvent::Done(operation),
            ])
            .boxed(),
            Ok(None) => return operation_not_found(&id),
            Err(error) => return history_unreadable(&error),
        },
    };
    Sse::new(events.map(|event| Event::default().event(event.name()).json_data(event.data())))
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

async fn app_settings(extract::State(state): extract::State<State>) -> Json<AppSettings> {
    Json(state.app_settings())
}

/// Validate and store new app folder settings, put them in effect, and ask
/// the apps task to look again. The default home comes from the settings
/// in effect, which the apps task fills in at startup.
async fn set_app_settings(
    extract::State(state): extract::State<State>,
    extract::State(store): extract::State<SharedStore>,
    body: Bytes,
) -> Response {
    let update: AppSettingsUpdate = match serde_json::from_slice(&body) {
        Ok(update) => update,
        Err(err) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &format!("invalid app settings: {err}"),
            );
        }
    };
    let default_home = state.app_settings().default_home;
    let default_home = default_home.as_deref().map(Path::new);
    let stored = match settings::validate(&update, default_home) {
        Ok(stored) => stored,
        Err(err) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::Invalid,
                &err.to_string(),
            );
        }
    };
    if let Err(err) = store.set_setting(settings::SETTINGS_KEY, &stored).await {
        tracing::warn!(error = %err, "saving the app folder settings failed");
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "alchemyd couldn't save the settings",
        );
    }
    let (_, effective) = settings::effective(&stored, default_home);
    state.set_app_settings(effective.clone());
    state.rescan_apps();
    Json(effective).into_response()
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
    use axum::http::request::Builder;
    use axum::http::{HeaderName, Request};
    use axum::routing::post;
    use tower::ServiceExt as _;

    const LISTEN: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const HOST: &str = "127.0.0.1:7240";
    const TOKEN: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    /// Sign-in state whose token is [`TOKEN`].
    fn test_auth() -> Auth {
        Auth::with_token(TOKEN.to_owned(), HOST.parse().unwrap())
    }

    /// The TCP listener on [`LISTEN`], accepting [`TOKEN`].
    fn tcp() -> Tcp {
        Tcp {
            listen: LISTEN,
            auth: test_auth(),
        }
    }

    fn bearer() -> String {
        format!("Bearer {TOKEN}")
    }

    /// A request as a same-origin browser page sends it, signed in with
    /// the token.
    fn req(method: Method, uri: &str) -> Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, HOST)
            .header(header::AUTHORIZATION, bearer())
    }

    fn memory() -> SharedStore {
        std::sync::Arc::new(crate::store::MemoryStore::new())
    }

    fn no_logs() -> SharedLogSource {
        std::sync::Arc::new(crate::logs::FakeLogSource::new())
    }

    /// Operations on `state` that run nothing and always succeed.
    fn ops(state: &State) -> Operations {
        ops_with(state, crate::compose::FakeComposeRunner::new(), memory())
    }

    fn ops_with(
        state: &State,
        runner: crate::compose::FakeComposeRunner,
        store: SharedStore,
    ) -> Operations {
        Operations::new(Arc::new(runner), state.clone(), store)
    }

    /// The daemon's router with only the engine topic on `/api/events`, so
    /// these tests read engine frames alone as other topics are added.
    fn test_router(state: State) -> (Router, watch::Sender<bool>) {
        test_router_with_logs(state, no_logs())
    }

    fn test_router_with_logs(state: State, logs: SharedLogSource) -> (Router, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        let engine = Topic::watch(State::watch_engine, ApiEvent::Engine);
        let operations = ops(&state);
        (
            router_with_topics(state, memory(), tcp(), vec![engine], rx, logs, operations),
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

    /// `blog`, whose compose file names its project `my-blog`, and `draft`,
    /// which Compose has not read yet, with `blog`'s container and another
    /// project's sampled.
    fn renamed_app_state() -> State {
        let state = State::new();
        let app = |name: &str, project: Option<&str>| -> App {
            serde_json::from_value(serde_json::json!({
                "name": name,
                "status": "running",
                "dir": format!("/srv/apps/{name}"),
                "root": "/srv/apps",
                "compose_file": "compose.yaml",
                "project": project,
                "images": project.map(|_| "registry"),
                "services": [],
                "problems": [],
            }))
            .unwrap()
        };
        state.set_apps(AppList {
            apps: vec![app("blog", Some("my-blog")), app("draft", None)],
            ..AppList::default()
        });
        let containers: Vec<alchemy_api::ContainerUsage> = ["my-blog", "blog"]
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
        state
    }

    #[tokio::test]
    async fn metrics_for_an_app_are_its_resolved_projects() {
        let state = renamed_app_state();
        for uri in [
            "/api/metrics?app=blog",
            "/api/metrics?project=my-blog",
            "/api/metrics/history?app=blog",
            "/api/metrics/history?range=24h&app=blog",
        ] {
            let (status, body) = get_json_from(state.clone(), uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            if uri.contains("history") {
                assert_eq!(body["project"], "my-blog", "{uri}");
            } else {
                assert_eq!(body["machine"], serde_json::Value::Null, "{uri}");
                assert_eq!(body["projects"][0]["name"], "my-blog", "{uri}");
                assert_eq!(body["containers"][0]["name"], "my-blog-1", "{uri}");
                assert_eq!(body["containers"].as_array().unwrap().len(), 1, "{uri}");
            }
        }

        let (_, body) = get_json_from(state.clone(), "/api/metrics?project=blog").await;
        assert_eq!(
            body["containers"][0]["name"], "blog-1",
            "project= still means the Compose project"
        );
    }

    #[tokio::test]
    async fn metrics_for_an_unread_app_are_empty() {
        let state = renamed_app_state();
        let (status, body) = get_json_from(state.clone(), "/api/metrics?app=draft").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "at": 120, "interval": 60, "containers_in_vm": false,
                "machine": null, "projects": [], "containers": [],
            })
        );
        let (status, body) = get_json_from(state, "/api/metrics/history?app=draft").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["project"], serde_json::Value::Null);
        assert_eq!(body["points"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn metrics_refuse_an_unknown_app_or_both_filters() {
        let cases = [
            (
                "/api/metrics?app=nope",
                StatusCode::NOT_FOUND,
                "app_not_found",
            ),
            (
                "/api/metrics/history?app=nope",
                StatusCode::NOT_FOUND,
                "app_not_found",
            ),
            (
                "/api/metrics?app=blog&project=my-blog",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                "/api/metrics/history?app=blog&project=my-blog",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
        ];
        for (uri, want, code) in cases {
            let (status, body) = get_json_from(renamed_app_state(), uri).await;
            assert_eq!(status, want, "{uri}");
            assert_eq!(body["code"], code, "{uri}");
        }
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
            tcp(),
            vec![topic(a_rx), topic(b_rx)],
            stop_rx,
            no_logs(),
            ops(&State::new()),
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
            let resp = api_router(
                State::new(),
                memory(),
                rx,
                no_logs(),
                ops(&State::new()),
                test_auth(),
            )
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

        /// The request, signed in with the token.
        fn request(&self, uri: &str) -> Request<Body> {
            let mut builder = Request::builder()
                .method(self.method.clone())
                .uri(uri)
                .header(header::AUTHORIZATION, bearer());
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
            let app = tcp_checks(things(), tcp());
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
    async fn cross_site_requests_are_refused_on_the_tcp_listener_only() {
        let cases = [
            ("/api/health", None, None, true),
            ("/api/health", Some("same-origin"), None, true),
            ("/api/health", Some("none"), None, true),
            ("/api/health", Some("cross-site"), None, false),
            ("/api/health", Some("same-site"), None, false),
            (
                "/api/apps/blog/logs?follow=true",
                Some("cross-site"),
                None,
                false,
            ),
            (
                "/api/health",
                Some("cross-site"),
                Some("https://evil.example"),
                false,
            ),
            (
                "/api/health",
                Some("cross-site"),
                Some("tauri://localhost"),
                true,
            ),
            (
                "/api/health",
                Some("cross-site"),
                Some("http://tauri.localhost"),
                true,
            ),
            ("/", Some("cross-site"), None, true),
        ];
        for (uri, site, origin, passes) in cases {
            let mut request = req(Method::GET, uri);
            if let Some(site) = site {
                request = request.header("sec-fetch-site", site);
            }
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let (app, _stop) = test_router(State::new());
            let (status, _, body) = send(app, request.body(Body::empty()).unwrap()).await;
            let what = format!("{uri} {site:?} {origin:?}");
            if passes {
                assert_ne!(status, StatusCode::FORBIDDEN, "{what}");
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN, "{what}");
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                    serde_json::json!({
                        "error": Refusal::CrossSite.message(),
                        "code": "forbidden",
                    }),
                    "{what}"
                );
            }

            let (_stop, rx) = watch::channel(false);
            let socket = api_router(
                State::new(),
                memory(),
                rx,
                no_logs(),
                ops(&State::new()),
                test_auth(),
            );
            let mut request = req(Method::GET, uri);
            if let Some(site) = site {
                request = request.header("sec-fetch-site", site);
            }
            let (status, _, _) = send(socket, request.body(Body::empty()).unwrap()).await;
            assert_ne!(status, StatusCode::FORBIDDEN, "socket {what}");
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
        let (status, acao, _) = send(tcp_checks(things(), tcp()), preflight).await;
        assert!(status.is_success(), "{status}");
        assert_eq!(acao.as_deref(), Some("tauri://localhost"));

        let probe = Probe::post(Some(HOST), Some("tauri://localhost"), Some("text/plain"));
        let app = tcp_checks(things(), tcp());
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
            let app = tcp_checks(
                things(),
                Tcp {
                    listen: lan,
                    auth: test_auth(),
                },
            );
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
        let app = api_router(
            State::new(),
            memory(),
            rx,
            no_logs(),
            ops(&State::new()),
            test_auth(),
        );
        let (status, _, _) = send(app, probe.request("/api/health")).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The socket router sharing `auth` with a TCP router.
    fn socket_with(auth: &Auth) -> Router {
        let (_stop, rx) = watch::channel(false);
        api_router(
            State::new(),
            memory(),
            rx,
            no_logs(),
            ops(&State::new()),
            auth.clone(),
        )
    }

    /// The TCP router accepting `auth`.
    fn tcp_with(auth: &Auth) -> Router {
        let (_stop, rx) = watch::channel(false);
        router(
            State::new(),
            memory(),
            Tcp {
                listen: LISTEN,
                auth: auth.clone(),
            },
            rx,
            no_logs(),
            ops(&State::new()),
        )
    }

    /// A same-origin request with only `credential` set, if any.
    fn unsigned(
        method: Method,
        uri: &str,
        credential: Option<(HeaderName, String)>,
    ) -> Request<Body> {
        let mutating = guard::is_mutating(&method);
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, HOST);
        if mutating {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        if let Some((name, value)) = credential {
            builder = builder.header(name, value);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// Launch a code on `auth`'s socket, redeem it on its TCP listener, and
    /// return the `name=value` pair the browser would send back.
    async fn sign_in(auth: &Auth) -> String {
        let (status, _, body) = send(
            socket_with(auth),
            unsigned(Method::POST, "/api/session/launch", None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let launch: alchemy_api::session::LaunchResponse = serde_json::from_slice(&body).unwrap();
        let resp = tcp_with(auth)
            .oneshot(unsigned(
                Method::GET,
                &format!("/login?code={}", launch.code),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let cookie = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        cookie.split(';').next().unwrap().to_owned()
    }

    #[tokio::test]
    async fn tcp_requires_signing_in_except_for_health_and_version() {
        let auth = test_auth();
        let cookie = sign_in(&auth).await;
        let wrong = format!("Bearer {}", "2".repeat(64));
        let stale = format!("{}={}", auth.cookie_name(), "3".repeat(64));
        let token = Some((header::AUTHORIZATION, bearer()));
        let session = Some((header::COOKIE, cookie));
        let cases = [
            (Method::GET, "/api/health", None, StatusCode::OK),
            (Method::HEAD, "/api/health", None, StatusCode::OK),
            (Method::GET, "/api/version", None, StatusCode::OK),
            (Method::GET, "/api/engine", None, StatusCode::UNAUTHORIZED),
            (Method::GET, "/api/apps", None, StatusCode::UNAUTHORIZED),
            (Method::POST, "/api/apps", None, StatusCode::UNAUTHORIZED),
            (
                Method::GET,
                "/api/apps/blog/files/env",
                None,
                StatusCode::UNAUTHORIZED,
            ),
            (
                Method::PUT,
                "/api/settings/apps",
                None,
                StatusCode::UNAUTHORIZED,
            ),
            (Method::GET, "/api/events", None, StatusCode::UNAUTHORIZED),
            (Method::GET, "/api/session", None, StatusCode::UNAUTHORIZED),
            (Method::POST, "/api/health", None, StatusCode::UNAUTHORIZED),
            (Method::GET, "/api/nope", None, StatusCode::UNAUTHORIZED),
            (Method::GET, "/api/engine", token.clone(), StatusCode::OK),
            (
                Method::GET,
                "/api/nope",
                token.clone(),
                StatusCode::NOT_FOUND,
            ),
            (Method::GET, "/api/engine", session.clone(), StatusCode::OK),
            (
                Method::GET,
                "/api/engine",
                Some((header::AUTHORIZATION, wrong)),
                StatusCode::UNAUTHORIZED,
            ),
            (
                Method::GET,
                "/api/engine",
                Some((header::COOKIE, stale)),
                StatusCode::UNAUTHORIZED,
            ),
            (
                Method::POST,
                "/api/health",
                token,
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                Method::POST,
                "/api/health",
                session,
                StatusCode::METHOD_NOT_ALLOWED,
            ),
        ];
        for (method, uri, credential, want) in cases {
            let what = format!("{method} {uri} {credential:?}");
            let resp = tcp_with(&auth)
                .oneshot(unsigned(method, uri, credential))
                .await
                .unwrap();
            assert_eq!(resp.status(), want, "{what}");
            if want == StatusCode::UNAUTHORIZED {
                assert_eq!(
                    resp.headers()[header::WWW_AUTHENTICATE],
                    "Bearer realm=\"alchemyd\"",
                    "{what}"
                );
                let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                assert_eq!(error_code(&body), "unauthorized", "{what}");
            }
        }
    }

    #[tokio::test]
    async fn the_web_ui_needs_no_sign_in() {
        let auth = test_auth();
        for uri in ["/", "/apps/blog", "/settings"] {
            let resp = tcp_with(&auth)
                .oneshot(unsigned(Method::GET, uri, None))
                .await
                .unwrap();
            assert_ne!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    #[tokio::test]
    async fn guard_refusals_come_before_signing_in() {
        let auth = test_auth();
        let cases = [
            (
                "rebinding host",
                Probe::post(Some("evil.example"), None, JSON),
                StatusCode::FORBIDDEN,
            ),
            (
                "cross-origin",
                Probe::post(Some(HOST), Some("https://evil.example"), JSON),
                StatusCode::FORBIDDEN,
            ),
            (
                "not json",
                Probe::post(Some(HOST), None, Some("text/plain")),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
        ];
        for (name, probe, want) in cases {
            let mut request = probe.request("/api/apps");
            request.headers_mut().remove(header::AUTHORIZATION);
            let (status, _, _) = send(tcp_with(&auth), request).await;
            assert_eq!(status, want, "{name}");
        }
    }

    #[tokio::test]
    async fn the_tauri_shell_reads_a_refusal_and_may_send_its_token() {
        let auth = test_auth();
        let preflight = req(Method::OPTIONS, "/api/apps")
            .header(header::ORIGIN, "tauri://localhost")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
            .body(Body::empty())
            .unwrap();
        let resp = tcp_with(&auth).oneshot(preflight).await.unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        let allowed = resp.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(allowed.contains("authorization"), "{allowed}");

        let mut request = unsigned(Method::GET, "/api/apps", None);
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("tauri://localhost"),
        );
        let (status, acao, body) = send(tcp_with(&auth), request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(acao.as_deref(), Some("tauri://localhost"));
        assert_eq!(error_code(&body), "unauthorized");
    }

    #[tokio::test]
    async fn launch_codes_come_from_the_socket_only() {
        let auth = test_auth();
        let (status, _, body) = send(
            socket_with(&auth),
            unsigned(Method::POST, "/api/session/launch", None),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let launch: alchemy_api::session::LaunchResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(launch.expires_in, 60);
        assert_eq!(
            launch.url,
            format!("http://127.0.0.1:7240/login?code={}", launch.code)
        );

        let refused = [
            (None, StatusCode::UNAUTHORIZED, "unauthorized"),
            (
                Some((header::AUTHORIZATION, bearer())),
                StatusCode::FORBIDDEN,
                "forbidden",
            ),
        ];
        for (credential, want, code) in refused {
            let what = format!("{credential:?}");
            let (status, _, body) = send(
                tcp_with(&auth),
                unsigned(Method::POST, "/api/session/launch", credential),
            )
            .await;
            assert_eq!(status, want, "{what}");
            assert_eq!(error_code(&body), code, "{what}");
        }
    }

    #[tokio::test]
    async fn login_redeems_a_code_once_and_sets_a_private_cookie() {
        let auth = test_auth();
        let (_, _, body) = send(
            socket_with(&auth),
            unsigned(Method::POST, "/api/session/launch", None),
        )
        .await;
        let launch: alchemy_api::session::LaunchResponse = serde_json::from_slice(&body).unwrap();
        let login = format!("/login?code={}", launch.code);

        let resp = tcp_with(&auth)
            .oneshot(unsigned(Method::GET, &login, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let headers = resp.headers();
        assert_eq!(headers[header::LOCATION], "/");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        let cookie = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(
            cookie.ends_with("; HttpOnly; SameSite=Strict; Path=/api"),
            "{cookie}"
        );
        assert!(cookie.starts_with("alchemy_session_7240="), "{cookie}");
        assert!(!cookie.contains(TOKEN), "{cookie}");
        let pair = cookie.split(';').next().unwrap().to_owned();

        let (status, _, body) = send(
            tcp_with(&auth),
            unsigned(Method::GET, "/api/session", Some((header::COOKIE, pair))),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({ "auth": "cookie" })
        );

        for uri in [
            login.as_str(),
            "/login",
            "/login?code=",
            "/login?code=nope",
            "/login?code=1111111111111111111111111111111111111111111111111111111111111111",
        ] {
            let resp = tcp_with(&auth)
                .oneshot(unsigned(Method::GET, uri, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
            let headers = resp.headers();
            assert!(headers.get(header::SET_COOKIE).is_none(), "{uri}");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store", "{uri}");
            assert_eq!(headers[header::REFERRER_POLICY], "no-referrer", "{uri}");
            assert_eq!(
                headers[header::CONTENT_TYPE],
                "text/html; charset=utf-8",
                "{uri}"
            );
        }

        let resp = socket_with(&auth)
            .oneshot(unsigned(Method::GET, "/login", None))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "the socket has no /login"
        );
    }

    #[tokio::test]
    async fn session_says_how_a_request_was_let_in() {
        let auth = test_auth();
        let cases = [
            (socket_with(&auth), None, "socket"),
            (
                tcp_with(&auth),
                Some((header::AUTHORIZATION, bearer())),
                "token",
            ),
        ];
        for (app, credential, want) in cases {
            let (status, _, body) =
                send(app, unsigned(Method::GET, "/api/session", credential)).await;
            assert_eq!(status, StatusCode::OK, "{want}");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({ "auth": want })
            );
        }
    }

    fn app_list() -> AppList {
        let app: App = serde_json::from_value(serde_json::json!({
            "name": "blog",
            "status": "running",
            "dir": "/srv/apps/blog",
            "root": "/srv/apps",
            "compose_file": "compose.yaml",
            "project": "blog",
            "images": "registry",
            "services": [],
            "problems": [],
        }))
        .unwrap();
        AppList {
            apps: vec![app],
            ..AppList::default()
        }
    }

    #[tokio::test]
    async fn apps_lists_and_shows_the_current_apps() {
        let state = State::new();
        state.set_apps(app_list());

        let (status, body) = get_json_from(state.clone(), "/api/apps").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::to_value(app_list()).unwrap());

        let (status, body) = get_json_from(state.clone(), "/api/apps/blog").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::to_value(&app_list().apps[0]).unwrap());

        for (uri, name) in [
            ("/api/apps/nope", "nope"),
            ("/api/apps/Blog", "Blog"),
            ("/api/apps/..%2Fetc", "../etc"),
        ] {
            let (status, body) = get_json_from(state.clone(), uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(
                body,
                serde_json::json!({
                    "error": format!("no app is named {name:?}"),
                    "code": "app_not_found",
                }),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn logs_refuse_unknown_apps_services_and_bad_queries() {
        let state = State::new();
        let mut apps = app_list();
        apps.apps[0].services.push(alchemy_api::apps::AppService {
            name: "web".into(),
            containers: vec![],
        });
        state.set_apps(apps);
        let cases = [
            (
                "/api/apps/nope/logs",
                StatusCode::NOT_FOUND,
                "app_not_found",
            ),
            (
                "/api/apps/blog/logs?service=db",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                "/api/apps/blog/logs?tail=all",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                "/api/apps/blog/logs?follow=maybe",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
        ];
        for (uri, want, code) in cases {
            let (status, body) = get_json_from(state.clone(), uri).await;
            assert_eq!(status, want, "{uri}: {body}");
            assert_eq!(body["code"], code, "{uri}");
        }
    }

    #[tokio::test]
    async fn logs_are_refused_as_busy_while_every_stream_is_taken() {
        let state = State::new();
        state.set_apps(app_list());
        let taken: Vec<_> = (0..crate::logs::MAX_STREAMS)
            .map(|_| state.log_streams().try_open().unwrap())
            .collect();
        let (status, body) = get_json_from(state.clone(), "/api/apps/blog/logs").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["code"], "busy");
        assert_eq!(
            body["error"],
            "alchemyd is already serving 16 log streams; close one and try again"
        );

        drop(taken);
        let (app, _stop) = test_router(state);
        let resp = app
            .oneshot(
                req(Method::GET, "/api/apps/blog/logs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn logs_stream_lines_then_done() {
        let state = State::new();
        let mut engine = EngineStatus::new(EngineState::Ready);
        engine.endpoint = Some("unix:///fake.sock".into());
        state.set_engine(engine);
        let mut apps = app_list();
        apps.apps[0].services.push(
            serde_json::from_value(serde_json::json!({
                "name": "web",
                "containers": [{
                    "id": "w1", "name": "blog-web-1", "image": null,
                    "state": "running", "health": null,
                }],
            }))
            .unwrap(),
        );
        state.set_apps(apps);
        let source = crate::logs::FakeLogSource::new();
        source.write(
            "w1",
            alchemy_api::logs::LogStream::Stdout,
            "2024-05-06T07:08:09.000000001Z",
            "hello",
        );
        let (app, _stop) = test_router_with_logs(state, std::sync::Arc::new(source));
        let resp = app
            .oneshot(
                req(Method::GET, "/api/apps/blog/logs?service=web&tail=5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut frames = Frames::new(resp);
        assert_eq!(
            frames.next().await.unwrap(),
            r#"event: log
data: {"service":"web","container":"blog-web-1","stream":"stdout","time":"2024-05-06T07:08:09.000000001Z","line":"hello"}"#
        );
        assert_eq!(
            frames.next().await.unwrap(),
            "event: done\ndata: {\"error\":null}"
        );
        assert_eq!(frames.next().await, None);
    }

    async fn put_settings(
        state: &State,
        store: &SharedStore,
        content_type: Option<&str>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let (_tx, rx) = watch::channel(false);
        let mut request = req(Method::PUT, "/api/settings/apps");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let resp = router(
            state.clone(),
            store.clone(),
            tcp(),
            rx,
            no_logs(),
            ops(state),
        )
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn with_default_home(home: &str) -> State {
        let state = State::new();
        state.set_app_settings(AppSettings {
            home: Some(home.into()),
            default_home: Some(home.into()),
            extra: vec![],
        });
        state
    }

    #[tokio::test]
    async fn app_settings_are_read_from_what_is_in_effect() {
        let state = with_default_home("/home/me/Alchemy/apps");
        let (status, body) = get_json_from(state, "/api/settings/apps").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "home": "/home/me/Alchemy/apps",
                "default_home": "/home/me/Alchemy/apps",
                "extra": [],
            })
        );
    }

    #[tokio::test]
    async fn putting_app_settings_stores_them_and_asks_for_a_rescan() {
        let state = with_default_home("/home/me/Alchemy/apps");
        let store = memory();
        let rescans = state.watch_app_rescans();

        let (status, body) = put_settings(
            &state,
            &store,
            Some("application/json"),
            r#"{"home":"/srv/apps/","extra":["/opt/stacks"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let want = serde_json::json!({
            "home": "/srv/apps",
            "default_home": "/home/me/Alchemy/apps",
            "extra": ["/opt/stacks"],
        });
        assert_eq!(body, want);
        assert_eq!(serde_json::to_value(state.app_settings()).unwrap(), want);
        assert!(rescans.has_changed().unwrap());
        assert_eq!(
            store
                .setting::<settings::StoredDirs>(settings::SETTINGS_KEY)
                .await
                .unwrap(),
            Some(settings::StoredDirs {
                home: Some("/srv/apps".into()),
                extra: vec!["/opt/stacks".into()],
            })
        );

        let (status, body) =
            put_settings(&state, &store, Some("application/json"), r#"{"home":null}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["home"], "/home/me/Alchemy/apps");
        assert_eq!(body["extra"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn bad_app_settings_are_refused_and_change_nothing() {
        let json = Some("application/json");
        let cases = [
            (
                json,
                r#"{"home":"apps"}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
            ),
            (
                json,
                r#"{"extra":["/home/me/Alchemy/apps"]}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
            ),
            (
                json,
                r#"{"home":3}"#,
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (json, "not json", StatusCode::BAD_REQUEST, "bad_request"),
            (
                None,
                r#"{"home":"/srv"}"#,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
            ),
        ];
        for (content_type, body, want, code) in cases {
            let state = with_default_home("/home/me/Alchemy/apps");
            let store = memory();
            let rescans = state.watch_app_rescans();
            let (status, got) = put_settings(&state, &store, content_type, body).await;
            assert_eq!(status, want, "{body}");
            assert_eq!(got["code"], code, "{body}");
            assert!(!rescans.has_changed().unwrap(), "{body}");
            assert_eq!(
                store
                    .setting::<settings::StoredDirs>(settings::SETTINGS_KEY)
                    .await
                    .unwrap(),
                None,
                "{body}"
            );
        }
    }

    fn ready_with_blog() -> State {
        let state = State::new();
        state.set_engine(EngineStatus::new(EngineState::Ready));
        state.set_apps(app_list());
        state
    }

    async fn send_json(
        app: Router,
        method: Method,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = req(method.clone(), uri);
        if method != Method::GET {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let resp = app
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn ops_router(state: &State, operations: &Operations) -> Router {
        let (_tx, rx) = watch::channel(false);
        router(
            state.clone(),
            memory(),
            tcp(),
            rx,
            no_logs(),
            operations.clone(),
        )
    }

    #[tokio::test]
    async fn actions_start_operations_and_refuse_a_busy_app() {
        use crate::compose::{ComposeAction, FakeComposeRunner, FakeOutcome, FakeScript};
        let state = ready_with_blog();
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Up,
            FakeScript::new().outcome(FakeOutcome::UntilCancelled),
        );
        let operations = ops_with(&state, runner, memory());
        let app = || ops_router(&state, &operations);

        let (status, body) = send_json(
            app(),
            Method::POST,
            "/api/apps/blog/actions",
            r#"{"action":"up"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["operation"]["id"].as_str().unwrap().to_owned();
        assert_eq!(body["operation"]["state"], "running");
        assert_eq!(body["operation"]["app"], "blog");

        let cases = [
            (
                r#"{"action":"stop"}"#,
                "/api/apps/blog/actions",
                StatusCode::CONFLICT,
                "conflict",
            ),
            (
                r#"{"action":"up"}"#,
                "/api/apps/nope/actions",
                StatusCode::NOT_FOUND,
                "app_not_found",
            ),
            (
                r#"{"action":"kill"}"#,
                "/api/apps/blog/actions",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                "",
                "/api/apps/blog/actions",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
        ];
        for (body, uri, want, code) in cases {
            let (status, got) = send_json(app(), Method::POST, uri, body).await;
            assert_eq!(status, want, "{uri} {body}");
            assert_eq!(got["code"], code, "{uri} {body}");
        }

        let (status, body) =
            send_json(app(), Method::GET, &format!("/api/operations/{id}"), "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], id.as_str());
        let (status, body) = send_json(app(), Method::GET, "/api/operations?app=blog", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["operations"][0]["id"], id.as_str());
        let (_, body) = send_json(app(), Method::GET, "/api/operations?app=other", "").await;
        assert_eq!(body, serde_json::json!({ "operations": [] }));
        let (status, body) = send_json(app(), Method::GET, "/api/operations/nope", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "operation_not_found");

        let cancel = format!("/api/operations/{id}/cancel");
        let (status, _) = send_json(app(), Method::POST, &cancel, "").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let resp = app()
            .oneshot(
                req(Method::GET, &format!("/api/operations/{id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut frames = Frames::new(resp);
        let mut last = String::new();
        while let Some(frame) = frames.next().await {
            last = frame;
        }
        let data = last.strip_prefix("event: done\ndata: ").unwrap();
        let done: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(done["state"], "cancelled");

        let (status, body) = send_json(app(), Method::POST, &cancel, "").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "conflict");
        let (status, _) = send_json(app(), Method::POST, "/api/operations/nope/cancel", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn events_stream_output_then_done() {
        use crate::compose::{ComposeAction, FakeComposeRunner, FakeScript, Stream};
        let state = ready_with_blog();
        let runner = FakeComposeRunner::new().with_script(
            ComposeAction::Stop,
            FakeScript::new().line(Stream::Stderr, "stopping"),
        );
        let operations = ops_with(&state, runner, memory());
        let started = operations.start("blog", Action::Stop, false).await.unwrap();
        let resp = ops_router(&state, &operations)
            .oneshot(
                req(
                    Method::GET,
                    &format!("/api/operations/{}/events", started.id),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let mut frames = Frames::new(resp);
        let mut names = Vec::new();
        while let Some(frame) = frames.next().await {
            let name = frame
                .lines()
                .next()
                .unwrap()
                .trim_start_matches("event: ")
                .to_owned();
            if name == "output" {
                assert!(frame.contains(r#""text":"stopping""#), "{frame}");
            }
            names.push(name);
        }
        names.dedup();
        assert_eq!(names.first().map(String::as_str), Some("operation"));
        assert!(names.contains(&"output".to_owned()), "{names:?}");
        assert_eq!(names.last().map(String::as_str), Some("done"));

        let (status, body) = send_json(
            ops_router(&state, &operations),
            Method::GET,
            "/api/operations/nope/events",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "operation_not_found");
    }

    #[tokio::test]
    async fn plans_show_the_command_lines() {
        let state = ready_with_blog();
        let operations = ops(&state);
        let app = || ops_router(&state, &operations);
        let (status, body) =
            send_json(app(), Method::GET, "/api/apps/blog/plan?action=down", "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["action"], "down");
        assert_eq!(
            body["steps"][0]["argv"],
            serde_json::json!([
                "docker",
                "compose",
                "--project-directory",
                "/srv/apps/blog",
                "--progress",
                "json",
                "down"
            ])
        );
        let cases = [
            (
                "/api/apps/blog/plan",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                "/api/apps/blog/plan?action=kill",
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                "/api/apps/nope/plan?action=up",
                StatusCode::NOT_FOUND,
                "app_not_found",
            ),
        ];
        for (uri, want, code) in cases {
            let (status, body) = send_json(app(), Method::GET, uri, "").await;
            assert_eq!(status, want, "{uri}");
            assert_eq!(body["code"], code, "{uri}");
        }
        assert_eq!(operations.recent(None, 10).await.unwrap(), []);
    }

    #[tokio::test]
    async fn actions_wait_for_docker() {
        let state = State::new();
        state.set_apps(app_list());
        let operations = ops(&state);
        let (status, body) = send_json(
            ops_router(&state, &operations),
            Method::POST,
            "/api/apps/blog/actions",
            r#"{"action":"start"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("Docker isn't running"),
            "{body}"
        );
    }
}
