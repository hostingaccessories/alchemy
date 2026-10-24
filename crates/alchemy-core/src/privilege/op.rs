//! The privileged actions, the helper operations that grant and undo
//! them, and the operations' argv.

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ports::{PortRange, PortRangeError};

/// Something on this machine that needs root, each with an undo.
///
/// Docker group membership is root-equivalent, so it is explained, never
/// an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Let alchemyd listen on ports below 1024, such as 80, on Linux.
    LowPorts,
    /// Open Alchemy's ports in the host firewall.
    Firewall,
    /// Run alchemyd at boot as a system service, as the user.
    SystemService,
}

/// Why an action name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown privilege {0:?}; use low-ports, firewall, or system-service")]
pub struct ActionError(pub String);

impl Action {
    /// Every action, in display order.
    pub const ALL: [Self; 3] = [Self::LowPorts, Self::Firewall, Self::SystemService];

    /// The wire name, such as `low_ports`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LowPorts => "low_ports",
            Self::Firewall => "firewall",
            Self::SystemService => "system_service",
        }
    }

    /// The name on a command line or in a URL path, such as `low-ports`.
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::LowPorts => "low-ports",
            Self::Firewall => "firewall",
            Self::SystemService => "system-service",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Action {
    type Err = ActionError;

    /// Parse either spelling: `low_ports` or `low-ports`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|a| a.as_str() == s || a.cli_name() == s)
            .ok_or_else(|| ActionError(s.to_owned()))
    }
}

/// The lowest port a [`ProxyPorts`] range may start at. The helper opens
/// 80/tcp itself, and nothing else below this.
pub const MIN_PROXY_PORT: u16 = 1024;

/// The TCP ports apps' own addresses are reserved from, opened in the
/// firewall as one range, such as `8100-8999`.
///
/// Never starts below [`MIN_PROXY_PORT`], so the firewall operations can't
/// be used to open SSH or another system port. Its argv form is always
/// `START-END`, even for one port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProxyPorts {
    start: u16,
    end: u16,
}

/// Why a [`ProxyPorts`] range was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProxyPortsError {
    /// The range is malformed, empty, or includes port 0.
    #[error(transparent)]
    Range(#[from] PortRangeError),
    /// The range starts below [`MIN_PROXY_PORT`].
    #[error("the proxy port range can't start below {MIN_PROXY_PORT}, but starts at {0}")]
    Privileged(u16),
}

impl ProxyPorts {
    /// The default range, 8100-8999.
    pub const DEFAULT: Self = Self {
        start: 8100,
        end: 8999,
    };

    /// The ports from `start` to `end`, inclusive.
    pub fn new(start: u16, end: u16) -> Result<Self, ProxyPortsError> {
        PortRange::new(start, end)?.try_into()
    }

    /// The first port.
    pub fn start(&self) -> u16 {
        self.start
    }

    /// The last port.
    pub fn end(&self) -> u16 {
        self.end
    }
}

impl Default for ProxyPorts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<PortRange> for ProxyPorts {
    type Error = ProxyPortsError;

    fn try_from(range: PortRange) -> Result<Self, Self::Error> {
        if range.start() < MIN_PROXY_PORT {
            return Err(ProxyPortsError::Privileged(range.start()));
        }
        Ok(Self {
            start: range.start(),
            end: range.end(),
        })
    }
}

impl fmt::Display for ProxyPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

impl FromStr for ProxyPorts {
    type Err = ProxyPortsError;

    /// Parse `8100-8999`, or a single port such as `8100`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<PortRange>()?.try_into()
    }
}

impl TryFrom<String> for ProxyPorts {
    type Error = ProxyPortsError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ProxyPorts> for String {
    fn from(value: ProxyPorts) -> Self {
        value.to_string()
    }
}

/// The user a system service runs alchemyd as: never root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct ServiceUid(NonZeroU32);

/// Why a [`ServiceUid`] was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ServiceUidError {
    /// UID 0: alchemyd never runs as root.
    #[error("alchemyd never runs as root")]
    Root,
}

impl ServiceUid {
    /// `uid`, unless it is root's.
    pub fn new(uid: u32) -> Result<Self, ServiceUidError> {
        NonZeroU32::new(uid).map(Self).ok_or(ServiceUidError::Root)
    }

