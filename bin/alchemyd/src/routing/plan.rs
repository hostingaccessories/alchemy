//! The routing task's decisions, as pure functions over the daemon's
//! state: what to plan routes from, which apps need a proxy port, which
//! containers to attach, and what clients are told.

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alchemy_api::apps::{
    AddressKind, AddressState, AppAddress, AppList, AppProblem, AppProblemKind,
};
use alchemy_api::network::{ListenerState, ProxyListener, ProxyReport};
use alchemy_core::engine::{ContainerState, Containers};
use alchemy_core::ports::PortRange;
use alchemy_core::route::{
    AppRouteInput, Reachability, Route, RouteContainer, RoutePlan, RouteTable, ServiceRouteInput,
    choose_target,
};

use crate::apps::ConfigOutcome;
use crate::proxy::{BindError, ListenerStatus, ProxyStatus};

/// What the routing task last decided, kept in
/// [`State`](crate::State) for the apps task and `GET /api/network`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routing {
    /// Each app's addresses and route problems, by app name. Apps with
    /// neither are left out.
    pub apps: BTreeMap<String, AppRouting>,
    /// The proxy's listeners and how it reaches apps.
    pub proxy: ProxyReport,
}

/// One app's part of [`Routing`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppRouting {
    /// Where to open it, best first.
    pub addresses: Vec<AppAddress>,
    /// Why it has no route, or only part of one.
    pub problems: Vec<AppProblem>,
}

/// The route inputs for every app whose Compose configuration was read,
/// with each app's proxy port from `reserved`, by app name.
///
/// An app is matched to its containers by its Compose project, as the app
/// list does. Containers without a Compose service are left out.
pub fn route_inputs(
    apps: &AppList,
    configs: &BTreeMap<PathBuf, ConfigOutcome>,
    containers: &Containers,
    reserved: &BTreeMap<String, u16>,
) -> Vec<AppRouteInput> {
    apps.apps
        .iter()
        .filter_map(|app| {
            let Some(ConfigOutcome::Ready(config)) = configs.get(Path::new(&app.dir)) else {
                return None;
            };
            let name = app.name.to_string();
            Some(AppRouteInput {
                x_alchemy: config.x_alchemy.clone(),
                services: config
                    .services
                    .iter()
                    .map(|s| ServiceRouteInput {
                        name: s.name.clone(),
                        x_alchemy: s.x_alchemy.clone(),
                        expose: s.expose.clone(),
                        ports: s.ports.clone(),
                    })
                    .collect(),
                containers: containers
                    .iter()
                    .filter(|c| c.project.as_deref() == Some(config.name.as_str()))
                    .filter_map(|c| {
                        Some(RouteContainer {
                            name: c.name.clone(),
                            service: c.service.clone()?,
                            running: c.state == ContainerState::Running,
                            ips: c
                                .networks
                                .iter()
                                .map(|(network, ip)| (network.clone(), IpAddr::V4(*ip)))
                                .collect(),
                            ports: c.ports.clone(),
                        })
                    })
                    .collect(),
                listen_port: reserved.get(&name).copied(),
                app: name,
            })
        })
        .collect()
}

/// The apps that have a route target but no proxy port yet, by name.
pub fn needs_port(inputs: &[AppRouteInput], reachability: Reachability) -> Vec<String> {
    inputs
        .iter()
        .filter(|input| input.listen_port.is_none())
        .filter(|input| choose_target(input, reachability).is_ok())
        .map(|input| input.app.clone())
        .collect()
}

