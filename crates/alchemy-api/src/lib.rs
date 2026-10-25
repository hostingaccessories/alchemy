//! HTTP API types shared by `alchemyd` and its web and desktop clients.
//!
//! Every route lives under [`API_PREFIX`]. These shapes are a stable
//! contract and only grow additively; the JSON is pinned in
//! `tests/wire_stability.rs`. Bump [`API_VERSION`] on any breaking change.
//!
//! `GET /api/engine` returns an [`EngineStatus`], re-exported from
//! `alchemy-core` so the wire type and the domain type cannot drift.
//! [`ContainerStatus`], its [`StopReason`], and the [`PublishedPort`]s it
//! holds are re-exported the same way for routes that show containers.
//! `GET /api/metrics` returns a [`MetricsResponse`] and
//! `GET /api/metrics/history` a [`MetricsHistory`], built from the
//! resource-usage types in `alchemy_core::metrics`.
//! `GET /api/events` streams [`ApiEvent`]s. The app routes and their
//! types are in [`apps`], an app's log stream in [`logs`], the
//! lifecycle operation routes in [`operations`], signing in to the TCP
//! listener in [`session`], the port registry in [`ports`],
//! `GET /api/network` in [`network`], the machine's privileges in
//! [`privileges`], bringing apps back after a restart, wake, or Docker
//! returning in [`reconcile`], starting Docker Desktop in [`engine`], the
//! `woke` event in [`wake`], and autostart in [`autostart`].
//!
//! The default build is pure. Enable the `socket` feature for the daemon
//! socket path shared by `alchemyd` and its local clients, and the
//! `state-dir` feature for the state directory and the TCP token file in
//! it, shared by `alchemyd` and the desktop shell.

pub use alchemy_core::engine::{
    ContainerStatus, EngineFlavor, EngineState, EngineStatus, StopReason,
};
pub use alchemy_core::metrics::{
    ContainerUsage, MachineUsage, MetricsPoint, MetricsRange, ProjectUsage,
};
pub use alchemy_core::ports::{PortRange, Protocol, PublishedPort};
use serde::{Deserialize, Serialize, Serializer};

pub mod apps;
pub mod autostart;
pub mod engine;
pub mod logs;
pub mod network;
pub mod operations;
pub mod ports;
pub mod privileges;
pub mod reconcile;
pub mod session;
#[cfg(feature = "socket")]
mod socket;
#[cfg(feature = "state-dir")]
mod state_dir;
pub mod wake;

pub use apps::AppList;
pub use operations::OperationList;
pub use privileges::PrivilegeList;
pub use reconcile::ReconcileStatus;
pub use wake::Wake;

#[cfg(feature = "socket")]
pub use socket::{SOCKET_ENV, SOCKET_FILE, default_socket_path, runtime_dir, socket_candidates};
#[cfg(feature = "state-dir")]
pub use state_dir::{STATE_DIR_ENV, TCP_TOKEN_FILE, default_state_dir, tcp_token_path};

/// Path prefix for every API route.
pub const API_PREFIX: &str = "/api";

/// The current API version, sent as [`VersionResponse::api`].
///
/// Bumped only on a breaking change to a route or a wire shape. Additive
/// changes keep it as is.
pub const API_VERSION: u32 = 1;

/// `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Always `"ok"` when the daemon can answer.
    pub status: HealthStatus,
}

/// Coarse daemon health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HealthStatus {
    /// The daemon is serving requests.
    Ok,
}

/// `GET /api/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResponse {
    /// The daemon's release version.
    pub version: String,
    /// The daemon's [`API_VERSION`].
    pub api: u32,
}

/// Body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable detail, safe to show to the user.
    pub error: String,
    /// Machine-readable failure category. Decodes as
    /// [`ErrorCode::Unknown`] when absent.
    #[serde(default)]
    pub code: ErrorCode,
}

impl ErrorResponse {
    /// An error response with `code` and a human-readable `error`.
    pub fn new(code: ErrorCode, error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code,
        }
    }
}

/// Machine-readable failure category carried by [`ErrorResponse::code`].
///
/// Clients branch on this rather than on the HTTP status or the message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// No route matches the request path.
    NotFound,
    /// The route exists but does not accept the request method.
    MethodNotAllowed,
    /// The request was malformed.
    BadRequest,
    /// The daemon refused the request as unsafe, such as one from another
    /// web origin or for a host name it does not answer to.
    Forbidden,
    /// The request needs signing in and carried no valid credential: on
    /// the TCP listener, neither the daemon's token as a bearer token nor
    /// a session cookie. See [`session`].
    Unauthorized,
    /// The request body is not in a media type the route accepts.
    UnsupportedMediaType,
    /// The request conflicts with current state, such as an operation
    /// already running on the same app.
    Conflict,
    /// The request was well formed but its content failed validation, such
    /// as a compose file Compose rejects.
    Invalid,
    /// No app has the requested name.
    AppNotFound,
    /// No operation has the requested id, or it is too old to be kept.
    OperationNotFound,
    /// The daemon is at a limit, such as on open log streams, and refused
    /// the request rather than queue it. Try again later.
    Busy,
    /// The daemon failed while handling a valid request.
    Internal,
    /// Alchemy can't do what was asked on this machine, such as a
    /// privilege whose helper is missing; the error says why.
    Unsupported,
    /// A code this build does not know, sent by a newer daemon, or no code
    /// at all.
    #[default]
    #[serde(other)]
    Unknown,
}

