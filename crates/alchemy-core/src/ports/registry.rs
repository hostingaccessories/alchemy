//! Who holds each host port: reservations, Compose declarations, running
//! containers, and other programs, merged into one list.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{Listener, PublishedPort};

/// Who holds a port.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortHolder {
    /// A service of a Compose project, usually an Alchemy app.
    App {
        /// The Compose project name.
        app: String,
        /// The service name.
        service: String,
    },
    /// A container outside Compose, such as one from `docker run`.
    Container {
        /// The container name.
        name: String,
    },
    /// A port Alchemy set aside for a feature or app.
    Reservation {
        /// What the port is reserved for, such as an app name or `proxy`.
        owner: String,
    },
    /// A process on the host that is not a Docker container.
    Program {
        /// The process name, when the OS reveals it.
        #[serde(default)]
        process: Option<String>,
        /// The process id, when the OS reveals it.
        #[serde(default)]
        pid: Option<u32>,
    },
}

impl PortHolder {
    /// Whether this holder is `app` itself: one of its services, or a
    /// reservation in its name. An app never conflicts with itself, so a
    /// restart is not blocked by the containers it is replacing.
    pub fn belongs_to(&self, app: &str) -> bool {
        match self {
            Self::App { app: a, .. } | Self::Reservation { owner: a } => a == app,
            Self::Container { .. } | Self::Program { .. } => false,
        }
    }
}

impl fmt::Display for PortHolder {
    /// A phrase for a sentence: `service web of app blog`,
    /// `container pihole`, `a reservation for proxy`,
    /// `another program (nginx, pid 42)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::App { app, service } => write!(f, "service {service} of app {app}"),
            Self::Container { name } => write!(f, "container {name}"),
            Self::Reservation { owner } => write!(f, "a reservation for {owner}"),
            Self::Program { process, pid } => match (process, pid) {
                (Some(name), Some(pid)) => write!(f, "another program ({name}, pid {pid})"),
                (Some(name), None) => write!(f, "another program ({name})"),
                (None, Some(pid)) => write!(f, "another program (pid {pid})"),
                (None, None) => f.write_str("another program"),
            },
        }
    }
}

/// Where the registry learned about a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortSource {
    /// A persisted reservation.
    Reserved,
    /// A Compose file publishes it, but nothing holds it right now.
    Declared,
    /// Something holds it right now: a running container or a host
    /// listener.
    Live,
}

/// One port in the registry and who holds it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortClaim {
    /// The port.
    pub port: PublishedPort,
    /// Who holds it.
    pub holder: PortHolder,
    /// How the registry knows.
    pub source: PortSource,
}

/// A port set aside for `owner`, as persisted by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Reservation {
    /// What the port is reserved for.
    pub owner: String,
    /// The port.
    pub port: PublishedPort,
}

/// A port a Compose service publishes, from its resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServicePort {
    /// The Compose project name.
    pub app: String,
    /// The service name.
    pub service: String,
    /// The port.
    pub port: PublishedPort,
}

/// A port a running container publishes, as Docker reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerPort {
    /// The container name.
    pub name: String,
    /// The Compose project label, if any.
    #[serde(default)]
    pub app: Option<String>,
    /// The Compose service label, if any.
    #[serde(default)]
    pub service: Option<String>,
    /// The port.
    pub port: PublishedPort,
}

impl ContainerPort {
    /// The app service when both Compose labels are present, otherwise the
    /// bare container.
    pub fn holder(&self) -> PortHolder {
        match (&self.app, &self.service) {
            (Some(app), Some(service)) => PortHolder::App {
                app: app.clone(),
                service: service.clone(),
            },
            _ => PortHolder::Container {
                name: self.name.clone(),
            },
        }
    }
}

/// Everything the registry merges, gathered by the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryInput {
    /// Persisted reservations.
    pub reservations: Vec<Reservation>,
    /// Ports each app's Compose configuration publishes.
    pub declared: Vec<ServicePort>,
    /// Ports running containers publish.
    pub containers: Vec<ContainerPort>,
    /// Sockets listening on the host.
    pub listeners: Vec<Listener>,
}