/// The running containers that serve a route but haven't joined the
/// `alchemy` network, by name. Only container IPs need it, so under
/// [`Reachability::PublishedOnly`] there are none.
pub fn to_attach(
    table: &RouteTable,
    inputs: &[AppRouteInput],
    reachability: Reachability,
) -> Vec<String> {
    if reachability != Reachability::ContainerIp {
        return Vec::new();
    }
    let mut names: Vec<String> = table
        .routes()
        .iter()
        .filter_map(|route| {
            let input = inputs.iter().find(|i| i.app == route.app)?;
            Some(input.containers.iter().filter(move |c| {
                c.running
                    && c.service == route.service
                    && !c.ips.contains_key(alchemy_core::network::SHARED_NETWORK)
            }))
        })
        .flatten()
        .map(|c| c.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Which of `wanted` to try now: those not tried within `cooldown` of
/// `now`, as `tried` records.
pub fn due(
    wanted: Vec<String>,
    tried: &BTreeMap<String, Instant>,
    now: Instant,
    cooldown: Duration,
) -> Vec<String> {
    wanted
        .into_iter()
        .filter(|name| {
            tried
                .get(name)
                .is_none_or(|at| now.saturating_duration_since(*at) >= cooldown)
        })
        .collect()
}

/// Where the proxy listens and what to call it in URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addressing {
    /// The Host listener's port, or `None` when there is none.
    pub host_port: Option<u16>,
    /// The host to put in app port URLs: the machine's LAN address, so
    /// they work from other devices.
    pub app_host: IpAddr,
}

/// Everything [`routing`] combines.
#[derive(Debug, Clone, Copy)]
pub struct RoutingInputs<'a> {
    /// The plan the proxy was given.
    pub plan: &'a RoutePlan,
    /// What it was planned from.
    pub inputs: &'a [AppRouteInput],
    /// How the proxy reaches containers.
    pub reachability: Reachability,
    /// Apps whose proxy port couldn't be reserved, with why.
    pub unreserved: &'a BTreeMap<String, String>,
    /// The range proxy ports are reserved from.
    pub range: PortRange,
    /// What the proxy is listening on.
    pub status: &'a ProxyStatus,
    /// What to call its listeners.
    pub addressing: Addressing,
}

/// Each app's addresses and route problems, and the proxy report.
pub fn routing(inputs: &RoutingInputs<'_>) -> Routing {
    let mut apps: BTreeMap<String, AppRouting> = BTreeMap::new();
    for route in inputs.plan.table.routes() {
        let addresses = addresses(route, inputs);
        if !addresses.is_empty() {
            apps.entry(route.app.clone()).or_default().addresses = addresses;
        }
    }
    for problem in &inputs.plan.problems {
        apps.entry(problem.app.clone())
            .or_default()
            .problems
            .push(AppProblem {
                kind: AppProblemKind::Route,
                message: problem.kind.to_string(),
            });
    }
    for (app, why) in inputs.unreserved {
        apps.entry(app.clone()).or_default().problems.push(AppProblem {
            kind: AppProblemKind::Route,
            message: format!(
                "Alchemy couldn't reserve a port between {} and {} for this app, so it has no address of its own: {why}.",
                inputs.range.start(),
                inputs.range.end()
            ),
        });
    }
    Routing {
        apps,
        proxy: proxy_report(inputs),
    }
}

fn addresses(route: &Route, inputs: &RoutingInputs<'_>) -> Vec<AppAddress> {
    let base = route_state(route, inputs);
    let address = |kind, url, listener: &ListenerStatus, port: u16| {
        let (state, message) = match (listener, &base) {
            (ListenerStatus::Unavailable(error), _) => {
                (AddressState::Unavailable, Some(cannot_listen(port, error)))
            }
            (_, (AddressState::NotRunning, message)) => (AddressState::NotRunning, message.clone()),
            (ListenerStatus::Off, _) => (
                AddressState::Pending,
                Some(format!("Alchemy is opening port {port}.")),
            ),
            (ListenerStatus::Listening(_), (state, message)) => (*state, message.clone()),
        };
        AppAddress {
            kind,
            url,
            state,
            service: route.service.clone(),
            port: route.port,
            message,
        }
    };

    let mut found = Vec::new();
    if let Some(configured) = inputs.addressing.host_port {
        let listener = &inputs.status.host;
        let port = match listener {
            ListenerStatus::Listening(addr) => addr.port(),
            ListenerStatus::Off | ListenerStatus::Unavailable(_) => configured,
        };
        for host in &route.hosts {
            let url = if port == 80 {
                format!("http://{host}/")
            } else {
                format!("http://{host}:{port}/")
            };
            found.push(address(AddressKind::Host, url, listener, port));
        }
    }
    if let Some(port) = route.listen_port {
        let listener = inputs.status.ports.get(&port).cloned().unwrap_or_default();
        let url = format!(
            "http://{}/",
            SocketAddr::new(inputs.addressing.app_host, port)
        );
        found.push(address(AddressKind::Port, url, &listener, port));
    }
    found.sort_by_key(|a| rank(a.state));
    found
}

