//! Planning routes: which app answers which address, and why an app
//! can't be routed.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use super::{
    ExposedPort, HostLabel, HostSuffix, Reachability, XAlchemy, XAlchemyBlock, match_host,
};
use crate::network::SHARED_NETWORK;
use crate::ports::{PortRange, Protocol, PublishedPort};

/// Everything planning needs to know about one app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppRouteInput {
    /// The app's name, which is also its Compose project name.
    pub app: String,
    /// The compose file's top-level `x-alchemy` block, if any.
    pub x_alchemy: Option<XAlchemyBlock>,
    /// The services enabled without any profile.
    pub services: Vec<ServiceRouteInput>,
    /// The app's containers, running or not.
    pub containers: Vec<RouteContainer>,
    /// The port reserved for the app as `proxy:<app>`, once there is one.
    pub listen_port: Option<u16>,
}

/// One service of an [`AppRouteInput`], from the resolved Compose config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceRouteInput {
    /// The service name.
    pub name: String,
    /// The service's own `x-alchemy` block, if any.
    pub x_alchemy: Option<XAlchemyBlock>,
    /// Its `expose` entries.
    pub expose: Vec<ExposedPort>,
    /// The ports it publishes, with their container side in `target`.
    pub ports: Vec<PublishedPort>,
}

/// One container of an [`AppRouteInput`], as the engine reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteContainer {
    /// The container name, used to pick the same replica every time.
    pub name: String,
    /// Its Compose service.
    pub service: String,
    /// Whether it is running.
    pub running: bool,
    /// Its IP address on each network it joined, by network name.
    pub ips: BTreeMap<String, IpAddr>,
    /// The host ports it publishes right now, with `target` set. Ephemeral
    /// ports show their assigned host port here.
    pub ports: Vec<PublishedPort>,
}

/// Settings that apply to every app's route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteOptions {
    /// How the host reaches containers.
    pub reachability: Reachability,
    /// The domains apps answer under on port 80, such as
    /// [`HostSuffix::Localhost`].
    pub suffixes: Vec<HostSuffix>,
}

/// Where an app's route points, before any container is consulted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RouteTarget {
    /// The service to route to.
    pub service: String,
    /// The container port to route to.
    pub port: u16,
    /// The label `x-alchemy` asks for, overriding the app's name.
    pub hostname: Option<HostLabel>,
    /// Whether its `.local` name may be announced over mDNS; `x-alchemy`
    /// turns it off with `mdns: false`.
    pub mdns: bool,
}

/// The address the proxy connects to for a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Upstream {
    /// The container's own address on the `alchemy` network.
    ContainerIp(SocketAddr),
    /// A port the container publishes on the host.
    Published(SocketAddr),
}

impl Upstream {
    /// The socket address to connect to.
    pub fn addr(&self) -> SocketAddr {
        match self {
            Self::ContainerIp(addr) | Self::Published(addr) => *addr,
        }
    }
}

/// One app's route.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Route {
    /// The app.
    pub app: String,
    /// The service requests go to.
    pub service: String,
    /// The container port requests go to.
    pub port: u16,
    /// The label it answers under on port 80, or `None` when it has none,
    /// such as when two apps want the same one.
    pub label: Option<HostLabel>,
    /// Its full hostnames, one per configured suffix, such as
    /// `wiki.localhost`.
    pub hosts: Vec<String>,
    /// The proxy port it answers on, or `None` until one is reserved or
    /// when two apps were given the same one.
    pub listen_port: Option<u16>,
    /// Where to connect, or `None` while no running container can be
    /// reached, such as when the app is stopped.
    pub upstream: Option<Upstream>,
    /// Whether its label may be announced as `<label>.local` over mDNS,
    /// as [`RouteTarget::mdns`].
    pub mdns: bool,
}

/// One candidate port a service could be routed on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CandidatePort {
    /// The service.
    pub service: String,
    /// Its container port or ports.
    pub ports: PortRange,
}

impl fmt::Display for CandidatePort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.service, self.ports)
    }
}

/// Why an app has no route, or only part of one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteProblemKind {
    /// An `x-alchemy` block didn't read; `service` is `None` for the
    /// top-level block.
    UnreadableBlock {
        /// The service whose block it is.
        service: Option<String>,
        /// What the reader said.
        reason: String,
    },
    /// More than one `x-alchemy` block was found.
    MultipleBlocks {
        /// Whether one is at the top level.
        top_level: bool,
        /// The services that have one.
        services: Vec<String>,
    },
    /// A service's own block names a different service.
    ServiceMismatch {
        /// The service the block is on.
        block: String,
        /// The service it names.
        named: String,
    },
    /// The top-level block names a service the app doesn't have.
    UnknownService {
        /// The service it names.
        service: String,
    },
    /// The top-level block names no service and the app has several.
    MissingService {
        /// The services to choose from.
        services: Vec<String>,
    },
    /// The block's port is 0.
    InvalidPort,
    /// The block's hostname isn't a DNS label.
    InvalidHostname {
        /// The hostname as written.
        hostname: String,
        /// Why it was rejected.
        reason: String,
    },
    /// Nothing exposes or publishes a port; `service` is set when a block
    /// named the service but no port.
    NoPort {
        /// The service the block names.
        service: Option<String>,
    },
    /// More than one port could be routed to.
    AmbiguousPort {
        /// The service the block names.
        service: Option<String>,
        /// The ports to choose from.
        candidates: Vec<CandidatePort>,
    },
    /// The host can only reach published ports and this one isn't.
    Unpublished {
        /// The service.
        service: String,
        /// The container port.
        port: u16,
    },
    /// The app name leaves nothing to use as a hostname.
    NoHostname,
    /// Another app wants the same hostname, so neither gets it.
    HostnameTaken {
        /// The label.
        label: HostLabel,
        /// Every app that wants it, sorted.
        apps: Vec<String>,
    },
    /// Another app was given the same proxy port, so neither gets it.
    ListenPortTaken {
        /// The port.
        port: u16,
        /// Every app given it, sorted.
        apps: Vec<String>,
    },
}

