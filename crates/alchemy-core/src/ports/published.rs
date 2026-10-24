//! Published ports, host listeners, and whether two of them collide.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A transport protocol a port is published on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// TCP, Compose's default.
    Tcp,
    /// UDP.
    Udp,
}

/// Why a protocol name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unsupported protocol {0:?}; use tcp or udp")]
pub struct ProtocolError(pub String);

impl Protocol {
    /// The lowercase name, `tcp` or `udp`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Protocol {
    type Err = ProtocolError;

    /// Parse `tcp` or `udp`, ignoring ASCII case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("tcp") {
            Ok(Self::Tcp)
        } else if s.eq_ignore_ascii_case("udp") {
            Ok(Self::Udp)
        } else {
            Err(ProtocolError(s.to_owned()))
        }
    }
}

/// An inclusive range of port numbers, such as `8080` or `8000-8010`.
///
/// Never empty and never includes port 0. Serializes as its text form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PortRange {
    start: u16,
    end: u16,
}

/// Why a candidate [`PortRange`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PortRangeError {
    /// Port 0 means "any port" to the OS and is never a published port.
    #[error("port 0 is not a usable port")]
    Zero,
    /// The range ended before it started.
    #[error("port range {start}-{end} ends before it starts")]
    Reversed {
        /// The first port.
        start: u16,
        /// The last port.
        end: u16,
    },
    /// The text was not a port or a `start-end` range.
    #[error("{0:?} is not a port or port range")]
    Invalid(String),
}

impl PortRange {
    /// The ports from `start` to `end`, inclusive.
    pub fn new(start: u16, end: u16) -> Result<Self, PortRangeError> {
        if start == 0 {
            return Err(PortRangeError::Zero);
        }
        if end < start {
            return Err(PortRangeError::Reversed { start, end });
        }
        Ok(Self { start, end })
    }

    /// A range of one port.
    pub fn single(port: u16) -> Result<Self, PortRangeError> {
        Self::new(port, port)
    }

    /// The first port.
    pub fn start(&self) -> u16 {
        self.start
    }

    /// The last port, equal to [`start`](Self::start) for a single port.
    pub fn end(&self) -> u16 {
        self.end
    }

    /// How many ports the range spans; at least 1.
    pub fn len(&self) -> u32 {
        u32::from(self.end) - u32::from(self.start) + 1
    }

    /// Always false: a range holds at least one port.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether `port` is in the range.
    pub fn contains(&self, port: u16) -> bool {
        (self.start..=self.end).contains(&port)
    }

    /// Whether the two ranges share a port.
    pub fn overlaps(&self, other: &PortRange) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// Each port in the range, in order.
    pub fn ports(&self) -> std::ops::RangeInclusive<u16> {
        self.start..=self.end
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

impl FromStr for PortRange {
    type Err = PortRangeError;

    /// Parse `8080` or `8000-8010`, the forms Compose uses.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let number = |part: &str| -> Result<u16, PortRangeError> {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(PortRangeError::Invalid(s.to_owned()));
            }
            part.parse()
                .map_err(|_| PortRangeError::Invalid(s.to_owned()))
        };
        match s.split_once('-') {
            Some((start, end)) => Self::new(number(start)?, number(end)?),
            None => Self::single(number(s)?),
        }
    }
}

impl TryFrom<String> for PortRange {
    type Error = PortRangeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<PortRange> for String {
    fn from(range: PortRange) -> Self {
        range.to_string()
    }
}

/// A port published on the host, as Compose declares it or Docker
/// reports it.
///
/// `published: None` is an ephemeral port: Docker picks a free one at
/// start, so it never conflicts. `host_ip: None` means every address,
/// v4 and v6, which is Docker's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublishedPort {
    /// TCP or UDP.
    pub protocol: Protocol,
    /// The host address the port is bound to, or `None` for every address.
    #[serde(default)]
    pub host_ip: Option<IpAddr>,
    /// The host port or ports, or `None` for an ephemeral port.
    #[serde(default)]
    pub published: Option<PortRange>,
    /// The container port or ports, for display only; never part of a
    /// conflict.
    #[serde(default)]
    pub target: Option<PortRange>,
}

impl PublishedPort {
    /// A port published on every host address, with no target recorded.
    pub fn new(protocol: Protocol, published: Option<PortRange>) -> Self {
        Self {
            protocol,
            host_ip: None,
            published,
            target: None,
        }
    }

    /// The same port bound to `host_ip` instead.
    #[must_use]
    pub fn with_host_ip(mut self, host_ip: Option<IpAddr>) -> Self {
        self.host_ip = host_ip;
        self
    }

    /// The same port with its container side recorded.
    #[must_use]
    pub fn with_target(mut self, target: Option<PortRange>) -> Self {
        self.target = target;
        self
    }

    /// Whether Docker picks the host port, so it can never conflict.
    pub fn is_ephemeral(&self) -> bool {
        self.published.is_none()
    }

