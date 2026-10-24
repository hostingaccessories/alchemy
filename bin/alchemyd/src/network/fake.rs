//! [`FakeNetwork`]: a [`HostNetwork`] that answers as told.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::Listener;
use futures_util::future::BoxFuture;

use super::{HostNetwork, LanAddress, NetworkError, Route};

#[derive(Debug, Default)]
struct Inner {
    lan_subnets: Vec<Ipv4Net>,
    lan_addresses: Vec<LanAddress>,
    routes: Vec<Route>,
    listeners: Vec<Listener>,
    failing: bool,
}

/// [`HostNetwork`] that answers with what it was given.
///
/// It starts with no subnets, addresses, routes, or listeners. Subnets
/// and addresses are set apart, so a test sets what it reads. Clones share their
/// answers, so a test can change them while the daemon holds a clone.
#[derive(Debug, Clone, Default)]
pub struct FakeNetwork {
    inner: Arc<Mutex<Inner>>,
}

impl FakeNetwork {
    /// A network with nothing on it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `lan_subnets` from now on.
    pub fn set_lan_subnets(&self, lan_subnets: Vec<Ipv4Net>) {
        self.lock().lan_subnets = lan_subnets;
    }

    /// Answer `lan_addresses` from now on.
    pub fn set_lan_addresses(&self, lan_addresses: Vec<LanAddress>) {
        self.lock().lan_addresses = lan_addresses;
    }

    /// Answer `routes` from now on.
    pub fn set_routes(&self, routes: Vec<Route>) {
        self.lock().routes = routes;
    }

    /// Answer `listeners` from now on.
    pub fn set_listeners(&self, listeners: Vec<Listener>) {
        self.lock().listeners = listeners;
    }

    /// Fail every read from now on when `failing`, as an unreadable
    /// `/proc` or a missing `netstat` would.
    pub fn set_failing(&self, failing: bool) {
        self.lock().failing = failing;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn answer<T>(&self, pick: impl FnOnce(&Inner) -> T) -> Result<T, NetworkError> {
        let inner = self.lock();
        if inner.failing {
            return Err(NetworkError::Task("the fake network is failing".into()));
        }
        Ok(pick(&inner))
    }
}

impl HostNetwork for FakeNetwork {
    fn lan_subnets(&self) -> BoxFuture<'_, Result<Vec<Ipv4Net>, NetworkError>> {
        let answer = self.answer(|inner| inner.lan_subnets.clone());
        Box::pin(async move { answer })
    }

    fn lan_addresses(&self) -> BoxFuture<'_, Result<Vec<LanAddress>, NetworkError>> {
        let answer = self.answer(|inner| inner.lan_addresses.clone());
        Box::pin(async move { answer })
    }

    fn routes(&self) -> BoxFuture<'_, Result<Vec<Route>, NetworkError>> {
        let answer = self.answer(|inner| inner.routes.clone());
        Box::pin(async move { answer })
    }

    fn listeners(&self) -> BoxFuture<'_, Result<Vec<Listener>, NetworkError>> {
        let answer = self.answer(|inner| inner.listeners.clone());
        Box::pin(async move { answer })
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
    use alchemy_core::ports::Protocol;

    use super::*;
    use crate::network::SharedNetwork;

    #[tokio::test]
    async fn answers_as_told_through_a_shared_handle() {
        let fake = FakeNetwork::new();
        let shared: SharedNetwork = Arc::new(fake.clone());
        assert_eq!(shared.lan_subnets().await.unwrap(), []);
        assert_eq!(shared.lan_addresses().await.unwrap(), []);
        assert_eq!(shared.routes().await.unwrap(), []);
        assert_eq!(shared.listeners().await.unwrap(), []);

        let lan: Ipv4Net = "10.0.1.0/24".parse().unwrap();
        let route = Route {
            destination: "100.64.0.0/10".parse().unwrap(),
            gateway: None,
            interface: "utun3".into(),
        };
        let listener = Listener {
            protocol: Protocol::Tcp,
            addr: "0.0.0.0".parse().unwrap(),
            port: 8080,
            process: Some("nginx".into()),
            pid: Some(42),
        };
        let address = LanAddress {
            interface: "en0".into(),
            addr: "10.0.1.50".parse().unwrap(),
            subnet: lan,
        };
        fake.set_lan_subnets(vec![lan]);
        fake.set_lan_addresses(vec![address.clone()]);
        fake.set_routes(vec![route.clone()]);
        fake.set_listeners(vec![listener.clone()]);
        assert_eq!(shared.lan_subnets().await.unwrap(), [lan]);
        assert_eq!(shared.lan_addresses().await.unwrap(), [address]);
        assert_eq!(shared.routes().await.unwrap(), [route]);
        assert_eq!(shared.listeners().await.unwrap(), [listener]);

        fake.set_failing(true);
        assert!(shared.lan_subnets().await.is_err());
        assert!(shared.lan_addresses().await.is_err());
        assert!(shared.routes().await.is_err());
        assert!(shared.listeners().await.is_err());
    }
}
