//! Routing apps through the web address [`proxy`](crate::proxy): which
//! app answers where, kept current as apps, their Compose files, their
//! containers, and the engine change.
//!
//! [`run`] plans the route table from the apps task's configurations
//! and the engine's containers, and hands it to the proxy. Along the
//! way it:
//!
//! - reserves a port for each routable app in the
//!   [`PortRegistry`] as `proxy:<app>`, from [`DEFAULT_RANGE`]. The
//!   reservation is never given back, so an app keeps its address;
//! - on engines whose container IPs the host can reach, attaches each
//!   routed container to the `alchemy` network through
//!   [`EngineNetworks::connect`], again whenever a container is
//!   recreated, without touching compose files;
//! - publishes each app's addresses and route problems, and the proxy's
//!   listeners, as a [`Routing`] in [`State`], for the app list and
//!   `GET /api/network`.
//!
//! While the `network.mdns` setting is on, hostnames are routed under
//! `.local` as well as `.localhost`, except for apps that set
//! `x-alchemy: { mdns: false }`. Each name [`desired`] for the LAN
//! address gets `.local` addresses, as far as the announcer's
//! [`MdnsStatus`] in [`State`] says it has got, and a name another device
//! answers for is an app problem.
//!
//! A proxy listener another program held is tried again every
//! [`ProxyDeps::interval`].
//!
//! A listener that opens isn't trusted yet: before its addresses are
//! ready, every listening port is sent a [`probe`](crate::proxy::probe)
//! through a [`ListenerProbe`] on loopback and the LAN address, and only
//! one whose probes all reach the proxy is ready. Where another program
//! answers instead, such as a `pf` redirect of port 80, its addresses are
//! `unavailable` and the next one is used. Every listener is checked
//! again each [`ProxyDeps::interval`].
//!
//! Its decisions are the pure functions in [`plan`] and
//! [`alchemy_core::route`].

pub mod plan;

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alchemy_core::engine::EngineState;
use alchemy_core::mdns::{LanIpv4, MdnsInputs, desired};
use alchemy_core::network::SHARED_NETWORK;
use alchemy_core::ports::{PortRange, Protocol};
use alchemy_core::route::{
    AppRouteInput, EngineFacts, Reachability, RouteOptions, RoutePlan, RouteTable, plan_routes,
    reachability,
};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

pub use plan::{AppRouting, MdnsStatus, Routing};

use self::plan::{Addressing, ListenerCheck, MdnsRouting, RoutingInputs};
use crate::State;
use crate::network::{SharedNetwork, preferred_lan_address};
use crate::ports::PortRegistry;
use crate::proxy::probe::classify;
use crate::proxy::{
    FakeConnector, HOST_PORT, Listen, ListenerStatus, ProbeMarker, ProxyConfig, ProxyStatus,
    SharedConnector, SharedProbe, TcpConnector, TcpProbe,
};
use crate::subnets::EngineNetworks;

/// The ports apps' own addresses are reserved from.
pub const DEFAULT_RANGE: (u16, u16) = (8100, 8999);

/// How often routes are checked again when nothing prompts it: the LAN
/// address is read again, failed reservations and attachments are
/// retried, and listeners that couldn't open are tried again.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// The owner name a proxy port is reserved under for `app`.
pub fn owner(app: &str) -> String {
    format!("proxy:{app}")
}

/// Where the proxy listens and how it connects to apps.
#[derive(Debug, Clone)]
pub struct ProxyDeps {
    /// How the proxy connects to an app.
    pub connector: SharedConnector,
    /// How the proxy's listeners are checked.
    pub probe: SharedProbe,
    /// The Host listener's address, or `None` to route by port only.
    pub host_listen: Option<SocketAddr>,
    /// The address app ports listen on.
    pub app_ip: IpAddr,
    /// The range app ports are reserved from.
    pub range: PortRange,
    /// How often [`run`] checks when nothing prompts it.
    pub interval: Duration,
}

