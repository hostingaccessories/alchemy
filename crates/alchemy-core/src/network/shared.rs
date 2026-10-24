//! Alchemy's own shared network, and the `daemon.json` change that moves
//! Docker's networks off the home network.

use std::fmt::Write as _;
use std::net::Ipv4Addr;

use super::{AddressPool, Ipv4Net, NetworkSurvey, Subject, fallback_range, pick_subnet};

/// The name of the network Alchemy creates for itself.
pub const SHARED_NETWORK: &str = "alchemy";

/// The label, set to `true`, on networks Alchemy created.
pub const MANAGED_LABEL: &str = "com.alchemy.managed";

/// The prefix length of the pool [`suggest_daemon_json`] offers in place
/// of Docker's defaults: 256 networks of 254 addresses each.
pub const SUGGESTED_POOL_SIZE: u8 = 24;

/// What to do about the [`SHARED_NETWORK`], from [`plan_shared_network`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SharedNetworkPlan {
    /// It exists and overlaps nothing on the host. `subnet` is its first
    /// IPv4 subnet, to remember, if it has one.
    Keep {
        /// Its subnet.
        subnet: Option<Ipv4Net>,
    },
    /// It exists on a subnet that overlaps the LAN or a route. It is left
    /// alone, since containers may be attached to it.
    Overlapping {
        /// Its overlapping subnet.
        subnet: Ipv4Net,
    },
    /// It does not exist: create it on `subnet`.
    Create {
        /// The subnet to create it on.
        subnet: Ipv4Net,
    },
    /// It does not exist, and every candidate subnet is taken.
    NoFreeSubnet,
}

/// What to do about the [`SHARED_NETWORK`] on the host `survey` describes.
///
/// `exists` is whether the engine has a network of that name; its subnets
/// are among `survey.networks`. A missing network is created on `saved`,
/// the subnet it had before, when that still overlaps nothing: the LAN,
/// a route, the bridge, or another Docker network. Otherwise it gets the
/// first free subnet from [`pick_subnet`].
pub fn plan_shared_network(
    survey: &NetworkSurvey,
    exists: bool,
    saved: Option<Ipv4Net>,
) -> SharedNetworkPlan {
    if exists {
        let overlapping = survey
            .collisions()
            .into_iter()
            .find_map(|c| match c.subject {
                Subject::Network { name } if name == SHARED_NETWORK => Some(c.subnet),
                _ => None,
            });
        if let Some(subnet) = overlapping {
            return SharedNetworkPlan::Overlapping { subnet };
        }
        let subnet = survey
            .networks
            .iter()
            .find(|n| n.name == SHARED_NETWORK)
            .map(|n| n.subnet);
        return SharedNetworkPlan::Keep { subnet };
    }
    let taken = taken(survey);
    let free = |subnet: &Ipv4Net| !taken.iter().any(|t| t.overlaps(subnet));
    match saved.filter(free).or_else(|| pick_subnet(&taken)) {
        Some(subnet) => SharedNetworkPlan::Create { subnet },
        None => SharedNetworkPlan::NoFreeSubnet,
    }
}

/// Every subnet in use on the host: the LAN, routes other than the
/// default, the bridge, and every Docker network.
fn taken(survey: &NetworkSurvey) -> Vec<Ipv4Net> {
    survey
        .lan
        .iter()
        .chain(&survey.routes)
        .chain(&survey.bridge)
        .copied()
        .chain(survey.networks.iter().map(|n| n.subnet))
        .filter(|net| !net.is_default_route())
        .collect()
}

/// Settings for Docker's `daemon.json` that move the default bridge and
/// the address pools off the host's networks, from [`suggest_daemon_json`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DaemonJsonFix {
    /// A new subnet for the default bridge (`bip`), when it overlaps.
    pub bip: Option<Ipv4Net>,
    /// New `default-address-pools`, when a pool overlaps.
    pub pools: Option<Vec<AddressPool>>,
}

impl DaemonJsonFix {
    /// The settings as a `daemon.json` fragment, to merge into the file.
    ///
    /// `bip` is written as the bridge's gateway address, the first in its
    /// subnet, as Docker expects.
    pub fn to_json(&self) -> String {
        let mut entries = Vec::new();
        if let Some(bip) = self.bip {
            let gateway = Ipv4Addr::from(u32::from(bip.network()).saturating_add(1));
            entries.push(format!("  \"bip\": \"{gateway}/{}\"", bip.prefix()));
        }
        if let Some(pools) = &self.pools {
            let mut entry = String::from("  \"default-address-pools\": [\n");
            for (index, pool) in pools.iter().enumerate() {
                let comma = if index + 1 < pools.len() { "," } else { "" };
                let _ = writeln!(
                    entry,
                    "    {{ \"base\": \"{}\", \"size\": {} }}{comma}",
                    pool.base(),
                    pool.size()
                );
            }
            entry.push_str("  ]");
            entries.push(entry);
        }
        format!("{{\n{}\n}}", entries.join(",\n"))
    }
}

