//! Which Docker subnets overlap the home network, and what that breaks.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{AddressPool, Ipv4Net};

/// One subnet of a Docker network, as the Engine reports it. A network
/// with several IPAM configs is several of these.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkSubnet {
    /// The network's name, such as `blog_default`.
    pub name: String,
    /// One of its subnets.
    pub subnet: Ipv4Net,
}

/// Everything collision analysis looks at, gathered by the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSurvey {
    /// Subnets of the host's LAN interfaces.
    pub lan: Vec<Ipv4Net>,
    /// Destinations in the host's routing table, such as a VPN's.
    pub routes: Vec<Ipv4Net>,
    /// The default bridge's subnet, when known.
    pub bridge: Option<Ipv4Net>,
    /// The address pools Docker allocates from: `daemon.json` when set,
    /// otherwise [`default_address_pools`](super::default_address_pools).
    pub pools: Vec<AddressPool>,
    /// Subnets of existing Docker networks.
    pub networks: Vec<NetworkSubnet>,
}

/// The Docker side of a [`Collision`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    /// The default `bridge` network.
    Bridge,
    /// An address pool future networks are carved from.
    Pool {
        /// The pool.
        pool: AddressPool,
    },
    /// An existing Docker network.
    Network {
        /// The network's name.
        name: String,
    },
}

/// The host side of a [`Collision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSide {
    /// A subnet of a LAN interface.
    Lan,
    /// A routing table destination that is not a LAN subnet, such as a VPN.
    Route,
}

/// A Docker subnet that overlaps a LAN subnet or route.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Collision {
    /// What on the Docker side overlaps.
    pub subject: Subject,
    /// Its subnet.
    pub subnet: Ipv4Net,
    /// Whether it hits a LAN subnet or a route.
    pub host: HostSide,
    /// The LAN subnet or route it overlaps.
    pub host_subnet: Ipv4Net,
}

impl NetworkSurvey {
    /// Every overlap between Docker's subnets and the host's, bridge first,
    /// then pools, then networks, each against LAN subnets before routes.
    ///
    /// Routes that are a LAN subnet, a default route, or one of Docker's
    /// own subnets (Docker adds a route per bridge) are not counted again.
    /// A network on the bridge's subnet is reported as the bridge.
    pub fn collisions(&self) -> Vec<Collision> {
        let docker: Vec<Ipv4Net> = self
            .bridge
            .iter()
            .copied()
            .chain(self.networks.iter().map(|n| n.subnet))
            .collect();
        let routes: Vec<Ipv4Net> = self
            .routes
            .iter()
            .filter(|r| !r.is_default_route() && !self.lan.contains(r) && !docker.contains(r))
            .copied()
            .collect();

        let subjects = self
            .bridge
            .map(|b| (Subject::Bridge, b))
            .into_iter()
            .chain(
                self.pools
                    .iter()
                    .map(|p| (Subject::Pool { pool: *p }, p.base())),
            )
            .chain(
                self.networks
                    .iter()
                    .filter(|n| Some(n.subnet) != self.bridge)
                    .map(|n| {
                        (
                            Subject::Network {
                                name: n.name.clone(),
                            },
                            n.subnet,
                        )
                    }),
            );

        let mut found = Vec::new();
        for (subject, subnet) in subjects {
            let hits = self
                .lan
                .iter()
                .map(|h| (HostSide::Lan, h))
                .chain(routes.iter().map(|h| (HostSide::Route, h)))
                .filter(|(_, h)| h.overlaps(&subnet));
            for (host, host_subnet) in hits {
                let c = Collision {
                    subject: subject.clone(),
                    subnet,
                    host,
                    host_subnet: *host_subnet,
                };
                if !found.contains(&c) {
                    found.push(c);
                }
            }
        }
        found
    }
}

