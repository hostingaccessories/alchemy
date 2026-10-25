//! What the OS says about alchemyd, its helper, and the firewall, read
//! through a [`PrivilegeSystem`], and the pure parsers for what it prints.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use alchemy_core::privilege::{FileMeta, Firewall, FirewallFacts, HelperPath};
use alchemy_core::route::HostOs;
use futures_util::future::{self, BoxFuture};

/// Reads the facts the privilege states are decided from.
///
/// Object safe, so the daemon can hold any implementation. Every call
/// reads afresh; nothing is cached.
pub trait PrivilegeSystem: Send + Sync + fmt::Debug {
    /// Read everything [`SystemFacts`] holds.
    fn read(&self) -> BoxFuture<'_, Result<SystemFacts, FactsError>>;
}

/// Why the facts could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FactsError {
    /// alchemyd's own binary could not be found, such as after it was
    /// deleted or replaced while running.
    #[error("cannot find alchemyd's own binary: {0}")]
    CurrentExe(String),
    /// A background read panicked or was cancelled.
    #[error("reading the privilege facts failed: {0}")]
    Task(String),
}

/// What the OS says, before any decision is made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SystemFacts {
    /// The canonical path of the running alchemyd.
    pub alchemyd: PathBuf,
    /// The UID alchemyd runs as.
    pub uid: u32,
    /// The helper next to alchemyd, with the `lstat` facts of it and
    /// every directory above it, or `None` when there is none.
    pub helper: Option<HelperReading>,
    /// Whether `pkexec` is installed, on Linux.
    pub pkexec: bool,
    /// Whether the process holds `cap_net_bind_service` in `CapEff`.
    pub capable: bool,
    /// Whether alchemyd's binary is on a `nosuid` mount.
    pub nosuid: bool,
    /// Whether the process runs with `no_new_privs`.
    pub no_new_privs: bool,
    /// Whether alchemyd's binary carries `cap_net_bind_service` in its
    /// `security.capability` xattr.
    pub file_capable: bool,
    /// The active firewall and whether Alchemy's ports are open in it.
    pub firewall: FirewallFacts,
    /// Whether Alchemy's system service is installed.
    pub service_installed: bool,
}

/// The helper and the metadata that decides whether it can be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperReading {
    /// Its canonical path.
    pub path: HelperPath,
    /// `lstat` of each of its [`trust_paths`](HelperPath::trust_paths)
    /// that could be read.
    pub meta: BTreeMap<PathBuf, FileMeta>,
}

/// What `/proc/self/status` says about privileges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcStatus {
    /// The effective capability set, `CapEff`.
    pub cap_eff: u64,
    /// `NoNewPrivs`.
    pub no_new_privs: bool,
}

/// `CAP_NET_BIND_SERVICE`, as a bit number.
pub const CAP_NET_BIND_SERVICE: u32 = 10;

impl ProcStatus {
    /// Whether `CapEff` holds `cap_net_bind_service`.
    pub fn capable(self) -> bool {
        self.cap_eff & (1 << CAP_NET_BIND_SERVICE) != 0
    }
}

/// Parse `/proc/self/status`. Missing or malformed fields read as unset.
pub fn parse_proc_status(text: &str) -> ProcStatus {
    let mut status = ProcStatus::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "CapEff" => status.cap_eff = u64::from_str_radix(value, 16).unwrap_or(0),
            "NoNewPrivs" => status.no_new_privs = value == "1",
            _ => {}
        }
    }
    status
}

/// Whether a `security.capability` xattr's bytes grant
/// `cap_net_bind_service` at exec: permitted, with the effective flag.
///
/// Both v2 (20 bytes) and v3 (24 bytes, with a root ID) are read; v1 and
/// anything malformed is not.
pub fn grants_net_bind_service(xattr: &[u8]) -> bool {
    const REVISION_MASK: u32 = 0xFF00_0000;
    const REVISION_2: u32 = 0x0200_0000;
    const REVISION_3: u32 = 0x0300_0000;
    const FLAG_EFFECTIVE: u32 = 0x0000_0001;
    let word = |i: usize| -> Option<u32> {
        let bytes = xattr.get(i * 4..i * 4 + 4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    };
    let Some(magic) = word(0) else {
        return false;
    };
    let sized = match magic & REVISION_MASK {
        REVISION_2 => xattr.len() == 20,
        REVISION_3 => xattr.len() == 24,
        _ => false,
    };
    let permitted = word(1).unwrap_or(0);
    sized && magic & FLAG_EFFECTIVE != 0 && permitted & (1 << CAP_NET_BIND_SERVICE) != 0
}

/// The file `install-service` writes for `uid` on `os`: a systemd system
/// unit on Linux, a `LaunchDaemon` on macOS. Its existence is what
/// "installed" means.
pub fn service_file(os: HostOs, uid: u32) -> PathBuf {
    alchemy_core::lifecycle::system_service_path(os, uid)
}

/// The firewall from what alchemyd can read without root on Linux:
/// whether `/etc/ufw/ufw.conf` says `ENABLED=yes`, whether
/// `systemctl is-active firewalld` says `active`, and whether Alchemy's
/// ufw profile or firewalld service file exists, which `open-firewall`
/// writes and `close-firewall` removes. ufw wins when both are on.
pub fn linux_firewall(
    ufw_conf: Option<&str>,
    ufw_profile: bool,
    firewalld_active: Option<&str>,
    firewalld_service: bool,
) -> FirewallFacts {
    let ufw = ufw_conf.is_some_and(|conf| {
        conf.lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix("ENABLED="))
            .next_back()
            .is_some_and(|value| value.trim_matches(['"', '\'']).eq_ignore_ascii_case("yes"))
    });
    let firewalld = firewalld_active.is_some_and(|out| out.trim() == "active");
    match (ufw, firewalld) {
        (true, _) => FirewallFacts {
            active: Some(Firewall::Ufw),
            opened: ufw_profile,
        },
        (false, true) => FirewallFacts {
            active: Some(Firewall::Firewalld),
            opened: firewalld_service,
        },
        (false, false) => FirewallFacts {
            active: None,
            opened: false,
        },
    }
}

