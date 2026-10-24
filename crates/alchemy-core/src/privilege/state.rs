//! Each action's state on this machine, and what to run to grant or undo
//! it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::elevate::{CommandLine, Elevation, TerminalReason, elevated_command, elevation};
use super::helper::{HelperPath, HelperTrust};
use super::op::{Action, HelperOp, ProxyPorts, ServiceUid};
use crate::route::HostOs;

/// How the Host listener (port 80 by default) is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostListener {
    /// Routing by name is turned off, so nothing listens.
    Off,
    /// It is listening.
    Listening,
    /// Binding failed with `EACCES`: a low port without the capability.
    Denied,
    /// Binding failed for another reason, such as the port being taken.
    /// Linux checks permission first, so this isn't a privilege problem.
    Failed,
}

/// What decides the low-ports action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LowPortFacts {
    /// The Host listener.
    pub host_listener: HostListener,
    /// Whether the running alchemyd holds `cap_net_bind_service`.
    pub capable: bool,
}

/// A host firewall Alchemy knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Firewall {
    /// ufw is active.
    Ufw,
    /// firewalld is running.
    Firewalld,
    /// The macOS application firewall is on.
    ApplicationFirewall,
    /// Raw nftables or iptables rules with neither ufw nor firewalld:
    /// reported, never edited.
    Unmanaged,
}

/// What decides the firewall action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FirewallFacts {
    /// The active firewall, or `None` when there is none.
    pub active: Option<Firewall>,
    /// Whether Alchemy's ufw profile or firewalld service is allowed, or
    /// alchemyd is added and unblocked in the application firewall.
    pub opened: bool,
}

/// What decides the system-service action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServiceFacts {
    /// Whether the service is installed.
    pub installed: bool,
    /// Whether alchemyd should run at boot. M9 decides when; until then
    /// the daemon passes `false`.
    pub wanted: bool,
}

/// The helper alchemyd would run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HelperFacts {
    /// Its canonical path.
    pub path: HelperPath,
    /// Whether a prompt in front of it is safe, from
    /// [`helper_trust`](super::helper_trust).
    pub trust: HelperTrust,
}

/// Everything the privilege states are decided from, gathered by the
/// daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeFacts {
    /// The OS alchemyd runs on.
    pub os: HostOs,
    /// The canonical path of the running alchemyd.
    pub alchemyd: PathBuf,
    /// The UID alchemyd runs as.
    pub uid: u32,
    /// The helper, or `None` when there is none to run.
    pub helper: Option<HelperFacts>,
    /// Whether a polkit agent answers, on Linux.
    pub polkit_agent: bool,
    /// The proxy range the firewall opens.
    pub proxy_ports: ProxyPorts,
    /// For low ports.
    pub low_ports: LowPortFacts,
    /// For the firewall.
    pub firewall: FirewallFacts,
    /// For the system service.
    pub service: ServiceFacts,
    /// Actions with a grant or undo in flight.
    pub pending: BTreeSet<Action>,
}

/// Where an action stands on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PrivilegeState {
    /// Nothing to do here, such as low ports on macOS or no firewall.
    NotNeeded,
    /// Not done yet, and a system prompt can do it.
    Needed,
    /// Done, and can be undone.
    Granted,
    /// A grant or undo is in flight, such as while the prompt is up.
    Pending,
    /// Not done yet, and no prompt is safe or available, so the user runs
    /// `command` in a terminal.
    NeedsTerminal {
        /// The exact `sudo alchemy-helper ...` command.
        command: CommandLine,
        /// Why there is no prompt.
        reason: TerminalReason,
    },
    /// Alchemy can't do it here.
    Unsupported {
        /// Why.
        reason: UnsupportedReason,
    },
}

impl PrivilegeState {
    /// The wire name, such as `needs_terminal`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotNeeded => "not_needed",
            Self::Needed => "needed",
            Self::Granted => "granted",
            Self::Pending => "pending",
            Self::NeedsTerminal { .. } => "needs_terminal",
            Self::Unsupported { .. } => "unsupported",
        }
    }
}

