//! What a Compose project says about routing: the `x-alchemy` block and
//! the container ports its services expose.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::ports::{PortRange, PortRangeError, Protocol};

/// The `x-alchemy` key, at the top of a compose file or on one service.
pub const X_ALCHEMY: &str = "x-alchemy";

/// The fields of an `x-alchemy` block.
///
/// ```yaml
/// x-alchemy:
///   service: web    # top-level only; a service's own block implies it
///   port: 8080      # the container port to route to
///   hostname: wiki  # the label before .localhost; the app name by default
///   mdns: false     # don't announce wiki.local on the LAN; true by default
/// ```
///
/// Every field is optional and unknown fields are ignored, so blocks
/// written for a newer Alchemy still read. `port` may be a number or a
/// string of digits. Validation happens when routes are planned, so a bad
/// value becomes an app problem rather than a parse error.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct XAlchemy {
    /// The service to route to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The container port to route to.
    #[serde(
        default,
        deserialize_with = "port_number_or_text",
        skip_serializing_if = "Option::is_none"
    )]
    pub port: Option<u16>,
    /// The DNS label to answer under instead of the app's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Whether to announce the app's `.local` name over mDNS.
    #[serde(default = "announce_by_default", skip_serializing_if = "is_announced")]
    pub mdns: bool,
}

impl Default for XAlchemy {
    fn default() -> Self {
        Self {
            service: None,
            port: None,
            hostname: None,
            mdns: true,
        }
    }
}

fn announce_by_default() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_announced(mdns: &bool) -> bool {
    *mdns
}

/// An `x-alchemy` key as the daemon found it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XAlchemyBlock {
    /// A block that read as an [`XAlchemy`].
    Read(XAlchemy),
    /// A block that didn't, such as a list or a port of `"http"`, with the
    /// reader's reason. Planning reports it instead of guessing.
    Unreadable(String),
}

fn port_number_or_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u16>, D::Error> {
    struct PortVisitor;

    impl Visitor<'_> for PortVisitor {
        type Value = Option<u16>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a port number from 1 to 65535")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            u16::try_from(v)
                .map(Some)
                .map_err(|_| E::invalid_value(de::Unexpected::Unsigned(v), &self))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            u16::try_from(v)
                .map(Some)
                .map_err(|_| E::invalid_value(de::Unexpected::Signed(v), &self))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let digits = v.trim();
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(E::invalid_value(de::Unexpected::Str(v), &self));
            }
            digits
                .parse()
                .map(Some)
                .map_err(|_| E::invalid_value(de::Unexpected::Str(v), &self))
        }
    }

    d.deserialize_any(PortVisitor)
}

/// One entry of a service's `expose`: container ports reachable from
/// other containers but not published on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExposedPort {
    /// The container port or ports.
    pub ports: PortRange,
    /// TCP or UDP.
    pub protocol: Protocol,
}

/// Why an `expose` entry was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ExposedPortError {
    /// The port part was not a port or range.
    #[error(transparent)]
    Ports(#[from] PortRangeError),
    /// The protocol part was not `tcp` or `udp`.
    #[error("unsupported protocol in expose entry {0:?}; use tcp or udp")]
    Protocol(String),
}

impl FromStr for ExposedPort {
    type Err = ExposedPortError;

    /// Parse `80`, `8000-8010`, or either with `/tcp` or `/udp`, the forms
    /// Compose prints `expose` in. TCP is the default.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (ports, protocol) = match s.trim().split_once('/') {
            Some((ports, proto)) => (
                ports,
                proto
                    .parse()
                    .map_err(|_| ExposedPortError::Protocol(s.to_owned()))?,
            ),
            None => (s.trim(), Protocol::Tcp),
        };
        Ok(Self {
            ports: ports.parse()?,
            protocol,
        })
    }
}

impl fmt::Display for ExposedPort {
    /// `80/tcp` or `8000-8010/udp`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.ports, self.protocol)
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

    #[test]
    fn x_alchemy_reads() {
        let cases = [
            ("{}", XAlchemy::default()),
            (
                r#"{"service":"web","port":8080,"hostname":"wiki"}"#,
                XAlchemy {
                    service: Some("web".into()),
                    port: Some(8080),
                    hostname: Some("wiki".into()),
                    mdns: true,
                },
            ),
            (
                r#"{"mdns":false}"#,
                XAlchemy {
                    mdns: false,
                    ..XAlchemy::default()
                },
            ),
            (r#"{"mdns":true}"#, XAlchemy::default()),
            (
                r#"{"port":"80"}"#,
                XAlchemy {
                    port: Some(80),
                    ..XAlchemy::default()
                },
            ),
            (r#"{"port":null}"#, XAlchemy::default()),
            (
                r#"{"port":0,"future":true}"#,
                XAlchemy {
                    port: Some(0),
                    ..XAlchemy::default()
                },
            ),
        ];
        for (json, want) in cases {
            assert_eq!(
                serde_json::from_str::<XAlchemy>(json).unwrap(),
                want,
                "{json}"
            );
        }
        let bad = [
            r#"{"port":"http"}"#,
            r#"{"port":""}"#,
            r#"{"port":65536}"#,
            r#"{"port":-1}"#,
            r#"{"port":80.5}"#,
            r#"{"service":3}"#,
            r#"{"mdns":"no"}"#,
            r#"{"mdns":null}"#,
            r#""web""#,
        ];
        for json in bad {
            assert!(serde_json::from_str::<XAlchemy>(json).is_err(), "{json}");
        }
    }

    #[test]
    fn x_alchemy_writes_only_set_fields() {
        let spec = XAlchemy {
            port: Some(80),
            ..XAlchemy::default()
        };
        assert_eq!(serde_json::to_string(&spec).unwrap(), r#"{"port":80}"#);
        let quiet = XAlchemy {
            mdns: false,
            ..XAlchemy::default()
        };
        assert_eq!(serde_json::to_string(&quiet).unwrap(), r#"{"mdns":false}"#);
    }

    #[test]
    fn exposed_port_text() {
        let ok = [
            ("80", "80", Protocol::Tcp, "80/tcp"),
            ("80/tcp", "80", Protocol::Tcp, "80/tcp"),
            ("53/udp", "53", Protocol::Udp, "53/udp"),
            ("8000-8010", "8000-8010", Protocol::Tcp, "8000-8010/tcp"),
            (" 443/TCP ", "443", Protocol::Tcp, "443/tcp"),
        ];
        for (input, ports, protocol, shown) in ok {
            let p: ExposedPort = input.parse().unwrap();
            assert_eq!(p.ports, ports.parse().unwrap(), "{input}");
            assert_eq!(p.protocol, protocol, "{input}");
            assert_eq!(p.to_string(), shown);
        }
        assert_eq!(
            "80/sctp".parse::<ExposedPort>(),
            Err(ExposedPortError::Protocol("80/sctp".into()))
        );
        assert_eq!(
            "0".parse::<ExposedPort>(),
            Err(ExposedPortError::Ports(PortRangeError::Zero))
        );
        assert!("http".parse::<ExposedPort>().is_err());
    }
}
