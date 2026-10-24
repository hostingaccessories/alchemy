//! [`FakeEngineNetworks`] and [`FakeDaemonConfig`]: an engine and a
//! `daemon.json` that answer as told.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use alchemy_core::network::Ipv4Net;
use futures_util::future::BoxFuture;

use super::{DaemonConfig, EngineNetworks, EngineView, SubnetsError};
use crate::engine::{AddressPool, ApiError, DockerNetwork, NetworkSpec};

#[derive(Debug, Default)]
struct Inner {
    view: EngineView,
    created: Vec<NetworkSpec>,
    failing: bool,
    refuse_create: bool,
}

/// [`EngineNetworks`] over an engine held in memory.
///
/// It starts with no networks, no bridge, and no configured pools.
/// Creating a network adds it, so the next read sees it. Clones share the
/// engine, so a test can change it while the daemon holds a clone.
#[derive(Debug, Clone, Default)]
pub struct FakeEngineNetworks {
    inner: Arc<Mutex<Inner>>,
}

impl FakeEngineNetworks {
    /// An engine with no networks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Report `networks` from now on.
    pub fn set_networks(&self, networks: Vec<DockerNetwork>) {
        self.lock().view.networks = networks;
    }

    /// Report `bridge` as the default bridge's subnets from now on.
    pub fn set_bridge(&self, bridge: Vec<Ipv4Net>) {
        self.lock().view.bridge = bridge;
    }

    /// Report `pools` as configured from now on.
    pub fn set_pools(&self, pools: Vec<AddressPool>) {
        self.lock().view.pools = pools;
    }

    /// Fail every read from now on when `failing`.
    pub fn set_failing(&self, failing: bool) {
        self.lock().failing = failing;
    }

    /// Refuse to create networks from now on when `refuse`.
    pub fn set_refuse_create(&self, refuse: bool) {
        self.lock().refuse_create = refuse;
    }

    /// Every network created so far, oldest first.
    pub fn created(&self) -> Vec<NetworkSpec> {
        self.lock().created.clone()
    }

    /// The networks the engine holds now.
    pub fn networks(&self) -> Vec<DockerNetwork> {
        self.lock().view.networks.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EngineNetworks for FakeEngineNetworks {
    fn read<'a>(&'a self, _endpoint: &'a str) -> BoxFuture<'a, Result<EngineView, SubnetsError>> {
        let inner = self.lock();
        let answer = if inner.failing {
            Err(SubnetsError::Api(ApiError::Timeout))
        } else {
            Ok(inner.view.clone())
        };
        Box::pin(std::future::ready(answer))
    }

    fn create<'a>(
        &'a self,
        _endpoint: &'a str,
        spec: NetworkSpec,
    ) -> BoxFuture<'a, Result<String, SubnetsError>> {
        let mut inner = self.lock();
        let answer = if inner.refuse_create || inner.failing {
            Err(SubnetsError::Api(ApiError::Server {
                status: 500,
                message: "the fake engine refuses".to_owned(),
            }))
        } else if inner.view.networks.iter().any(|n| n.name == spec.name) {
            Err(SubnetsError::Api(ApiError::Server {
                status: 409,
                message: format!("network with name {} already exists", spec.name),
            }))
        } else {
            let id = format!("fake-{}", inner.created.len() + 1);
            inner.view.networks.push(DockerNetwork {
                id: id.clone(),
                name: spec.name.clone(),
                driver: Some("bridge".to_owned()),
                subnets: vec![spec.subnet],
                labels: spec.labels.clone(),
                project: None,
            });
            inner.created.push(spec);
            Ok(id)
        };
        Box::pin(std::future::ready(answer))
    }
}

/// [`DaemonConfig`] holding a fixed `daemon.json`, or none.
#[derive(Debug, Clone, Default)]
pub struct FakeDaemonConfig {
    text: Option<String>,
}

impl FakeDaemonConfig {
    /// No `daemon.json`.
    pub fn new() -> Self {
        Self::default()
    }

    /// A `daemon.json` holding `text`.
    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
        }
    }
}

impl DaemonConfig for FakeDaemonConfig {
    fn daemon_json(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(std::future::ready(self.text.clone()))
    }
}