/// The `daemon.json` settings that stop the default bridge and Docker's
/// address pools from overlapping the LAN or a route, or `None` when
/// neither does.
///
/// Overlapping pools are replaced by one pool of `/24`s carved from
/// [`fallback_range`] when that is free, and otherwise by the pools that
/// do not overlap. An overlapping bridge moves to the first free subnet
/// [`pick_subnet`] finds. Either is left out when no free choice exists.
pub fn suggest_daemon_json(survey: &NetworkSurvey) -> Option<DaemonJsonFix> {
    let collisions = survey.collisions();
    let bridge_hit = collisions.iter().any(|c| c.subject == Subject::Bridge);
    let pool_hit = |pool: &AddressPool| {
        collisions
            .iter()
            .any(|c| c.subject == Subject::Pool { pool: *pool })
    };
    let any_pool_hit = survey.pools.iter().any(pool_hit);
    if !bridge_hit && !any_pool_hit {
        return None;
    }
    let host: Vec<Ipv4Net> = survey
        .lan
        .iter()
        .chain(&survey.routes)
        .copied()
        .filter(|net| !net.is_default_route())
        .collect();
    let pools = any_pool_hit
        .then(|| {
            let fallback = fallback_range();
            if host.iter().any(|h| h.overlaps(&fallback)) {
                let kept: Vec<AddressPool> = survey
                    .pools
                    .iter()
                    .filter(|p| !pool_hit(p))
                    .copied()
                    .collect();
                (!kept.is_empty()).then_some(kept)
            } else {
                AddressPool::new(fallback, SUGGESTED_POOL_SIZE)
                    .ok()
                    .map(|pool| vec![pool])
            }
        })
        .flatten();
    let bip = bridge_hit
        .then(|| {
            let mut taken = host.clone();
            taken.extend(
                survey
                    .networks
                    .iter()
                    .filter(|n| Some(n.subnet) != survey.bridge)
                    .map(|n| n.subnet),
            );
            pick_subnet(&taken)
        })
        .flatten();
    (bip.is_some() || pools.is_some()).then_some(DaemonJsonFix { bip, pools })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::super::{NetworkSubnet, default_address_pools, default_bridge};
    use super::*;

    fn n(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn survey(lan: &str, routes: &[&str], networks: &[(&str, &str)]) -> NetworkSurvey {
        NetworkSurvey {
            lan: vec![n(lan)],
            routes: routes.iter().map(|r| n(r)).collect(),
            bridge: Some(default_bridge()),
            pools: default_address_pools(),
            networks: networks
                .iter()
                .map(|(name, subnet)| NetworkSubnet {
                    name: (*name).into(),
                    subnet: n(subnet),
                })
                .collect(),
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn plans_the_shared_network() {
        use SharedNetworkPlan::{Create, Keep, NoFreeSubnet, Overlapping};
        let bridge = ("bridge", "172.17.0.0/16");
        let cases: [(&str, NetworkSurvey, bool, Option<&str>, SharedNetworkPlan); 9] = [
            (
                "10.0.1.0/24, fresh",
                survey("10.0.1.0/24", &["0.0.0.0/0"], &[bridge]),
                false,
                None,
                Create {
                    subnet: n("172.18.0.0/24"),
                },
            ),
            (
                "172.17.0.0/16 LAN with the bridge moved",
                NetworkSurvey {
                    bridge: Some(n("10.99.0.0/24")),
                    ..survey("172.17.0.0/16", &[], &[("bridge", "10.99.0.0/24")])
                },
                false,
                None,
                Create {
                    subnet: n("172.18.0.0/24"),
                },
            ),
            (
                "192.168.0.0/16 and a Compose network",
                survey(
                    "192.168.0.0/16",
                    &[],
                    &[bridge, ("blog_default", "172.18.0.0/16")],
                ),
                false,
                None,
                Create {
                    subnet: n("172.19.0.0/24"),
                },
            ),
            (
                "saved subnet still free",
                survey("10.0.1.0/24", &[], &[bridge]),
                false,
                Some("172.20.5.0/24"),
                Create {
                    subnet: n("172.20.5.0/24"),
                },
            ),
            (
                "saved subnet now taken by a Compose network",
                survey(
                    "10.0.1.0/24",
                    &[],
                    &[bridge, ("blog_default", "172.20.0.0/16")],
                ),
                false,
                Some("172.20.5.0/24"),
                Create {
                    subnet: n("172.18.0.0/24"),
                },
            ),
            (
                "saved subnet now on the LAN",
                survey("172.20.0.0/16", &[], &[bridge]),
                false,
                Some("172.20.5.0/24"),
                Create {
                    subnet: n("172.18.0.0/24"),
                },
            ),
            (
                "exists and is clear",
                survey("10.0.1.0/24", &[], &[bridge, ("alchemy", "172.18.0.0/24")]),
                true,
                Some("172.30.0.0/24"),
                Keep {
                    subnet: Some(n("172.18.0.0/24")),
                },
            ),
            (
                "exists on the LAN",
                survey(
                    "172.18.0.0/16",
                    &[],
                    &[bridge, ("alchemy", "172.18.0.0/24")],
                ),
                true,
                None,
                Overlapping {
                    subnet: n("172.18.0.0/24"),
                },
            ),
            (
                "everything taken",
                survey(
                    "10.0.0.0/8",
                    &["172.16.0.0/12", "192.168.0.0/16"],
                    &[bridge],
                ),
                false,
                None,
                NoFreeSubnet,
            ),
        ];
        for (name, survey, exists, saved, want) in cases {
            assert_eq!(
                plan_shared_network(&survey, exists, saved.map(n)),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn existing_network_with_its_own_route_is_kept() {
        let s = survey(
            "10.0.1.0/24",
            &["0.0.0.0/0", "10.0.1.0/24", "172.18.0.0/24"],
            &[("alchemy", "172.18.0.0/24")],
        );
        assert_eq!(
            plan_shared_network(&s, true, None),
            SharedNetworkPlan::Keep {
                subnet: Some(n("172.18.0.0/24"))
            }
        );
    }

    #[test]
    fn suggests_daemon_json() {
        let bridge = ("bridge", "172.17.0.0/16");
        let fallback = vec![AddressPool::new(n("10.200.0.0/16"), 24).unwrap()];
        let cases: [(&str, NetworkSurvey, Option<DaemonJsonFix>); 5] = [
            (
                "10.0.1.0/24 needs nothing",
                survey("10.0.1.0/24", &[], &[bridge]),
                None,
            ),
            (
                "192.168.0.0/16 replaces the pools",
                survey("192.168.0.0/16", &[], &[bridge]),
                Some(DaemonJsonFix {
                    bip: None,
                    pools: Some(fallback.clone()),
                }),
            ),
            (
                "172.17.0.0/16 moves the bridge too",
                survey("172.17.0.0/16", &[], &[bridge]),
                Some(DaemonJsonFix {
                    bip: Some(n("172.18.0.0/24")),
                    pools: Some(fallback),
                }),
            ),
            (
                "10.200.0.0/16 on a route keeps the clear defaults",
                survey("192.168.1.0/24", &["10.0.0.0/8"], &[bridge]),
                Some(DaemonJsonFix {
                    bip: None,
                    pools: Some(default_address_pools()[..15].to_vec()),
                }),
            ),
            (
                "nothing free",
                survey(
                    "172.16.0.0/12",
                    &["192.168.0.0/16", "10.0.0.0/8"],
                    &[bridge],
                ),
                None,
            ),
        ];
        for (name, survey, want) in cases {
            assert_eq!(suggest_daemon_json(&survey), want, "{name}");
        }
    }

    #[test]
    fn daemon_json_fragment() {
        let fix = DaemonJsonFix {
            bip: Some(n("172.18.0.0/24")),
            pools: Some(vec![
                AddressPool::new(n("10.200.0.0/16"), 24).unwrap(),
                AddressPool::new(n("172.20.0.0/16"), 20).unwrap(),
            ]),
        };
        assert_eq!(
            fix.to_json(),
            "{\n  \"bip\": \"172.18.0.1/24\",\n  \"default-address-pools\": [\n    { \"base\": \"10.200.0.0/16\", \"size\": 24 },\n    { \"base\": \"172.20.0.0/16\", \"size\": 20 }\n  ]\n}"
        );
        let pools_only = DaemonJsonFix {
            bip: None,
            pools: Some(vec![AddressPool::new(n("10.200.0.0/16"), 24).unwrap()]),
        };
        assert_eq!(
            pools_only.to_json(),
            "{\n  \"default-address-pools\": [\n    { \"base\": \"10.200.0.0/16\", \"size\": 24 }\n  ]\n}"
        );
        let bip_only = DaemonJsonFix {
            bip: Some(n("10.9.0.0/16")),
            pools: None,
        };
        assert_eq!(bip_only.to_json(), "{\n  \"bip\": \"10.9.0.1/16\"\n}");
    }
}
