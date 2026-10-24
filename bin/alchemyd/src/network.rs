//! The host's own network as the OS sees it: LAN addresses and subnets,
//! routes, and listening sockets, none of which Docker knows about.
//!
//! Everything is read through a [`HostNetwork`]: [`SystemNetwork`] for this
//! host, [`FakeNetwork`] in tests. What the OS prints is parsed by pure
//! functions, [`proc`] for Linux's `/proc/net` and [`netstat`] and
//! [`lsof`] for macOS, which are compiled and tested on every OS.
//!
//! The network settings, such as `network.mdns`, are in [`settings`].

mod fake;
pub mod lsof;
pub mod netstat;
pub mod proc;
pub mod settings;
mod system;

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::Listener;
use futures_util::future::BoxFuture;

pub use fake::FakeNetwork;
pub use system::SystemNetwork;

use crate::metrics::counts_toward_total;

/// Reads the host's LAN addresses and subnets, routes, and listening
/// sockets.
///
/// Object safe, so the daemon can hold any implementation as a
/// [`SharedNetwork`]. Every call reads afresh; nothing is cached.
pub trait HostNetwork: Send + Sync + fmt::Debug {
    /// The IPv4 subnets of the physical interfaces, such as
    /// `10.0.1.0/24`. Loopback, link-local, and virtual interfaces
    /// (containers, VMs, VPNs) are left out.
    fn lan_subnets(&self) -> BoxFuture<'_, Result<Vec<Ipv4Net>, NetworkError>>;

    /// The host's own IPv4 addresses on the physical interfaces, with the
    /// subnet each is on: what another device on the LAN reaches it at.
    /// The same interfaces and addresses count as for
    /// [`lan_subnets`](Self::lan_subnets).
    fn lan_addresses(&self) -> BoxFuture<'_, Result<Vec<LanAddress>, NetworkError>>;

    /// The IPv4 routes of the main routing table, including the default
    /// route, VPN routes, and those of Docker's own bridges on Linux.
    fn routes(&self) -> BoxFuture<'_, Result<Vec<Route>, NetworkError>>;

    /// Listening TCP sockets and bound, unconnected UDP sockets, on IPv4
    /// and IPv6, each once.
    fn listeners(&self) -> BoxFuture<'_, Result<Vec<Listener>, NetworkError>>;
}

/// A [`HostNetwork`] shared by the daemon's tasks and routes.
pub type SharedNetwork = Arc<dyn HostNetwork>;

/// One of the host's own IPv4 addresses on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LanAddress {
    /// The interface it is on, such as `en0` or `eth0`.
    pub interface: String,
    /// The address, such as `10.0.1.50`.
    pub addr: Ipv4Addr,
    /// The subnet it is on, such as `10.0.1.0/24`.
    pub subnet: Ipv4Net,
}

/// One IPv4 route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Route {
    /// The destination, `0.0.0.0/0` for the default route.
    pub destination: Ipv4Net,
    /// The next hop, or `None` when the destination is on the link.
    pub gateway: Option<Ipv4Addr>,
    /// The interface the route goes out of, such as `eth0` or `utun3`.
    pub interface: String,
}

/// Why the host's network could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NetworkError {
    /// A file under `/proc` could not be read.
    #[error("cannot read {}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A command such as `netstat` could not be started.
    #[error("failed to run {}", binary.display())]
    Spawn {
        /// The binary that was run.
        binary: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A read did not finish in time. A command is killed.
    #[error("{what} did not finish within {after:?}")]
    Timeout {
        /// What was being read, such as `netstat -anv -p tcp`.
        what: String,
        /// How long it was given.
        after: Duration,
    },
    /// A command exited unsuccessfully.
    #[error("{} exited with {}: {stderr}", binary.display(), code.map_or_else(|| "a signal".to_owned(), |c| format!("status {c}")))]
    Failed {
        /// The binary that was run.
        binary: PathBuf,
        /// The exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// The start of its standard error.
        stderr: String,
    },
    /// A background read panicked or was cancelled.
    #[error("reading the host's network failed: {0}")]
    Task(String),
}