impl RouteProblemKind {
    /// Whether the app still has a route, only without a hostname or a
    /// proxy port.
    pub fn is_partial(&self) -> bool {
        matches!(
            self,
            Self::NoHostname | Self::HostnameTaken { .. } | Self::ListenPortTaken { .. }
        )
    }
}

impl fmt::Display for RouteProblemKind {
    /// A sentence or two saying what is wrong and what to add.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnreadableBlock {
                service: None,
                reason,
            } => write!(
                f,
                "The x-alchemy block can't be read: {reason}. It takes service, port, hostname, and mdns."
            ),
            Self::UnreadableBlock {
                service: Some(service),
                reason,
            } => write!(
                f,
                "Service {service}'s x-alchemy block can't be read: {reason}. It takes port, hostname, and mdns."
            ),
            Self::MultipleBlocks {
                top_level,
                services,
            } => {
                let mut places = Vec::new();
                if *top_level {
                    places.push("the top level".to_owned());
                }
                places.extend(services.iter().map(|s| format!("service {s}")));
                write!(
                    f,
                    "There are x-alchemy blocks in {}. Keep only one.",
                    places.join(", ")
                )
            }
            Self::ServiceMismatch { block, named } => write!(
                f,
                "Service {block}'s x-alchemy block names service {named}. Remove service from it, or move it to {named}."
            ),
            Self::UnknownService { service } => write!(
                f,
                "x-alchemy names service {service}, which this app doesn't run. Name one of its services."
            ),
            Self::MissingService { services } => write!(
                f,
                "x-alchemy doesn't say which service to route to. Add service with one of {}.",
                services.join(", ")
            ),
            Self::InvalidPort => f.write_str(
                "x-alchemy port 0 isn't a port. Use the container port the app listens on, such as 80.",
            ),
            Self::InvalidHostname { reason, .. } => write!(f, "In x-alchemy, {reason}."),
            Self::NoPort { service: None } => f.write_str(
                "No service exposes or publishes a port, so Alchemy doesn't know where to send requests. Add an x-alchemy block with service and port.",
            ),
            Self::NoPort {
                service: Some(service),
            } => write!(
                f,
                "Service {service} doesn't expose or publish a port. Add port to x-alchemy."
            ),
            Self::AmbiguousPort {
                service: None,
                candidates,
            } => write!(
                f,
                "This app has more than one port it could route to ({}). Add an x-alchemy block with service and port.",
                join(candidates)
            ),
            Self::AmbiguousPort {
                service: Some(service),
                candidates,
            } => write!(
                f,
                "Service {service} has more than one port it could route to ({}). Add port to x-alchemy.",
                join(candidates)
            ),
            Self::Unpublished { service, port } => write!(
                f,
                "Publish port {port} of service {service} to reach this app. This Docker engine only lets the host reach published ports."
            ),
            Self::NoHostname => f.write_str(
                "The app's name has nothing to use as a hostname. Add hostname to x-alchemy.",
            ),
            Self::HostnameTaken { label, apps } => write!(
                f,
                "Apps {} all want the hostname {label}, so none of them gets it. Set a different hostname in x-alchemy.",
                apps.join(", ")
            ),
            Self::ListenPortTaken { port, apps } => write!(
                f,
                "Apps {} were all given proxy port {port}, so none of them gets it.",
                apps.join(", ")
            ),
        }
    }
}

fn join(candidates: &[CandidatePort]) -> String {
    candidates
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A [`RouteProblemKind`] for one app.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RouteProblem {
    /// The app.
    pub app: String,
    /// What is wrong.
    pub kind: RouteProblemKind,
}

/// What [`plan_routes`] decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutePlan {
    /// The routes that can be served.
    pub table: RouteTable,
    /// Every problem found, in app order.
    pub problems: Vec<RouteProblem>,
}

/// Every app's route, indexed for the proxy.
///
/// A plain value: the daemon publishes a new one whenever an input
/// changes, and the proxy looks up each request in the latest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteTable {
    routes: Vec<Route>,
    by_port: BTreeMap<u16, usize>,
    by_label: BTreeMap<HostLabel, usize>,
    suffixes: Vec<HostSuffix>,
}

impl RouteTable {
    /// Every route, sorted by app.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// The route for `app`.
    pub fn get(&self, app: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.app == app)
    }

    /// The route served on proxy port `port`.
    pub fn lookup_port(&self, port: u16) -> Option<&Route> {
        self.by_port.get(&port).and_then(|&i| self.routes.get(i))
    }

    /// The route a request's `Host` header names, matched as
    /// [`match_host`] does against the table's suffixes.
    pub fn lookup_host(&self, host: &str) -> Option<&Route> {
        let label = HostLabel::new(&match_host(host, &self.suffixes)?).ok()?;
        self.by_label.get(&label).and_then(|&i| self.routes.get(i))
    }

    /// The proxy ports routes answer on, ascending.
    pub fn listen_ports(&self) -> impl Iterator<Item = u16> + '_ {
        self.by_port.keys().copied()
    }

    /// The suffixes hostnames are matched under.
    pub fn suffixes(&self) -> &[HostSuffix] {
        &self.suffixes
    }

    /// How many apps are routed.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether no app is routed.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Where `app`'s route should point, from its `x-alchemy` blocks or, with
