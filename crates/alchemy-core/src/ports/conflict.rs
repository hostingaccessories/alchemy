//! Whether an app can start without a port clash, and picking free ports.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{PortClaim, PortRange, PortSource, Protocol, PublishedPort, ServicePort};

/// A port an app wants that something else already holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortConflict {
    /// The service that wants the port.
    pub service: String,
    /// The port it wants.
    pub wanted: PublishedPort,
    /// What already holds it.
    pub claim: PortClaim,
}

impl fmt::Display for PortConflict {
    /// A plain sentence naming the port and its holder.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = match self.claim.source {
            PortSource::Reserved => "is reserved",
            PortSource::Declared => "is published by",
            PortSource::Live => "is in use by",
        };
        let holder = &self.claim.holder;
        if self.claim.source == PortSource::Reserved {
            write!(
                f,
                "Port {} for service {} {verb} as {holder}.",
                self.wanted, self.service
            )
        } else {
            write!(
                f,
                "Port {} for service {} {verb} {holder}.",
                self.wanted, self.service
            )
        }
    }
}

/// Every port `app` wants that collides with a claim held by something
/// else, in the order of `wanted`, then `claims`.
///
/// Only live and reserved claims block a start: a port another app
/// declares but is not running is free to use until that app starts.
/// Claims that [`belong to`](super::PortHolder::belongs_to) `app` are
/// skipped, so restarting an app never conflicts with its own containers.
/// Ephemeral ports never conflict.
pub fn find_conflicts(
    app: &str,
    wanted: &[ServicePort],
    claims: &[PortClaim],
) -> Vec<PortConflict> {
    let blocking: Vec<&PortClaim> = claims
        .iter()
        .filter(|c| c.source != PortSource::Declared && !c.holder.belongs_to(app))
        .collect();
    wanted
        .iter()
        .filter(|w| w.app == app)
        .flat_map(|w| {
            blocking
                .iter()
                .filter(|c| w.port.conflicts_with(&c.port))
                .map(|c| PortConflict {
                    service: w.service.clone(),
                    wanted: w.port,
                    claim: (*c).clone(),
                })
        })
        .collect()
}

/// Why [`allocate`] found no port.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum AllocateError {
    /// Every port in the range is claimed.
    #[error("no free {protocol} port between {} and {}", range.start(), range.end())]
    Exhausted {
        /// The range searched.
        range: PortRange,
        /// The protocol searched.
        protocol: Protocol,
    },
}