    /// The UID.
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for ServiceUid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<u32> for ServiceUid {
    type Error = ServiceUidError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ServiceUid> for u32 {
    fn from(value: ServiceUid) -> Self {
        value.get()
    }
}

/// One `alchemy-helper` operation: a subcommand and its typed arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelperOp {
    /// `grant-low-ports`: set `cap_net_bind_service` on the sibling
    /// alchemyd (Linux).
    GrantLowPorts,
    /// `revoke-low-ports`: clear it again.
    RevokeLowPorts,
    /// `open-firewall`: allow 80/tcp, the proxy range over TCP, and
    /// 5353/udp, or on macOS add and unblock the sibling alchemyd.
    OpenFirewall {
        /// The proxy range.
        proxy_ports: ProxyPorts,
    },
    /// `close-firewall`: remove exactly what `open-firewall` added.
    CloseFirewall {
        /// The proxy range.
        proxy_ports: ProxyPorts,
    },
    /// `install-service`: run the sibling alchemyd at boot as `uid`.
    InstallService {
        /// The user it runs as.
        uid: ServiceUid,
    },
    /// `remove-service`: stop and delete that service.
    RemoveService {
        /// The user it runs as.
        uid: ServiceUid,
    },
}

impl HelperOp {
    /// The action the operation grants or undoes.
    pub fn action(&self) -> Action {
        match self {
            Self::GrantLowPorts | Self::RevokeLowPorts => Action::LowPorts,
            Self::OpenFirewall { .. } | Self::CloseFirewall { .. } => Action::Firewall,
            Self::InstallService { .. } | Self::RemoveService { .. } => Action::SystemService,
        }
    }

    /// Whether the operation grants its action, rather than undoing it.
    pub fn grants(&self) -> bool {
        matches!(
            self,
            Self::GrantLowPorts | Self::OpenFirewall { .. } | Self::InstallService { .. }
        )
    }

    /// The operation that reverses this one, with the same arguments.
    #[must_use]
    pub fn undo(&self) -> Self {
        match *self {
            Self::GrantLowPorts => Self::RevokeLowPorts,
            Self::RevokeLowPorts => Self::GrantLowPorts,
            Self::OpenFirewall { proxy_ports } => Self::CloseFirewall { proxy_ports },
            Self::CloseFirewall { proxy_ports } => Self::OpenFirewall { proxy_ports },
            Self::InstallService { uid } => Self::RemoveService { uid },
            Self::RemoveService { uid } => Self::InstallService { uid },
        }
    }

    /// The helper subcommand, such as `grant-low-ports`.
    pub fn subcommand(&self) -> &'static str {
        match self {
            Self::GrantLowPorts => "grant-low-ports",
            Self::RevokeLowPorts => "revoke-low-ports",
            Self::OpenFirewall { .. } => "open-firewall",
            Self::CloseFirewall { .. } => "close-firewall",
            Self::InstallService { .. } => "install-service",
            Self::RemoveService { .. } => "remove-service",
        }
    }

    /// The argv after the helper's path: the subcommand, then each flag
    /// and its value as separate elements.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![self.subcommand().to_owned()];
        match self {
            Self::GrantLowPorts | Self::RevokeLowPorts => {}
            Self::OpenFirewall { proxy_ports } | Self::CloseFirewall { proxy_ports } => {
                args.extend(["--proxy-ports".to_owned(), proxy_ports.to_string()]);
            }
            Self::InstallService { uid } | Self::RemoveService { uid } => {
                args.extend(["--uid".to_owned(), uid.to_string()]);
            }
        }
        args
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
    fn action_names() {
        let cases = [
            (Action::LowPorts, "low_ports", "low-ports"),
            (Action::Firewall, "firewall", "firewall"),
            (Action::SystemService, "system_service", "system-service"),
        ];
        for (action, wire, cli) in cases {
            assert_eq!(action.as_str(), wire);
            assert_eq!(action.to_string(), wire);
            assert_eq!(action.cli_name(), cli);
            assert_eq!(wire.parse::<Action>().unwrap(), action);
            assert_eq!(cli.parse::<Action>().unwrap(), action);
            assert_eq!(serde_json::to_value(action).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<Action>(wire.into()).unwrap(),
                action
            );
        }
        for bad in ["", "docker-group", "Low-Ports", "low ports"] {
            assert_eq!(bad.parse::<Action>(), Err(ActionError(bad.into())), "{bad}");
        }
        assert_eq!(Action::ALL.len(), 3);
    }

