//! The web address proxy: an HTTP/1.1 reverse proxy inside the daemon
//! that gives every routed app a web address.
//!
//! [`run`] keeps a set of listeners in step with a
//! [`RouteTable`] it is fed over a `watch` channel:
//!
//! - one per app port, routed by [`RouteTable::lookup_port`], and
//! - the Host listener, usually port 80, routed by the `Host` header with
//!   [`RouteTable::lookup_host`], open while any route has a hostname.
//!
//! These listeners serve the proxy alone and never the API router. Each
//! request uses the latest table; a request or WebSocket already in
//! flight keeps the app it started with. A port that leaves the table
//! stops accepting, and its open connections carry on until they end.
//! A listener that can't bind, such as port 80 without privilege on
//! Linux, is reported in the [`ProxyStatus`] and retried with the next
//! table, while the others keep serving.
//!
//! Bodies stream both ways with no buffering or size limit. The pure
//! header rules are in [`headers`]; connecting goes through an
//! [`UpstreamConnector`], which is never asked for the daemon's own
//! addresses or the proxy's ([`is_own_address`]).

mod connect;
mod forward;
pub mod headers;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use alchemy_core::route::RouteTable;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

pub use connect::{
    FakeConnector, SharedConnector, TcpConnector, UpstreamConnector, is_own_address,
};
pub use forward::{CONNECT_TIMEOUT, ProxyBody};

use forward::Shared;

/// The port Host routing listens on.
pub const HOST_PORT: u16 = 80;

/// How long a client may take to send a request's head.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long shutdown waits for open connections to finish before it
/// drops them.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Where the proxy listens and what it must never connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// The Host listener's address, or `None` to route by port only.
    pub host_listen: Option<SocketAddr>,
    /// The address app ports listen on.
    pub app_ip: IpAddr,
    /// The daemon's own listening addresses, such as the API's, which no
    /// route may point at. The proxy's own are added to these.
    pub own: Vec<SocketAddr>,
}

impl ProxyConfig {
    /// The daemon's proxy: Host routing on port 80 and app ports on
    /// every IPv4 address, so `http://<LAN-IP>:<port>` works, never
    /// routing to `api`, the daemon's API listener.
    pub fn system(api: SocketAddr) -> Self {
        Self {
            host_listen: Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                HOST_PORT,
            )),
            app_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            own: vec![api],
        }
    }
}

/// One proxy listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Listen {
    /// The Host listener, routed by the `Host` header.
    Host,
    /// An app port, routed by the port.
    App(u16),
}

/// Why a listener couldn't be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindError {
    /// The address it tried.
    pub addr: SocketAddr,
    /// The kind of failure: [`io::ErrorKind::PermissionDenied`] for a
    /// privileged port, [`io::ErrorKind::AddrInUse`] when it's taken.
    pub kind: io::ErrorKind,
    /// The OS's message.
    pub message: String,
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot listen on {}: {}", self.addr, self.message)
    }
}

/// The state of one listener.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ListenerStatus {
    /// Not wanted: no route needs it, or it is turned off.
    #[default]
    Off,
    /// Accepting connections on this address.
    Listening(SocketAddr),
    /// Wanted but couldn't be opened; it is tried again with the next
    /// route table.
    Unavailable(BindError),
}

/// What the proxy is listening on, published by [`run`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyStatus {
    /// The Host listener.
    pub host: ListenerStatus,
    /// Each app port the route table names, listening or unavailable.
    pub ports: BTreeMap<u16, ListenerStatus>,
}

impl ProxyStatus {
    /// The status of `listen`.
    pub fn get(&self, listen: Listen) -> ListenerStatus {
        match listen {
            Listen::Host => self.host.clone(),
            Listen::App(port) => self.ports.get(&port).cloned().unwrap_or_default(),
        }
    }

    /// Every address the proxy is listening on.
    pub fn listening(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        std::iter::once(&self.host)
            .chain(self.ports.values())
            .filter_map(|s| match s {
                ListenerStatus::Listening(addr) => Some(*addr),
                ListenerStatus::Off | ListenerStatus::Unavailable(_) => None,
            })
    }

    fn set(&mut self, listen: Listen, status: ListenerStatus) {
        match listen {
            Listen::Host => self.host = status,
            Listen::App(port) => {
                if status == ListenerStatus::Off {
                    self.ports.remove(&port);
                } else {
                    self.ports.insert(port, status);
                }
            }
        }
    }
}

/// The listeners `table` needs: one per app port, and the Host listener
/// when `host` is on and any route has a hostname.
pub fn wanted_listeners(table: &RouteTable, host: bool) -> BTreeSet<Listen> {
    let mut wanted: BTreeSet<Listen> = table.listen_ports().map(Listen::App).collect();
    if host && table.routes().iter().any(|r| !r.hosts.is_empty()) {
        wanted.insert(Listen::Host);
    }
    wanted
}