impl RegistryInput {
    /// One list of every claimed host port, sorted by port.
    ///
    /// Ephemeral ports are left out: they hold no port until Docker picks
    /// one, and then the container reports it. A declared port that its
    /// own running container already reports is listed once, as live. A
    /// host listener that a running container's port explains (Docker's
    /// `docker-proxy`, or `com.docker.backend` on Docker Desktop) is folded
    /// into the container. Exact duplicates are dropped.
    pub fn merge(&self) -> Vec<PortClaim> {
        let containers: Vec<PortClaim> = self
            .containers
            .iter()
            .filter(|c| !c.port.is_ephemeral())
            .map(|c| PortClaim {
                port: c.port,
                holder: c.holder(),
                source: PortSource::Live,
            })
            .collect();

        let reserved = self
            .reservations
            .iter()
            .filter(|r| !r.port.is_ephemeral())
            .map(|r| PortClaim {
                port: r.port,
                holder: PortHolder::Reservation {
                    owner: r.owner.clone(),
                },
                source: PortSource::Reserved,
            });

        let declared = self
            .declared
            .iter()
            .filter(|d| !d.port.is_ephemeral())
            .map(|d| PortClaim {
                port: d.port,
                holder: PortHolder::App {
                    app: d.app.clone(),
                    service: d.service.clone(),
                },
                source: PortSource::Declared,
            })
            .filter(|d| {
                !containers
                    .iter()
                    .any(|c| c.holder == d.holder && c.port.conflicts_with(&d.port))
            });

        let listeners = self.listeners.iter().filter_map(|l| {
            let port = l.published()?;
            let explained = containers.iter().any(|c| c.port.conflicts_with(&port));
            (!explained).then(|| PortClaim {
                port,
                holder: PortHolder::Program {
                    process: l.process.clone(),
                    pid: l.pid,
                },
                source: PortSource::Live,
            })
        });

        let mut claims: Vec<PortClaim> = Vec::new();
        for claim in reserved
            .chain(declared)
            .chain(containers.iter().cloned())
            .chain(listeners)
        {
            if !claims.contains(&claim) {
                claims.push(claim);
            }
        }
        claims.sort_by_key(|c| (c.port.published, c.port.protocol, c.port.host_ip, c.source));
        claims
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
    use super::super::Protocol;
    use super::*;

    fn tcp(range: &str) -> PublishedPort {
        PublishedPort::new(
            Protocol::Tcp,
            (range != "-").then(|| range.parse().unwrap()),
        )
    }

    fn listener(addr: &str, port: u16, process: Option<&str>) -> Listener {
        Listener {
            protocol: Protocol::Tcp,
            addr: addr.parse().unwrap(),
            port,
            process: process.map(Into::into),
            pid: process.map(|_| 100),
        }
    }

    fn container(app: &str, service: &str, range: &str) -> ContainerPort {
        ContainerPort {
            name: format!("{app}-{service}-1"),
            app: Some(app.into()),
            service: Some(service.into()),
            port: tcp(range),
        }
    }

    fn row(c: &PortClaim) -> String {
        let source = match c.source {
            PortSource::Reserved => "reserved",
            PortSource::Declared => "declared",
            PortSource::Live => "live",
        };
        format!("{} {source} {}", c.port, c.holder)
    }

    #[test]
    fn holder_phrases() {
        let cases = [
            (
                PortHolder::App {
                    app: "blog".into(),
                    service: "web".into(),
                },
                "service web of app blog",
            ),
            (
                PortHolder::Container {
                    name: "pihole".into(),
                },
                "container pihole",
            ),
            (
                PortHolder::Reservation {
                    owner: "proxy".into(),
                },
                "a reservation for proxy",
            ),
            (
                PortHolder::Program {
                    process: Some("nginx".into()),
                    pid: Some(42),
                },
                "another program (nginx, pid 42)",
            ),
            (
                PortHolder::Program {
                    process: Some("nginx".into()),
                    pid: None,
                },
                "another program (nginx)",
            ),
            (
                PortHolder::Program {
                    process: None,
                    pid: Some(42),
                },
                "another program (pid 42)",
            ),
            (
                PortHolder::Program {
                    process: None,
                    pid: None,
                },
                "another program",
            ),
        ];
        for (holder, want) in cases {
            assert_eq!(holder.to_string(), want);
        }
    }

    #[test]
    fn belongs_to() {
        let cases = [
            (
                PortHolder::App {
                    app: "blog".into(),
                    service: "web".into(),
                },
                true,
            ),
            (
                PortHolder::App {
                    app: "wiki".into(),
                    service: "web".into(),
                },
                false,
            ),
            (
                PortHolder::Reservation {
                    owner: "blog".into(),
                },
                true,
            ),
            (
                PortHolder::Container {
                    name: "blog".into(),
                },
                false,
            ),
            (
                PortHolder::Program {
                    process: Some("blog".into()),
                    pid: None,
                },
                false,
            ),
        ];
        for (holder, want) in cases {
            assert_eq!(holder.belongs_to("blog"), want, "{holder}");
        }
    }

    #[test]
    fn container_holder_needs_both_labels() {
        let mut c = container("blog", "web", "8080");
        assert_eq!(
            c.holder(),
            PortHolder::App {
                app: "blog".into(),
                service: "web".into()
            }
        );
        c.service = None;
        assert_eq!(
            c.holder(),
            PortHolder::Container {
                name: "blog-web-1".into()
            }
        );
    }

    #[test]
    fn merges_every_source() {
        let input = RegistryInput {
            reservations: vec![Reservation {
                owner: "proxy".into(),
                port: tcp("443"),
            }],
            declared: vec![
                ServicePort {
                    app: "blog".into(),
                    service: "web".into(),
                    port: tcp("8080"),
                },
                ServicePort {
                    app: "wiki".into(),
                    service: "web".into(),
                    port: tcp("8081"),
                },
                ServicePort {
                    app: "wiki".into(),
                    service: "db".into(),
                    port: tcp("-"),
                },
            ],
            containers: vec![
                container("blog", "web", "8080"),
                ContainerPort {
                    name: "pihole".into(),
                    app: None,
                    service: None,
                    port: PublishedPort::new(Protocol::Udp, Some("53".parse().unwrap())),
                },
                container("blog", "worker", "-"),
            ],
            listeners: vec![
                listener("0.0.0.0", 8080, Some("docker-proxy")),
                listener("::", 8080, Some("docker-proxy")),
                listener("127.0.0.1", 5432, Some("postgres")),
                listener("127.0.0.1", 5432, Some("postgres")),
                listener("0.0.0.0", 22, None),
            ],
        };
        let rows: Vec<String> = input.merge().iter().map(row).collect();
        assert_eq!(
            rows,
            [
                "0.0.0.0:22/tcp live another program",
                "53/udp live container pihole",
                "443/tcp reserved a reservation for proxy",
                "127.0.0.1:5432/tcp live another program (postgres, pid 100)",
                "8080/tcp live service web of app blog",
                "8081/tcp declared service web of app wiki",
            ]
        );
    }

    #[test]
    fn listener_on_other_address_is_not_explained() {
        let input = RegistryInput {
            containers: vec![ContainerPort {
                port: tcp("8080").with_host_ip(Some("127.0.0.1".parse().unwrap())),
                ..container("blog", "web", "8080")
            }],
            listeners: vec![listener("10.0.1.5", 8080, Some("nc"))],
            ..RegistryInput::default()
        };
        let rows: Vec<String> = input.merge().iter().map(row).collect();
        assert_eq!(
            rows,
            [
                "10.0.1.5:8080/tcp live another program (nc, pid 100)",
                "127.0.0.1:8080/tcp live service web of app blog",
            ]
        );
    }

    #[test]
    fn serde_shape() {
        let claim = PortClaim {
            port: tcp("8080"),
            holder: PortHolder::Program {
                process: Some("nc".into()),
                pid: None,
            },
            source: PortSource::Live,
        };
        assert_eq!(
            serde_json::to_string(&claim).unwrap(),
            r#"{"port":{"protocol":"tcp","host_ip":null,"published":"8080","target":null},"holder":{"kind":"program","process":"nc","pid":null},"source":"live"}"#
        );
    }
}