/// Why Alchemy can't do an action on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum UnsupportedReason {
    /// There is no `alchemy-helper` to run.
    NoHelper,
    /// The helper only acts on the alchemyd next to it, and that isn't
    /// the one running.
    NotHelperSibling {
        /// The running alchemyd.
        alchemyd: PathBuf,
        /// The alchemyd the helper would act on.
        sibling: PathBuf,
    },
    /// The firewall is raw nftables or iptables, which Alchemy never
    /// edits.
    UnmanagedFirewall {
        /// The proxy range to allow by hand.
        proxy_ports: ProxyPorts,
    },
    /// alchemyd runs as root, and a service never does.
    RunsAsRoot,
}

impl fmt::Display for UnsupportedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHelper => f.write_str("Alchemy can't find alchemy-helper next to alchemyd."),
            Self::NotHelperSibling { alchemyd, sibling } => write!(
                f,
                "alchemy-helper only changes {}, the alchemyd next to it, but this alchemyd runs from {}.",
                sibling.display(),
                alchemyd.display()
            ),
            Self::UnmanagedFirewall { proxy_ports } => write!(
                f,
                "Alchemy only changes ufw and firewalld, not this firewall's rules. Allow 80/tcp, {proxy_ports}/tcp, and 5353/udp yourself."
            ),
            Self::RunsAsRoot => f.write_str(
                "alchemyd is running as root, and Alchemy never installs it to run as root.",
            ),
        }
    }
}

/// What to do to grant or undo an action.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Step {
    /// It is already that way, or isn't needed.
    Nothing,
    /// A grant or undo is already in flight.
    Busy,
    /// Alchemy can't do it here.
    Unsupported(UnsupportedReason),
    /// Run this command: it shows a system prompt, then runs the helper.
    Prompt(CommandLine),
    /// Ask the user to run this `sudo` command in a terminal.
    Terminal {
        /// The command.
        command: CommandLine,
        /// Why there is no prompt.
        reason: TerminalReason,
    },
}

enum Base {
    NotNeeded,
    Needed,
    Granted,
    Unsupported(UnsupportedReason),
}

fn base(action: Action, facts: &PrivilegeFacts) -> Base {
    match action {
        Action::LowPorts => match (facts.os, facts.low_ports) {
            (HostOs::Linux, LowPortFacts { capable: true, .. }) => Base::Granted,
            (
                HostOs::Linux,
                LowPortFacts {
                    host_listener: HostListener::Denied,
                    ..
                },
            ) => Base::Needed,
            (HostOs::MacOs | HostOs::Linux, _) => Base::NotNeeded,
        },
        Action::Firewall => match facts.firewall {
            FirewallFacts { active: None, .. } => Base::NotNeeded,
            FirewallFacts {
                active: Some(Firewall::Unmanaged),
                ..
            } => Base::Unsupported(UnsupportedReason::UnmanagedFirewall {
                proxy_ports: facts.proxy_ports,
            }),
            FirewallFacts { opened: true, .. } => Base::Granted,
            FirewallFacts { opened: false, .. } => Base::Needed,
        },
        Action::SystemService => match facts.service {
            ServiceFacts {
                installed: true, ..
            } => Base::Granted,
            ServiceFacts { wanted: true, .. } => Base::Needed,
            ServiceFacts { wanted: false, .. } => Base::NotNeeded,
        },
    }
}

fn needs_sibling(action: Action, facts: &PrivilegeFacts) -> bool {
    match action {
        Action::LowPorts | Action::SystemService => true,
        Action::Firewall => facts.firewall.active == Some(Firewall::ApplicationFirewall),
    }
}

fn operation(
    action: Action,
    grant: bool,
    facts: &PrivilegeFacts,
) -> Result<HelperOp, UnsupportedReason> {
    let op = match action {
        Action::LowPorts => HelperOp::GrantLowPorts,
        Action::Firewall => HelperOp::OpenFirewall {
            proxy_ports: facts.proxy_ports,
        },
        Action::SystemService => HelperOp::InstallService {
            uid: ServiceUid::new(facts.uid).map_err(|_| UnsupportedReason::RunsAsRoot)?,
        },
    };
    Ok(if grant { op } else { op.undo() })
}