/// `GET /api/metrics`, and the `metrics` event: the latest usage of the
/// machine, each Compose project, and each running container.
///
/// With `?app=NAME`, only that app's figures: the totals and containers of
/// the Compose project its folder resolves to, and no machine figures. An
/// unknown app is a 404 [`ErrorCode::AppNotFound`], and an app Compose has
/// not read yet has no figures. `?project=NAME` does the same for a
/// Compose project by name. Giving both is a 400. A project with no running
/// containers has no figures.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricsResponse {
    /// When the figures were sampled, in Unix seconds, or `None` before
    /// the first sample.
    pub at: Option<i64>,
    /// Seconds between samples.
    pub interval: u32,
    /// Whether containers run in a virtual machine, as with Docker Desktop
    /// or on macOS. The machine figures are then the host Alchemy runs
    /// on, not that VM, and container CPU is a share of the VM.
    pub containers_in_vm: bool,
    /// The machine, or `None` before the first sample or if it could not
    /// be read.
    pub machine: Option<MachineUsage>,
    /// Each Compose project's containers added up, by name.
    pub projects: Vec<ProjectUsage>,
    /// Each running container, by name.
    pub containers: Vec<ContainerUsage>,
}

/// `GET /api/metrics/history?range=24h[&app=NAME|&project=NAME]`: stored
/// usage over a range, oldest first.
///
/// Points are [`step`](Self::step) seconds apart where data exists; a
/// longer gap means nothing was recorded, and clients draw it as a gap.
/// With `app` or `project`, points hold only that project's totals and
/// containers, and no machine figures, as on [`MetricsResponse`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsHistory {
    /// The range asked for; `1h` when none was.
    pub range: MetricsRange,
    /// Seconds per point.
    pub step: u32,
    /// The project asked for, or the one the app asked for resolves to.
    /// `None` for the whole machine or an app Compose has not read yet.
    pub project: Option<String>,
    /// Every stored point in the range, oldest first.
    pub points: Vec<MetricsPoint>,
}

/// One message on the `GET /api/events` server-sent event stream.
///
/// Each variant is sent as an SSE frame whose `event:` field is
/// [`ApiEvent::name`] and whose `data:` field is the variant's payload as
/// JSON. Serialized with serde, the same event is
/// `{"event": <name>, "data": <payload>}`.
///
/// Not `#[non_exhaustive]`, so the daemon's SSE mapping must handle every
/// variant. Adding one is still additive on the wire: clients ignore event
/// names they do not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ApiEvent {
    /// The engine status, sent once on connect and again on every change.
    Engine(EngineStatus),
    /// The latest usage, as `GET /api/metrics` returns it, sent once on
    /// connect and again after every sample.
    Metrics(MetricsResponse),
    /// Every app, sent once on connect and again on every change.
    Apps(AppList),
    /// Every operation the daemon holds in memory, running or recently
    /// finished, newest first, sent once on connect and again on every
    /// change.
    Operations(OperationList),
    /// Every privilege, as `GET /api/machine/privileges` returns them,
    /// sent once on connect and again on every change.
    Privileges(PrivilegeList),
    /// The last reconcile, as `GET /api/reconcile` returns it, sent once
    /// on connect and again on every change, including as each app's
    /// operation ends.
    Reconcile(ReconcileStatus),
    /// The computer woke from sleep. Sent once per wake, and not on
    /// connect: see [`wake`].
    Woke(Wake),
    /// Autostart, as `GET /api/machine/autostart` returns it, sent once on
    /// connect and again on every change.
    Autostart(autostart::Autostart),
}

impl ApiEvent {
    /// The SSE `event:` name for this variant.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Engine(_) => "engine",
            Self::Metrics(_) => "metrics",
            Self::Apps(_) => "apps",
            Self::Operations(_) => "operations",
            Self::Privileges(_) => "privileges",
            Self::Reconcile(_) => "reconcile",
            Self::Woke(_) => "woke",
            Self::Autostart(_) => "autostart",
        }
    }

    /// The payload alone, which serializes as the SSE `data:` field.
    pub fn data(&self) -> EventData<'_> {
        EventData(self)
    }
}

/// An [`ApiEvent`]'s payload without its name, from [`ApiEvent::data`].
#[derive(Debug, Clone, Copy)]
pub struct EventData<'a>(&'a ApiEvent);

impl Serialize for EventData<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            ApiEvent::Engine(status) => status.serialize(serializer),
            ApiEvent::Metrics(metrics) => metrics.serialize(serializer),
            ApiEvent::Apps(apps) => apps.serialize(serializer),
            ApiEvent::Operations(operations) => operations.serialize(serializer),
            ApiEvent::Privileges(privileges) => privileges.serialize(serializer),
            ApiEvent::Reconcile(reconcile) => reconcile.serialize(serializer),
            ApiEvent::Woke(wake) => wake.serialize(serializer),
            ApiEvent::Autostart(autostart) => autostart.serialize(serializer),
        }
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
    use super::*;

    #[test]
    fn api_event_data_is_the_payload() {
        let status = EngineStatus::new(EngineState::Ready);
        let event = ApiEvent::Engine(status.clone());
        assert_eq!(
            serde_json::to_string(&event.data()).unwrap(),
            serde_json::to_string(&status).unwrap()
        );
    }
}