    /// Whether the two could not both be bound at once: same protocol,
    /// overlapping ports, and overlapping host addresses.
    pub fn conflicts_with(&self, other: &PublishedPort) -> bool {
        match (self.published, other.published) {
            (Some(a), Some(b)) => {
                self.protocol == other.protocol
                    && a.overlaps(&b)
                    && host_ips_overlap(self.host_ip, other.host_ip)
            }
            _ => false,
        }
    }
}

impl fmt::Display for PublishedPort {
    /// `8080/tcp`, `127.0.0.1:8080/tcp`, `[::1]:53/udp`, or
    /// `ephemeral/tcp`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host_ip {
            Some(IpAddr::V4(ip)) => write!(f, "{ip}:")?,
            Some(IpAddr::V6(ip)) => write!(f, "[{ip}]:")?,
            None => {}
        }
        match self.published {
            Some(range) => write!(f, "{range}/{}", self.protocol),
            None => write!(f, "ephemeral/{}", self.protocol),
        }
    }
}

/// Whether binding the same port on `a` and `b` would collide.
///
/// `None` is every address. `0.0.0.0` is every IPv4 address. `::` is
/// treated as every address, v4 and v6, because Linux and macOS open
/// dual-stack sockets on it by default. IPv4-mapped IPv6 addresses
/// compare as their IPv4 form. Two specific addresses collide only when
/// equal.
pub fn host_ips_overlap(a: Option<IpAddr>, b: Option<IpAddr>) -> bool {
    match (a.map(|ip| ip.to_canonical()), b.map(|ip| ip.to_canonical())) {
        (Some(a), Some(b)) => covers(a, b) || covers(b, a),
        _ => true,
    }
}

fn covers(wide: IpAddr, narrow: IpAddr) -> bool {
    wide == narrow
        || match wide {
            IpAddr::V6(ip) => ip.is_unspecified(),
            IpAddr::V4(ip) => ip.is_unspecified() && narrow.is_ipv4(),
        }
}

/// A socket listening on the host, as the OS reports it.
///
/// Process name and pid are filled in when the OS reveals them, which
/// often needs privileges the daemon lacks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Listener {
    /// TCP (a listening socket) or UDP (a bound socket).
    pub protocol: Protocol,
    /// The local address, `0.0.0.0` or `::` for every address.
    pub addr: IpAddr,
    /// The local port.
    pub port: u16,
    /// The owning process's name, such as `nginx`.
    #[serde(default)]
    pub process: Option<String>,
    /// The owning process's id.
    #[serde(default)]
    pub pid: Option<u32>,
}

