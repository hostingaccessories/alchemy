//! The host's networks next to Docker's, and what overlaps.
//!
//! `GET /api/network` returns a [`NetworkReport`]: the LAN subnets and
//! routes the daemon sees, Docker's bridge, address pools, and networks,
//! Alchemy's own [`AlchemyNetwork`], and a [`NetworkWarning`] for each
//! overlap, with the fix spelled out. The daemon refreshes it in the
//! background, so it may be up to half a minute old. Its
//! [`ProxyReport`] says what the web address proxy listens on, such as
//! whether port 80 could be opened, and its [`MdnsReport`] whether apps'
//! `.local` names are announced; both are always current.
//!
//! `GET /api/settings/network` returns the [`NetworkSettings`]; `PUT`
//! takes a [`NetworkSettingsUpdate`] and returns the new settings.
//!
//! Every human-readable `message` and `fix` here is written for the people
//! using Alchemy and safe to show as is.

pub use alchemy_core::network::{AddressPool, HostSide, Ipv4Net};
use serde::{Deserialize, Serialize};

use crate::privileges::PrivilegeAction;

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
    /// The web address proxy. Defaults to an idle one when an older daemon
    /// leaves it out.
    #[serde(default)]
    pub proxy: ProxyReport,
    /// Announcing apps' `.local` names on the LAN. Defaults to `off` when
    /// an older daemon, which announces nothing, leaves it out.
    #[serde(default)]
    pub mdns: MdnsReport,
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
            proxy: ProxyReport::default(),
            mdns: MdnsReport::default(),
        }
    }
}

/// The network settings, from `GET /api/settings/network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSettings {
    /// Whether apps' `<label>.local` names are announced on the LAN over
    /// mDNS: the `network.mdns` setting, on by default. While it is off,
    /// no app has a `.local` address. An app can also opt out on its own
    /// with `x-alchemy: { mdns: false }`.
    pub mdns: bool,
}

impl Default for NetworkSettings {
    /// Everything on.
    fn default() -> Self {
        Self { mdns: true }
    }
}

impl NetworkSettings {
    /// These settings with `update` applied.
    #[must_use]
    pub fn updated(self, update: NetworkSettingsUpdate) -> Self {
        Self {
            mdns: update.mdns.unwrap_or(self.mdns),
        }
    }
}

/// The body of `PUT /api/settings/network`. A setting left out keeps its
/// current value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSettingsUpdate {
    /// The new [`NetworkSettings::mdns`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdns: Option<bool>,
}

/// Whether apps' `.local` names are announced on the LAN, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdnsReport {
    /// Whether names are being announced.
    pub state: MdnsState,
    /// What announces them, once one was chosen.
    pub backend: Option<MdnsBackend>,
    /// What is wrong or still to happen, when `state` isn't `announcing`.
    pub message: Option<String>,
    /// What to do about it, when the user can do something.
    pub fix: Option<String>,
}

impl Default for MdnsReport {
    /// Nothing announced, and nothing to say.
    fn default() -> Self {
        Self {
            state: MdnsState::Off,
            backend: None,
            message: None,
            fix: None,
        }
    }
}

/// The state of an [`MdnsReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MdnsState {
    /// Not announcing, since the `network.mdns` setting is off.
    Off,
    /// Not announcing yet: the announcer is being chosen or started.
    Starting,
    /// Announcing the names apps' `.local` addresses use. Each address
    /// says whether its own name is announced yet.
    Announcing,
    /// Can't announce, such as when this computer doesn't allow it or no
    /// announcer works; `message` says why.
    Unavailable,
    /// A state this build does not know.
    #[serde(other)]
    Other,
}

/// What announces `.local` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MdnsBackend {
    /// The avahi daemon, over D-Bus, on Linux.
    Avahi,
    /// macOS's mDNSResponder, through `dns-sd`.
    DnsSd,
    /// Alchemy's own responder, on Linux without avahi.
    BuiltIn,
    /// A backend this build does not know.
    #[serde(other)]
    Other,
}

/// What the web address proxy listens on and how it reaches apps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyReport {
    /// How the proxy reaches apps' containers, or `None` until the engine
    /// has been ready.
    pub reachability: Option<Reachability>,
    /// The Host listener, port 80, which routes `<app>.localhost` and,
    /// while names are announced, `<app>.local`. It is
    /// `off` while no app has a hostname.
    pub host: ProxyListener,
    /// Each app's own proxy port, ascending.
    pub ports: Vec<ProxyListener>,
}

impl Default for ProxyReport {
    /// An idle proxy: nothing routed and nothing open.
    fn default() -> Self {
        Self {
            reachability: None,
            host: ProxyListener {
                port: HOST_PORT,
                app: None,
                state: ListenerState::Off,
                message: None,
                fix: None,
                privilege: None,
            },
            ports: Vec::new(),
        }
    }
}

/// The port the proxy routes hostnames on.
pub const HOST_PORT: u16 = 80;

/// One port the proxy listens on, or wants to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyListener {
    /// The port.
    pub port: u16,
    /// The app it serves, or `None` for the Host listener.
    pub app: Option<String>,
    /// Whether it is open.
    pub state: ListenerState,
    /// Why it isn't open, when `state` is `unavailable`, and what still
    /// works.
    pub message: Option<String>,
    /// What to do about it, when the user can do something.
    pub fix: Option<String>,
    /// The privilege that would open it, such as `low_ports` for port 80
    /// refused for lack of permission on Linux. Its state, and how to
    /// grant it, are in `GET /api/machine/privileges`. Defaults to `None`
    /// when an older daemon leaves it out.
    #[serde(default)]
    pub privilege: Option<PrivilegeAction>,
}

/// The state of a [`ProxyListener`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ListenerState {
    /// Not wanted, since no route needs it.
    Off,
    /// Accepting connections. Its addresses are ready only once a request
    /// sent to it over loopback and the LAN address reached the proxy.
    Listening,
    /// Wanted but couldn't be opened, such as port 80 without permission
    /// or a port another program holds, or open but another program
    /// answers its traffic, such as a firewall redirect. It is tried
    /// again when the routes change and checked again every few seconds.
    Unavailable,
    /// A state this build does not know.
    #[serde(other)]
    Other,
}

/// How the proxy reaches apps' containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reachability {
    /// Through container IPs on the `alchemy` network, so ports that
    /// aren't published work too. A native Linux engine.
    ContainerIp,
    /// Only through ports published on this machine, as with Docker
    /// Desktop or rootless Docker.
    PublishedOnly,
    /// A way this build does not know.
    #[serde(other)]
    Other,
}

impl From<alchemy_core::route::Reachability> for Reachability {
    fn from(reach: alchemy_core::route::Reachability) -> Self {
        match reach {
            alchemy_core::route::Reachability::ContainerIp => Self::ContainerIp,
            alchemy_core::route::Reachability::PublishedOnly => Self::PublishedOnly,
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
