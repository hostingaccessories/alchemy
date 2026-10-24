//! mDNS names: which `<label>.local` names this host announces on the LAN,
//! what to change when routes or the LAN address change, and the app
//! problem for a name another device already answers for.
//!
//! Everything here is a pure decision over plain values. The daemon picks
//! the LAN address from the interfaces it classifies as LAN (never Docker
//! bridges or VPNs) and passes it in with the route table and the
//! `network.mdns` setting; an announcer puts each [`MdnsChange`] on the
//! network and reports back the names that conflict.
//!
//! Only IPv4 A records are modelled. A name is never renamed to dodge a
//! conflict: it stays in the desired set and its app gets an
//! [`MdnsProblem`] until the user picks another hostname.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::route::{HostLabel, HostSuffix, RouteTable};

/// An IPv4 address on one of the host's LAN interfaces: where other
/// devices reach the names this host announces.
///
/// Built with [`LanIpv4::new`], which turns away addresses no other device
/// could use, such as loopback or link-local.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LanIpv4 {
    interface: String,
    addr: Ipv4Addr,
}

/// Why a [`LanIpv4`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LanIpv4Error {
    /// The interface name was empty.
    #[error("a LAN address needs an interface")]
    NoInterface,
    /// The address isn't a unicast address another device could reach,
    /// such as loopback, link-local, multicast, or broadcast.
    #[error("{0} isn't an address other devices on the LAN can reach")]
    Unreachable(Ipv4Addr),
}

impl LanIpv4 {
    /// `addr` on `interface`, such as `192.168.1.20` on `en0`.
    ///
    /// The caller decides which interfaces are LAN; this only checks that
    /// the address is one a LAN peer could connect to.
    pub fn new(interface: &str, addr: Ipv4Addr) -> Result<Self, LanIpv4Error> {
        if interface.is_empty() {
            return Err(LanIpv4Error::NoInterface);
        }
        let [first, ..] = addr.octets();
        if first == 0
            || addr.is_loopback()
            || addr.is_link_local()
            || addr.is_multicast()
            || addr.is_broadcast()
        {
            return Err(LanIpv4Error::Unreachable(addr));
        }
        Ok(Self {
            interface: interface.to_owned(),
            addr,
        })
    }

    /// The interface, such as `en0` or `eth0`.
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// The address, such as `192.168.1.20`.
    pub fn addr(&self) -> Ipv4Addr {
        self.addr
    }
}

/// One name to announce: `<label>.local` answers with an A record for
/// [`lan`](Self::lan)'s address, on its interface only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Announcement {
    /// The label, such as `immich`.
    pub label: HostLabel,
    /// The app the name reaches. For reporting only: an announcer never
    /// needs it, and [`diff`] ignores it.
    pub app: String,
    /// Where the name points.
    pub lan: LanIpv4,
}

impl Announcement {
    /// The full name, such as `immich.local`.
    pub fn name(&self) -> String {
        self.label.host(HostSuffix::Local)
    }
}

/// A set of [`Announcement`]s, one per label: what should be announced,
/// or what an announcer has announced so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announcements {
    by_label: BTreeMap<HostLabel, Announcement>,
}

impl Announcements {
    /// The announcement for `label`.
    pub fn get(&self, label: &HostLabel) -> Option<&Announcement> {
        self.by_label.get(label)
    }

    /// Whether `label` is in the set: for the desired set, whether its
    /// `.local` address should be offered at all.
    pub fn contains(&self, label: &HostLabel) -> bool {
        self.by_label.contains_key(label)
    }

    /// Every announcement, by label.
    pub fn iter(&self) -> impl Iterator<Item = &Announcement> {
        self.by_label.values()
    }

    /// How many names are in the set.
    pub fn len(&self) -> usize {
        self.by_label.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.by_label.is_empty()
    }

    /// Record that `change` was made, so that the set keeps matching what
    /// the announcer holds when some changes fail.
    pub fn apply(&mut self, change: &MdnsChange) {
        match change {
            MdnsChange::Add(to) | MdnsChange::Readdress { to, .. } => {
                self.by_label.insert(to.label.clone(), to.clone());
            }
            MdnsChange::Withdraw(gone) => {
                self.by_label.remove(&gone.label);
            }
        }
    }
}

impl FromIterator<Announcement> for Announcements {
    /// A later announcement for the same label replaces an earlier one.
    fn from_iter<I: IntoIterator<Item = Announcement>>(iter: I) -> Self {
        Self {
            by_label: iter.into_iter().map(|a| (a.label.clone(), a)).collect(),
        }
    }
}