/// Whether `route`'s app can be reached, apart from the proxy's
/// listeners, and why not.
fn route_state(route: &Route, inputs: &RoutingInputs<'_>) -> (AddressState, Option<String>) {
    if route.upstream.is_some() {
        return (AddressState::Ready, None);
    }
    let running = inputs
        .inputs
        .iter()
        .find(|i| i.app == route.app)
        .is_some_and(|i| {
            i.containers
                .iter()
                .any(|c| c.running && c.service == route.service)
        });
    if !running {
        return (
            AddressState::NotRunning,
            Some(format!("Service {} isn't running.", route.service)),
        );
    }
    let why = match inputs.reachability {
        Reachability::ContainerIp => format!(
            "Alchemy is connecting service {} to the alchemy network.",
            route.service
        ),
        Reachability::PublishedOnly => format!(
            "Service {} isn't publishing port {} right now. Restart the app to apply its compose file.",
            route.service, route.port
        ),
    };
    (AddressState::Pending, Some(why))
}

fn rank(state: AddressState) -> u8 {
    match state {
        AddressState::Ready => 0,
        AddressState::Pending => 1,
        AddressState::NotRunning => 2,
        AddressState::Unavailable => 3,
        _ => 4,
    }
}

fn cannot_listen(port: u16, error: &BindError) -> String {
    match error.kind {
        io::ErrorKind::PermissionDenied => {
            format!("Alchemy doesn't have permission to listen on port {port}.")
        }
        io::ErrorKind::AddrInUse => format!("Another program is using port {port}."),
        _ => format!("Alchemy can't listen on port {port}: {}.", error.message),
    }
}

/// The [`ProxyReport`]: the Host listener, with what to do when it can't
/// open, and each app port, named by the app `table` serves on it.
fn proxy_report(inputs: &RoutingInputs<'_>) -> ProxyReport {
    let table = &inputs.plan.table;
    let host_port = inputs.addressing.host_port.unwrap_or(80);
    let (state, message, fix) = match &inputs.status.host {
        ListenerStatus::Off => (ListenerState::Off, None, None),
        ListenerStatus::Listening(_) => (ListenerState::Listening, None, None),
        ListenerStatus::Unavailable(error) => {
            let (message, fix) = host_unavailable(host_port, error);
            (ListenerState::Unavailable, Some(message), fix)
        }
    };
    let host = ProxyListener {
        port: match &inputs.status.host {
            ListenerStatus::Listening(addr) => addr.port(),
            _ => host_port,
        },
        app: None,
        state,
        message,
        fix,
    };
    let ports = inputs
        .status
        .ports
        .iter()
        .map(|(&port, status)| {
            let (state, message) = match status {
                ListenerStatus::Off => (ListenerState::Off, None),
                ListenerStatus::Listening(_) => (ListenerState::Listening, None),
                ListenerStatus::Unavailable(error) => {
                    (ListenerState::Unavailable, Some(cannot_listen(port, error)))
                }
            };
            ProxyListener {
                port,
                app: table.lookup_port(port).map(|r| r.app.clone()),
                state,
                message,
                fix: None,
            }
        })
        .collect();
    ProxyReport {
        reachability: Some(inputs.reachability.into()),
        host,
        ports,
    }
}