/// none, the one port it exposes or publishes.
///
/// Under [`Reachability::PublishedOnly`] the port must also be published
/// in the Compose config. Containers aren't consulted, so the daemon can
/// call this to decide which apps need a proxy port before reserving one.
pub fn choose_target(
    app: &AppRouteInput,
    reachability: Reachability,
) -> Result<RouteTarget, RouteProblemKind> {
    let target = if let Some((scope, spec)) = single_block(app)? {
        from_block(app, scope, spec)?
    } else {
        let port = single_port(&app.services, None)?;
        RouteTarget {
            service: port.service,
            port: port.ports.start(),
            hostname: None,
            mdns: true,
        }
    };
    if reachability == Reachability::PublishedOnly
        && published_target(&app.services, &target).is_none()
    {
        return Err(RouteProblemKind::Unpublished {
            service: target.service,
            port: target.port,
        });
    }
    Ok(target)
}

fn single_block(
    app: &AppRouteInput,
) -> Result<Option<(Option<&str>, &XAlchemy)>, RouteProblemKind> {
    let blocks: Vec<(Option<&str>, &XAlchemyBlock)> = app
        .x_alchemy
        .iter()
        .map(|b| (None, b))
        .chain(
            app.services
                .iter()
                .filter_map(|s| s.x_alchemy.as_ref().map(|b| (Some(s.name.as_str()), b))),
        )
        .collect();
    for (scope, block) in &blocks {
        if let XAlchemyBlock::Unreadable(reason) = block {
            return Err(RouteProblemKind::UnreadableBlock {
                service: scope.map(str::to_owned),
                reason: reason.clone(),
            });
        }
    }
    match blocks.as_slice() {
        [] => Ok(None),
        [(scope, XAlchemyBlock::Read(spec))] => Ok(Some((*scope, spec))),
        _ => Err(RouteProblemKind::MultipleBlocks {
            top_level: app.x_alchemy.is_some(),
            services: blocks
                .iter()
                .filter_map(|(scope, _)| scope.map(str::to_owned))
                .collect(),
        }),
    }
}

fn from_block(
    app: &AppRouteInput,
    scope: Option<&str>,
    spec: &XAlchemy,
) -> Result<RouteTarget, RouteProblemKind> {
    let service = match (scope, spec.service.as_deref()) {
        (Some(block), Some(named)) if block != named => {
            return Err(RouteProblemKind::ServiceMismatch {
                block: block.to_owned(),
                named: named.to_owned(),
            });
        }
        (Some(block), _) => block.to_owned(),
        (None, Some(named)) => {
            if !app.services.iter().any(|s| s.name == named) {
                return Err(RouteProblemKind::UnknownService {
                    service: named.to_owned(),
                });
            }
            named.to_owned()
        }
        (None, None) => match app.services.as_slice() {
            [only] => only.name.clone(),
            services => {
                return Err(RouteProblemKind::MissingService {
                    services: services.iter().map(|s| s.name.clone()).collect(),
                });
            }
        },
    };
    let hostname = spec
        .hostname
        .as_deref()
        .map(|h| {
            HostLabel::new(h).map_err(|e| RouteProblemKind::InvalidHostname {
                hostname: h.to_owned(),
                reason: e.to_string(),
            })
        })
        .transpose()?;
    let port = match spec.port {
        Some(0) => return Err(RouteProblemKind::InvalidPort),
        Some(port) => port,
        None => single_port(&app.services, Some(&service))?.ports.start(),
    };
    Ok(RouteTarget {
        service,
        port,
        hostname,
        mdns: spec.mdns,
    })
}

/// The one TCP container port among `services` (or just `only`), or a
/// problem listing what there is.
fn single_port(
    services: &[ServiceRouteInput],
    only: Option<&str>,
) -> Result<CandidatePort, RouteProblemKind> {
    let mut candidates: Vec<CandidatePort> = services
        .iter()
        .filter(|s| only.is_none_or(|name| s.name == name))
        .flat_map(|s| {
            let exposed = s
                .expose
                .iter()
                .filter(|e| e.protocol == Protocol::Tcp)
                .map(|e| e.ports);
            let published = s
                .ports
                .iter()
                .filter(|p| p.protocol == Protocol::Tcp)
                .filter_map(|p| p.target);
            exposed.chain(published).map(|ports| CandidatePort {
                service: s.name.clone(),
                ports,
            })
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    let service = only.map(str::to_owned);
    match candidates.as_slice() {
        [] => Err(RouteProblemKind::NoPort { service }),
        [one] if one.ports.len() == 1 => Ok(one.clone()),
        _ => Err(RouteProblemKind::AmbiguousPort {
            service,
            candidates,
        }),
    }
}

/// The Compose port that publishes `target`'s container port over TCP.
fn published_target<'a>(
    services: &'a [ServiceRouteInput],
    target: &RouteTarget,
) -> Option<&'a PublishedPort> {
    services
        .iter()
        .find(|s| s.name == target.service)?
        .ports
        .iter()
        .find(|p| p.protocol == Protocol::Tcp && p.target.is_some_and(|t| t.contains(target.port)))
}

/// Where to connect for `target`, from the first running replica, by
/// name, that can be reached.
pub fn resolve_upstream(
    target: &RouteTarget,
    containers: &[RouteContainer],
    reachability: Reachability,
) -> Option<Upstream> {
    let mut replicas: Vec<&RouteContainer> = containers
        .iter()
        .filter(|c| c.running && c.service == target.service)
        .collect();
    replicas.sort_by(|a, b| a.name.cmp(&b.name));
    replicas.into_iter().find_map(|c| match reachability {
        Reachability::ContainerIp => c
            .ips
            .get(SHARED_NETWORK)
            .map(|&ip| Upstream::ContainerIp(SocketAddr::new(ip, target.port))),
        Reachability::PublishedOnly => c
            .ports
            .iter()
            .find_map(|p| host_addr(p, target.port))
            .map(Upstream::Published),
    })
}