/// The lowest port in `range` that no claim holds for `protocol` on
/// `host_ip` (`None` for every address).
///
/// Every claim counts, declared ones included, so a stopped app's ports
/// are not handed out to someone else.
pub fn allocate(
    range: PortRange,
    protocol: Protocol,
    host_ip: Option<IpAddr>,
    claims: &[PortClaim],
) -> Result<u16, AllocateError> {
    let relevant: Vec<&PortClaim> = claims
        .iter()
        .filter(|c| c.port.published.is_some_and(|p| p.overlaps(&range)))
        .collect();
    range
        .ports()
        .find(|&port| {
            let candidate =
                PublishedPort::new(protocol, PortRange::single(port).ok()).with_host_ip(host_ip);
            !candidate.is_ephemeral() && !relevant.iter().any(|c| c.port.conflicts_with(&candidate))
        })
        .ok_or(AllocateError::Exhausted { range, protocol })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::super::PortHolder;
    use super::*;

    fn port(proto: Protocol, host: &str, range: &str) -> PublishedPort {
        PublishedPort::new(proto, (range != "-").then(|| range.parse().unwrap()))
            .with_host_ip((host != "*").then(|| host.parse().unwrap()))
    }

    fn want(app: &str, service: &str, p: PublishedPort) -> ServicePort {
        ServicePort {
            app: app.into(),
            service: service.into(),
            port: p,
        }
    }

    fn claim(p: PublishedPort, holder: PortHolder, source: PortSource) -> PortClaim {
        PortClaim {
            port: p,
            holder,
            source,
        }
    }

    fn app(app: &str, service: &str) -> PortHolder {
        PortHolder::App {
            app: app.into(),
            service: service.into(),
        }
    }

    fn program() -> PortHolder {
        PortHolder::Program {
            process: Some("nc".into()),
            pid: Some(7),
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn conflict_detection() {
        use PortSource::{Declared, Live, Reserved};
        use Protocol::{Tcp, Udp};
        let cases: Vec<(&str, PublishedPort, PortClaim, bool)> = vec![
            (
                "same port, both wildcard",
                port(Tcp, "*", "8080"),
                claim(port(Tcp, "*", "8080"), app("wiki", "web"), Live),
                true,
            ),
            (
                "other protocol",
                port(Tcp, "*", "53"),
                claim(port(Udp, "*", "53"), program(), Live),
                false,
            ),
            (
                "range covers a live port",
                port(Udp, "*", "60000-60010"),
                claim(port(Udp, "0.0.0.0", "60005"), program(), Live),
                true,
            ),
            (
                "range next to a live range",
                port(Tcp, "*", "8000-8009"),
                claim(port(Tcp, "*", "8010-8020"), program(), Live),
                false,
            ),
            (
                "different specific host IPs",
                port(Tcp, "127.0.0.1", "80"),
                claim(port(Tcp, "10.0.1.5", "80"), program(), Live),
                false,
            ),
            (
                "specific IP against v4 wildcard listener",
                port(Tcp, "127.0.0.1", "80"),
                claim(port(Tcp, "0.0.0.0", "80"), program(), Live),
                true,
            ),
            (
                "v6 loopback against v4 loopback",
                port(Tcp, "::1", "80"),
                claim(port(Tcp, "127.0.0.1", "80"), program(), Live),
                false,
            ),
            (
                "v4 wildcard against dual-stack listener",
                port(Tcp, "0.0.0.0", "80"),
                claim(port(Tcp, "::", "80"), program(), Live),
                true,
            ),
            (
                "v4 wildcard against v6 loopback",
                port(Tcp, "0.0.0.0", "80"),
                claim(port(Tcp, "::1", "80"), program(), Live),
                false,
            ),
            (
                "ephemeral wanted",
                port(Tcp, "*", "-"),
                claim(port(Tcp, "*", "8080"), program(), Live),
                false,
            ),
            (
                "reservation for someone else",
                port(Tcp, "*", "443"),
                claim(
                    port(Tcp, "*", "443"),
                    PortHolder::Reservation {
                        owner: "proxy".into(),
                    },
                    Reserved,
                ),
                true,
            ),
            (
                "own reservation",
                port(Tcp, "*", "443"),
                claim(
                    port(Tcp, "*", "443"),
                    PortHolder::Reservation {
                        owner: "blog".into(),
                    },
                    Reserved,
                ),
                false,
            ),
            (
                "own running container (restart)",
                port(Tcp, "*", "8080"),
                claim(port(Tcp, "*", "8080"), app("blog", "web"), Live),
                false,
            ),
            (
                "own other service",
                port(Tcp, "*", "8080"),
                claim(port(Tcp, "*", "8080"), app("blog", "admin"), Live),
                false,
            ),
            (
                "stopped app only declares it",
                port(Tcp, "*", "8080"),
                claim(port(Tcp, "*", "8080"), app("wiki", "web"), Declared),
                false,
            ),
            (
                "plain container",
                port(Tcp, "*", "8080"),
                claim(
                    port(Tcp, "*", "8080"),
                    PortHolder::Container {
                        name: "blog".into(),
                    },
                    Live,
                ),
                true,
            ),
        ];
        for (name, wanted, held, conflict) in cases {
            let found = find_conflicts(
                "blog",
                &[want("blog", "web", wanted)],
                std::slice::from_ref(&held),
            );
            assert_eq!(found.len(), usize::from(conflict), "{name}");
            if conflict {
                assert_eq!(found[0].claim, held, "{name}");
                assert_eq!(found[0].wanted, wanted, "{name}");
                assert_eq!(found[0].service, "web", "{name}");
            }
        }
    }

    #[test]
    fn ignores_other_apps_wanted_ports() {
        let held = claim(
            port(Protocol::Tcp, "*", "8080"),
            program(),
            PortSource::Live,
        );
        let wanted = [want("wiki", "web", port(Protocol::Tcp, "*", "8080"))];
        assert_eq!(
            find_conflicts("blog", &wanted, &[held]),
            Vec::<PortConflict>::new()
        );
    }

    #[test]
    fn messages_name_port_and_holder() {
        let cases = [
            (
                claim(
                    port(Protocol::Tcp, "*", "8080"),
                    app("wiki", "web"),
                    PortSource::Live,
                ),
                "Port 8080/tcp for service web is in use by service web of app wiki.",
            ),
            (
                claim(
                    port(Protocol::Tcp, "0.0.0.0", "8080"),
                    program(),
                    PortSource::Live,
                ),
                "Port 8080/tcp for service web is in use by another program (nc, pid 7).",
            ),
            (
                claim(
                    port(Protocol::Tcp, "*", "8080"),
                    PortHolder::Reservation {
                        owner: "proxy".into(),
                    },
                    PortSource::Reserved,
                ),
                "Port 8080/tcp for service web is reserved as a reservation for proxy.",
            ),
            (
                claim(
                    port(Protocol::Tcp, "*", "8080"),
                    app("wiki", "web"),
                    PortSource::Declared,
                ),
                "Port 8080/tcp for service web is published by service web of app wiki.",
            ),
        ];
        for (held, message) in cases {
            let c = PortConflict {
                service: "web".into(),
                wanted: port(Protocol::Tcp, "*", "8080"),
                claim: held,
            };
            assert_eq!(c.to_string(), message);
        }
    }

    #[test]
    fn allocates_lowest_free_port() {
        use PortSource::{Declared, Live, Reserved};
        use Protocol::{Tcp, Udp};
        let range: PortRange = "8000-8003".parse().unwrap();
        let claims = [
            claim(port(Tcp, "*", "8000"), program(), Live),
            claim(port(Tcp, "*", "8001"), app("wiki", "web"), Declared),
            claim(
                port(Tcp, "*", "8002"),
                PortHolder::Reservation {
                    owner: "proxy".into(),
                },
                Reserved,
            ),
            claim(port(Tcp, "127.0.0.1", "8003"), program(), Live),
            claim(port(Tcp, "*", "-"), program(), Live),
        ];
        let cases: [(&str, Protocol, Option<&str>, Option<u16>); 4] = [
            ("udp is free", Udp, None, Some(8000)),
            ("every address blocked", Tcp, None, None),
            ("loopback blocked at 8003", Tcp, Some("127.0.0.1"), None),
            (
                "other address free at 8003",
                Tcp,
                Some("10.0.1.5"),
                Some(8003),
            ),
        ];
        for (name, protocol, host, want) in cases {
            let host = host.map(|h| h.parse().unwrap());
            let got = allocate(range, protocol, host, &claims);
            let want = want.ok_or(AllocateError::Exhausted { range, protocol });
            assert_eq!(got, want, "{name}");
        }
        assert_eq!(
            AllocateError::Exhausted {
                range,
                protocol: Tcp
            }
            .to_string(),
            "no free tcp port between 8000 and 8003"
        );
    }
}