/// What [`desired`] decides from.
#[derive(Debug, Clone, Copy)]
pub struct MdnsInputs<'a> {
    /// The `network.mdns` setting: whether to announce anything at all.
    pub enabled: bool,
    /// The routes, whose labels and `x-alchemy` `mdns` flags pick the
    /// names.
    pub routes: &'a RouteTable,
    /// The LAN address to announce, from a LAN interface only, or `None`
    /// when the host has none.
    pub lan: Option<&'a LanIpv4>,
}

/// The names to announce: `<label>.local` for every routed app with a
/// label and `mdns` left on, each pointing at `inputs.lan`.
///
/// Empty when the setting is off or there is no LAN address. A stopped
/// app keeps its name, so names don't come and go with containers and a
/// conflict shows before the app is first opened.
pub fn desired(inputs: &MdnsInputs<'_>) -> Announcements {
    let Some(lan) = inputs.lan.filter(|_| inputs.enabled) else {
        return Announcements::default();
    };
    inputs
        .routes
        .routes()
        .iter()
        .filter(|route| route.mdns)
        .filter_map(|route| {
            Some(Announcement {
                label: route.label.clone()?,
                app: route.app.clone(),
                lan: lan.clone(),
            })
        })
        .collect()
}

/// One change to what is announced.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MdnsChange {
    /// Start announcing a name.
    Add(Announcement),
    /// Point an announced name at a new address or interface, such as
    /// after the LAN address changed.
    Readdress {
        /// What is announced now.
        from: Announcement,
        /// What to announce instead.
        to: Announcement,
    },
    /// Stop announcing a name, with a goodbye.
    Withdraw(Announcement),
}

impl MdnsChange {
    /// The label the change is about.
    pub fn label(&self) -> &HostLabel {
        match self {
            Self::Add(a) | Self::Readdress { to: a, .. } | Self::Withdraw(a) => &a.label,
        }
    }
}

/// The changes that turn `current` into `desired`: withdrawals first, so
/// a name is freed before anything else is announced, then readdresses,
/// then additions, each in label order.
///
/// Only the label, address, and interface count; an announcement that
/// moved to another app with the same label needs no change.
pub fn diff(current: &Announcements, desired: &Announcements) -> Vec<MdnsChange> {
    let withdraw = current
        .iter()
        .filter(|a| !desired.contains(&a.label))
        .cloned()
        .map(MdnsChange::Withdraw);
    let readdress = desired.iter().filter_map(|to| {
        let from = current.get(&to.label)?;
        (from.lan != to.lan).then(|| MdnsChange::Readdress {
            from: from.clone(),
            to: to.clone(),
        })
    });
    let add = desired
        .iter()
        .filter(|a| !current.contains(&a.label))
        .cloned()
        .map(MdnsChange::Add);
    withdraw.chain(readdress).chain(add).collect()
}

/// Withdraw everything in `current`, such as on shutdown or when the
/// setting is turned off.
pub fn withdraw_all(current: &Announcements) -> Vec<MdnsChange> {
    diff(current, &Announcements::default())
}

/// Why an app's `.local` name can't be used.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MdnsProblemKind {
    /// Another device on the LAN answers for the name. Alchemy never
    /// renames, so it stays unavailable until the hostname changes.
    NameInUse {
        /// The label, such as `immich` for `immich.local`.
        label: HostLabel,
    },
}

impl fmt::Display for MdnsProblemKind {
    /// A sentence saying what is wrong and what to change.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameInUse { label } => write!(
                f,
                "Another device on your network uses {}. Set a hostname in x-alchemy.",
                label.host(HostSuffix::Local)
            ),
        }
    }
}

/// An [`MdnsProblemKind`] for one app.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MdnsProblem {
    /// The app.
    pub app: String,
    /// What is wrong.
    pub kind: MdnsProblemKind,
}

/// The problems for the names an announcer reports another device
/// answers for, in label order.
///
/// Only names in `desired` count, so a conflict on a name that has since
/// been withdrawn, or whose app turned `mdns` off, is dropped.
pub fn conflict_problems<'a>(
    desired: &Announcements,
    conflicts: impl IntoIterator<Item = &'a HostLabel>,
) -> Vec<MdnsProblem> {
    let conflicts: BTreeSet<&HostLabel> = conflicts.into_iter().collect();
    conflicts
        .into_iter()
        .filter_map(|label| desired.get(label))
        .map(|a| MdnsProblem {
            app: a.app.clone(),
            kind: MdnsProblemKind::NameInUse {
                label: a.label.clone(),
            },
        })
        .collect()
}