/// The host address that reaches container port `port` through `p`.
///
/// An unspecified host address becomes loopback of the same family. A
/// range maps port for port when both sides are the same length.
fn host_addr(p: &PublishedPort, port: u16) -> Option<SocketAddr> {
    let (published, target) = (p.published?, p.target?);
    if p.protocol != Protocol::Tcp || !target.contains(port) {
        return None;
    }
    let host_port = if published.len() == target.len() {
        published.start().checked_add(port - target.start())?
    } else if published.len() == 1 {
        published.start()
    } else {
        return None;
    };
    let ip = match p.host_ip.map(|ip| ip.to_canonical()) {
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Some(IpAddr::V4(ip)) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Some(IpAddr::V6(ip)) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        Some(ip) => ip,
    };
    Some(SocketAddr::new(ip, host_port))
}

/// Build the route table and problem list for `apps`.
///
/// Apps are planned in name order. Each gets a route if
/// [`choose_target`] finds one; its upstream comes from
/// [`resolve_upstream`]. Two apps that want the same hostname, or were
/// given the same proxy port, both lose it and both get a problem, so
/// which one wins never depends on order.
pub fn plan_routes(apps: &[AppRouteInput], options: &RouteOptions) -> RoutePlan {
    let mut sorted: Vec<&AppRouteInput> = apps.iter().collect();
    sorted.sort_by(|a, b| a.app.cmp(&b.app));
    let mut problems = Vec::new();
    let mut routes = Vec::new();
    for app in sorted {
        let target = match choose_target(app, options.reachability) {
            Ok(target) => target,
            Err(kind) => {
                problems.push(RouteProblem {
                    app: app.app.clone(),
                    kind,
                });
                continue;
            }
        };
        let label = target
            .hostname
            .clone()
            .or_else(|| HostLabel::from_app(&app.app));
        if label.is_none() {
            problems.push(RouteProblem {
                app: app.app.clone(),
                kind: RouteProblemKind::NoHostname,
            });
        }
        routes.push(Route {
            app: app.app.clone(),
            upstream: resolve_upstream(&target, &app.containers, options.reachability),
            service: target.service,
            port: target.port,
            label,
            hosts: Vec::new(),
            listen_port: app.listen_port,
            mdns: target.mdns,
        });
    }

    let by_label = unique_index(
        &mut routes,
        &mut problems,
        |r| r.label.clone(),
        |r| r.label = None,
        |label, apps| RouteProblemKind::HostnameTaken { label, apps },
    );
    let by_port = unique_index(
        &mut routes,
        &mut problems,
        |r| r.listen_port,
        |r| r.listen_port = None,
        |port, apps| RouteProblemKind::ListenPortTaken { port, apps },
    );
    for route in &mut routes {
        route.hosts = route
            .label
            .iter()
            .flat_map(|l| options.suffixes.iter().map(|&s| l.host(s)))
            .collect();
    }
    problems.sort_by(|a, b| a.app.cmp(&b.app));

    RoutePlan {
        table: RouteTable {
            routes,
            by_port,
            by_label,
            suffixes: options.suffixes.clone(),
        },
        problems,
    }
}