impl Listener {
    /// The listener as a published port, for conflict checks. `None` for
    /// port 0, which the OS never reports for a bound socket.
    pub fn published(&self) -> Option<PublishedPort> {
        let range = PortRange::single(self.port).ok()?;
        Some(PublishedPort::new(self.protocol, Some(range)).with_host_ip(Some(self.addr)))
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

    fn ip(s: &str) -> Option<IpAddr> {
        (s != "*").then(|| s.parse().unwrap())
    }

    fn port(proto: Protocol, host: &str, range: &str) -> PublishedPort {
        PublishedPort::new(proto, (range != "-").then(|| range.parse().unwrap()))
            .with_host_ip(ip(host))
    }

    #[test]
    fn protocol_text() {
        assert_eq!("tcp".parse::<Protocol>(), Ok(Protocol::Tcp));
        assert_eq!("UDP".parse::<Protocol>(), Ok(Protocol::Udp));
        assert_eq!(
            "sctp".parse::<Protocol>(),
            Err(ProtocolError("sctp".into()))
        );
        assert_eq!(Protocol::Udp.to_string(), "udp");
        assert_eq!(serde_json::to_string(&Protocol::Tcp).unwrap(), "\"tcp\"");
    }

    #[test]
    fn port_range_text() {
        let ok = [
            ("8080", 8080, 8080, "8080"),
            ("8000-8010", 8000, 8010, "8000-8010"),
            ("53-53", 53, 53, "53"),
            ("65535", 65535, 65535, "65535"),
        ];
        for (input, start, end, shown) in ok {
            let r: PortRange = input.parse().unwrap();
            assert_eq!((r.start(), r.end()), (start, end), "{input}");
            assert_eq!(r.to_string(), shown);
        }
        let bad = [
            ("0", PortRangeError::Zero),
            ("0-10", PortRangeError::Zero),
            ("10-5", PortRangeError::Reversed { start: 10, end: 5 }),
            ("", PortRangeError::Invalid(String::new())),
            ("80-", PortRangeError::Invalid("80-".into())),
            ("+80", PortRangeError::Invalid("+80".into())),
            ("65536", PortRangeError::Invalid("65536".into())),
            ("http", PortRangeError::Invalid("http".into())),
        ];
        for (input, want) in bad {
            assert_eq!(input.parse::<PortRange>(), Err(want), "{input}");
        }
    }

    #[test]
    fn port_range_overlap() {
        let cases = [
            ("8080", "8080", true),
            ("8080", "8081", false),
            ("8000-8010", "8010", true),
            ("8000-8010", "7990-8000", true),
            ("8000-8010", "8011-8020", false),
            ("1-65535", "443", true),
        ];
        for (a, b, want) in cases {
            let (a, b): (PortRange, PortRange) = (a.parse().unwrap(), b.parse().unwrap());
            assert_eq!(a.overlaps(&b), want, "{a} {b}");
            assert_eq!(b.overlaps(&a), want, "{b} {a}");
        }
        let r: PortRange = "8000-8002".parse().unwrap();
        assert_eq!(r.len(), 3);
        assert!(r.contains(8001) && !r.contains(8003));
        assert_eq!(r.ports().collect::<Vec<_>>(), vec![8000, 8001, 8002]);
    }

    #[test]
    fn host_ip_overlap() {
        let cases = [
            ("*", "*", true),
            ("*", "127.0.0.1", true),
            ("*", "::1", true),
            ("0.0.0.0", "127.0.0.1", true),
            ("0.0.0.0", "192.168.1.5", true),
            ("0.0.0.0", "::1", false),
            ("0.0.0.0", "::", true),
            ("::", "127.0.0.1", true),
            ("::", "::1", true),
            ("127.0.0.1", "127.0.0.1", true),
            ("127.0.0.1", "192.168.1.5", false),
            ("127.0.0.1", "::1", false),
            ("::1", "::1", true),
            ("::ffff:127.0.0.1", "127.0.0.1", true),
            ("::ffff:0.0.0.0", "10.0.1.5", true),
        ];
        for (a, b, want) in cases {
            assert_eq!(host_ips_overlap(ip(a), ip(b)), want, "{a} {b}");
            assert_eq!(host_ips_overlap(ip(b), ip(a)), want, "{b} {a}");
        }
    }

    #[test]
    fn published_port_conflicts() {
        use Protocol::{Tcp, Udp};
        let cases = [
            (port(Tcp, "*", "8080"), port(Tcp, "*", "8080"), true),
            (port(Tcp, "*", "8080"), port(Udp, "*", "8080"), false),
            (port(Tcp, "*", "8080"), port(Tcp, "*", "8081"), false),
            (port(Tcp, "*", "8000-8010"), port(Tcp, "*", "8005"), true),
            (port(Tcp, "*", "-"), port(Tcp, "*", "8080"), false),
            (port(Tcp, "*", "-"), port(Tcp, "*", "-"), false),
            (
                port(Tcp, "127.0.0.1", "80"),
                port(Tcp, "10.0.1.5", "80"),
                false,
            ),
            (
                port(Tcp, "127.0.0.1", "80"),
                port(Tcp, "0.0.0.0", "80"),
                true,
            ),
            (port(Udp, "::1", "53"), port(Udp, "127.0.0.1", "53"), false),
            (port(Udp, "::", "53"), port(Udp, "127.0.0.1", "53"), true),
        ];
        for (a, b, want) in cases {
            assert_eq!(a.conflicts_with(&b), want, "{a} vs {b}");
            assert_eq!(b.conflicts_with(&a), want, "{b} vs {a}");
        }
    }

    #[test]
    fn published_port_display_and_serde() {
        use Protocol::{Tcp, Udp};
        assert_eq!(port(Tcp, "*", "8080").to_string(), "8080/tcp");
        assert_eq!(port(Tcp, "127.0.0.1", "80").to_string(), "127.0.0.1:80/tcp");
        assert_eq!(port(Udp, "::1", "53").to_string(), "[::1]:53/udp");
        assert_eq!(port(Tcp, "*", "-").to_string(), "ephemeral/tcp");
        assert!(port(Tcp, "*", "-").is_ephemeral());

        let p = port(Tcp, "127.0.0.1", "8000-8010").with_target(Some("80".parse().unwrap()));
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(
            json,
            r#"{"protocol":"tcp","host_ip":"127.0.0.1","published":"8000-8010","target":"80"}"#
        );
        assert_eq!(serde_json::from_str::<PublishedPort>(&json).unwrap(), p);
        let bare: PublishedPort = serde_json::from_str(r#"{"protocol":"udp"}"#).unwrap();
        assert_eq!(bare, PublishedPort::new(Udp, None));
    }

    #[test]
    fn listener_as_published_port() {
        let l = Listener {
            protocol: Protocol::Tcp,
            addr: "0.0.0.0".parse().unwrap(),
            port: 8080,
            process: Some("nginx".into()),
            pid: Some(42),
        };
        assert_eq!(l.published(), Some(port(Protocol::Tcp, "0.0.0.0", "8080")));
        assert_eq!(Listener { port: 0, ..l }.published(), None);
    }
}