/// Serve the proxy until `shutdown` turns true, following the route
/// table in `routes` and publishing listener state to `status`.
///
/// With an empty table nothing is bound. If the sender of `routes` is
/// dropped the last table keeps serving. On shutdown every listener
/// closes, open connections finish their current request, and upgraded
/// connections close; anything still open after [`DRAIN_TIMEOUT`] is
/// left to the runtime.
pub async fn run(
    config: ProxyConfig,
    connector: SharedConnector,
    mut routes: watch::Receiver<RouteTable>,
    status: watch::Sender<ProxyStatus>,
    mut shutdown: watch::Receiver<bool>,
) {
    let (drain, mut drained) = mpsc::channel::<()>(1);
    let shared = Arc::new(Shared {
        routes: routes.clone(),
        status: status.subscribe(),
        connector,
        own: config.own.clone(),
        shutdown: shutdown.clone(),
    });
    let mut listeners = Listeners::default();
    loop {
        let wanted = wanted_listeners(&routes.borrow_and_update(), config.host_listen.is_some());
        listeners
            .reconcile(&wanted, &config, &shared, &status, &drain)
            .await;
        tokio::select! {
            changed = routes.changed() => {
                if changed.is_err() {
                    stopped(&mut shutdown).await;
                    break;
                }
            }
            () = stopped(&mut shutdown) => break,
        }
    }

    listeners.close_all().await;
    status.send_replace(ProxyStatus::default());
    drop(drain);
    if tokio::time::timeout(DRAIN_TIMEOUT, drained.recv())
        .await
        .is_err()
    {
        tracing::warn!("proxy connections still open at shutdown; dropping them");
    }
}

/// The open listeners, each with the sender that stops its accept loop
/// when dropped.
#[derive(Debug, Default)]
struct Listeners {
    open: BTreeMap<Listen, (oneshot::Sender<()>, JoinHandle<()>)>,
}

impl Listeners {
    /// Close what is open but not `wanted`, then open what is wanted but
    /// not open, publishing each change.
    async fn reconcile(
        &mut self,
        wanted: &BTreeSet<Listen>,
        config: &ProxyConfig,
        shared: &Arc<Shared>,
        status: &watch::Sender<ProxyStatus>,
        drain: &mpsc::Sender<()>,
    ) {
        let closing: Vec<Listen> = self
            .open
            .keys()
            .filter(|l| !wanted.contains(l))
            .copied()
            .collect();
        for listen in closing {
            if let Some((stop, task)) = self.open.remove(&listen) {
                drop(stop);
                let _ = task.await;
            }
            status.send_modify(|s| s.set(listen, ListenerStatus::Off));
            tracing::debug!(?listen, "proxy listener closed");
        }
        status.send_modify(|s| {
            if !wanted.contains(&Listen::Host) {
                s.set(Listen::Host, ListenerStatus::Off);
            }
            s.ports.retain(|p, _| wanted.contains(&Listen::App(*p)));
        });

        for &listen in wanted {
            if self.open.contains_key(&listen) {
                continue;
            }
            let Some(addr) = listen_addr(listen, config) else {
                continue;
            };
            match TcpListener::bind(addr).await {
                Ok(listener) => {
                    let bound = listener.local_addr().unwrap_or(addr);
                    let (stop, stopped) = oneshot::channel();
                    let task = tokio::spawn(accept_loop(
                        listener,
                        listen,
                        shared.clone(),
                        stopped,
                        drain.clone(),
                    ));
                    self.open.insert(listen, (stop, task));
                    status.send_modify(|s| s.set(listen, ListenerStatus::Listening(bound)));
                    tracing::info!(?listen, addr = %bound, "proxy listening");
                }
                Err(error) => {
                    let failure = BindError {
                        addr,
                        kind: error.kind(),
                        message: error.to_string(),
                    };
                    let again =
                        status.borrow().get(listen) == ListenerStatus::Unavailable(failure.clone());
                    if again {
                        tracing::debug!(?listen, %failure, "proxy listener still unavailable");
                    } else {
                        tracing::warn!(?listen, %failure, "proxy listener unavailable");
                    }
                    status.send_modify(|s| s.set(listen, ListenerStatus::Unavailable(failure)));
                }
            }
        }
    }

    async fn close_all(&mut self) {
        for (_, (stop, task)) in std::mem::take(&mut self.open) {
            drop(stop);
            let _ = task.await;
        }
    }
}

/// Where `listen` binds under `config`, or `None` for the Host listener
/// when it is off.
fn listen_addr(listen: Listen, config: &ProxyConfig) -> Option<SocketAddr> {
    match listen {
        Listen::Host => config.host_listen,
        Listen::App(port) => Some(SocketAddr::new(config.app_ip, port)),
    }
}

