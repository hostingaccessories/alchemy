//! A small IPv4 CIDR block type, so the core stays dependency-free.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// An IPv4 network in CIDR form, such as `10.0.1.0/24`.
///
/// The address is always the network address: host bits are cleared on
/// construction, so `10.0.1.5/24` and `10.0.1.0/24` are the same value.
/// Serializes as its CIDR string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Ipv4Net {
    network: Ipv4Addr,
    prefix: u8,
}

/// Why a candidate [`Ipv4Net`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Ipv4NetError {
    /// The text had no `/prefix` part.
    #[error("subnet {0:?} needs a prefix length, such as /24")]
    MissingPrefix(String),
    /// The address part was not an IPv4 address.
    #[error("{0:?} is not an IPv4 address")]
    InvalidAddress(String),
    /// The prefix part was not a number.
    #[error("{0:?} is not a prefix length")]
    InvalidPrefix(String),
    /// The prefix length was above 32.
    #[error("prefix length /{0} is longer than /32")]
    PrefixTooLong(u8),
    /// A netmask had a zero bit before a one bit, such as `255.0.255.0`.
    #[error("netmask {0} is not contiguous")]
    NonContiguousMask(Ipv4Addr),
    /// [`Ipv4Net::subnets`] was asked for blocks larger than the network.
    #[error("cannot split /{prefix} into /{new_prefix} blocks")]
    InvalidSplit {
        /// The network's own prefix length.
        prefix: u8,
        /// The requested prefix length of each block.
        new_prefix: u8,
    },
}

impl Ipv4Net {
    /// The network containing `addr` with the given prefix length.
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Result<Self, Ipv4NetError> {
        if prefix > 32 {
            return Err(Ipv4NetError::PrefixTooLong(prefix));
        }
        Ok(Self::clamped(addr, prefix))
    }

    /// Like [`new`](Self::new), with a prefix above 32 treated as 32, for
    /// built-in tables.
    pub(crate) fn clamped(addr: Ipv4Addr, prefix: u8) -> Self {
        let prefix = prefix.min(32);
        Self {
            network: Ipv4Addr::from_bits(addr.to_bits() & mask_bits(prefix)),
            prefix,
        }
    }

    /// The network containing `addr` under a dotted netmask, as routing
    /// tables report it (`255.255.255.0` is `/24`).
    pub fn from_netmask(addr: Ipv4Addr, netmask: Ipv4Addr) -> Result<Self, Ipv4NetError> {
        let bits = netmask.to_bits();
        let prefix = u8::try_from(bits.leading_ones()).unwrap_or(32);
        if mask_bits(prefix) != bits {
            return Err(Ipv4NetError::NonContiguousMask(netmask));
        }
        Self::new(addr, prefix)
    }

    /// The network address, with all host bits clear.
    pub fn network(&self) -> Ipv4Addr {
        self.network
    }

    /// The prefix length, from 0 to 32.
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// The netmask, such as `255.255.255.0` for a `/24`.
    pub fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from_bits(mask_bits(self.prefix))
    }

    /// The last address in the network (the broadcast address for most).
    pub fn last(&self) -> Ipv4Addr {
        Ipv4Addr::from_bits(self.network.to_bits() | !mask_bits(self.prefix))
    }

    /// How many addresses the network spans, including network and
    /// broadcast addresses.
    pub fn size(&self) -> u64 {
        1u64 << (32 - u32::from(self.prefix))
    }

    /// Whether this is `0.0.0.0/0`, the default route.
    pub fn is_default_route(&self) -> bool {
        self.prefix == 0
    }

    /// Whether `addr` falls inside the network.
    pub fn contains(&self, addr: Ipv4Addr) -> bool {
        addr.to_bits() & mask_bits(self.prefix) == self.network.to_bits()
    }

    /// Whether every address of `other` falls inside this network.
    pub fn contains_net(&self, other: &Ipv4Net) -> bool {
        self.prefix <= other.prefix && self.contains(other.network)
    }

    /// Whether the two networks share at least one address.
    ///
    /// CIDR blocks either nest or are disjoint, so this is containment in
    /// one direction or the other.
    pub fn overlaps(&self, other: &Ipv4Net) -> bool {
        self.contains_net(other) || other.contains_net(self)
    }

    /// Split the network into consecutive blocks of `new_prefix`, in
    /// address order. Asking for the network's own prefix yields itself.
    pub fn subnets(&self, new_prefix: u8) -> Result<Subnets, Ipv4NetError> {
        if new_prefix > 32 {
            return Err(Ipv4NetError::PrefixTooLong(new_prefix));
        }
        if new_prefix < self.prefix {
            return Err(Ipv4NetError::InvalidSplit {
                prefix: self.prefix,
                new_prefix,
            });
        }
        let start = u64::from(self.network.to_bits());
        Ok(Subnets {
            next: start,
            end: start + self.size(),
            step: 1u64 << (32 - u32::from(new_prefix)),
            prefix: new_prefix,
        })
    }
}

