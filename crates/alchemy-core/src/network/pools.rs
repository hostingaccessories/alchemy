//! Docker's address pools and the subnet Alchemy picks for its own network.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use super::{Ipv4Net, Ipv4NetError};

/// A range Docker carves network subnets from, as in the
/// `default-address-pools` setting of `daemon.json`.
///
/// Serializes as `{"base": "172.17.0.0/16", "size": 16}`, the same shape
/// as `daemon.json`, and validates on deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawPool")]
pub struct AddressPool {
    base: Ipv4Net,
    size: u8,
}

#[derive(Deserialize)]
struct RawPool {
    base: Ipv4Net,
    size: u8,
}

impl TryFrom<RawPool> for AddressPool {
    type Error = Ipv4NetError;

    fn try_from(raw: RawPool) -> Result<Self, Self::Error> {
        Self::new(raw.base, raw.size)
    }
}

impl AddressPool {
    /// A pool handing out `/size` subnets of `base`.
    pub fn new(base: Ipv4Net, size: u8) -> Result<Self, Ipv4NetError> {
        base.subnets(size)?;
        Ok(Self { base, size })
    }

    /// The range subnets come from.
    pub fn base(&self) -> Ipv4Net {
        self.base
    }

    /// The prefix length of each subnet handed out.
    pub fn size(&self) -> u8 {
        self.size
    }
}

/// The subnet of Docker's default `bridge` network (`docker0`),
/// `172.17.0.0/16`, unless `bip` in `daemon.json` moves it.
pub fn default_bridge() -> Ipv4Net {
    net(172, 17, 0, 0, 16)
}

/// Docker's documented default address pools: `172.17.0.0/16` through
/// `172.31.0.0/16` as whole /16s, then `192.168.0.0/16` in /20s.
pub fn default_address_pools() -> Vec<AddressPool> {
    (17..=31)
        .map(|octet| pool(net(172, octet, 0, 0, 16), 16))
        .chain([pool(net(192, 168, 0, 0, 16), 20)])
        .collect()
}

/// The range Alchemy tries after Docker's default pools are used up,
/// `10.200.0.0/16`.
pub fn fallback_range() -> Ipv4Net {
    net(10, 200, 0, 0, 16)
}

/// The prefix length of the subnet Alchemy picks for its own network.
pub const PICKED_PREFIX: u8 = 24;

/// Every subnet [`pick_subnet`] considers, in order: each /24 of Docker's
/// default pools, then each /24 of [`fallback_range`].
pub fn subnet_candidates() -> impl Iterator<Item = Ipv4Net> {
    default_address_pools()
        .into_iter()
        .map(|p| p.base())
        .chain([fallback_range()])
        .flat_map(|range| range.subnets(PICKED_PREFIX).into_iter().flatten())
}

/// The first of [`subnet_candidates`] that overlaps nothing in `taken`.
///
/// `taken` holds LAN subnets, routes, and existing Docker network
/// subnets. Default routes (`/0`) are ignored, because they overlap
/// everything. `None` when every candidate is taken.
pub fn pick_subnet(taken: &[Ipv4Net]) -> Option<Ipv4Net> {
    let taken: Vec<&Ipv4Net> = taken.iter().filter(|t| !t.is_default_route()).collect();
    subnet_candidates().find(|candidate| !taken.iter().any(|t| t.overlaps(candidate)))
}

fn net(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> Ipv4Net {
    Ipv4Net::clamped(Ipv4Addr::new(a, b, c, d), prefix)
}

/// A pool from the built-in table, where `size` is known to fit `base`.
fn pool(base: Ipv4Net, size: u8) -> AddressPool {
    AddressPool { base, size }
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

    fn n(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    #[test]
    fn default_pool_table() {
        let pools = default_address_pools();
        assert_eq!(pools.len(), 16);
        assert_eq!(pools[0].base(), n("172.17.0.0/16"));
        assert_eq!(pools[0].size(), 16);
        assert_eq!(pools[14].base(), n("172.31.0.0/16"));
        assert_eq!(pools[15].base(), n("192.168.0.0/16"));
        assert_eq!(pools[15].size(), 20);
        assert_eq!(default_bridge(), n("172.17.0.0/16"));
    }

    #[test]
    fn pool_validation_and_serde() {
        assert_eq!(
            AddressPool::new(n("10.0.0.0/16"), 8),
            Err(Ipv4NetError::InvalidSplit {
                prefix: 16,
                new_prefix: 8
            })
        );
        let p: AddressPool = serde_json::from_str(r#"{"base":"10.10.0.0/16","size":24}"#).unwrap();
        assert_eq!((p.base(), p.size()), (n("10.10.0.0/16"), 24));
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"base":"10.10.0.0/16","size":24}"#
        );
        assert!(
            serde_json::from_str::<AddressPool>(r#"{"base":"10.10.0.0/16","size":8}"#).is_err()
        );
    }

    #[test]
    fn candidates_cover_pools_then_fallback() {
        let all: Vec<Ipv4Net> = subnet_candidates().collect();
        assert_eq!(all.len(), 15 * 256 + 256 + 256);
        assert_eq!(all[0], n("172.17.0.0/24"));
        assert_eq!(all[256], n("172.18.0.0/24"));
        assert_eq!(all[15 * 256], n("192.168.0.0/24"));
        assert_eq!(all[16 * 256], n("10.200.0.0/24"));
        assert_eq!(all.last(), Some(&n("10.200.255.0/24")));
    }

    #[test]
    fn picks_first_free_subnet() {
        let bridge = "172.17.0.0/16";
        let cases: [(&str, &[&str], Option<&str>); 9] = [
            ("empty host", &[], Some("172.17.0.0/24")),
            ("only the bridge", &[bridge], Some("172.18.0.0/24")),
            (
                "LAN 10.0.1.0/24",
                &["10.0.1.0/24", bridge, "0.0.0.0/0"],
                Some("172.18.0.0/24"),
            ),
            (
                "LAN 172.17.0.0/16 with the bridge moved",
                &["172.17.0.0/16", "172.18.0.0/16"],
                Some("172.19.0.0/24"),
            ),
            (
                "LAN 192.168.0.0/16",
                &["192.168.0.0/16", bridge, "0.0.0.0/0"],
                Some("172.18.0.0/24"),
            ),
            (
                "Compose networks fill some /24s",
                &[bridge, "172.18.0.0/24", "172.18.1.0/24"],
                Some("172.18.2.0/24"),
            ),
            (
                "VPN route over all of 172.16/12",
                &[bridge, "172.16.0.0/12", "192.168.0.0/16"],
                Some("10.200.0.0/24"),
            ),
            (
                "everything taken",
                &["172.16.0.0/12", "192.168.0.0/16", "10.0.0.0/8"],
                None,
            ),
            (
                "host route inside a candidate",
                &["172.17.0.5/32"],
                Some("172.17.1.0/24"),
            ),
        ];
        for (name, taken, want) in cases {
            let taken: Vec<Ipv4Net> = taken.iter().map(|t| n(t)).collect();
            assert_eq!(pick_subnet(&taken), want.map(n), "{name}");
        }
    }
}
