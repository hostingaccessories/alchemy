//! Docker's subnets against the home network: Alchemy's own `alchemy`
//! network, and warnings when Docker's bridge, address pools, or networks
//! overlap the LAN or a route.
//!
//! [`run`] keeps a [`NetworkReport`] in [`State`] current. While the
//! engine is ready, it makes sure the `alchemy` network exists, creating
//! it on a free subnet that is remembered in the settings, so it comes
//! back on the same subnet if Docker loses it. An `alchemy` network that
//! already exists is never changed, even when it overlaps: containers may
//! be attached to it. Users' compose files are never touched.
//!
//! The engine is read through [`EngineNetworks`] and Docker's
//! `daemon.json` through [`DaemonConfig`]; everything else is decided by
//! the pure functions in [`report`] and [`alchemy_core::network`].

mod fake;
pub mod report;
mod system;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use alchemy_api::network::{NetworkReport, PoolsSource};
use alchemy_core::engine::EngineState;
use alchemy_core::network::{
    Ipv4Net, MANAGED_LABEL, SHARED_NETWORK, SharedNetworkPlan, plan_shared_network,
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

pub use fake::{FakeDaemonConfig, FakeEngineNetworks};
pub use system::{ApiNetworks, SystemDaemonConfig};

use self::report::{AlchemyOutcome, EngineFindings, EngineSide, Findings};
use crate::State;
use crate::engine::{AddressPool, ApiError, DockerNetwork, EndpointError, NetworkSpec};
use crate::network::SharedNetwork;
use crate::store::SharedStore;

/// The setting that remembers the `alchemy` network's subnet.
pub const SETTINGS_KEY: &str = "network.alchemy";

/// How often the networks are checked again when nothing prompts it.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// How long to wait after the engine or the containers change before
/// checking, so a burst of container events leads to one check.
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(2);

/// Reads and creates networks on the engine at an endpoint.
///
/// Object safe, so the daemon can hold any implementation in
/// [`SubnetsDeps`].
pub trait EngineNetworks: Send + Sync + fmt::Debug {
    /// The engine's networks, default bridge subnet, and configured
    /// address pools.
    fn read<'a>(&'a self, endpoint: &'a str) -> BoxFuture<'a, Result<EngineView, SubnetsError>>;

    /// Create the bridge network `spec` describes, returning its ID.
    fn create<'a>(
        &'a self,
        endpoint: &'a str,
        spec: NetworkSpec,
    ) -> BoxFuture<'a, Result<String, SubnetsError>>;
}

/// What [`EngineNetworks::read`] reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineView {
    /// Every network.
    pub networks: Vec<DockerNetwork>,
    /// The default bridge's IPv4 subnets, empty when there is no bridge.
    pub bridge: Vec<Ipv4Net>,
    /// The configured pools, empty when Docker's defaults are in use.
    pub pools: Vec<AddressPool>,
}

/// Reads Docker's `daemon.json`.
pub trait DaemonConfig: Send + Sync + fmt::Debug {
    /// The file's contents, or `None` when there is none, it cannot be
    /// read, or this OS keeps it out of reach, as Docker Desktop does.
    fn daemon_json(&self) -> BoxFuture<'_, Option<String>>;
}

/// Why the engine's networks could not be read or created.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubnetsError {
    /// The engine's endpoint is not one Alchemy can use.
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    /// The engine refused or did not answer.
    #[error(transparent)]
    Api(#[from] ApiError),
}

/// What [`run`] reads the engine and its settings through, and how often.
#[derive(Debug, Clone)]
pub struct SubnetsDeps {
    /// The engine's networks.
    pub engine: Arc<dyn EngineNetworks>,
    /// Docker's `daemon.json`.
    pub daemon_config: Arc<dyn DaemonConfig>,
    /// How often to check when nothing prompts it.
    pub interval: Duration,
    /// How long to wait after a change before checking.
    pub settle: Duration,
}

impl SubnetsDeps {
    /// The real engine and, on Linux, `/etc/docker/daemon.json`.
    pub fn system() -> Self {
        Self::new(
            Arc::new(ApiNetworks::system()),
            Arc::new(SystemDaemonConfig),
        )
    }

    /// These effects, checked every [`DEFAULT_INTERVAL`] and
    /// [`DEFAULT_SETTLE`] after a change.
    pub fn new(engine: Arc<dyn EngineNetworks>, daemon_config: Arc<dyn DaemonConfig>) -> Self {
        Self {
            engine,
            daemon_config,
            interval: DEFAULT_INTERVAL,
            settle: DEFAULT_SETTLE,
        }
    }

    /// Wait `settle` after a change instead.
    #[must_use]
    pub fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }
}

/// The `alchemy` network's subnet, as stored under [`SETTINGS_KEY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    subnet: Ipv4Net,
}

/// Keep the [`NetworkReport`] in `state` current until `shutdown` flips
/// to `true` or its sender is dropped.
///
/// It checks at start, [`SubnetsDeps::settle`] after the engine status or
/// the containers change, and every [`SubnetsDeps::interval`]. Each check
/// reads the host's subnets and routes from `host` and, while the engine
/// is ready, Docker's networks, bridge, and pools. A missing `alchemy`
/// network is created then, so it appears when the engine becomes ready
/// and comes back if it is removed. Its subnet is stored in `store`.
/// Each new warning is logged once.
pub async fn run(
    deps: SubnetsDeps,
    host: SharedNetwork,
    state: State,
    store: SharedStore,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut checker = Checker::new(deps, host, state, store).await;
    let mut engine = checker.state.watch_engine();
    let mut containers = checker.state.watch_containers();
    let mut tick = tokio::time::interval(checker.deps.interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        engine.borrow_and_update();
        containers.borrow_and_update();
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => return,
            () = checker.check() => {}
        }
        let changed = tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => return,
            Ok(()) = engine.changed() => true,
            Ok(()) = containers.changed() => true,
            _ = tick.tick() => false,
        };
        if changed {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => return,
                () = tokio::time::sleep(checker.deps.settle) => {}
            }
        }
    }
}