/// The macOS application firewall from `socketfilterfw`, which always
/// exits 0: `--getglobalstate` prints `(State = N)`, on for 1 or 2, and
/// alchemyd counts as opened when `--listapps` lists it and
/// `--getappblocked` says it is permitted.
pub fn macos_firewall(
    global_state: Option<&str>,
    list_apps: Option<&str>,
    app_blocked: Option<&str>,
    alchemyd: &str,
) -> FirewallFacts {
    let on = global_state
        .and_then(|out| out.split("(State = ").nth(1))
        .and_then(|rest| rest.split(')').next())
        .and_then(|n| n.trim().parse::<u8>().ok())
        .is_some_and(|n| n >= 1);
    if !on {
        return FirewallFacts {
            active: None,
            opened: false,
        };
    }
    let listed = list_apps.is_some_and(|out| out.lines().any(|line| line.contains(alchemyd)));
    let permitted = app_blocked.is_some_and(|out| out.contains("is permitted"));
    FirewallFacts {
        active: Some(Firewall::ApplicationFirewall),
        opened: listed && permitted,
    }
}

/// A [`PrivilegeSystem`] for tests that answers what it is told.
#[derive(Debug, Clone)]
pub struct FakePrivilegeSystem {
    facts: Arc<Mutex<Result<SystemFacts, FactsError>>>,
}

impl FakePrivilegeSystem {
    /// A system with `facts`.
    pub fn new(facts: SystemFacts) -> Self {
        Self {
            facts: Arc::new(Mutex::new(Ok(facts))),
        }
    }

    /// An installed alchemyd at `/usr/libexec/alchemy/alchemyd`, running
    /// as UID 1000 with nothing granted, and no helper next to it.
    pub fn plain() -> Self {
        Self::new(SystemFacts {
            alchemyd: PathBuf::from("/usr/libexec/alchemy/alchemyd"),
            uid: 1000,
            helper: None,
            pkexec: false,
            capable: false,
            nosuid: false,
            no_new_privs: false,
            file_capable: false,
            firewall: FirewallFacts {
                active: None,
                opened: false,
            },
            service_installed: false,
        })
    }

    /// Answer `facts` from now on.
    pub fn set(&self, facts: Result<SystemFacts, FactsError>) {
        if let Ok(mut current) = self.facts.lock() {
            *current = facts;
        }
    }

    /// Change the facts in place.
    pub fn update(&self, change: impl FnOnce(&mut SystemFacts)) {
        if let Ok(mut current) = self.facts.lock()
            && let Ok(facts) = current.as_mut()
        {
            change(facts);
        }
    }
}

impl PrivilegeSystem for FakePrivilegeSystem {
    fn read(&self) -> BoxFuture<'_, Result<SystemFacts, FactsError>> {
        let facts = self.facts.lock().map_or_else(
            |_| Err(FactsError::Task("the fake's lock is poisoned".into())),
            |facts| facts.clone(),
        );
        Box::pin(future::ready(facts))
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
    fn proc_status() {
        let cases = [
            (
                "Name:\talchemyd\nCapInh:\t0000000000000000\nCapPrm:\t0000000000000400\nCapEff:\t0000000000000400\nNoNewPrivs:\t0\n",
                ProcStatus {
                    cap_eff: 0x400,
                    no_new_privs: false,
                },
                true,
            ),
            (
                "CapEff:\t0000000000000000\nNoNewPrivs:\t1\n",
                ProcStatus {
                    cap_eff: 0,
                    no_new_privs: true,
                },
                false,
            ),
            (
                "CapEff:\t000001ffffffffff\n",
                ProcStatus {
                    cap_eff: 0x01ff_ffff_ffff,
                    no_new_privs: false,
                },
                true,
            ),
            ("CapEff:\tnope\n", ProcStatus::default(), false),
            ("", ProcStatus::default(), false),
        ];
        for (text, want, capable) in cases {
            let got = parse_proc_status(text);
            assert_eq!(got, want, "{text:?}");
            assert_eq!(got.capable(), capable, "{text:?}");
        }
    }