/// The LAN addresses among `addresses`, each an interface name, one of
/// its addresses, and that address's prefix length.
///
/// Only IPv4 addresses on interfaces that [`counts_toward_total`] count.
/// Loopback, link-local, and unspecified addresses and `/0` prefixes are
/// left out. The result is sorted by interface, then address, each once.
pub fn lan_addresses_from<'a>(
    addresses: impl IntoIterator<Item = (&'a str, IpAddr, u8)>,
) -> Vec<LanAddress> {
    let mut found: Vec<LanAddress> = addresses
        .into_iter()
        .filter(|(name, _, prefix)| *prefix > 0 && counts_toward_total(name))
        .filter_map(|(name, addr, prefix)| match addr {
            IpAddr::V4(addr)
                if !addr.is_loopback() && !addr.is_link_local() && !addr.is_unspecified() =>
            {
                Some(LanAddress {
                    interface: name.to_owned(),
                    addr,
                    subnet: Ipv4Net::new(addr, prefix).ok()?,
                })
            }
            IpAddr::V4(_) | IpAddr::V6(_) => None,
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// The subnets of `addresses` from [`lan_addresses_from`], sorted, each
/// once.
pub fn lan_subnets_from<'a>(
    addresses: impl IntoIterator<Item = (&'a str, IpAddr, u8)>,
) -> Vec<Ipv4Net> {
    let mut subnets: Vec<Ipv4Net> = lan_addresses_from(addresses)
        .into_iter()
        .map(|address| address.subnet)
        .collect();
    subnets.sort_unstable();
    subnets.dedup();
    subnets
}

/// The address to give other devices for this host: the one on the
/// interface the default route leaves through, preferring the one whose
/// subnet holds the gateway, or else the first.
///
/// A default route through an interface with no LAN address, such as a
/// VPN's `utun3`, says nothing about the LAN, so the first address is
/// used then too.
pub fn preferred_lan_address(addresses: &[LanAddress], routes: &[Route]) -> Option<Ipv4Addr> {
    let default = routes
        .iter()
        .filter(|route| route.destination.is_default_route())
        .find(|route| addresses.iter().any(|a| a.interface == route.interface));
    let on_default =
        |address: &&LanAddress| default.is_some_and(|route| address.interface == route.interface);
    let holds_gateway = |address: &&LanAddress| {
        default
            .and_then(|route| route.gateway)
            .is_some_and(|gateway| address.subnet.contains(gateway))
    };
    addresses
        .iter()
        .filter(on_default)
        .find(holds_gateway)
        .or_else(|| addresses.iter().find(on_default))
        .or_else(|| addresses.first())
        .map(|address| address.addr)
}

/// `listeners` with repeats removed, keeping the first of each.
///
/// Linux lists one socket per process sharing a port with `SO_REUSEPORT`,
/// and macOS repeats sockets in some states.
pub fn unique(listeners: Vec<Listener>) -> Vec<Listener> {
    let mut seen = HashSet::new();
    listeners
        .into_iter()
        .filter(|listener| seen.insert(listener.clone()))
        .collect()
}

/// `addr`, as IPv4 when it is an IPv4-mapped IPv6 address such as
/// `::ffff:127.0.0.1`: a dual-stack socket bound to one is bound to the
/// IPv4 address.
fn unmapped(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        IpAddr::V4(_) => addr,
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

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn lan_subnets_keep_physical_ipv4_networks() {
        type Case = (
            &'static str,
            Vec<(&'static str, &'static str, u8)>,
            Vec<&'static str>,
        );
        let cases: [Case; 6] = [
            (
                "home Mac",
                vec![
                    ("lo0", "127.0.0.1", 8),
                    ("lo0", "::1", 128),
                    ("en0", "10.0.1.50", 24),
                    ("en0", "fe80::1", 64),
                    ("utun3", "100.101.102.103", 32),
                    ("bridge100", "192.168.64.1", 24),
                    ("awdl0", "fe80::2", 64),
                ],
                vec!["10.0.1.0/24"],
            ),
            (
                "Linux server with Docker",
                vec![
                    ("lo", "127.0.0.1", 8),
                    ("eth0", "192.168.1.5", 16),
                    ("docker0", "172.17.0.1", 16),
                    ("br-0123abcd", "172.18.0.1", 16),
                    ("veth1a2b3c", "169.254.1.1", 16),
                    ("tailscale0", "100.64.0.7", 32),
                    ("wg0", "10.8.0.1", 24),
                ],
                vec!["192.168.0.0/16"],
            ),
            (
                "two interfaces on one LAN, a second LAN",
                vec![
                    ("eth0", "172.17.0.5", 16),
                    ("wlan0", "172.17.0.6", 16),
                    ("enp3s0", "10.0.1.5", 24),
                ],
                vec!["10.0.1.0/24", "172.17.0.0/16"],
            ),
            (
                "link-local only",
                vec![("en0", "169.254.10.20", 16)],
                vec![],
            ),
            (
                "unspecified and /0",
                vec![("en0", "0.0.0.0", 8), ("en1", "10.0.0.5", 0)],
                vec![],
            ),
            ("nothing", vec![], vec![]),
        ];
        for (name, addresses, want) in cases {
            let got = lan_subnets_from(
                addresses
                    .iter()
                    .map(|(iface, addr, prefix)| (*iface, ip(addr), *prefix)),
            );
            let want: Vec<Ipv4Net> = want.iter().map(|s| s.parse().unwrap()).collect();
            assert_eq!(got, want, "{name}");
        }
    }

    fn lan(interface: &str, addr: &str, subnet: &str) -> LanAddress {
        LanAddress {
            interface: interface.into(),
            addr: addr.parse().unwrap(),
            subnet: subnet.parse().unwrap(),
        }
    }

    #[test]
    fn lan_addresses_keep_the_hosts_own_address() {
        let got = lan_addresses_from(
            [
                ("lo0", "127.0.0.1", 8),
                ("en0", "10.0.1.50", 24),
                ("en0", "fe80::1", 64),
                ("utun3", "100.101.102.103", 32),
                ("docker0", "172.17.0.1", 16),
                ("wlan0", "192.168.1.9", 16),
                ("en0", "10.0.1.50", 24),
                ("en0", "10.0.1.7", 24),
                ("en1", "169.254.3.4", 16),
                ("en2", "10.9.0.5", 0),
            ]
            .iter()
            .map(|(iface, addr, prefix)| (*iface, ip(addr), *prefix)),
        );
        assert_eq!(
            got,
            [
                lan("en0", "10.0.1.7", "10.0.1.0/24"),
                lan("en0", "10.0.1.50", "10.0.1.0/24"),
                lan("wlan0", "192.168.1.9", "192.168.0.0/16"),
            ]
        );
    }

    #[test]
    fn preferred_lan_address_follows_the_default_route() {
        type Case = (
            &'static str,
            Vec<LanAddress>,
            Vec<Route>,
            Option<&'static str>,
        );
        let route = |destination: &str, gateway: Option<&str>, interface: &str| Route {
            destination: destination.parse().unwrap(),
            gateway: gateway.map(|g| g.parse().unwrap()),
            interface: interface.into(),
        };
        let wired = lan("en0", "10.0.1.50", "10.0.1.0/24");
        let wired_alias = lan("en0", "10.0.9.2", "10.0.9.0/24");
        let wifi = lan("en1", "192.168.1.9", "192.168.1.0/24");
        let cases: [Case; 6] = [
            (
                "the default route's interface",
                vec![wired.clone(), wifi.clone()],
                vec![route("0.0.0.0/0", Some("192.168.1.1"), "en1")],
                Some("192.168.1.9"),
            ),
            (
                "the address whose subnet holds the gateway",
                vec![wired.clone(), wired_alias.clone()],
                vec![route("0.0.0.0/0", Some("10.0.9.1"), "en0")],
                Some("10.0.9.2"),
            ),
            (
                "a default route through a VPN",
                vec![wired.clone(), wifi.clone()],
                vec![
                    route("0.0.0.0/0", None, "utun3"),
                    route("10.0.1.0/24", None, "en0"),
                ],
                Some("10.0.1.50"),
            ),
            (
                "a VPN's default route, then the LAN's",
                vec![wired.clone(), wifi.clone()],
                vec![
                    route("0.0.0.0/0", None, "utun3"),
                    route("0.0.0.0/0", Some("192.168.1.1"), "en1"),
                ],
                Some("192.168.1.9"),
            ),
            ("no routes", vec![wifi.clone()], vec![], Some("192.168.1.9")),
            (
                "no addresses",
                vec![],
                vec![route("0.0.0.0/0", Some("10.0.1.1"), "en0")],
                None,
            ),
        ];
        for (name, addresses, routes, want) in cases {
            assert_eq!(
                preferred_lan_address(&addresses, &routes),
                want.map(|w| w.parse().unwrap()),
                "{name}"
            );
        }
    }

    #[test]
    fn unique_keeps_the_first_of_each() {
        let listener = |port, pid| Listener {
            protocol: Protocol::Tcp,
            addr: ip("0.0.0.0"),
            port,
            process: None,
            pid,
        };
        let got = unique(vec![
            listener(80, None),
            listener(443, None),
            listener(80, None),
            listener(80, Some(7)),
        ]);
        assert_eq!(
            got,
            [
                listener(80, None),
                listener(443, None),
                listener(80, Some(7))
            ]
        );
    }

    #[test]
    fn unmapped_turns_mapped_addresses_into_ipv4() {
        let cases = [
            ("::ffff:127.0.0.1", "127.0.0.1"),
            ("::ffff:10.0.1.5", "10.0.1.5"),
            ("::", "::"),
            ("::1", "::1"),
            ("fe80::1", "fe80::1"),
            ("10.0.1.5", "10.0.1.5"),
        ];
        for (addr, want) in cases {
            assert_eq!(unmapped(ip(addr)), ip(want), "{addr}");
        }
    }
}