fn mask_bits(prefix: u8) -> u32 {
    u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0)
}

/// The blocks of a network split by [`Ipv4Net::subnets`].
#[derive(Debug, Clone)]
pub struct Subnets {
    next: u64,
    end: u64,
    step: u64,
    prefix: u8,
}

impl Iterator for Subnets {
    type Item = Ipv4Net;

    fn next(&mut self) -> Option<Ipv4Net> {
        if self.next >= self.end {
            return None;
        }
        let addr = u32::try_from(self.next).ok()?;
        self.next += self.step;
        Some(Ipv4Net {
            network: Ipv4Addr::from_bits(addr),
            prefix: self.prefix,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = usize::try_from(self.end.saturating_sub(self.next) / self.step).ok();
        (left.unwrap_or(usize::MAX), left)
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = Ipv4NetError;

    /// Parse `a.b.c.d/n`. Host bits are cleared, so an interface address
    /// such as `10.0.1.5/24` parses as its network.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| Ipv4NetError::MissingPrefix(s.to_owned()))?;
        let addr: Ipv4Addr = addr
            .parse()
            .map_err(|_| Ipv4NetError::InvalidAddress(addr.to_owned()))?;
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Ipv4NetError::InvalidPrefix(prefix.to_owned()));
        }
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| Ipv4NetError::InvalidPrefix(prefix.to_owned()))?;
        Self::new(addr, prefix)
    }
}