    #[test]
    fn capability_xattr() {
        let v2 = |magic: u32, permitted: u32| -> Vec<u8> {
            [magic, permitted, 0, 0, 0]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect()
        };
        let mut v3 = v2(0x0300_0001, 0x400);
        v3.extend(0u32.to_le_bytes());
        let setcap: [u8; 20] = [
            0x01, 0x00, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let cases: Vec<(&str, Vec<u8>, bool)> = vec![
            ("setcap cap_net_bind_service=+ep", setcap.to_vec(), true),
            ("v3", v3, true),
            ("permitted, not effective", v2(0x0200_0000, 0x400), false),
            ("another capability", v2(0x0200_0001, 0x800), false),
            ("several, including it", v2(0x0200_0001, 0x3400), true),
            ("v1", v2(0x0100_0001, 0x400)[..12].to_vec(), false),
            ("v2 too short", v2(0x0200_0001, 0x400)[..16].to_vec(), false),
            ("empty", Vec::new(), false),
        ];
        for (name, bytes, want) in cases {
            assert_eq!(grants_net_bind_service(&bytes), want, "{name}");
        }
    }

    #[test]
    fn service_files() {
        assert_eq!(
            service_file(HostOs::Linux, 1000),
            PathBuf::from("/etc/systemd/system/alchemyd-1000.service")
        );
        assert_eq!(
            service_file(HostOs::MacOs, 501),
            PathBuf::from("/Library/LaunchDaemons/computer.alchemy.alchemyd.501.plist")
        );
    }

    #[test]
    fn linux_firewalls() {
        let ufw = |opened| FirewallFacts {
            active: Some(Firewall::Ufw),
            opened,
        };
        let firewalld = |opened| FirewallFacts {
            active: Some(Firewall::Firewalld),
            opened,
        };
        let none = FirewallFacts {
            active: None,
            opened: false,
        };
        let cases = [
            (None, false, None, false, none),
            (Some("ENABLED=no\n"), true, Some("inactive\n"), true, none),
            (
                Some("# x\nENABLED=yes\nLOGLEVEL=low\n"),
                false,
                None,
                false,
                ufw(false),
            ),
            (Some("ENABLED=\"yes\"\n"), true, None, false, ufw(true)),
            (
                Some("ENABLED=yes\n"),
                true,
                Some("active\n"),
                true,
                ufw(true),
            ),
            (None, false, Some("active\n"), false, firewalld(false)),
            (
                Some("ENABLED=no"),
                false,
                Some("active"),
                true,
                firewalld(true),
            ),
            (None, false, Some("activating\n"), true, none),
        ];
        for (conf, profile, active, service, want) in cases {
            assert_eq!(
                linux_firewall(conf, profile, active, service),
                want,
                "{conf:?} {profile} {active:?} {service}"
            );
        }
    }

    #[test]
    fn macos_firewalls() {
        let me = "/Applications/Alchemy.app/Contents/MacOS/alchemyd";
        let listed = format!(
            "ALF: total number of apps = 1 \n\n1 :  {me} \n \t ( Allow incoming connections ) \n"
        );
        let permitted = format!("Incoming connection to {me} is permitted. \n");
        let blocked = format!("Incoming connection to {me} is blocked. \n");
        let app = |opened| FirewallFacts {
            active: Some(Firewall::ApplicationFirewall),
            opened,
        };
        let none = FirewallFacts {
            active: None,
            opened: false,
        };
        let cases = [
            (
                Some("Firewall is disabled. (State = 0)\n"),
                Some(listed.as_str()),
                Some(permitted.as_str()),
                none,
            ),
            (
                Some("Firewall is enabled. (State = 1)\n"),
                Some(listed.as_str()),
                Some(permitted.as_str()),
                app(true),
            ),
            (
                Some("Firewall is blocking all non-essential incoming connections. (State = 2)\n"),
                Some(listed.as_str()),
                Some(blocked.as_str()),
                app(false),
            ),
            (
                Some("Firewall is enabled. (State = 1)\n"),
                Some("ALF: total number of apps = 0 \n"),
                Some(permitted.as_str()),
                app(false),
            ),
            (
                Some("Firewall is enabled. (State = 1)\n"),
                None,
                None,
                app(false),
            ),
            (None, None, None, none),
        ];
        for (global, list, blocked, want) in cases {
            assert_eq!(
                macos_firewall(global, list, blocked, me),
                want,
                "{global:?}"
            );
        }
    }
}
