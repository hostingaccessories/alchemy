//! [`ApiNetworks`] and [`SystemDaemonConfig`]: the real engine and
//! `daemon.json`.

use std::fmt;

use futures_util::future::BoxFuture;

use super::{DaemonConfig, EngineNetworks, EngineView, SubnetsError};
use crate::engine::{Connector, DockerApi, Endpoint, NetworkSpec, SystemConnector};

/// [`EngineNetworks`] through the Engine API clients `connector` builds.
#[derive(Debug, Clone)]
pub struct ApiNetworks<C> {
    connector: C,
}

impl ApiNetworks<SystemConnector> {
    /// The engine's real API.
    pub fn system() -> Self {
        Self::new(SystemConnector)
    }
}

impl<C> ApiNetworks<C> {
    /// Networks read through clients from `connector`.
    pub fn new(connector: C) -> Self {
        Self { connector }
    }
}

impl<C> EngineNetworks for ApiNetworks<C>
where
    C: Connector + Send + Sync + fmt::Debug,
    C::Api: Send + Sync,
{
    /// The bridge and the pools are best effort: an engine with no
    /// default bridge, or one that hides its pools, reports none.
    fn read<'a>(&'a self, endpoint: &'a str) -> BoxFuture<'a, Result<EngineView, SubnetsError>> {
        Box::pin(async move {
            let api = self.connector.api(&Endpoint::parse(endpoint)?)?;
            let (networks, bridge, pools) = tokio::join!(
                api.list_networks(),
                api.bridge_subnets(),
                api.address_pools()
            );
            let bridge = bridge.unwrap_or_else(|error| {
                tracing::debug!(%error, "no default bridge network");
                Vec::new()
            });
            let pools = pools.unwrap_or_else(|error| {
                tracing::debug!(%error, "Docker's address pools unavailable");
                Vec::new()
            });
            Ok(EngineView {
                networks: networks?,
                bridge,
                pools,
            })
        })
    }

    fn create<'a>(
        &'a self,
        endpoint: &'a str,
        spec: NetworkSpec,
    ) -> BoxFuture<'a, Result<String, SubnetsError>> {
        Box::pin(async move {
            let api = self.connector.api(&Endpoint::parse(endpoint)?)?;
            Ok(api.create_network(spec).await?)
        })
    }
}

/// [`DaemonConfig`] for this host: `/etc/docker/daemon.json` on Linux.
///
/// macOS has none to read: Docker Desktop keeps its settings in its VM
/// and reports configured pools through the engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDaemonConfig;

impl DaemonConfig for SystemDaemonConfig {
    #[cfg(target_os = "linux")]
    fn daemon_json(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async {
            let path = super::report::DAEMON_JSON;
            match tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await {
                Ok(Ok(text)) => Some(text),
                Ok(Err(error)) => {
                    tracing::debug!(%error, path, "Docker's daemon.json not read");
                    None
                }
                Err(error) => {
                    tracing::debug!(%error, path, "reading Docker's daemon.json failed");
                    None
                }
            }
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn daemon_json(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(std::future::ready(None))
    }
}