/// How far a desired name has got, for the `.local` addresses that use
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NameState {
    /// Not on the network yet, or not at its current address: being
    /// announced or re-addressed.
    Announcing,
    /// Announced at the address it should have.
    Announced,
    /// Another device answers for it, so it can't be used.
    InUse,
}

/// The state of every name in `desired`, given what an announcer has
/// `announced` so far and the labels it reported `conflicts` on.
///
/// A conflict wins over anything else. A name counts as announced only
/// at the desired address and interface, so after the LAN address
/// changes it is [`NameState::Announcing`] until the readdress is made.
/// Labels outside `desired` are left out: they get no `.local` address.
pub fn name_states<'a>(
    desired: &Announcements,
    announced: &Announcements,
    conflicts: impl IntoIterator<Item = &'a HostLabel>,
) -> BTreeMap<HostLabel, NameState> {
    let conflicts: BTreeSet<&HostLabel> = conflicts.into_iter().collect();
    desired
        .iter()
        .map(|want| {
            let state = if conflicts.contains(&want.label) {
                NameState::InUse
            } else if announced
                .get(&want.label)
                .is_some_and(|a| a.lan == want.lan)
            {
                NameState::Announced
            } else {
                NameState::Announcing
            };
            (want.label.clone(), state)
        })
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
    use crate::route::{
        AppRouteInput, Reachability, RouteOptions, ServiceRouteInput, XAlchemy, XAlchemyBlock,
        plan_routes,
    };

    fn lan(interface: &str, addr: &str) -> LanIpv4 {
        LanIpv4::new(interface, addr.parse().unwrap()).unwrap()
    }

    fn label(text: &str) -> HostLabel {
        HostLabel::new(text).unwrap()
    }

    fn ann(label_text: &str, app: &str, on: &LanIpv4) -> Announcement {
        Announcement {
            label: label(label_text),
            app: app.into(),
            lan: on.clone(),
        }
    }

    /// An app routed to `web:80`, with an `x-alchemy` block when
    /// `hostname` or `mdns` asks for one.
    fn app(name: &str, hostname: Option<&str>, mdns: bool) -> AppRouteInput {
        let block = (hostname.is_some() || !mdns).then(|| {
            XAlchemyBlock::Read(XAlchemy {
                hostname: hostname.map(str::to_owned),
                mdns,
                ..XAlchemy::default()
            })
        });
        AppRouteInput {
            app: name.into(),
            x_alchemy: block,
            services: vec![ServiceRouteInput {
                name: "web".into(),
                expose: vec!["80".parse().unwrap()],
                ..ServiceRouteInput::default()
            }],
            ..AppRouteInput::default()
        }
    }

    fn table(apps: &[AppRouteInput]) -> RouteTable {
        plan_routes(
            apps,
            &RouteOptions {
                reachability: Reachability::ContainerIp,
                suffixes: vec![HostSuffix::Localhost],
            },
        )
        .table
    }

    #[test]
    fn lan_addresses() {
        let ok = [
            ("en0", "192.168.1.20"),
            ("eth0", "10.0.0.5"),
            ("wlan0", "172.16.4.1"),
        ];
        for (interface, addr) in ok {
            let got = lan(interface, addr);
            assert_eq!(
                (got.interface(), got.addr()),
                (interface, addr.parse().unwrap())
            );
        }
        let bad = [
            ("", "192.168.1.20", LanIpv4Error::NoInterface),
            (
                "lo",
                "127.0.0.1",
                LanIpv4Error::Unreachable([127, 0, 0, 1].into()),
            ),
            (
                "en0",
                "0.0.0.0",
                LanIpv4Error::Unreachable([0, 0, 0, 0].into()),
            ),
            (
                "en0",
                "0.1.2.3",
                LanIpv4Error::Unreachable([0, 1, 2, 3].into()),
            ),
            (
                "en0",
                "169.254.10.1",
                LanIpv4Error::Unreachable([169, 254, 10, 1].into()),
            ),
            (
                "en0",
                "224.0.0.251",
                LanIpv4Error::Unreachable([224, 0, 0, 251].into()),
            ),
            (
                "en0",
                "255.255.255.255",
                LanIpv4Error::Unreachable(Ipv4Addr::BROADCAST),
            ),
        ];
        for (interface, addr, want) in bad {
            assert_eq!(
                LanIpv4::new(interface, addr.parse().unwrap()),
                Err(want),
                "{interface} {addr}"
            );
        }
    }

    #[test]
    fn desired_names() {
        let home = lan("en0", "192.168.1.20");
        let apps = vec![
            app("immich", None, true),
            app("blog", Some("journal"), true),
            app("private", None, false),
            app("quiet", Some("hush"), false),
            app("___", None, true),
            app("my_app", None, true),
            app("my-app", None, true),
        ];
        let routes = table(&apps);
        let all = vec![
            ann("immich", "immich", &home),
            ann("journal", "blog", &home),
        ];
        let cases: Vec<(&str, bool, Option<&LanIpv4>, Vec<Announcement>)> = vec![
            ("announcing", true, Some(&home), all),
            ("setting off", false, Some(&home), vec![]),
            ("no LAN address", true, None, vec![]),
            ("both off", false, None, vec![]),
        ];
        for (name, enabled, lan, want) in cases {
            let got = desired(&MdnsInputs {
                enabled,
                routes: &routes,
                lan,
            });
            assert_eq!(got.iter().cloned().collect::<Vec<_>>(), want, "{name}");
            assert_eq!(got.len(), want.len(), "{name}");
        }
        assert!(
            desired(&MdnsInputs {
                enabled: true,
                routes: &RouteTable::default(),
                lan: Some(&home),
            })
            .is_empty()
        );
    }

    #[test]
    fn announcement_name() {
        let a = ann("immich", "immich", &lan("en0", "192.168.1.20"));
        assert_eq!(a.name(), "immich.local");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn diffing() {
        let home = lan("en0", "192.168.1.20");
        let moved = lan("en0", "192.168.1.21");
        let wired = lan("eth0", "192.168.1.20");
        let set = |items: &[Announcement]| items.iter().cloned().collect::<Announcements>();
        let a = |l: &str, on: &LanIpv4| ann(l, l, on);
        let cases: Vec<(&str, Announcements, Announcements, Vec<MdnsChange>)> = vec![
            ("nothing", set(&[]), set(&[]), vec![]),
            (
                "unchanged",
                set(&[a("immich", &home)]),
                set(&[a("immich", &home)]),
                vec![],
            ),
            (
                "first announcements",
                set(&[]),
                set(&[a("wiki", &home), a("immich", &home)]),
                vec![
                    MdnsChange::Add(a("immich", &home)),
                    MdnsChange::Add(a("wiki", &home)),
                ],
            ),
            (
                "app removed",
                set(&[a("immich", &home), a("wiki", &home)]),
                set(&[a("wiki", &home)]),
                vec![MdnsChange::Withdraw(a("immich", &home))],
            ),
            (
                "LAN address changed",
                set(&[a("immich", &home)]),
                set(&[a("immich", &moved)]),
                vec![MdnsChange::Readdress {
                    from: a("immich", &home),
                    to: a("immich", &moved),
                }],
            ),
            (
                "interface changed",
                set(&[a("immich", &home)]),
                set(&[a("immich", &wired)]),
                vec![MdnsChange::Readdress {
                    from: a("immich", &home),
                    to: a("immich", &wired),
                }],
            ),
            (
                "label moved to another app",
                set(&[ann("journal", "blog", &home)]),
                set(&[ann("journal", "notes", &home)]),
                vec![],
            ),
            (
                "hostname changed",
                set(&[ann("blog", "blog", &home)]),
                set(&[ann("journal", "blog", &home)]),
                vec![
                    MdnsChange::Withdraw(ann("blog", "blog", &home)),
                    MdnsChange::Add(ann("journal", "blog", &home)),
                ],
            ),
            (
                "withdraw, then readdress, then add",
                set(&[a("zulu", &home), a("immich", &home)]),
                set(&[a("immich", &moved), a("alpha", &moved)]),
                vec![
                    MdnsChange::Withdraw(a("zulu", &home)),
                    MdnsChange::Readdress {
                        from: a("immich", &home),
                        to: a("immich", &moved),
                    },
                    MdnsChange::Add(a("alpha", &moved)),
                ],
            ),
        ];
        for (name, current, want_set, want) in cases {
            let changes = diff(&current, &want_set);
            assert_eq!(changes, want, "{name}");
            let mut applied = current.clone();
            for change in &changes {
                applied.apply(change);
            }
            let labels = |s: &Announcements| {
                s.iter()
                    .map(|x| (x.label.clone(), x.lan.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(labels(&applied), labels(&want_set), "{name}: applied");
        }
    }

    #[test]
    fn withdrawing_everything() {
        let home = lan("en0", "192.168.1.20");
        let current: Announcements = [ann("wiki", "wiki", &home), ann("immich", "immich", &home)]
            .into_iter()
            .collect();
        assert_eq!(
            withdraw_all(&current),
            vec![
                MdnsChange::Withdraw(ann("immich", "immich", &home)),
                MdnsChange::Withdraw(ann("wiki", "wiki", &home)),
            ]
        );
        assert_eq!(withdraw_all(&Announcements::default()), vec![]);
    }

    #[test]
    fn change_labels() {
        let home = lan("en0", "192.168.1.20");
        let moved = lan("en0", "192.168.1.21");
        let cases = [
            (MdnsChange::Add(ann("a", "a", &home)), "a"),
            (
                MdnsChange::Readdress {
                    from: ann("b", "b", &home),
                    to: ann("b", "b", &moved),
                },
                "b",
            ),
            (MdnsChange::Withdraw(ann("c", "c", &home)), "c"),
        ];
        for (change, want) in cases {
            assert_eq!(change.label().as_str(), want);
        }
    }

    #[test]
    fn conflicts_become_app_problems() {
        let home = lan("en0", "192.168.1.20");
        let desired: Announcements = [
            ann("immich", "immich", &home),
            ann("journal", "blog", &home),
            ann("wiki", "wiki", &home),
        ]
        .into_iter()
        .collect();
        let problem = |app: &str, l: &str| MdnsProblem {
            app: app.into(),
            kind: MdnsProblemKind::NameInUse { label: label(l) },
        };
        let cases: Vec<(&str, Vec<&str>, Vec<MdnsProblem>)> = vec![
            ("none", vec![], vec![]),
            ("one", vec!["immich"], vec![problem("immich", "immich")]),
            (
                "by the app's hostname, in label order",
                vec!["wiki", "journal", "wiki"],
                vec![problem("blog", "journal"), problem("wiki", "wiki")],
            ),
            ("no longer announced", vec!["gone"], vec![]),
        ];
        for (name, conflicts, want) in cases {
            let labels: Vec<HostLabel> = conflicts.iter().map(|l| label(l)).collect();
            assert_eq!(conflict_problems(&desired, &labels), want, "{name}");
        }
    }

    #[test]
    fn problem_text() {
        let kind = MdnsProblemKind::NameInUse {
            label: label("immich"),
        };
        assert_eq!(
            kind.to_string(),
            "Another device on your network uses immich.local. Set a hostname in x-alchemy."
        );
        assert_eq!(
            serde_json::to_value(&kind).unwrap(),
            serde_json::json!({"kind": "name_in_use", "label": "immich"})
        );
    }

    #[test]
    fn name_states_table() {
        use NameState::{Announced, Announcing, InUse};
        let home = lan("en0", "192.168.1.20");
        let moved = lan("en0", "192.168.1.21");
        let wifi = lan("en1", "192.168.1.20");
        let desired: Announcements = [
            ann("blog", "blog", &home),
            ann("photos", "photos", &home),
            ann("wiki", "notes", &home),
        ]
        .into_iter()
        .collect();
        type Case = (
            &'static str,
            Vec<Announcement>,
            Vec<&'static str>,
            [NameState; 3],
        );
        let cases: [Case; 4] = [
            (
                "nothing announced yet",
                vec![],
                vec![],
                [Announcing, Announcing, Announcing],
            ),
            (
                "all announced, one for an app it moved from",
                vec![
                    ann("blog", "blog", &home),
                    ann("photos", "photos", &home),
                    ann("wiki", "wiki", &home),
                ],
                vec![],
                [Announced, Announced, Announced],
            ),
            (
                "old address, other interface, in use",
                vec![
                    ann("blog", "blog", &moved),
                    ann("photos", "photos", &wifi),
                    ann("wiki", "notes", &home),
                ],
                vec!["wiki", "gone"],
                [Announcing, Announcing, InUse],
            ),
            (
                "in use before it was ever announced",
                vec![],
                vec!["blog"],
                [InUse, Announcing, Announcing],
            ),
        ];
        for (name, announced, conflicts, want) in cases {
            let announced: Announcements = announced.into_iter().collect();
            let conflicts: Vec<HostLabel> = conflicts.into_iter().map(label).collect();
            let want: BTreeMap<HostLabel, NameState> = ["blog", "photos", "wiki"]
                .into_iter()
                .map(label)
                .zip(want)
                .collect();
            assert_eq!(
                name_states(&desired, &announced, &conflicts),
                want,
                "{name}"
            );
        }
        assert!(name_states(&Announcements::default(), &desired, &[]).is_empty());
    }
}
