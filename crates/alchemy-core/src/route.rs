//! Web addresses: which app answers which address, and why an app can't
//! be routed.
//!
//! Everything here is a pure decision over plain values. The daemon reads
//! Compose configs, containers, the engine, and port reservations, then
//! passes them in; the proxy looks requests up in the [`RouteTable`].

mod hostname;
mod reach;
mod spec;
mod table;

pub use hostname::{
    HostLabel, HostLabelError, HostSuffix, MAX_LABEL_LEN, match_host, match_host_suffix,
};
pub use reach::{EngineFacts, HostOs, Reachability, reachability};
pub use spec::{ExposedPort, ExposedPortError, X_ALCHEMY, XAlchemy, XAlchemyBlock};
pub use table::{
    AppRouteInput, CandidatePort, Route, RouteContainer, RouteOptions, RoutePlan, RouteProblem,
    RouteProblemKind, RouteTable, RouteTarget, ServiceRouteInput, Upstream, choose_target,
    plan_routes, resolve_upstream,
};