fn run(action: Action, grant: bool, facts: &PrivilegeFacts) -> Step {
    let Some(helper) = &facts.helper else {
        return Step::Unsupported(UnsupportedReason::NoHelper);
    };
    let sibling = helper.path.sibling_alchemyd();
    if needs_sibling(action, facts) && sibling != facts.alchemyd {
        return Step::Unsupported(UnsupportedReason::NotHelperSibling {
            alchemyd: facts.alchemyd.clone(),
            sibling,
        });
    }
    let op = match operation(action, grant, facts) {
        Ok(op) => op,
        Err(reason) => return Step::Unsupported(reason),
    };
    let how = elevation(facts.os, &helper.trust, facts.polkit_agent);
    let command = elevated_command(&how, &helper.path, &op);
    match how {
        Elevation::Terminal { reason } => Step::Terminal { command, reason },
        Elevation::Osascript | Elevation::Pkexec => Step::Prompt(command),
    }
}

/// What to do to grant (`grant` true) or undo `action`.
///
/// Granting runs the helper only when the action is
/// [`Needed`](PrivilegeState::Needed); undoing only when it is
/// [`Granted`](PrivilegeState::Granted). Low ports, the macOS firewall
/// entry, and the service all act on the helper's sibling alchemyd, so
/// they are [`Unsupported`](Step::Unsupported) when that isn't the one
/// running.
pub fn step(action: Action, grant: bool, facts: &PrivilegeFacts) -> Step {
    if facts.pending.contains(&action) {
        return Step::Busy;
    }
    match (base(action, facts), grant) {
        (Base::Unsupported(reason), true) => Step::Unsupported(reason),
        (Base::Needed, true) | (Base::Granted, false) => run(action, grant, facts),
        (Base::NotNeeded | Base::Granted, true)
        | (Base::NotNeeded | Base::Needed | Base::Unsupported(_), false) => Step::Nothing,
    }
}

/// Where `action` stands on this machine.
///
/// A needed action is [`NeedsTerminal`](PrivilegeState::NeedsTerminal)
/// when granting it means a `sudo` command, and
/// [`Unsupported`](PrivilegeState::Unsupported) when it can't be granted
/// at all. A granted action stays granted even when undoing it would need
/// the terminal; [`step`] says so when the undo is asked for.
pub fn state(action: Action, facts: &PrivilegeFacts) -> PrivilegeState {
    if facts.pending.contains(&action) {
        return PrivilegeState::Pending;
    }
    match base(action, facts) {
        Base::NotNeeded => PrivilegeState::NotNeeded,
        Base::Granted => PrivilegeState::Granted,
        Base::Unsupported(reason) => PrivilegeState::Unsupported { reason },
        Base::Needed => match run(action, true, facts) {
            Step::Terminal { command, reason } => PrivilegeState::NeedsTerminal { command, reason },
            Step::Unsupported(reason) => PrivilegeState::Unsupported { reason },
            Step::Prompt(_) | Step::Nothing | Step::Busy => PrivilegeState::Needed,
        },
    }
}