/// Index `routes` by `key`, clearing the key on every route that shares
/// it and reporting each of them.
fn unique_index<K: Ord + Clone>(
    routes: &mut [Route],
    problems: &mut Vec<RouteProblem>,
    key: impl Fn(&Route) -> Option<K>,
    clear: impl Fn(&mut Route),
    problem: impl Fn(K, Vec<String>) -> RouteProblemKind,
) -> BTreeMap<K, usize> {
    let mut groups: BTreeMap<K, Vec<usize>> = BTreeMap::new();
    for (i, route) in routes.iter().enumerate() {
        if let Some(k) = key(route) {
            groups.entry(k).or_default().push(i);
        }
    }
    let mut index = BTreeMap::new();
    for (k, members) in groups {
        if let [only] = members.as_slice() {
            index.insert(k, *only);
            continue;
        }
        let apps: Vec<String> = members
            .iter()
            .filter_map(|&i| routes.get(i).map(|r| r.app.clone()))
            .collect();
        for &i in &members {
            if let Some(route) = routes.get_mut(i) {
                clear(route);
                problems.push(RouteProblem {
                    app: route.app.clone(),
                    kind: problem(k.clone(), apps.clone()),
                });
            }
        }
    }
    index
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

    /// A Compose port: `80` (ephemeral), `8080:80`, `127.0.0.1:8080:80`,
    /// or any of them with `/udp`. IPv6 hosts are written `::` or `::1`
    /// before the last two fields.
    fn port(spec: &str) -> PublishedPort {
        let (spec, protocol) = match spec.split_once('/') {
            Some((s, p)) => (s, p.parse().unwrap()),
            None => (spec, Protocol::Tcp),
        };
        let mut fields = spec.rsplitn(3, ':');
        let target = fields.next().unwrap();
        let published = fields.next();
        let host_ip = fields.next().map(|ip| ip.parse::<IpAddr>().unwrap());
        PublishedPort::new(protocol, published.map(|p| p.parse().unwrap()))
            .with_host_ip(host_ip)
            .with_target(Some(target.parse().unwrap()))
    }

    fn svc(name: &str, expose: &[&str], ports: &[&str]) -> ServiceRouteInput {
        ServiceRouteInput {
            name: name.into(),
            x_alchemy: None,
            expose: expose.iter().map(|e| e.parse().unwrap()).collect(),
            ports: ports.iter().map(|p| port(p)).collect(),
        }
    }

    fn block(service: Option<&str>, port: Option<u16>, hostname: Option<&str>) -> XAlchemyBlock {
        XAlchemyBlock::Read(XAlchemy {
            service: service.map(str::to_owned),
            port,
            hostname: hostname.map(str::to_owned),
            ..XAlchemy::default()
        })
    }

    fn with_block(mut s: ServiceRouteInput, b: XAlchemyBlock) -> ServiceRouteInput {
        s.x_alchemy = Some(b);
        s
    }

    fn app(
        name: &str,
        top: Option<XAlchemyBlock>,
        services: Vec<ServiceRouteInput>,
    ) -> AppRouteInput {
        AppRouteInput {
            app: name.into(),
            x_alchemy: top,
            services,
            containers: Vec::new(),
            listen_port: None,
        }
    }

    fn target(service: &str, port: u16, hostname: Option<&str>) -> RouteTarget {
        RouteTarget {
            service: service.into(),
            port,
            hostname: hostname.map(|h| HostLabel::new(h).unwrap()),
            mdns: true,
        }
    }

    fn cand(service: &str, ports: &str) -> CandidatePort {
        CandidatePort {
            service: service.into(),
            ports: ports.parse().unwrap(),
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn choosing_a_target() {
        use Reachability::{ContainerIp, PublishedOnly};
        use RouteProblemKind as P;
        let quiet = XAlchemyBlock::Read(XAlchemy {
            mdns: false,
            ..XAlchemy::default()
        });
        let cases: Vec<(&str, AppRouteInput, Reachability, Result<RouteTarget, P>)> = vec![
            (
                "one service, one exposed port",
                app("a", None, vec![svc("web", &["80"], &[])]),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "a top-level block turns mdns off",
                app("a", Some(quiet.clone()), vec![svc("web", &["80"], &[])]),
                ContainerIp,
                Ok(RouteTarget {
                    mdns: false,
                    ..target("web", 80, None)
                }),
            ),
            (
                "a service's block turns mdns off",
                app(
                    "a",
                    None,
                    vec![with_block(svc("web", &["80"], &[]), quiet.clone())],
                ),
                ContainerIp,
                Ok(RouteTarget {
                    mdns: false,
                    ..target("web", 80, None)
                }),
            ),
            (
                "one service, one published port",
                app("a", None, vec![svc("web", &[], &["8080:80"])]),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "the same port exposed and published",
                app("a", None, vec![svc("web", &["80"], &["8080:80"])]),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "one service with a port beside one without",
                app(
                    "a",
                    None,
                    vec![svc("web", &[], &["8080:80"]), svc("db", &[], &[])],
                ),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "UDP ports don't count",
                app(
                    "a",
                    None,
                    vec![svc("web", &["53/udp"], &["8080:80", "53:53/udp"])],
                ),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "two services with ports",
                app(
                    "a",
                    None,
                    vec![svc("web", &[], &["8080:80"]), svc("cache", &["6379"], &[])],
                ),
                ContainerIp,
                Err(P::AmbiguousPort {
                    service: None,
                    candidates: vec![cand("cache", "6379"), cand("web", "80")],
                }),
            ),
            (
                "an exposed range",
                app("a", None, vec![svc("web", &["8000-8001"], &[])]),
                ContainerIp,
                Err(P::AmbiguousPort {
                    service: None,
                    candidates: vec![cand("web", "8000-8001")],
                }),
            ),
            (
                "no ports at all",
                app(
                    "a",
                    None,
                    vec![svc("web", &[], &[]), svc("db", &["53/udp"], &[])],
                ),
                ContainerIp,
                Err(P::NoPort { service: None }),
            ),
            (
                "no services",
                app("a", None, vec![]),
                ContainerIp,
                Err(P::NoPort { service: None }),
            ),
            (
                "top-level block with service and port",
                app(
                    "a",
                    Some(block(Some("web"), Some(3000), None)),
                    vec![svc("web", &[], &["8080:80"]), svc("cache", &["6379"], &[])],
                ),
                ContainerIp,
                Ok(target("web", 3000, None)),
            ),
            (
                "top-level block, port only, one service",
                app(
                    "a",
                    Some(block(None, Some(3000), None)),
                    vec![svc("web", &[], &[])],
                ),
                ContainerIp,
                Ok(target("web", 3000, None)),
            ),
            (
                "top-level block, service only, port from the service",
                app(
                    "a",
                    Some(block(Some("web"), None, None)),
                    vec![svc("web", &[], &["8080:80"]), svc("cache", &["6379"], &[])],
                ),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "top-level block, no service, several services",
                app(
                    "a",
                    Some(block(None, Some(80), None)),
                    vec![svc("web", &[], &[]), svc("db", &[], &[])],
                ),
                ContainerIp,
                Err(P::MissingService {
                    services: vec!["web".into(), "db".into()],
                }),
            ),
            (
                "top-level block names a missing service",
                app(
                    "a",
                    Some(block(Some("api"), Some(80), None)),
                    vec![svc("web", &[], &[])],
                ),
                ContainerIp,
                Err(P::UnknownService {
                    service: "api".into(),
                }),
            ),
            (
                "service block, port from the service",
                app(
                    "a",
                    None,
                    vec![
                        with_block(svc("web", &["80"], &[]), block(None, None, None)),
                        svc("cache", &["6379"], &[]),
                    ],
                ),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "service block naming its own service",
                app(
                    "a",
                    None,
                    vec![with_block(
                        svc("web", &[], &[]),
                        block(Some("web"), Some(80), None),
                    )],
                ),
                ContainerIp,
                Ok(target("web", 80, None)),
            ),
            (
                "service block naming another service",
                app(
                    "a",
                    None,
                    vec![
                        with_block(svc("web", &[], &[]), block(Some("api"), Some(80), None)),
                        svc("api", &[], &[]),
                    ],
                ),
                ContainerIp,
                Err(P::ServiceMismatch {
                    block: "web".into(),
                    named: "api".into(),
                }),
            ),
            (
                "service block without a port on a service with two",
                app(
                    "a",
                    None,
                    vec![with_block(
                        svc("web", &["80", "443"], &[]),
                        block(None, None, None),
                    )],
                ),
                ContainerIp,
                Err(P::AmbiguousPort {
                    service: Some("web".into()),
                    candidates: vec![cand("web", "80"), cand("web", "443")],
                }),
            ),
            (
                "service block without a port on a service with none",
                app(
                    "a",
                    None,
                    vec![with_block(svc("web", &[], &[]), block(None, None, None))],
                ),
                ContainerIp,
                Err(P::NoPort {
                    service: Some("web".into()),
                }),
            ),
            (
                "blocks at the top level and on services",
                app(
                    "a",
                    Some(block(Some("web"), Some(80), None)),
                    vec![
                        with_block(svc("web", &[], &[]), block(None, Some(80), None)),
                        with_block(svc("api", &[], &[]), block(None, Some(80), None)),
                    ],
                ),
                ContainerIp,
                Err(P::MultipleBlocks {
                    top_level: true,
                    services: vec!["web".into(), "api".into()],
                }),
            ),
            (
                "an unreadable service block",
                app(
                    "a",
                    None,
                    vec![with_block(
                        svc("web", &["80"], &[]),
                        XAlchemyBlock::Unreadable("port must be a number".into()),
                    )],
                ),
                ContainerIp,
                Err(P::UnreadableBlock {
                    service: Some("web".into()),
                    reason: "port must be a number".into(),
                }),
            ),
            (
                "port 0",
                app(
                    "a",
                    Some(block(None, Some(0), None)),
                    vec![svc("web", &[], &[])],
                ),
                ContainerIp,
                Err(P::InvalidPort),
            ),
            (
                "a hostname",
                app(
                    "a",
                    Some(block(None, Some(80), Some("Wiki"))),
                    vec![svc("web", &[], &[])],
                ),
                ContainerIp,
                Ok(target("web", 80, Some("wiki"))),
            ),
            (
                "a hostname with a dot",
                app(
                    "a",
                    Some(block(None, Some(80), Some("wiki.localhost"))),
                    vec![svc("web", &[], &[])],
                ),
                ContainerIp,
                Err(P::InvalidHostname {
                    hostname: "wiki.localhost".into(),
                    reason: "hostname \"wiki.localhost\" may only use letters, digits, and -, such as wiki".into(),
                }),
            ),
            (
                "published only: a published port",
                app("a", None, vec![svc("web", &[], &["8080:80"])]),
                PublishedOnly,
                Ok(target("web", 80, None)),
            ),
            (
                "published only: an ephemeral port",
                app("a", None, vec![svc("web", &[], &["80"])]),
                PublishedOnly,
                Ok(target("web", 80, None)),
            ),
            (
                "published only: a port in a published range",
                app(
                    "a",
                    Some(block(None, Some(8001), None)),
                    vec![svc("web", &[], &["9000-9002:8000-8002"])],
                ),
                PublishedOnly,
                Ok(target("web", 8001, None)),
            ),
            (
                "published only: an exposed port",
                app("a", None, vec![svc("web", &["80"], &[])]),
                PublishedOnly,
                Err(P::Unpublished {
                    service: "web".into(),
                    port: 80,
                }),
            ),
            (
                "published only: a block port that isn't published",
                app(
                    "a",
                    Some(block(None, Some(3000), None)),
                    vec![svc("web", &[], &["8080:80"])],
                ),
                PublishedOnly,
                Err(P::Unpublished {
                    service: "web".into(),
                    port: 3000,
                }),
            ),
            (
                "published only: published over UDP",
                app(
                    "a",
                    Some(block(None, Some(53), None)),
                    vec![svc("web", &[], &["53:53/udp"])],
                ),
                PublishedOnly,
                Err(P::Unpublished {
                    service: "web".into(),
                    port: 53,
                }),
            ),
        ];
        for (name, input, reach, want) in cases {
            assert_eq!(choose_target(&input, reach), want, "{name}");
        }
    }

    fn container(name: &str, service: &str, running: bool) -> RouteContainer {
        RouteContainer {
            name: name.into(),
            service: service.into(),
            running,
            ips: BTreeMap::new(),
            ports: Vec::new(),
        }
    }

    fn on_network(mut c: RouteContainer, network: &str, ip: &str) -> RouteContainer {
        c.ips.insert(network.into(), ip.parse().unwrap());
        c
    }

    fn publishing(mut c: RouteContainer, ports: &[&str]) -> RouteContainer {
        c.ports = ports.iter().map(|p| port(p)).collect();
        c
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn resolving_an_upstream() {
        use Reachability::{ContainerIp, PublishedOnly};
        type Case = (
            &'static str,
            RouteTarget,
            Vec<RouteContainer>,
            Reachability,
            Option<Upstream>,
        );
        let web80 = target("web", 80, None);
        let running = || container("a-web-1", "web", true);
        let cases: Vec<Case> = vec![
            (
                "container IP on the alchemy network",
                web80.clone(),
                vec![on_network(
                    on_network(running(), "a_default", "172.18.0.2"),
                    "alchemy",
                    "10.250.0.5",
                )],
                ContainerIp,
                Some(Upstream::ContainerIp(addr("10.250.0.5:80"))),
            ),
            (
                "not attached to the alchemy network yet",
                web80.clone(),
                vec![on_network(running(), "a_default", "172.18.0.2")],
                ContainerIp,
                None,
            ),
            (
                "stopped",
                web80.clone(),
                vec![on_network(
                    container("a-web-1", "web", false),
                    "alchemy",
                    "10.250.0.5",
                )],
                ContainerIp,
                None,
            ),
            (
                "another service's container",
                web80.clone(),
                vec![on_network(
                    container("a-db-1", "db", true),
                    "alchemy",
                    "10.250.0.6",
                )],
                ContainerIp,
                None,
            ),
            (
                "first replica by name",
                web80.clone(),
                vec![
                    on_network(container("a-web-2", "web", true), "alchemy", "10.250.0.8"),
                    on_network(container("a-web-1", "web", true), "alchemy", "10.250.0.7"),
                ],
                ContainerIp,
                Some(Upstream::ContainerIp(addr("10.250.0.7:80"))),
            ),
            (
                "published on every address",
                web80.clone(),
                vec![publishing(running(), &["8080:80"])],
                PublishedOnly,
                Some(Upstream::Published(addr("127.0.0.1:8080"))),
            ),
            (
                "published on 0.0.0.0 and ::",
                web80.clone(),
                vec![publishing(running(), &["0.0.0.0:32768:80", ":::32768:80"])],
                PublishedOnly,
                Some(Upstream::Published(addr("127.0.0.1:32768"))),
            ),
            (
                "published on :: only",
                web80.clone(),
                vec![publishing(running(), &[":::8080:80"])],
                PublishedOnly,
                Some(Upstream::Published(addr("[::1]:8080"))),
            ),
            (
                "published on one address",
                web80.clone(),
                vec![publishing(running(), &["192.168.1.5:8080:80"])],
                PublishedOnly,
                Some(Upstream::Published(addr("192.168.1.5:8080"))),
            ),
            (
                "a port in a published range",
                target("web", 8002, None),
                vec![publishing(running(), &["9000-9002:8000-8002"])],
                PublishedOnly,
                Some(Upstream::Published(addr("127.0.0.1:9002"))),
            ),
            (
                "a range published on one port",
                target("web", 8002, None),
                vec![publishing(running(), &["9000:8000-8002"])],
                PublishedOnly,
                Some(Upstream::Published(addr("127.0.0.1:9000"))),
            ),
            (
                "a different port published",
                web80.clone(),
                vec![publishing(running(), &["8443:443"])],
                PublishedOnly,
                None,
            ),
            (
                "published over UDP",
                target("web", 53, None),
                vec![publishing(running(), &["53:53/udp"])],
                PublishedOnly,
                None,
            ),
            (
                "container IP ignored when only published ports reach",
                web80.clone(),
                vec![on_network(running(), "alchemy", "10.250.0.5")],
                PublishedOnly,
                None,
            ),
        ];
        for (name, target, containers, reach, want) in cases {
            assert_eq!(
                resolve_upstream(&target, &containers, reach),
                want,
                "{name}"
            );
        }
    }

    fn routed(name: &str, listen_port: Option<u16>, hostname: Option<&str>) -> AppRouteInput {
        let mut a = app(
            name,
            hostname.map(|h| block(None, None, Some(h))),
            vec![svc("web", &[], &["80"])],
        );
        a.listen_port = listen_port;
        a.containers = vec![on_network(
            container(&format!("{name}-web-1"), "web", true),
            "alchemy",
            "10.250.0.5",
        )];
        a
    }

    fn options() -> RouteOptions {
        RouteOptions {
            reachability: Reachability::ContainerIp,
            suffixes: vec![HostSuffix::Localhost],
        }
    }

    #[test]
    fn planning_routes() {
        let mut stopped = routed("stopped", None, None);
        stopped.containers.clear();
        let apps = vec![
            routed("wiki", Some(8101), None),
            routed("photos", Some(8100), None),
            app("broken", None, vec![svc("web", &[], &[])]),
            stopped,
        ];
        let plan = plan_routes(&apps, &options());

        assert_eq!(
            plan.problems,
            vec![RouteProblem {
                app: "broken".into(),
                kind: RouteProblemKind::NoPort { service: None },
            }]
        );
        let names: Vec<&str> = plan.table.routes().iter().map(|r| r.app.as_str()).collect();
        assert_eq!(names, ["photos", "stopped", "wiki"]);

        let wiki = plan.table.get("wiki").unwrap();
        assert_eq!(
            wiki,
            &Route {
                app: "wiki".into(),
                service: "web".into(),
                port: 80,
                label: Some(HostLabel::new("wiki").unwrap()),
                hosts: vec!["wiki.localhost".into()],
                listen_port: Some(8101),
                upstream: Some(Upstream::ContainerIp(addr("10.250.0.5:80"))),
                mdns: true,
            }
        );
        let table = &plan.table;
        assert_eq!(table.lookup_port(8101), Some(wiki));
        assert_eq!(table.lookup_port(8100).unwrap().app, "photos");
        assert_eq!(table.lookup_port(8102), None);
        assert_eq!(table.listen_ports().collect::<Vec<_>>(), [8100, 8101]);
        assert_eq!(table.lookup_host("WIKI.localhost:80"), Some(wiki));
        let stopped = table.lookup_host("stopped.localhost").unwrap();
        assert_eq!((stopped.listen_port, stopped.upstream), (None, None));
        assert_eq!(table.lookup_host("wiki.local"), None);
        assert_eq!(table.lookup_host("broken.localhost"), None);
        assert_eq!(table.lookup_host("localhost"), None);
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn colliding_hostnames_and_ports() {
        let apps = vec![
            routed("my_app", Some(8100), None),
            routed("my-app", Some(8101), None),
            routed("blog", Some(8101), Some("journal")),
            routed("journal", Some(8102), None),
            routed("notes", Some(8103), Some("Wiki")),
        ];
        let plan = plan_routes(&apps, &options());
        let taken = |label: &str, apps: &[&str]| RouteProblemKind::HostnameTaken {
            label: HostLabel::new(label).unwrap(),
            apps: apps.iter().map(|&a| a.to_owned()).collect(),
        };
        let port_taken = RouteProblemKind::ListenPortTaken {
            port: 8101,
            apps: vec!["blog".into(), "my-app".into()],
        };
        let problems: Vec<(String, RouteProblemKind)> =
            plan.problems.into_iter().map(|p| (p.app, p.kind)).collect();
        assert_eq!(
            problems,
            vec![
                ("blog".into(), taken("journal", &["blog", "journal"])),
                ("blog".into(), port_taken.clone()),
                ("journal".into(), taken("journal", &["blog", "journal"])),
                ("my-app".into(), taken("my-app", &["my-app", "my_app"])),
                ("my-app".into(), port_taken),
                ("my_app".into(), taken("my-app", &["my-app", "my_app"])),
            ]
        );
        assert!(problems.iter().all(|(_, k)| k.is_partial()));

        let table = &plan.table;
        assert_eq!(table.len(), 5);
        for app in ["blog", "journal", "my-app", "my_app"] {
            let route = table.get(app).unwrap();
            assert_eq!(route.label, None, "{app}");
            assert!(route.hosts.is_empty(), "{app}");
        }
        assert_eq!(table.get("blog").unwrap().listen_port, None);
        assert_eq!(table.get("my-app").unwrap().listen_port, None);
        assert_eq!(table.lookup_port(8100).unwrap().app, "my_app");
        assert_eq!(table.lookup_port(8101), None);
        assert_eq!(table.lookup_host("journal.localhost"), None);
        assert_eq!(table.lookup_host("my-app.localhost"), None);
        assert_eq!(table.lookup_host("wiki.localhost").unwrap().app, "notes");
        assert_eq!(table.lookup_host("notes.localhost"), None);
    }

    #[test]
    fn hosts_follow_the_suffixes() {
        let opts = RouteOptions {
            reachability: Reachability::ContainerIp,
            suffixes: vec![HostSuffix::Localhost, HostSuffix::Local],
        };
        let plan = plan_routes(&[routed("wiki", None, None)], &opts);
        let wiki = plan.table.get("wiki").unwrap();
        assert_eq!(wiki.hosts, ["wiki.localhost", "wiki.local"]);
        assert_eq!(plan.table.lookup_host("wiki.local.").unwrap().app, "wiki");
        assert_eq!(plan.table.suffixes(), opts.suffixes.as_slice());
    }

    #[test]
    fn unpublished_apps_have_no_route_where_only_published_ports_reach() {
        let opts = RouteOptions {
            reachability: Reachability::PublishedOnly,
            suffixes: vec![HostSuffix::Localhost],
        };
        let apps = vec![
            app("hidden", None, vec![svc("web", &["80"], &[])]),
            routed("shown", Some(8100), None),
        ];
        let plan = plan_routes(&apps, &opts);
        assert_eq!(
            plan.problems,
            vec![RouteProblem {
                app: "hidden".into(),
                kind: RouteProblemKind::Unpublished {
                    service: "web".into(),
                    port: 80,
                },
            }]
        );
        assert_eq!(plan.table.get("hidden"), None);
        assert_eq!(plan.table.get("shown").unwrap().upstream, None);
        assert_eq!(
            plan.problems[0].kind.to_string(),
            "Publish port 80 of service web to reach this app. This Docker engine only lets the host reach published ports."
        );
    }

    #[test]
    fn empty_table() {
        let table = RouteTable::default();
        assert!(table.is_empty());
        assert_eq!(table.lookup_port(80), None);
        assert_eq!(table.lookup_host("wiki.localhost"), None);
    }

    #[test]
    fn problem_text() {
        let cases = [
            (
                RouteProblemKind::MultipleBlocks {
                    top_level: true,
                    services: vec!["web".into()],
                },
                "There are x-alchemy blocks in the top level, service web. Keep only one.",
            ),
            (
                RouteProblemKind::AmbiguousPort {
                    service: None,
                    candidates: vec![cand("cache", "6379"), cand("web", "80")],
                },
                "This app has more than one port it could route to (cache:6379, web:80). Add an x-alchemy block with service and port.",
            ),
            (
                RouteProblemKind::InvalidHostname {
                    hostname: "a.b".into(),
                    reason: "hostname \"a.b\" may only use letters, digits, and -, such as wiki"
                        .into(),
                },
                "In x-alchemy, hostname \"a.b\" may only use letters, digits, and -, such as wiki.",
            ),
            (
                RouteProblemKind::HostnameTaken {
                    label: HostLabel::new("my-app").unwrap(),
                    apps: vec!["my-app".into(), "my_app".into()],
                },
                "Apps my-app, my_app all want the hostname my-app, so none of them gets it. Set a different hostname in x-alchemy.",
            ),
        ];
        for (kind, want) in cases {
            assert_eq!(kind.to_string(), want);
        }
    }

    #[test]
    fn problem_wire_shape() {
        let problem = RouteProblem {
            app: "wiki".into(),
            kind: RouteProblemKind::Unpublished {
                service: "web".into(),
                port: 80,
            },
        };
        let json = serde_json::to_value(&problem).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "app": "wiki",
                "kind": {"kind": "unpublished", "service": "web", "port": 80},
            })
        );
        assert_eq!(
            serde_json::from_value::<RouteProblem>(json).unwrap(),
            problem
        );
    }
}