impl ProxyDeps {
    /// This machine's proxy: Host routing on port 80 and app ports on
    /// every IPv4 address, from [`DEFAULT_RANGE`], connecting and
    /// checking listeners over TCP.
    pub fn system() -> Self {
        Self {
            connector: Arc::new(TcpConnector),
            probe: Arc::new(TcpProbe),
            host_listen: Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                HOST_PORT,
            )),
            app_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            range: default_range(),
            interval: DEFAULT_INTERVAL,
        }
    }

    /// A proxy for tests: app ports on loopback only, no Host listener,
    /// and a [`FakeConnector`] that reaches nothing until told to. Its
    /// listeners are checked for real, over loopback.
    pub fn loopback() -> Self {
        Self {
            connector: Arc::new(FakeConnector::new()),
            probe: Arc::new(TcpProbe),
            host_listen: None,
            app_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            range: default_range(),
            interval: DEFAULT_INTERVAL,
        }
    }

    /// Connect to apps through `connector` instead.
    #[must_use]
    pub fn with_connector(mut self, connector: SharedConnector) -> Self {
        self.connector = connector;
        self
    }

    /// Check listeners through `probe` instead.
    #[must_use]
    pub fn with_probe(mut self, probe: SharedProbe) -> Self {
        self.probe = probe;
        self
    }

    /// Reserve app ports from `range` instead.
    #[must_use]
    pub fn with_range(mut self, range: PortRange) -> Self {
        self.range = range;
        self
    }

    /// Route by `Host` on `addr` instead, or not at all with `None`.
    #[must_use]
    pub fn with_host_listen(mut self, addr: Option<SocketAddr>) -> Self {
        self.host_listen = addr;
        self
    }

    /// Check every `interval` instead.
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// The [`ProxyConfig`] for these settings, never routing to `own`.
    pub fn config(&self, own: Vec<SocketAddr>) -> ProxyConfig {
        ProxyConfig {
            host_listen: self.host_listen,
            app_ip: self.app_ip,
            own,
        }
    }
}

fn default_range() -> PortRange {
    PortRange::new(DEFAULT_RANGE.0, DEFAULT_RANGE.1).unwrap_or(PortRange::ALL)
}

/// The proxy's side of [`run`]: where to send the route table, and what
/// the proxy is listening on.
#[derive(Debug)]
pub struct ProxyHandle {
    /// The route table the proxy serves.
    pub routes: watch::Sender<RouteTable>,
    /// What the proxy is listening on.
    pub status: watch::Receiver<ProxyStatus>,
    /// The marker the proxy answers probes with, set only while a round
    /// of checks is in flight.
    pub probe: watch::Sender<Option<ProbeMarker>>,
}

/// Keep the proxy's route table and [`State::routing`] current until
/// `shutdown` flips to `true` or its sender is dropped.
///
/// It plans at start, whenever the apps, their configurations, the
/// containers, the engine, the network check, the network settings, the
/// mDNS announcer's status, or the proxy's listeners change, and every
/// [`ProxyDeps::interval`]. Nothing is routed until the
/// engine has been ready once, since how the proxy reaches containers
/// depends on it; after that the last engine's facts are kept while it
/// is away. Docker's networks are reached through `engine`, the LAN
/// address through `host`, and ports are reserved through `ports`.
pub async fn run(
    deps: ProxyDeps,
    engine: Arc<dyn EngineNetworks>,
    host: SharedNetwork,
    state: State,
    ports: PortRegistry,
    proxy: ProxyHandle,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut router = Router {
        addressing: Addressing {
            host_port: deps.host_listen.map(|a| a.port()),
            app_host: deps.app_ip,
        },
        deps,
        engine,
        host,
        state,
        ports,
        proxy,
        facts: None,
        reserved: BTreeMap::new(),
        unreserved: BTreeMap::new(),
        attached: BTreeMap::new(),
        lan: None,
        lan_ipv4: None,
        checks: BTreeMap::new(),
        recheck: false,
    };
    let mut apps = router.state.watch_apps();
    let mut configs = router.state.watch_app_configs();
    let mut containers = router.state.watch_containers();
    let mut facts = router.state.watch_engine_facts();
    let mut engine = router.state.watch_engine();
    let mut network = router.state.watch_network();
    let mut settings = router.state.watch_network_settings();
    let mut mdns = router.state.watch_mdns();
    let mut status = router.proxy.status.clone();
    let mut tick = tokio::time::interval(router.deps.interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await;
    router.on_tick().await;
    loop {
        apps.borrow_and_update();
        configs.borrow_and_update();
        containers.borrow_and_update();
        facts.borrow_and_update();
        engine.borrow_and_update();
        network.borrow_and_update();
        settings.borrow_and_update();
        mdns.borrow_and_update();
        status.borrow_and_update();
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => return,
            () = router.pass() => {}
        }
        let ticked = tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => return,
            Ok(()) = apps.changed() => false,
            Ok(()) = configs.changed() => false,
            Ok(()) = containers.changed() => false,
            Ok(()) = facts.changed() => false,
            Ok(()) = engine.changed() => false,
            Ok(()) = network.changed() => false,
            Ok(()) = settings.changed() => false,
            Ok(()) = mdns.changed() => false,
            Ok(()) = status.changed() => false,
            _ = tick.tick() => true,
        };
        if ticked {
            router.on_tick().await;
        }
    }
}