struct Checker {
    deps: SubnetsDeps,
    host: SharedNetwork,
    state: State,
    store: SharedStore,
    saved: Option<Ipv4Net>,
}

impl Checker {
    async fn new(deps: SubnetsDeps, host: SharedNetwork, state: State, store: SharedStore) -> Self {
        let saved = match store.setting::<Saved>(SETTINGS_KEY).await {
            Ok(saved) => saved.map(|s| s.subnet),
            Err(error) => {
                tracing::warn!(%error, "cannot read the alchemy network's subnet; picking a new one");
                None
            }
        };
        Self {
            deps,
            host,
            state,
            store,
            saved,
        }
    }

    async fn check(&mut self) {
        let engine = self.state.engine();
        let (lan, routes, host_error) = self.read_host().await;
        let side = match (engine.state, engine.endpoint.as_deref()) {
            (EngineState::Ready, Some(endpoint)) => self.read_engine(endpoint, &lan, &routes).await,
            _ => EngineSide::NotReady,
        };
        let report = report::report(&Findings {
            flavor: engine.flavor,
            lan,
            routes,
            host_error,
            engine: side,
        });
        let before = self.state.network();
        log_new_warnings(&before, &report);
        self.state.set_network(report);
    }

    async fn read_host(&self) -> (Vec<Ipv4Net>, Vec<Ipv4Net>, Option<String>) {
        let (lan, routes) = tokio::join!(self.host.lan_subnets(), self.host.routes());
        match (lan, routes) {
            (Ok(lan), Ok(routes)) => (
                lan,
                routes.into_iter().map(|r| r.destination).collect(),
                None,
            ),
            (Err(error), _) | (_, Err(error)) => (Vec::new(), Vec::new(), Some(error.to_string())),
        }
    }

    async fn read_engine(
        &mut self,
        endpoint: &str,
        lan: &[Ipv4Net],
        routes: &[Ipv4Net],
    ) -> EngineSide {
        let view = match self.deps.engine.read(endpoint).await {
            Ok(view) => view,
            Err(error) => return EngineSide::Unreadable(error.to_string()),
        };
        let daemon_json = if view.pools.is_empty() {
            self.deps.daemon_config.daemon_json().await
        } else {
            None
        };
        let (pools, pools_source) = report::effective_pools(&view.pools, daemon_json.as_deref());
        let bridge = view.bridge.first().copied();
        let mut networks = view.networks;
        let survey = report::survey(lan, routes, bridge, &pools, &networks);
        let exists = networks.iter().any(|n| n.name == SHARED_NETWORK);
        let alchemy = match plan_shared_network(&survey, exists, self.saved) {
            SharedNetworkPlan::Keep { subnet } => {
                if let Some(subnet) = subnet {
                    self.remember(subnet).await;
                }
                AlchemyOutcome::Ready(subnet)
            }
            SharedNetworkPlan::Overlapping { subnet } => AlchemyOutcome::Overlapping(subnet),
            SharedNetworkPlan::NoFreeSubnet => AlchemyOutcome::NoFreeSubnet,
            SharedNetworkPlan::Create { subnet } => match self.create(endpoint, subnet).await {
                Ok(network) => {
                    networks.push(network);
                    self.remember(subnet).await;
                    AlchemyOutcome::Ready(Some(subnet))
                }
                Err(error) => AlchemyOutcome::CreateFailed {
                    subnet,
                    error: error.to_string(),
                },
            },
        };
        EngineSide::Read(EngineFindings {
            networks,
            bridge,
            pools,
            pools_source,
            alchemy,
        })
    }

    async fn create(&self, endpoint: &str, subnet: Ipv4Net) -> Result<DockerNetwork, SubnetsError> {
        let labels = BTreeMap::from([(MANAGED_LABEL.to_owned(), "true".to_owned())]);
        let spec = NetworkSpec {
            name: SHARED_NETWORK.to_owned(),
            subnet,
            labels: labels.clone(),
        };
        let id = self.deps.engine.create(endpoint, spec).await?;
        tracing::info!(%subnet, "created the alchemy network");
        Ok(DockerNetwork {
            id,
            name: SHARED_NETWORK.to_owned(),
            driver: Some("bridge".to_owned()),
            subnets: vec![subnet],
            labels,
            project: None,
        })
    }

    async fn remember(&mut self, subnet: Ipv4Net) {
        if self.saved == Some(subnet) {
            return;
        }
        match self
            .store
            .set_setting(SETTINGS_KEY, &Saved { subnet })
            .await
        {
            Ok(()) => self.saved = Some(subnet),
            Err(error) => {
                tracing::warn!(%error, %subnet, "cannot remember the alchemy network's subnet");
            }
        }
    }
}

fn log_new_warnings(before: &NetworkReport, after: &NetworkReport) {
    let seen: BTreeSet<&str> = before.warnings.iter().map(|w| w.message.as_str()).collect();
    for warning in after
        .warnings
        .iter()
        .filter(|w| !seen.contains(w.message.as_str()))
    {
        tracing::warn!(kind = ?warning.kind, "{}", warning.message);
    }
    if after.pools_source == PoolsSource::DaemonJson
        && before.pools_source != PoolsSource::DaemonJson
    {
        tracing::debug!("address pools read from {}", report::DAEMON_JSON);
    }
}
