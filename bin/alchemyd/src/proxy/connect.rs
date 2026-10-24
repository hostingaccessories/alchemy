//! Connecting to an app: the [`UpstreamConnector`] trait, and the pure
//! check that keeps the proxy from connecting to the daemon itself.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use alchemy_core::route::Upstream;
use futures_util::future::BoxFuture;
use tokio::net::TcpStream;

/// Opens the TCP connection a proxied request goes over.
///
/// Object safe, so the proxy can hold any implementation as a
/// [`SharedConnector`].
pub trait UpstreamConnector: Send + Sync + fmt::Debug {
    /// Connect to `addr`.
    fn connect(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>>;
}

/// An [`UpstreamConnector`] shared by every proxy connection.
pub type SharedConnector = Arc<dyn UpstreamConnector>;

/// Connects over this host's network.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpConnector;

impl UpstreamConnector for TcpConnector {
    fn connect(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>> {
        Box::pin(async move {
            let stream = TcpStream::connect(addr).await?;
            stream.set_nodelay(true)?;
            Ok(stream)
        })
    }
}

/// An [`UpstreamConnector`] for tests: connects to a real address only
/// for addresses it was told to redirect, such as a container IP to a
/// fake app on loopback, and records every address it was asked for.
/// Anything else is refused.
#[derive(Debug, Clone, Default)]
pub struct FakeConnector {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug, Default)]
struct FakeInner {
    redirects: HashMap<SocketAddr, SocketAddr>,
    attempts: Vec<SocketAddr>,
}

impl FakeConnector {
    /// A connector that refuses everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect to `to` whenever `from` is asked for.
    pub fn redirect(&self, from: SocketAddr, to: SocketAddr) {
        self.lock().redirects.insert(from, to);
    }

    /// Every address asked for so far, in order.
    pub fn attempts(&self) -> Vec<SocketAddr> {
        self.lock().attempts.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl UpstreamConnector for FakeConnector {
    fn connect(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>> {
        let target = {
            let mut inner = self.lock();
            inner.attempts.push(addr);
            inner.redirects.get(&addr).copied()
        };
        Box::pin(async move {
            match target {
                Some(to) => TcpStream::connect(to).await,
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("fake connector has no redirect for {addr}"),
                )),
            }
        })
    }
}

/// Whether connecting to `upstream` would reach one of `own`, the
/// daemon's and the proxy's own listening addresses.
///
/// A port on an unspecified address answers on every address of this
/// host, and loopback and unspecified destinations both reach this host.
/// A [`Upstream::ContainerIp`] is an address on a Docker network, never
/// one of the host's, so it matches only exactly or through loopback.
pub fn is_own_address(upstream: &Upstream, own: &[SocketAddr]) -> bool {
    let addr = upstream.addr();
    let ip = addr.ip().to_canonical();
    let local = ip.is_loopback() || ip.is_unspecified();
    let host_side = matches!(upstream, Upstream::Published(_)) || local;
    own.iter().filter(|o| o.port() == addr.port()).any(|o| {
        let own_ip = o.ip().to_canonical();
        ip == own_ip
            || (local && (own_ip.is_loopback() || own_ip.is_unspecified()))
            || (host_side && own_ip.is_unspecified())
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn own_address_table() {
        let own = [
            addr("127.0.0.1:7240"),
            addr("0.0.0.0:80"),
            addr("0.0.0.0:8100"),
            addr("192.168.1.5:9000"),
        ];
        let published = |s| Upstream::Published(addr(s));
        let container = |s| Upstream::ContainerIp(addr(s));
        let cases = [
            (published("127.0.0.1:7240"), true),
            (published("127.0.0.2:7240"), true),
            (published("[::1]:7240"), true),
            (published("[::ffff:127.0.0.1]:7240"), true),
            (published("0.0.0.0:7240"), true),
            (published("192.168.1.5:7240"), false),
            (published("127.0.0.1:7241"), false),
            (published("127.0.0.1:80"), true),
            (published("192.168.1.5:80"), true),
            (published("192.168.1.5:8100"), true),
            (published("192.168.1.5:9000"), true),
            (published("127.0.0.1:9000"), false),
            (published("127.0.0.1:3000"), false),
            (container("172.18.0.2:80"), false),
            (container("172.18.0.2:8100"), false),
            (container("172.18.0.2:7240"), false),
            (container("192.168.1.5:9000"), true),
            (container("127.0.0.1:7240"), true),
            (container("127.0.0.1:80"), true),
        ];
        for (upstream, want) in cases {
            assert_eq!(is_own_address(&upstream, &own), want, "{upstream:?}");
        }
    }

    #[test]
    fn nothing_is_own_with_no_listeners() {
        let upstream = Upstream::Published(addr("127.0.0.1:7240"));
        assert!(!is_own_address(&upstream, &[]));
    }

    #[tokio::test]
    async fn fake_connector_refuses_what_it_was_not_told() {
        let fake = FakeConnector::new();
        let err = fake.connect(addr("10.0.0.1:80")).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(fake.attempts(), [addr("10.0.0.1:80")]);
    }
}
