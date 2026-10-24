//! The host's networks next to Docker's, and what overlaps.
//!
//! `GET /api/network` returns a [`NetworkReport`]: the LAN subnets and
//! routes the daemon sees, Docker's bridge, address pools, and networks,
//! Alchemy's own [`AlchemyNetwork`], and a [`NetworkWarning`] for each
//! overlap, with the fix spelled out. The daemon refreshes it in the
//! background, so it may be up to half a minute old.
//!
//! Every human-readable `message` and `fix` here is written for the people
//! using Alchemy and safe to show as is.

pub use alchemy_core::network::{AddressPool, HostSide, Ipv4Net};
use serde::{Deserialize, Serialize};

/// `GET /api/network`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkReport {
    /// Whether Docker's side was read. While it is `false`, `bridge`,
    /// `pools`, and `networks` are empty and only the host is checked.
    pub engine_ready: bool,
    /// The IPv4 subnets of this machine's physical interfaces, sorted.
    pub lan: Vec<Ipv4Net>,
    /// The machine's other IPv4 routes, such as a VPN's, sorted. The
    /// default route, the LAN subnets, and Docker's own routes are left
    /// out.
    pub routes: Vec<Ipv4Net>,
    /// The default `bridge` network's subnet, when known.
    pub bridge: Option<Ipv4Net>,
    /// The pools Docker carves new networks from.
    pub pools: Vec<AddressPool>,
    /// Where `pools` came from.
    pub pools_source: PoolsSource,
    /// Every Docker network, by name.
    pub networks: Vec<DockerNetwork>,
    /// Alchemy's own network.
    pub alchemy: AlchemyNetwork,
    /// What overlaps or went wrong, worst first. Empty when nothing did.
    pub warnings: Vec<NetworkWarning>,
}

impl Default for NetworkReport {
    /// What the daemon reports before its first check.
    fn default() -> Self {
        Self {
            engine_ready: false,
            lan: Vec::new(),
            routes: Vec::new(),
            bridge: None,
            pools: Vec::new(),
            pools_source: PoolsSource::Unknown,
            networks: Vec::new(),
            alchemy: AlchemyNetwork {
                name: alchemy_core::network::SHARED_NETWORK.to_owned(),
                state: AlchemyNetworkState::Pending,
                subnet: None,
            },
            warnings: Vec::new(),
        }
    }
}

/// Where [`NetworkReport::pools`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PoolsSource {
    /// The engine reported the pools it was configured with.
    Engine,
    /// Read from `/etc/docker/daemon.json`, on Linux.
    DaemonJson,
    /// Docker's documented defaults, since nothing configures others.
    Defaults,
    /// Not known yet, such as while the engine is not ready, or a source
    /// this build does not know.
    #[serde(other)]
    Unknown,
}

/// One Docker network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerNetwork {
    /// Its name, such as `bridge` or `blog_default`.
    pub name: String,
    /// Its driver, such as `bridge` or `host`.
    pub driver: Option<String>,
    /// Its IPv4 subnets.
    pub subnets: Vec<Ipv4Net>,
    /// The Compose project that created it.
    pub project: Option<String>,
    /// Whether Alchemy created it.
    pub managed: bool,
}

/// Alchemy's own network, shared by the apps it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlchemyNetwork {
    /// Its name, `alchemy`.
    pub name: String,
    /// Whether it is there and usable.
    pub state: AlchemyNetworkState,
    /// Its subnet, when it exists, or the one it will be created on.
    pub subnet: Option<Ipv4Net>,
}

/// The state of [`AlchemyNetwork`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlchemyNetworkState {
    /// Not checked yet, such as while the engine is not ready.
    Pending,
    /// It exists on a subnet that overlaps nothing.
    Ready,
    /// It exists on a subnet that overlaps the LAN or a route. Alchemy
    /// leaves it alone, since containers may be attached; a warning says
    /// how to fix it.
    Overlapping,
    /// It does not exist and could not be created; a warning says why.
    Unavailable,
    /// A state this build does not know.
    #[serde(other)]
    Other,
}

/// Something that overlaps or went wrong, with how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkWarning {
    /// What kind of warning it is, for clients that branch on it.
    pub kind: NetworkWarningKind,
    /// What is wrong, in a sentence or two.
    pub message: String,
    /// What to do about it, when the user can do something. May span
    /// lines, such as a `daemon.json` fragment.
    pub fix: Option<String>,
    /// Docker's overlapping subnet or pool base.
    pub subnet: Option<Ipv4Net>,
    /// What it overlaps on the host.
    pub host: Option<HostSide>,
    /// The host's overlapping LAN subnet or route.
    pub host_subnet: Option<Ipv4Net>,
    /// The Docker network involved, if any.
    pub network: Option<String>,
}

/// The kinds of [`NetworkWarning`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NetworkWarningKind {
    /// The default bridge overlaps the LAN or a route.
    BridgeOverlap,
    /// One of Docker's address pools overlaps the LAN or a route, so a
    /// new network may land on it.
    PoolOverlap,
    /// A Docker network overlaps the LAN or a route.
    NetworkOverlap,
    /// Alchemy's own network overlaps the LAN or a route. It is left as is.
    AlchemyNetworkOverlap,
    /// No free subnet is left for Alchemy's own network.
    NoFreeSubnet,
    /// Creating Alchemy's own network failed.
    CreateFailed,
    /// The host's subnets or routes could not be read.
    HostUnreadable,
    /// Docker's networks could not be read.
    EngineUnreadable,
    /// A kind this build does not know.
    #[serde(other)]
    Other,
}
