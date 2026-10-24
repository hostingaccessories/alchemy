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
    connected: Vec<(String, String)>,
    failing: bool,
    refuse_create: bool,
}

/// [`EngineNetworks`] over an engine held in memory.
///
/// It starts with no networks, no bridge, and no configured pools.
/// Creating a network adds it, so the next read sees it. Connecting to a
/// network it doesn't hold fails with `404`, as Docker does. Clones share the
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

    /// Every `(network, container)` pair connected so far, oldest first,
    /// each once.
    pub fn connected(&self) -> Vec<(String, String)> {
        self.lock().connected.clone()
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

    fn connect<'a>(
        &'a self,
        _endpoint: &'a str,
        network: &'a str,
        container: &'a str,
    ) -> BoxFuture<'a, Result<(), SubnetsError>> {
        let mut inner = self.lock();
        let answer = if inner.failing {
            Err(SubnetsError::Api(ApiError::Timeout))
        } else if !inner
            .view
            .networks
            .iter()
            .any(|n| n.name == network || n.id == network)
        {
            Err(SubnetsError::Api(ApiError::Server {
                status: 404,
                message: format!("network {network} not found"),
            }))
        } else {
            let pair = (network.to_owned(), container.to_owned());
            if !inner.connected.contains(&pair) {
                inner.connected.push(pair);
            }
            Ok(())
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const ENDPOINT: &str = "unix:///run/fake/docker.sock";

    #[tokio::test]
    async fn connects_containers_to_networks_it_holds() {
        let engine = FakeEngineNetworks::new();
        let missing = engine
            .connect(ENDPOINT, "alchemy", "web")
            .await
            .unwrap_err();
        assert!(
            matches!(
                missing,
                SubnetsError::Api(ApiError::Server { status: 404, .. })
            ),
            "{missing:?}"
        );
        let id = engine
            .create(
                ENDPOINT,
                NetworkSpec {
                    name: "alchemy".into(),
                    subnet: "10.200.0.0/24".parse().unwrap(),
                    labels: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
        engine.connect(ENDPOINT, "alchemy", "web").await.unwrap();
        engine.connect(ENDPOINT, "alchemy", "web").await.unwrap();
        engine.connect(ENDPOINT, &id, "db").await.unwrap();
        assert_eq!(
            engine.connected(),
            [
                ("alchemy".to_owned(), "web".to_owned()),
                (id, "db".to_owned())
            ]
        );

        engine.set_failing(true);
        assert!(engine.connect(ENDPOINT, "alchemy", "web").await.is_err());
    }
}