struct Router {
    deps: ProxyDeps,
    engine: Arc<dyn EngineNetworks>,
    host: SharedNetwork,
    state: State,
    ports: PortRegistry,
    proxy: ProxyHandle,
    addressing: Addressing,
    /// The last ready engine's facts.
    facts: Option<EngineFacts>,
    /// Each app's proxy port, by app name.
    reserved: BTreeMap<String, u16>,
    /// Apps whose port couldn't be reserved, with why and when.
    unreserved: BTreeMap<String, (String, Instant)>,
    /// When each container was last attached, by name.
    attached: BTreeMap<String, Instant>,
    /// The LAN address, when app ports listen on every address.
    lan: Option<Ipv4Addr>,
    /// The same, with its interface, for `.local` names.
    lan_ipv4: Option<LanIpv4>,
    /// Each listening listener's last check, with the addresses it
    /// probed.
    checks: BTreeMap<Listen, (Vec<SocketAddr>, ListenerCheck)>,
    /// Whether every listener is due a check, not just new ones.
    recheck: bool,
}

impl Router {
    /// Plan, reserve, attach, and publish.
    async fn pass(&mut self) {
        if let Some(facts) = self.state.engine_facts() {
            self.facts = Some(facts);
        }
        let settings = self.state.network_settings();
        let mdns = self.state.mdns();
        let Some(facts) = self.facts else {
            self.proxy.routes.send_if_modified(|table| {
                let changed = !table.is_empty();
                *table = RouteTable::default();
                changed
            });
            self.state.set_routing(Routing {
                mdns: plan::mdns_report(settings.mdns, &mdns.report),
                ..Routing::default()
            });
            return;
        };
        let reach = reachability(&facts);
        let apps = self.state.apps();
        let configs = self.state.app_configs();
        let containers = self.state.containers();
        let mut inputs = plan::route_inputs(&apps, &configs, &containers, &self.reserved);
        if self.reserve(&plan::needs_port(&inputs, reach)).await {
            inputs = plan::route_inputs(&apps, &configs, &containers, &self.reserved);
        }
        let plan = plan_routes(
            &inputs,
            &RouteOptions {
                reachability: reach,
                suffixes: plan::suffixes(settings.mdns),
            },
        );
        self.attach(plan::to_attach(&plan.table, &inputs, reach))
            .await;
        let status = self.check_listeners().await;
        let want = desired(&MdnsInputs {
            enabled: settings.mdns,
            routes: &plan.table,
            lan: self.lan_ipv4.as_ref(),
        });
        self.publish(
            &plan,
            &inputs,
            reach,
            &status,
            MdnsRouting {
                enabled: settings.mdns,
                desired: &want,
                status: &mdns,
            },
        );
    }

    /// Probe each listening listener that is new, moved, or due again,
    /// and return the listener state the checks are for. The proxy
    /// answers the probes only while this round's marker is set.
    async fn check_listeners(&mut self) -> ProxyStatus {
        let status = self.proxy.status.borrow().clone();
        let wanted = plan::listeners_to_probe(&status, self.lan);
        self.checks
            .retain(|listen, (targets, _)| wanted.get(listen) == Some(targets));
        let recheck = std::mem::take(&mut self.recheck);
        let due: Vec<(Listen, Vec<SocketAddr>)> = wanted
            .into_iter()
            .filter(|(listen, _)| recheck || !self.checks.contains_key(listen))
            .collect();
        if due.is_empty() {
            return status;
        }
        let marker = match ProbeMarker::generate() {
            Ok(marker) => marker,
            Err(error) => {
                tracing::warn!(%error, "cannot make a marker to check the proxy's listeners");
                return status;
            }
        };
        self.proxy.probe.send_replace(Some(marker.clone()));
        let probe = &self.deps.probe;
        let replies = futures_util::future::join_all(due.iter().flat_map(|(_, targets)| {
            targets
                .iter()
                .map(|&addr| probe.probe(addr, marker.clone()))
        }))
        .await;
        self.proxy.probe.send_replace(None);

        let mut replies = replies.into_iter();
        for (listen, targets) in due {
            let results: Vec<_> = targets
                .iter()
                .zip(replies.by_ref())
                .map(|(&addr, reply)| (addr, classify(&reply, &marker)))
                .collect();
            let check = plan::check_listener(&results);
            let before = self.checks.get(&listen).map(|(_, c)| c);
            if before != Some(&check) {
                if check == ListenerCheck::Ours {
                    tracing::debug!(?listen, "proxy listener checked");
                } else {
                    tracing::warn!(?listen, ?check, "proxy listener doesn't reach the proxy");
                }
            }
            self.checks.insert(listen, (targets, check));
        }
        status
    }