/// What to tell people when the Host listener can't open, and how to fix
/// it, if they can.
fn host_unavailable(port: u16, error: &BindError) -> (String, Option<String>) {
    let consequence =
        "so addresses like http://<app>.localhost don't work. Each app's own port still does.";
    match error.kind {
        io::ErrorKind::PermissionDenied => (
            format!(
                "Alchemy doesn't have permission to listen on port {port}, {consequence}"
            ),
            Some(
                "Let alchemyd use low ports with `sudo setcap cap_net_bind_service=+ep \"$(command -v alchemyd)\"`, then restart it."
                    .to_owned(),
            ),
        ),
        io::ErrorKind::AddrInUse => (
            format!("Another program is using port {port}, {consequence}"),
            Some(format!(
                "Stop the program listening on port {port}; `alchemy machine ports` shows which it is. Alchemy tries the port again every few seconds."
            )),
        ),
        _ => (
            format!(
                "Alchemy can't listen on port {port} ({}), {consequence}",
                error.message
            ),
            None,
        ),
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
    use std::net::Ipv4Addr;

    use alchemy_api::apps::{App, StackName, StackStatus, StopReason};
    use alchemy_core::engine::ContainerStatus;
    use alchemy_core::ports::{Protocol, PublishedPort};
    use alchemy_core::route::{
        ExposedPort, HostSuffix, RouteOptions, XAlchemy, XAlchemyBlock, plan_routes,
    };

    use super::*;
    use crate::compose::{ComposeConfig, ServiceConfig};

    fn app(name: &str, project: &str) -> App {
        App {
            name: StackName::new(name).unwrap(),
            status: StackStatus::Running,
            dir: format!("/srv/apps/{name}"),
            root: "/srv/apps".into(),
            compose_file: "compose.yaml".into(),
            project: Some(project.into()),
            images: None,
            services: Vec::new(),
            problems: Vec::new(),
            stop_reason: StopReason::default(),
            addresses: Vec::new(),
        }
    }

    fn service(name: &str, expose: &[&str], published: &[(u16, u16)]) -> ServiceConfig {
        ServiceConfig {
            name: name.into(),
            expose: expose
                .iter()
                .map(|e| e.parse::<ExposedPort>().unwrap())
                .collect(),
            ports: published.iter().map(|&(p, t)| tcp(p, t)).collect(),
            ..ServiceConfig::default()
        }
    }

    fn tcp(published: u16, target: u16) -> PublishedPort {
        PublishedPort::new(Protocol::Tcp, PortRange::single(published).ok())
            .with_target(PortRange::single(target).ok())
    }

    fn config(project: &str, services: Vec<ServiceConfig>) -> ConfigOutcome {
        ConfigOutcome::Ready(ComposeConfig {
            name: project.into(),
            services,
            ..ComposeConfig::default()
        })
    }

    fn container(
        name: &str,
        project: &str,
        service: &str,
        alchemy: Option<[u8; 4]>,
    ) -> ContainerStatus {
        let mut c = ContainerStatus::new(name, name, ContainerState::Running);
        c.project = Some(project.into());
        c.service = Some(service.into());
        if let Some(ip) = alchemy {
            c.networks.insert("alchemy".into(), Ipv4Addr::from(ip));
        }
        c
    }

    /// `blog` (project `blog`) exposing web:80 with its container on the
    /// alchemy network, `wiki` (project `notes`) exposing app:3000 with a
    /// container not attached yet, `db` with nothing to route, and `new`
    /// not read yet.
    fn world() -> (AppList, BTreeMap<PathBuf, ConfigOutcome>, Containers) {
        let apps = AppList {
            apps: vec![
                app("blog", "blog"),
                app("wiki", "notes"),
                app("db", "db"),
                app("new", "new"),
            ],
            ..AppList::default()
        };
        let configs = BTreeMap::from([
            (
                PathBuf::from("/srv/apps/blog"),
                config("blog", vec![service("web", &["80"], &[])]),
            ),
            (
                PathBuf::from("/srv/apps/wiki"),
                config("notes", vec![service("app", &["3000"], &[])]),
            ),
            (
                PathBuf::from("/srv/apps/db"),
                config("db", vec![service("postgres", &[], &[])]),
            ),
        ]);
        let mut containers = Containers::new();
        containers.replace_all([
            container("blog-web-1", "blog", "web", Some([10, 89, 0, 2])),
            container("notes-app-1", "notes", "app", None),
            container("stray", "other", "web", None),
        ]);
        (apps, configs, containers)
    }

    fn options(reachability: Reachability) -> RouteOptions {
        RouteOptions {
            reachability,
            suffixes: vec![HostSuffix::Localhost],
        }
    }

    #[test]
    fn inputs_cover_read_apps_with_their_containers_and_ports() {
        let (apps, configs, containers) = world();
        let reserved = BTreeMap::from([("blog".to_owned(), 8100)]);
        let inputs = route_inputs(&apps, &configs, &containers, &reserved);
        let summary: Vec<(String, Vec<String>, Option<u16>)> = inputs
            .iter()
            .map(|i| {
                (
                    i.app.clone(),
                    i.containers.iter().map(|c| c.name.clone()).collect(),
                    i.listen_port,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("blog".to_owned(), vec!["blog-web-1".to_owned()], Some(8100)),
                ("wiki".to_owned(), vec!["notes-app-1".to_owned()], None),
                ("db".to_owned(), vec![], None),
            ]
        );
        let blog = &inputs[0];
        assert_eq!(
            blog.containers[0].ips.get("alchemy"),
            Some(&IpAddr::V4(Ipv4Addr::new(10, 89, 0, 2)))
        );
        assert_eq!(blog.services[0].expose.len(), 1);
    }

    #[test]
    fn only_routable_apps_without_a_port_need_one() {
        let (apps, configs, containers) = world();
        let reserved = BTreeMap::from([("blog".to_owned(), 8100)]);
        let inputs = route_inputs(&apps, &configs, &containers, &reserved);
        let cases = [
            (Reachability::ContainerIp, vec!["wiki"]),
            (Reachability::PublishedOnly, vec![]),
        ];
        for (reach, want) in cases {
            assert_eq!(needs_port(&inputs, reach), want, "{reach:?}");
        }
    }

    #[test]
    fn attaches_running_routed_containers_not_on_the_network() {
        let (apps, configs, mut containers) = world();
        let mut stopped = container("notes-app-2", "notes", "app", None);
        stopped.state = ContainerState::Exited;
        let other_service = container("notes-cache-1", "notes", "cache", None);
        containers.replace_all(
            containers
                .iter()
                .cloned()
                .chain([stopped, other_service])
                .collect::<Vec<_>>(),
        );
        let inputs = route_inputs(&apps, &configs, &containers, &BTreeMap::new());
        let cases = [
            (Reachability::ContainerIp, vec!["notes-app-1"]),
            (Reachability::PublishedOnly, vec![]),
        ];
        for (reach, want) in cases {
            let plan = plan_routes(&inputs, &options(reach));
            assert_eq!(to_attach(&plan.table, &inputs, reach), want, "{reach:?}");
        }
    }

    #[test]
    fn due_skips_what_was_tried_within_the_cooldown() {
        let now = Instant::now();
        let cooldown = Duration::from_secs(10);
        let tried = BTreeMap::from([
            (
                "recent".to_owned(),
                now.checked_sub(Duration::from_secs(1)).unwrap(),
            ),
            (
                "old".to_owned(),
                now.checked_sub(Duration::from_secs(11)).unwrap(),
            ),
        ]);
        let wanted = vec!["recent".to_owned(), "old".to_owned(), "new".to_owned()];
        assert_eq!(due(wanted, &tried, now, cooldown), ["old", "new"]);
    }

    fn bind_error(port: u16, kind: io::ErrorKind) -> BindError {
        BindError {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            kind,
            message: "nope".into(),
        }
    }

    fn listening(port: u16) -> ListenerStatus {
        ListenerStatus::Listening(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
    }

    fn summarize(routing: &Routing) -> Vec<String> {
        routing
            .apps
            .iter()
            .flat_map(|(app, r)| {
                r.addresses
                    .iter()
                    .map(move |a| {
                        format!(
                            "{app} {:?} {} {:?} {}",
                            a.kind,
                            a.url,
                            a.state,
                            a.message.as_deref().unwrap_or("-")
                        )
                    })
                    .chain(
                        r.problems
                            .iter()
                            .map(move |p| format!("{app} problem {}", p.message)),
                    )
            })
            .collect()
    }

    #[test]
    fn addresses_say_whether_they_answer_and_why_not() {
        type Case = (&'static str, ProxyStatus, Vec<&'static str>);
        let (apps, configs, containers) = world();
        let reserved = BTreeMap::from([("blog".to_owned(), 8100), ("wiki".to_owned(), 8101)]);
        let inputs = route_inputs(&apps, &configs, &containers, &reserved);
        let plan = plan_routes(&inputs, &options(Reachability::ContainerIp));
        let addressing = Addressing {
            host_port: Some(80),
            app_host: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
        };
        let unreserved = BTreeMap::new();
        let base = |status: &ProxyStatus| {
            summarize(&routing(&RoutingInputs {
                plan: &plan,
                inputs: &inputs,
                reachability: Reachability::ContainerIp,
                unreserved: &unreserved,
                range: PortRange::new(8100, 8999).unwrap(),
                status,
                addressing,
            }))
        };
        let no_port = "db problem No service exposes or publishes a port, so Alchemy doesn't know where to send requests. Add an x-alchemy block with service and port.";
        let cases: [Case; 3] = [
            (
                "everything listening",
                ProxyStatus {
                    host: listening(80),
                    ports: BTreeMap::from([(8100, listening(8100)), (8101, listening(8101))]),
                },
                vec![
                    "blog Host http://blog.localhost/ Ready -",
                    "blog Port http://192.168.1.20:8100/ Ready -",
                    no_port,
                    "wiki Host http://wiki.localhost/ Pending Alchemy is connecting service app to the alchemy network.",
                    "wiki Port http://192.168.1.20:8101/ Pending Alchemy is connecting service app to the alchemy network.",
                ],
            ),
            (
                "port 80 refused, 8101 not open yet",
                ProxyStatus {
                    host: ListenerStatus::Unavailable(bind_error(
                        80,
                        io::ErrorKind::PermissionDenied,
                    )),
                    ports: BTreeMap::from([(8100, listening(8100))]),
                },
                vec![
                    "blog Port http://192.168.1.20:8100/ Ready -",
                    "blog Host http://blog.localhost/ Unavailable Alchemy doesn't have permission to listen on port 80.",
                    no_port,
                    "wiki Port http://192.168.1.20:8101/ Pending Alchemy is opening port 8101.",
                    "wiki Host http://wiki.localhost/ Unavailable Alchemy doesn't have permission to listen on port 80.",
                ],
            ),
            (
                "port taken",
                ProxyStatus {
                    host: listening(8080),
                    ports: BTreeMap::from([
                        (
                            8100,
                            ListenerStatus::Unavailable(bind_error(8100, io::ErrorKind::AddrInUse)),
                        ),
                        (8101, listening(8101)),
                    ]),
                },
                vec![
                    "blog Host http://blog.localhost:8080/ Ready -",
                    "blog Port http://192.168.1.20:8100/ Unavailable Another program is using port 8100.",
                    no_port,
                    "wiki Host http://wiki.localhost:8080/ Pending Alchemy is connecting service app to the alchemy network.",
                    "wiki Port http://192.168.1.20:8101/ Pending Alchemy is connecting service app to the alchemy network.",
                ],
            ),
        ];
        for (name, status, want) in cases {
            assert_eq!(base(&status), want, "{name}");
        }
    }

    #[test]
    fn stopped_apps_and_unpublished_ports_and_failed_reservations() {
        let (apps, configs, mut containers) = world();
        containers.replace_all([]);
        let inputs = route_inputs(&apps, &configs, &containers, &BTreeMap::new());
        let unreserved = BTreeMap::from([("wiki".to_owned(), "every port is taken".to_owned())]);
        let status = ProxyStatus {
            host: listening(80),
            ports: BTreeMap::new(),
        };
        let cases = [
            (
                Reachability::ContainerIp,
                vec![
                    "blog Host http://blog.localhost/ NotRunning Service web isn't running.",
                    "db problem No service exposes or publishes a port, so Alchemy doesn't know where to send requests. Add an x-alchemy block with service and port.",
                    "wiki Host http://wiki.localhost/ NotRunning Service app isn't running.",
                    "wiki problem Alchemy couldn't reserve a port between 8100 and 8999 for this app, so it has no address of its own: every port is taken.",
                ],
            ),
            (
                Reachability::PublishedOnly,
                vec![
                    "blog problem Publish port 80 of service web to reach this app. This Docker engine only lets the host reach published ports.",
                    "db problem No service exposes or publishes a port, so Alchemy doesn't know where to send requests. Add an x-alchemy block with service and port.",
                    "wiki problem Publish port 3000 of service app to reach this app. This Docker engine only lets the host reach published ports.",
                    "wiki problem Alchemy couldn't reserve a port between 8100 and 8999 for this app, so it has no address of its own: every port is taken.",
                ],
            ),
        ];
        for (reach, want) in cases {
            let plan = plan_routes(&inputs, &options(reach));
            let got = routing(&RoutingInputs {
                plan: &plan,
                inputs: &inputs,
                reachability: reach,
                unreserved: &unreserved,
                range: PortRange::new(8100, 8999).unwrap(),
                status: &status,
                addressing: Addressing {
                    host_port: Some(80),
                    app_host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                },
            });
            assert_eq!(summarize(&got), want, "{reach:?}");
        }
    }

    #[test]
    fn a_published_port_is_reached_on_loopback() {
        let (apps, mut configs, mut containers) = world();
        configs.insert(
            PathBuf::from("/srv/apps/blog"),
            config("blog", vec![service("web", &[], &[(8080, 80)])]),
        );
        let mut web = container("blog-web-1", "blog", "web", None);
        web.ports = vec![tcp(8080, 80)];
        containers.replace_all([web]);
        let reserved = BTreeMap::from([("blog".to_owned(), 8100)]);
        let inputs = route_inputs(&apps, &configs, &containers, &reserved);
        let plan = plan_routes(&inputs, &options(Reachability::PublishedOnly));
        let route = plan.table.get("blog").unwrap();
        assert_eq!(
            route.upstream.map(|u| u.addr()),
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
        );
    }

    #[test]
    fn x_alchemy_picks_the_service() {
        let (apps, mut configs, containers) = world();
        configs.insert(
            PathBuf::from("/srv/apps/wiki"),
            ConfigOutcome::Ready(ComposeConfig {
                name: "notes".into(),
                services: vec![
                    service("app", &["3000"], &[]),
                    service("cache", &["6379"], &[]),
                ],
                x_alchemy: Some(XAlchemyBlock::Read(XAlchemy {
                    service: Some("app".into()),
                    port: Some(3000),
                    hostname: Some("notes".into()),
                })),
                ..ComposeConfig::default()
            }),
        );
        let inputs = route_inputs(&apps, &configs, &containers, &BTreeMap::new());
        let plan = plan_routes(&inputs, &options(Reachability::ContainerIp));
        let route = plan.table.get("wiki").unwrap();
        assert_eq!((route.service.as_str(), route.port), ("app", 3000));
        assert_eq!(route.hosts, ["notes.localhost"]);
    }

    #[test]
    fn the_proxy_report_explains_port_80() {
        let plan = RoutePlan::default();
        let unreserved = BTreeMap::new();
        let cases = [
            (ListenerStatus::Off, ListenerState::Off, None, false),
            (listening(80), ListenerState::Listening, None, false),
            (
                ListenerStatus::Unavailable(bind_error(80, io::ErrorKind::PermissionDenied)),
                ListenerState::Unavailable,
                Some(
                    "Alchemy doesn't have permission to listen on port 80, so addresses like http://<app>.localhost don't work. Each app's own port still does.",
                ),
                true,
            ),
            (
                ListenerStatus::Unavailable(bind_error(80, io::ErrorKind::AddrInUse)),
                ListenerState::Unavailable,
                Some(
                    "Another program is using port 80, so addresses like http://<app>.localhost don't work. Each app's own port still does.",
                ),
                true,
            ),
            (
                ListenerStatus::Unavailable(bind_error(80, io::ErrorKind::Other)),
                ListenerState::Unavailable,
                Some(
                    "Alchemy can't listen on port 80 (nope), so addresses like http://<app>.localhost don't work. Each app's own port still does.",
                ),
                false,
            ),
        ];
        for (host, state, message, fix) in cases {
            let status = ProxyStatus {
                host,
                ports: BTreeMap::new(),
            };
            let report = routing(&RoutingInputs {
                plan: &plan,
                inputs: &[],
                reachability: Reachability::PublishedOnly,
                unreserved: &unreserved,
                range: PortRange::new(8100, 8999).unwrap(),
                status: &status,
                addressing: Addressing {
                    host_port: Some(80),
                    app_host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                },
            })
            .proxy;
            assert_eq!(report.host.port, 80);
            assert_eq!(report.host.state, state);
            assert_eq!(report.host.message.as_deref(), message);
            assert_eq!(report.host.fix.is_some(), fix, "{state:?}");
            assert_eq!(
                report.reachability,
                Some(alchemy_api::network::Reachability::PublishedOnly)
            );
        }
    }
}