/// Accept connections on `listener` until `stop` fires or is dropped,
/// or the daemon shuts down. Connections outlive it.
async fn accept_loop(
    listener: TcpListener,
    listen: Listen,
    shared: Arc<Shared>,
    mut stop: oneshot::Receiver<()>,
    drain: mpsc::Sender<()>,
) {
    let mut shutdown = shared.shutdown.clone();
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::debug!(?listen, %error, "proxy accept failed");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
            _ = &mut stop => break,
            () = stopped(&mut shutdown) => break,
        };
        tokio::spawn(serve_connection(
            stream,
            peer,
            listen,
            shared.clone(),
            drain.clone(),
        ));
    }
}

/// Serve HTTP/1.1 on one client connection until it closes. On shutdown
/// it finishes the request in flight, then closes.
async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    listen: Listen,
    shared: Arc<Shared>,
    drain: mpsc::Sender<()>,
) {
    let _ = stream.set_nodelay(true);
    let mut shutdown = shared.shutdown.clone();
    let tunnel_drain = drain.clone();
    let service = service_fn(move |req| {
        forward::handle(shared.clone(), listen, peer, tunnel_drain.clone(), req)
    });
    let mut conn = pin!(
        hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
    );
    let result = tokio::select! {
        result = conn.as_mut() => result,
        () = stopped(&mut shutdown) => {
            conn.as_mut().graceful_shutdown();
            conn.await
        }
    };
    if let Err(error) = result {
        tracing::debug!(%peer, %error, "proxy connection ended");
    }
    drop(drain);
}

/// Resolve once `shutdown` turns true or its sender is gone.
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use std::collections::BTreeMap;

    use alchemy_core::route::{
        AppRouteInput, HostSuffix, Reachability, RouteContainer, RouteOptions, ServiceRouteInput,
        XAlchemy, XAlchemyBlock, plan_routes,
    };

    use super::*;

    fn app(name: &str, hostname: Option<&str>, listen_port: Option<u16>) -> AppRouteInput {
        AppRouteInput {
            app: name.into(),
            x_alchemy: Some(XAlchemyBlock::Read(XAlchemy {
                service: Some("web".into()),
                port: Some(80),
                hostname: hostname.map(Into::into),
                ..XAlchemy::default()
            })),
            services: vec![ServiceRouteInput {
                name: "web".into(),
                ..ServiceRouteInput::default()
            }],
            containers: vec![RouteContainer {
                name: format!("{name}-web-1"),
                service: "web".into(),
                running: true,
                ips: BTreeMap::new(),
                ports: Vec::new(),
            }],
            listen_port,
        }
    }

    fn table(apps: &[AppRouteInput], suffixes: Vec<HostSuffix>) -> RouteTable {
        plan_routes(
            apps,
            &RouteOptions {
                reachability: Reachability::ContainerIp,
                suffixes,
            },
        )
        .table
    }

    #[test]
    fn wanted_listeners_table() {
        let localhost = || vec![HostSuffix::Localhost];
        let cases = [
            ("empty", RouteTable::default(), true, vec![]),
            (
                "ports and host",
                table(
                    &[app("blog", None, Some(8101)), app("wiki", None, Some(8100))],
                    localhost(),
                ),
                true,
                vec![Listen::Host, Listen::App(8100), Listen::App(8101)],
            ),
            (
                "host off",
                table(&[app("blog", None, Some(8101))], localhost()),
                false,
                vec![Listen::App(8101)],
            ),
            (
                "no suffixes means no hostnames",
                table(&[app("blog", None, Some(8101))], vec![]),
                true,
                vec![Listen::App(8101)],
            ),
            (
                "hostname but no port yet",
                table(&[app("blog", None, None)], localhost()),
                true,
                vec![Listen::Host],
            ),
        ];
        for (name, table, host, want) in cases {
            let got: Vec<Listen> = wanted_listeners(&table, host).into_iter().collect();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn status_set_and_listening() {
        let addr: SocketAddr = "0.0.0.0:8100".parse().unwrap();
        let mut status = ProxyStatus::default();
        status.set(Listen::App(8100), ListenerStatus::Listening(addr));
        status.set(
            Listen::App(8101),
            ListenerStatus::Unavailable(BindError {
                addr,
                kind: io::ErrorKind::AddrInUse,
                message: "in use".into(),
            }),
        );
        assert_eq!(status.listening().collect::<Vec<_>>(), [addr]);
        assert_eq!(status.get(Listen::Host), ListenerStatus::Off);
        status.set(Listen::App(8100), ListenerStatus::Off);
        assert!(!status.ports.contains_key(&8100));
        assert_eq!(status.get(Listen::App(8100)), ListenerStatus::Off);
    }

    #[test]
    fn system_config_never_routes_to_the_api() {
        let api: SocketAddr = "127.0.0.1:7240".parse().unwrap();
        let config = ProxyConfig::system(api);
        assert_eq!(config.own, [api]);
        assert_eq!(config.host_listen.map(|a| a.port()), Some(HOST_PORT));
    }
}