/// Every action's [`state`], in [`Action::ALL`] order.
pub fn states(facts: &PrivilegeFacts) -> BTreeMap<Action, PrivilegeState> {
    Action::ALL
        .into_iter()
        .map(|action| (action, state(action, facts)))
        .collect()
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
    use crate::privilege::{Distrust, OSASCRIPT, PKEXEC};

    const HELPER: &str = "/usr/libexec/alchemy/alchemy-helper";
    const ALCHEMYD: &str = "/usr/libexec/alchemy/alchemyd";

    fn facts(os: HostOs) -> PrivilegeFacts {
        PrivilegeFacts {
            os,
            alchemyd: PathBuf::from(ALCHEMYD),
            uid: 1000,
            helper: Some(HelperFacts {
                path: HelperPath::new(HELPER).unwrap(),
                trust: HelperTrust::Trusted,
            }),
            polkit_agent: true,
            proxy_ports: ProxyPorts::DEFAULT,
            low_ports: LowPortFacts {
                host_listener: HostListener::Listening,
                capable: false,
            },
            firewall: FirewallFacts {
                active: None,
                opened: false,
            },
            service: ServiceFacts {
                installed: false,
                wanted: false,
            },
            pending: BTreeSet::new(),
        }
    }

    fn untrusted() -> HelperTrust {
        HelperTrust::Untrusted {
            why: Distrust::NotRootOwned {
                path: PathBuf::from(HELPER),
                uid: 1000,
            },
        }
    }

    fn sudo(args: &[&str]) -> CommandLine {
        let helper = HelperPath::new(HELPER).unwrap();
        let mut line = vec!["sudo".to_owned(), helper.as_str().to_owned()];
        line.extend(args.iter().map(|a| (*a).to_owned()));
        serde_json::from_value(serde_json::json!({ "argv": line })).unwrap()
    }

    fn denied(f: &mut PrivilegeFacts) {
        f.low_ports.host_listener = HostListener::Denied;
    }

    fn not_sibling() -> UnsupportedReason {
        UnsupportedReason::NotHelperSibling {
            alchemyd: PathBuf::from("/home/me/.cargo/bin/alchemyd"),
            sibling: PathBuf::from(ALCHEMYD),
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn low_ports_states() {
        type Case = (
            &'static str,
            HostOs,
            fn(&mut PrivilegeFacts),
            PrivilegeState,
        );
        let cases: Vec<Case> = vec![
            (
                "macOS never needs it",
                HostOs::MacOs,
                denied,
                PrivilegeState::NotNeeded,
            ),
            (
                "macOS, even with the capability",
                HostOs::MacOs,
                |f| f.low_ports.capable = true,
                PrivilegeState::NotNeeded,
            ),
            (
                "listening",
                HostOs::Linux,
                |_| {},
                PrivilegeState::NotNeeded,
            ),
            (
                "routing by name off",
                HostOs::Linux,
                |f| f.low_ports.host_listener = HostListener::Off,
                PrivilegeState::NotNeeded,
            ),
            (
                "port taken",
                HostOs::Linux,
                |f| f.low_ports.host_listener = HostListener::Failed,
                PrivilegeState::NotNeeded,
            ),
            ("denied", HostOs::Linux, denied, PrivilegeState::Needed),
            (
                "granted",
                HostOs::Linux,
                |f| f.low_ports.capable = true,
                PrivilegeState::Granted,
            ),
            (
                "granted, still denied before the re-exec",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.low_ports.capable = true;
                },
                PrivilegeState::Granted,
            ),
            (
                "prompt up",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.pending.insert(Action::LowPorts);
                },
                PrivilegeState::Pending,
            ),
            (
                "another action pending",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.pending.insert(Action::Firewall);
                },
                PrivilegeState::Needed,
            ),
            (
                "no polkit agent",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.polkit_agent = false;
                },
                PrivilegeState::NeedsTerminal {
                    command: sudo(&["grant-low-ports"]),
                    reason: TerminalReason::NoPolkitAgent,
                },
            ),
            (
                "dev build",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.helper.as_mut().unwrap().trust = untrusted();
                },
                PrivilegeState::NeedsTerminal {
                    command: sudo(&["grant-low-ports"]),
                    reason: TerminalReason::UntrustedHelper {
                        why: Distrust::NotRootOwned {
                            path: PathBuf::from(HELPER),
                            uid: 1000,
                        },
                    },
                },
            ),
            (
                "not the helper's sibling",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.alchemyd = PathBuf::from("/home/me/.cargo/bin/alchemyd");
                },
                PrivilegeState::Unsupported {
                    reason: not_sibling(),
                },
            ),
            (
                "not the sibling outranks the terminal",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.polkit_agent = false;
                    f.alchemyd = PathBuf::from("/home/me/.cargo/bin/alchemyd");
                },
                PrivilegeState::Unsupported {
                    reason: not_sibling(),
                },
            ),
            (
                "no helper",
                HostOs::Linux,
                |f| {
                    denied(f);
                    f.helper = None;
                },
                PrivilegeState::Unsupported {
                    reason: UnsupportedReason::NoHelper,
                },
            ),
        ];
        for (name, os, tweak, want) in cases {
            let mut f = facts(os);
            tweak(&mut f);
            assert_eq!(state(Action::LowPorts, &f), want, "{name}");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn firewall_states() {
        let on = |firewall: Firewall, opened: bool| FirewallFacts {
            active: Some(firewall),
            opened,
        };
        let cases: Vec<(&str, HostOs, FirewallFacts, &str, PrivilegeState)> = vec![
            (
                "no firewall",
                HostOs::Linux,
                FirewallFacts {
                    active: None,
                    opened: false,
                },
                ALCHEMYD,
                PrivilegeState::NotNeeded,
            ),
            (
                "ufw closed",
                HostOs::Linux,
                on(Firewall::Ufw, false),
                ALCHEMYD,
                PrivilegeState::Needed,
            ),
            (
                "ufw opened",
                HostOs::Linux,
                on(Firewall::Ufw, true),
                ALCHEMYD,
                PrivilegeState::Granted,
            ),
            (
                "firewalld closed",
                HostOs::Linux,
                on(Firewall::Firewalld, false),
                ALCHEMYD,
                PrivilegeState::Needed,
            ),
            (
                "firewalld, any alchemyd",
                HostOs::Linux,
                on(Firewall::Firewalld, false),
                "/home/me/.cargo/bin/alchemyd",
                PrivilegeState::Needed,
            ),
            (
                "raw nftables",
                HostOs::Linux,
                on(Firewall::Unmanaged, false),
                ALCHEMYD,
                PrivilegeState::Unsupported {
                    reason: UnsupportedReason::UnmanagedFirewall {
                        proxy_ports: ProxyPorts::DEFAULT,
                    },
                },
            ),
            (
                "macOS firewall on",
                HostOs::MacOs,
                on(Firewall::ApplicationFirewall, false),
                ALCHEMYD,
                PrivilegeState::Needed,
            ),
            (
                "macOS firewall, alchemyd added",
                HostOs::MacOs,
                on(Firewall::ApplicationFirewall, true),
                ALCHEMYD,
                PrivilegeState::Granted,
            ),
            (
                "macOS firewall, not the sibling",
                HostOs::MacOs,
                on(Firewall::ApplicationFirewall, false),
                "/home/me/.cargo/bin/alchemyd",
                PrivilegeState::Unsupported {
                    reason: not_sibling(),
                },
            ),
        ];
        for (name, os, firewall, alchemyd, want) in cases {
            let mut f = facts(os);
            f.firewall = firewall;
            f.alchemyd = PathBuf::from(alchemyd);
            assert_eq!(state(Action::Firewall, &f), want, "{name}");
        }
    }

    #[test]
    fn service_states() {
        let cases: Vec<(&str, bool, bool, u32, PrivilegeState)> = vec![
            ("not wanted", false, false, 1000, PrivilegeState::NotNeeded),
            ("wanted", false, true, 1000, PrivilegeState::Needed),
            ("installed", true, false, 1000, PrivilegeState::Granted),
            (
                "installed and wanted",
                true,
                true,
                1000,
                PrivilegeState::Granted,
            ),
            (
                "alchemyd runs as root",
                false,
                true,
                0,
                PrivilegeState::Unsupported {
                    reason: UnsupportedReason::RunsAsRoot,
                },
            ),
        ];
        for (name, installed, wanted, uid, want) in cases {
            for os in [HostOs::Linux, HostOs::MacOs] {
                let mut f = facts(os);
                f.service = ServiceFacts { installed, wanted };
                f.uid = uid;
                assert_eq!(state(Action::SystemService, &f), want, "{name} on {os:?}");
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn steps() {
        type Case = (
            &'static str,
            HostOs,
            Action,
            bool,
            fn(&mut PrivilegeFacts),
            Box<dyn Fn(&Step) -> bool>,
        );
        let helper_args = |args: &[&str]| -> Vec<String> {
            std::iter::once(HELPER)
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect()
        };
        let prompt = |program: &str, args: &[&str]| -> Box<dyn Fn(&Step) -> bool> {
            let program = program.to_owned();
            let tail = helper_args(args);
            Box::new(move |step| match step {
                Step::Prompt(c) => c.program() == program && c.argv().ends_with(&tail),
                _ => false,
            })
        };
        let is = |want: Step| -> Box<dyn Fn(&Step) -> bool> { Box::new(move |s| *s == want) };
        let cases: Vec<Case> = vec![
            (
                "grant low ports with pkexec",
                HostOs::Linux,
                Action::LowPorts,
                true,
                denied,
                prompt(PKEXEC, &["grant-low-ports"]),
            ),
            (
                "revoke low ports",
                HostOs::Linux,
                Action::LowPorts,
                false,
                |f| f.low_ports.capable = true,
                prompt(PKEXEC, &["revoke-low-ports"]),
            ),
            (
                "revoke low ports from a terminal",
                HostOs::Linux,
                Action::LowPorts,
                false,
                |f| {
                    f.low_ports.capable = true;
                    f.polkit_agent = false;
                },
                is(Step::Terminal {
                    command: sudo(&["revoke-low-ports"]),
                    reason: TerminalReason::NoPolkitAgent,
                }),
            ),
            (
                "revoke low ports, not the sibling",
                HostOs::Linux,
                Action::LowPorts,
                false,
                |f| {
                    f.low_ports.capable = true;
                    f.alchemyd = PathBuf::from("/home/me/.cargo/bin/alchemyd");
                },
                is(Step::Unsupported(not_sibling())),
            ),
            (
                "grant low ports already granted",
                HostOs::Linux,
                Action::LowPorts,
                true,
                |f| f.low_ports.capable = true,
                is(Step::Nothing),
            ),
            (
                "revoke low ports never granted",
                HostOs::Linux,
                Action::LowPorts,
                false,
                denied,
                is(Step::Nothing),
            ),
            (
                "grant low ports on macOS",
                HostOs::MacOs,
                Action::LowPorts,
                true,
                denied,
                is(Step::Nothing),
            ),
            (
                "grant while the prompt is up",
                HostOs::Linux,
                Action::LowPorts,
                true,
                |f| {
                    denied(f);
                    f.pending.insert(Action::LowPorts);
                },
                is(Step::Busy),
            ),
            (
                "open ufw",
                HostOs::Linux,
                Action::Firewall,
                true,
                |f| {
                    f.firewall.active = Some(Firewall::Ufw);
                    f.proxy_ports = ProxyPorts::new(9000, 9099).unwrap();
                },
                prompt(PKEXEC, &["open-firewall", "--proxy-ports", "9000-9099"]),
            ),
            (
                "close firewalld",
                HostOs::Linux,
                Action::Firewall,
                false,
                |f| {
                    f.firewall = FirewallFacts {
                        active: Some(Firewall::Firewalld),
                        opened: true,
                    };
                },
                prompt(PKEXEC, &["close-firewall", "--proxy-ports", "8100-8999"]),
            ),
            (
                "open the macOS firewall with osascript",
                HostOs::MacOs,
                Action::Firewall,
                true,
                |f| f.firewall.active = Some(Firewall::ApplicationFirewall),
                prompt(OSASCRIPT, &["open-firewall", "--proxy-ports", "8100-8999"]),
            ),
            (
                "open raw nftables",
                HostOs::Linux,
                Action::Firewall,
                true,
                |f| f.firewall.active = Some(Firewall::Unmanaged),
                is(Step::Unsupported(UnsupportedReason::UnmanagedFirewall {
                    proxy_ports: ProxyPorts::DEFAULT,
                })),
            ),
            (
                "close raw nftables",
                HostOs::Linux,
                Action::Firewall,
                false,
                |f| f.firewall.active = Some(Firewall::Unmanaged),
                is(Step::Nothing),
            ),
            (
                "install the service on macOS",
                HostOs::MacOs,
                Action::SystemService,
                true,
                |f| f.service.wanted = true,
                prompt(OSASCRIPT, &["install-service", "--uid", "1000"]),
            ),
            (
                "remove the service from a dev build",
                HostOs::MacOs,
                Action::SystemService,
                false,
                |f| {
                    f.service.installed = true;
                    f.helper.as_mut().unwrap().trust = untrusted();
                },
                is(Step::Terminal {
                    command: sudo(&["remove-service", "--uid", "1000"]),
                    reason: TerminalReason::UntrustedHelper {
                        why: Distrust::NotRootOwned {
                            path: PathBuf::from(HELPER),
                            uid: 1000,
                        },
                    },
                }),
            ),
            (
                "remove the service as root",
                HostOs::Linux,
                Action::SystemService,
                false,
                |f| {
                    f.service.installed = true;
                    f.uid = 0;
                },
                is(Step::Unsupported(UnsupportedReason::RunsAsRoot)),
            ),
            (
                "no helper",
                HostOs::Linux,
                Action::SystemService,
                true,
                |f| {
                    f.service.wanted = true;
                    f.helper = None;
                },
                is(Step::Unsupported(UnsupportedReason::NoHelper)),
            ),
        ];
        for (name, os, action, grant, tweak, check) in cases {
            let mut f = facts(os);
            tweak(&mut f);
            let got = step(action, grant, &f);
            assert!(check(&got), "{name}: {got:?}");
        }
    }

    #[test]
    fn all_states() {
        let mut f = facts(HostOs::Linux);
        denied(&mut f);
        f.firewall.active = Some(Firewall::Ufw);
        f.firewall.opened = true;
        let got = states(&f);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec![
                (Action::LowPorts, PrivilegeState::Needed),
                (Action::Firewall, PrivilegeState::Granted),
                (Action::SystemService, PrivilegeState::NotNeeded),
            ]
        );
    }

    #[test]
    fn state_names_and_wire() {
        let cases = [
            (PrivilegeState::NotNeeded, "not_needed"),
            (PrivilegeState::Needed, "needed"),
            (PrivilegeState::Granted, "granted"),
            (PrivilegeState::Pending, "pending"),
            (
                PrivilegeState::NeedsTerminal {
                    command: sudo(&["grant-low-ports"]),
                    reason: TerminalReason::NoPolkitAgent,
                },
                "needs_terminal",
            ),
            (
                PrivilegeState::Unsupported {
                    reason: UnsupportedReason::NoHelper,
                },
                "unsupported",
            ),
        ];
        for (state, name) in cases {
            assert_eq!(state.as_str(), name);
            let json = serde_json::to_value(&state).unwrap();
            assert_eq!(json["state"], name);
            assert_eq!(
                serde_json::from_value::<PrivilegeState>(json).unwrap(),
                state
            );
        }
        assert_eq!(
            serde_json::to_value(PrivilegeState::NeedsTerminal {
                command: sudo(&["grant-low-ports"]),
                reason: TerminalReason::NoPolkitAgent,
            })
            .unwrap(),
            serde_json::json!({
                "state": "needs_terminal",
                "command": {"argv": ["sudo", HELPER, "grant-low-ports"]},
                "reason": {"kind": "no_polkit_agent"},
            })
        );
        assert_eq!(
            serde_json::to_value(PrivilegeState::Unsupported {
                reason: not_sibling()
            })
            .unwrap(),
            serde_json::json!({
                "state": "unsupported",
                "reason": {
                    "kind": "not_helper_sibling",
                    "alchemyd": "/home/me/.cargo/bin/alchemyd",
                    "sibling": ALCHEMYD,
                },
            })
        );
        assert_eq!(
            serde_json::to_value(Firewall::ApplicationFirewall).unwrap(),
            "application_firewall"
        );
    }

    #[test]
    fn unsupported_text() {
        let cases = [
            (
                UnsupportedReason::NoHelper,
                "Alchemy can't find alchemy-helper next to alchemyd.",
            ),
            (
                not_sibling(),
                "alchemy-helper only changes /usr/libexec/alchemy/alchemyd, the alchemyd next to it, but this alchemyd runs from /home/me/.cargo/bin/alchemyd.",
            ),
            (
                UnsupportedReason::UnmanagedFirewall {
                    proxy_ports: ProxyPorts::DEFAULT,
                },
                "Alchemy only changes ufw and firewalld, not this firewall's rules. Allow 80/tcp, 8100-8999/tcp, and 5353/udp yourself.",
            ),
            (
                UnsupportedReason::RunsAsRoot,
                "alchemyd is running as root, and Alchemy never installs it to run as root.",
            ),
        ];
        for (reason, want) in cases {
            assert_eq!(reason.to_string(), want);
        }
    }
}