impl fmt::Display for Collision {
    /// A plain sentence saying what overlaps and what breaks.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = match self.host {
            HostSide::Lan => format!("your local network {}", self.host_subnet),
            HostSide::Route => format!("the route to {}", self.host_subnet),
        };
        let reach = match self.host {
            HostSide::Lan => "devices on it",
            HostSide::Route => "addresses behind it",
        };
        match &self.subject {
            Subject::Bridge => write!(
                f,
                "Docker's default bridge network ({}) overlaps {host}, so containers on it can't reach {reach}.",
                self.subnet
            ),
            Subject::Pool { .. } => write!(
                f,
                "Docker's address pool {} overlaps {host}, so a new network may be given addresses that hide {reach}.",
                self.subnet
            ),
            Subject::Network { name } => write!(
                f,
                "Docker network {name} ({}) overlaps {host}, so its containers can't reach {reach}.",
                self.subnet
            ),
        }
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
    use super::super::{default_address_pools, default_bridge};
    use super::*;

    fn n(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn home(lan: &str, networks: &[(&str, &str)]) -> NetworkSurvey {
        NetworkSurvey {
            lan: vec![n(lan)],
            routes: vec![n("0.0.0.0/0"), n(lan), default_bridge()],
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

    fn summary(survey: &NetworkSurvey) -> Vec<String> {
        survey
            .collisions()
            .iter()
            .map(|c| {
                let who = match &c.subject {
                    Subject::Bridge => "bridge".to_owned(),
                    Subject::Pool { pool } => format!("pool {}", pool.base()),
                    Subject::Network { name } => format!("network {name}"),
                };
                let side = match c.host {
                    HostSide::Lan => "lan",
                    HostSide::Route => "route",
                };
                format!("{who} {side} {}", c.host_subnet)
            })
            .collect()
    }

    #[test]
    fn home_networks() {
        let bridge_net = [
            ("bridge", "172.17.0.0/16"),
            ("blog_default", "172.18.0.0/16"),
        ];
        let cases: [(&str, NetworkSurvey, &[&str]); 4] = [
            (
                "10.0.1.0/24 is clear",
                home("10.0.1.0/24", &bridge_net),
                &[],
            ),
            (
                "172.17.0.0/16 hits the bridge and its pool",
                home("172.17.0.0/16", &bridge_net),
                &[
                    "bridge lan 172.17.0.0/16",
                    "pool 172.17.0.0/16 lan 172.17.0.0/16",
                ],
            ),
            (
                "192.168.0.0/16 hits the 192.168 pool",
                home("192.168.0.0/16", &bridge_net),
                &["pool 192.168.0.0/16 lan 192.168.0.0/16"],
            ),
            (
                "192.168.0.0/16 also hits a Compose network in it",
                home(
                    "192.168.0.0/16",
                    &[
                        ("bridge", "172.17.0.0/16"),
                        ("blog_default", "192.168.0.0/20"),
                    ],
                ),
                &[
                    "pool 192.168.0.0/16 lan 192.168.0.0/16",
                    "network blog_default lan 192.168.0.0/16",
                ],
            ),
        ];
        for (name, survey, want) in cases {
            assert_eq!(summary(&survey), want, "{name}");
        }
    }

    #[test]
    fn routes_count_only_when_not_lan_or_docker() {
        let mut survey = home(
            "10.0.1.0/24",
            &[
                ("bridge", "172.17.0.0/16"),
                ("blog_default", "172.18.0.0/16"),
            ],
        );
        survey.routes.push(n("172.18.0.0/16"));
        survey.routes.push(n("172.20.0.0/14"));
        assert_eq!(
            summary(&survey),
            [
                "pool 172.20.0.0/16 route 172.20.0.0/14",
                "pool 172.21.0.0/16 route 172.20.0.0/14",
                "pool 172.22.0.0/16 route 172.20.0.0/14",
                "pool 172.23.0.0/16 route 172.20.0.0/14",
            ]
        );
    }

    #[test]
    fn empty_survey_finds_nothing() {
        assert_eq!(
            NetworkSurvey::default().collisions(),
            Vec::<Collision>::new()
        );
    }

    #[test]
    fn messages_are_plain_sentences() {
        let survey = home("172.17.0.0/16", &[("blog_default", "172.17.0.0/16")]);
        let mut survey = survey;
        survey.bridge = None;
        survey.pools.clear();
        survey.routes = vec![n("10.8.0.0/16")];
        survey.networks.push(NetworkSubnet {
            name: "vpn_net".into(),
            subnet: n("10.8.1.0/24"),
        });
        let messages: Vec<String> = survey
            .collisions()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            messages,
            [
                "Docker network blog_default (172.17.0.0/16) overlaps your local network 172.17.0.0/16, so its containers can't reach devices on it.",
                "Docker network vpn_net (10.8.1.0/24) overlaps the route to 10.8.0.0/16, so its containers can't reach addresses behind it.",
            ]
        );

        let bridge = Collision {
            subject: Subject::Bridge,
            subnet: n("172.17.0.0/16"),
            host: HostSide::Lan,
            host_subnet: n("172.17.0.0/16"),
        };
        assert_eq!(
            bridge.to_string(),
            "Docker's default bridge network (172.17.0.0/16) overlaps your local network 172.17.0.0/16, so containers on it can't reach devices on it."
        );
        let pool = Collision {
            subject: Subject::Pool {
                pool: default_address_pools()[15],
            },
            subnet: n("192.168.0.0/16"),
            host: HostSide::Lan,
            host_subnet: n("192.168.0.0/16"),
        };
        assert_eq!(
            pool.to_string(),
            "Docker's address pool 192.168.0.0/16 overlaps your local network 192.168.0.0/16, so a new network may be given addresses that hide devices on it."
        );
    }

    #[test]
    fn serde_shape() {
        let c = Collision {
            subject: Subject::Network {
                name: "blog_default".into(),
            },
            subnet: n("192.168.0.0/20"),
            host: HostSide::Lan,
            host_subnet: n("192.168.0.0/16"),
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"subject":{"kind":"network","name":"blog_default"},"subnet":"192.168.0.0/20","host":"lan","host_subnet":"192.168.0.0/16"}"#
        );
    }
}