    #[test]
    fn proxy_ports() {
        type Case = (&'static str, Result<(u16, u16), ProxyPortsError>);
        let cases: [Case; 8] = [
            ("8100-8999", Ok((8100, 8999))),
            ("1024-1024", Ok((1024, 1024))),
            ("9000", Ok((9000, 9000))),
            ("1023-2000", Err(ProxyPortsError::Privileged(1023))),
            ("22", Err(ProxyPortsError::Privileged(22))),
            ("0-9000", Err(PortRangeError::Zero.into())),
            (
                "9000-8100",
                Err(PortRangeError::Reversed {
                    start: 9000,
                    end: 8100,
                }
                .into()),
            ),
            ("8100-", Err(PortRangeError::Invalid("8100-".into()).into())),
        ];
        for (text, want) in cases {
            let got = text.parse::<ProxyPorts>();
            assert_eq!(got.map(|p| (p.start(), p.end())), want, "{text}");
        }
        assert_eq!(ProxyPorts::new(8100, 8999).unwrap(), ProxyPorts::DEFAULT);
        assert_eq!(ProxyPorts::default(), ProxyPorts::DEFAULT);
        assert_eq!(
            ProxyPorts::new(80, 80),
            Err(ProxyPortsError::Privileged(80))
        );
        assert_eq!(
            ProxyPorts::new(9000, 9000).unwrap().to_string(),
            "9000-9000"
        );
        let json = serde_json::to_value(ProxyPorts::DEFAULT).unwrap();
        assert_eq!(json, "8100-8999");
        assert_eq!(
            serde_json::from_value::<ProxyPorts>(json).unwrap(),
            ProxyPorts::DEFAULT
        );
        assert!(serde_json::from_value::<ProxyPorts>("80-90".into()).is_err());
    }

    #[test]
    fn service_uid() {
        assert_eq!(ServiceUid::new(0), Err(ServiceUidError::Root));
        assert_eq!(ServiceUid::new(501).unwrap().get(), 501);
        assert_eq!(ServiceUid::new(u32::MAX).unwrap().to_string(), "4294967295");
        assert_eq!(
            serde_json::to_value(ServiceUid::new(1000).unwrap()).unwrap(),
            1000
        );
        assert!(serde_json::from_value::<ServiceUid>(0.into()).is_err());
    }

    #[test]
    fn helper_ops() {
        let ports = ProxyPorts::DEFAULT;
        let uid = ServiceUid::new(501).unwrap();
        let cases = [
            (
                HelperOp::GrantLowPorts,
                Action::LowPorts,
                true,
                vec!["grant-low-ports"],
            ),
            (
                HelperOp::RevokeLowPorts,
                Action::LowPorts,
                false,
                vec!["revoke-low-ports"],
            ),
            (
                HelperOp::OpenFirewall { proxy_ports: ports },
                Action::Firewall,
                true,
                vec!["open-firewall", "--proxy-ports", "8100-8999"],
            ),
            (
                HelperOp::CloseFirewall { proxy_ports: ports },
                Action::Firewall,
                false,
                vec!["close-firewall", "--proxy-ports", "8100-8999"],
            ),
            (
                HelperOp::InstallService { uid },
                Action::SystemService,
                true,
                vec!["install-service", "--uid", "501"],
            ),
            (
                HelperOp::RemoveService { uid },
                Action::SystemService,
                false,
                vec!["remove-service", "--uid", "501"],
            ),
        ];
        for (op, action, grants, argv) in cases {
            assert_eq!(op.action(), action, "{op:?}");
            assert_eq!(op.grants(), grants, "{op:?}");
            assert_eq!(op.args(), argv, "{op:?}");
            assert_eq!(op.subcommand(), argv[0], "{op:?}");
            let undo = op.undo();
            assert_eq!(undo.action(), action, "{op:?}");
            assert_eq!(undo.grants(), !grants, "{op:?}");
            assert_eq!(undo.undo(), op, "{op:?}");
            assert_eq!(undo.args()[1..], op.args()[1..], "{op:?}: same arguments");
        }
    }
}