    /// Reserve a port for each of `apps`, skipping those that failed
    /// within the last interval. Returns whether any was reserved.
    async fn reserve(&mut self, apps: &[String]) -> bool {
        let now = Instant::now();
        let mut any = false;
        for app in apps {
            if let Some((_, at)) = self.unreserved.get(app)
                && now.saturating_duration_since(*at) < self.deps.interval
            {
                continue;
            }
            match self
                .ports
                .reserve(
                    &owner(app),
                    self.deps.range,
                    Protocol::Tcp,
                    None,
                    unix_now(),
                )
                .await
            {
                Ok(port) => {
                    self.unreserved.remove(app);
                    self.reserved.insert(app.clone(), port);
                    any = true;
                }
                Err(error) => {
                    tracing::warn!(app, %error, "cannot reserve a proxy port");
                    self.unreserved
                        .insert(app.clone(), (error.to_string(), Instant::now()));
                }
            }
        }
        any
    }

    /// Attach each of `containers` to the `alchemy` network, unless it was
    /// tried within the last interval.
    async fn attach(&mut self, containers: Vec<String>) {
        let now = Instant::now();
        self.attached
            .retain(|_, at| now.saturating_duration_since(*at) < self.deps.interval);
        let due = plan::due(containers, &self.attached, now, self.deps.interval);
        if due.is_empty() {
            return;
        }
        let engine = self.state.engine();
        let Some(endpoint) = engine
            .endpoint
            .filter(|_| engine.state == EngineState::Ready)
        else {
            return;
        };
        for container in due {
            self.attached.insert(container.clone(), Instant::now());
            match self
                .engine
                .connect(&endpoint, SHARED_NETWORK, &container)
                .await
            {
                Ok(()) => {
                    tracing::info!(container, "attached to the alchemy network");
                }
                Err(error) => {
                    tracing::warn!(container, %error, "cannot attach to the alchemy network");
                }
            }
        }
    }

    fn publish(
        &self,
        plan: &RoutePlan,
        inputs: &[AppRouteInput],
        reach: Reachability,
        status: &ProxyStatus,
        mdns: MdnsRouting<'_>,
    ) {
        let table = plan.table.clone();
        self.proxy.routes.send_if_modified(|current| {
            if *current == table {
                false
            } else {
                *current = table;
                true
            }
        });
        let checks: BTreeMap<Listen, ListenerCheck> = self
            .checks
            .iter()
            .map(|(listen, (_, check))| (*listen, check.clone()))
            .collect();
        let unreserved: BTreeMap<String, String> = self
            .unreserved
            .iter()
            .map(|(app, (why, _))| (app.clone(), why.clone()))
            .collect();
        self.state.set_routing(plan::routing(&RoutingInputs {
            plan,
            inputs,
            reachability: reach,
            unreserved: &unreserved,
            range: self.deps.range,
            status,
            checks: &checks,
            addressing: self.addressing,
            mdns,
        }));
    }

    /// Read the LAN address again, mark every listener due a check, and
    /// nudge the proxy to retry any listener another program held. One
    /// refused for lack of permission stays refused until the daemon
    /// restarts, so it waits for the next route change.
    async fn on_tick(&mut self) {
        self.recheck = true;
        if self.deps.app_ip.is_unspecified() {
            let (addresses, routes) = tokio::join!(self.host.lan_addresses(), self.host.routes());
            let (lan, lan_ipv4) = match (addresses, routes) {
                (Ok(addresses), Ok(routes)) => {
                    let lan = preferred_lan_address(&addresses, &routes);
                    (lan, plan::lan_ipv4(&addresses, lan))
                }
                (Err(error), _) | (_, Err(error)) => {
                    tracing::debug!(%error, "cannot read the LAN address");
                    (None, None)
                }
            };
            self.lan = lan;
            self.lan_ipv4 = lan_ipv4;
            self.addressing.app_host = IpAddr::V4(lan.unwrap_or(Ipv4Addr::LOCALHOST));
        }
        let retry = {
            let status = self.proxy.status.borrow();
            std::iter::once(&status.host)
                .chain(status.ports.values())
                .any(|s| {
                    matches!(s, ListenerStatus::Unavailable(e) if e.kind != io::ErrorKind::PermissionDenied)
                })
        };
        if retry {
            self.proxy.routes.send_modify(|_| {});
        }
    }
}

/// The wall clock in Unix seconds, for when a port was reserved.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}