impl TryFrom<String> for Ipv4Net {
    type Error = Ipv4NetError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Ipv4Net> for String {
    fn from(net: Ipv4Net) -> Self {
        net.to_string()
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
    use super::*;

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_displays() {
        let cases = [
            ("10.0.1.0/24", "10.0.1.0/24"),
            ("10.0.1.5/24", "10.0.1.0/24"),
            ("172.17.0.0/16", "172.17.0.0/16"),
            ("192.168.1.5/16", "192.168.0.0/16"),
            ("0.0.0.0/0", "0.0.0.0/0"),
            ("8.8.8.8/0", "0.0.0.0/0"),
            ("10.0.0.1/32", "10.0.0.1/32"),
        ];
        for (input, want) in cases {
            assert_eq!(net(input).to_string(), want, "{input}");
        }
    }

    #[test]
    fn rejects_bad_text() {
        let cases = [
            ("10.0.1.0", Ipv4NetError::MissingPrefix("10.0.1.0".into())),
            ("10.0.1/24", Ipv4NetError::InvalidAddress("10.0.1".into())),
            ("::1/128", Ipv4NetError::InvalidAddress("::1".into())),
            ("10.0.1.0/", Ipv4NetError::InvalidPrefix(String::new())),
            ("10.0.1.0/+8", Ipv4NetError::InvalidPrefix("+8".into())),
            ("10.0.1.0/abc", Ipv4NetError::InvalidPrefix("abc".into())),
            ("10.0.1.0/300", Ipv4NetError::InvalidPrefix("300".into())),
            ("10.0.1.0/33", Ipv4NetError::PrefixTooLong(33)),
        ];
        for (input, want) in cases {
            assert_eq!(input.parse::<Ipv4Net>(), Err(want), "{input}");
        }
    }

    #[test]
    fn accessors() {
        let n = net("192.168.1.77/20");
        assert_eq!(n.network(), Ipv4Addr::new(192, 168, 0, 0));
        assert_eq!(n.prefix(), 20);
        assert_eq!(n.netmask(), Ipv4Addr::new(255, 255, 240, 0));
        assert_eq!(n.last(), Ipv4Addr::new(192, 168, 15, 255));
        assert_eq!(n.size(), 4096);
        assert_eq!(net("0.0.0.0/0").size(), 1 << 32);
        assert_eq!(net("0.0.0.0/0").netmask(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(net("0.0.0.0/0").last(), Ipv4Addr::BROADCAST);
        assert!(net("0.0.0.0/0").is_default_route());
        assert!(!n.is_default_route());
    }

    #[test]
    fn from_netmask() {
        let cases = [
            ("10.0.1.5", "255.255.255.0", Ok("10.0.1.0/24")),
            ("172.17.0.0", "255.255.0.0", Ok("172.17.0.0/16")),
            ("0.0.0.0", "0.0.0.0", Ok("0.0.0.0/0")),
            ("10.0.0.1", "255.255.255.255", Ok("10.0.0.1/32")),
            (
                "10.0.0.0",
                "255.0.255.0",
                Err(Ipv4NetError::NonContiguousMask(Ipv4Addr::new(
                    255, 0, 255, 0,
                ))),
            ),
        ];
        for (addr, mask, want) in cases {
            let got = Ipv4Net::from_netmask(addr.parse().unwrap(), mask.parse().unwrap());
            assert_eq!(got, want.map(net), "{addr} {mask}");
        }
    }

    #[test]
    fn contains_addresses() {
        let cases = [
            ("10.0.1.0/24", "10.0.1.0", true),
            ("10.0.1.0/24", "10.0.1.255", true),
            ("10.0.1.0/24", "10.0.2.0", false),
            ("172.17.0.0/16", "172.17.200.1", true),
            ("172.17.0.0/16", "172.18.0.1", false),
            ("192.168.0.0/16", "192.168.1.1", true),
            ("0.0.0.0/0", "1.2.3.4", true),
            ("10.0.0.1/32", "10.0.0.1", true),
            ("10.0.0.1/32", "10.0.0.2", false),
        ];
        for (n, addr, want) in cases {
            assert_eq!(net(n).contains(addr.parse().unwrap()), want, "{n} {addr}");
        }
    }

    #[test]
    fn overlaps_is_symmetric_and_nests() {
        let cases = [
            ("10.0.1.0/24", "10.0.1.0/24", true),
            ("10.0.0.0/8", "10.0.1.0/24", true),
            ("10.0.1.0/24", "10.0.2.0/24", false),
            ("172.17.0.0/16", "172.17.5.0/24", true),
            ("172.16.0.0/12", "172.31.0.0/16", true),
            ("172.16.0.0/12", "172.32.0.0/16", false),
            ("192.168.0.0/16", "192.168.240.0/20", true),
            ("192.168.0.0/20", "192.168.16.0/20", false),
            ("0.0.0.0/0", "8.8.8.8/32", true),
        ];
        for (a, b, want) in cases {
            assert_eq!(net(a).overlaps(&net(b)), want, "{a} {b}");
            assert_eq!(net(b).overlaps(&net(a)), want, "{b} {a}");
        }
        assert!(net("10.0.0.0/8").contains_net(&net("10.0.1.0/24")));
        assert!(!net("10.0.1.0/24").contains_net(&net("10.0.0.0/8")));
    }

    #[test]
    fn splits_into_blocks() {
        let blocks: Vec<String> = net("192.168.0.0/16")
            .subnets(20)
            .unwrap()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(blocks.len(), 16);
        assert_eq!(blocks[0], "192.168.0.0/20");
        assert_eq!(blocks[1], "192.168.16.0/20");
        assert_eq!(blocks[15], "192.168.240.0/20");

        let own: Vec<Ipv4Net> = net("10.0.1.0/24").subnets(24).unwrap().collect();
        assert_eq!(own, vec![net("10.0.1.0/24")]);

        let ends = net("255.255.255.0/24").subnets(32).unwrap();
        assert_eq!(ends.size_hint(), (256, Some(256)));
        assert_eq!(ends.last(), Some(net("255.255.255.255/32")));

        assert_eq!(
            net("10.0.1.0/24").subnets(16).unwrap_err(),
            Ipv4NetError::InvalidSplit {
                prefix: 24,
                new_prefix: 16
            }
        );
        assert_eq!(
            net("10.0.1.0/24").subnets(33).unwrap_err(),
            Ipv4NetError::PrefixTooLong(33)
        );
    }

    #[test]
    fn serde_round_trips_as_text() {
        let n = net("10.200.3.0/24");
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"10.200.3.0/24\"");
        assert_eq!(serde_json::from_str::<Ipv4Net>(&json).unwrap(), n);
        assert!(serde_json::from_str::<Ipv4Net>("\"10.0.0.0\"").is_err());
    }
}
