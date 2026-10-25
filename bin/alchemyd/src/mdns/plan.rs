//! The mDNS task's decisions, as pure functions: which calls bring the
//! announcer from what it publishes to what routing wants, which names
//! count as announced, and what `GET /api/network` says about it.

use std::collections::{BTreeMap, BTreeSet};

use alchemy_api::network::{MdnsReport, MdnsState};
use alchemy_core::mdns::{Announcement, Announcements, LanIpv4, MdnsChange, diff};
use alchemy_core::route::HostLabel;

use super::{Backend, RecordState, RecordStates};

/// The port a name's service is announced on when routing gave none.
const FALLBACK_PORT: u16 = 80;

/// What still works while names can't be announced.
const STILL_WORKS: &str = "Your apps still work at their IP:port address.";

/// The names routing wants announced, and where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted {
    /// The LAN address names point at, or `None` when this computer has
    /// none.
    pub lan: Option<LanIpv4>,
    /// The names, from [`desired`](alchemy_core::mdns::desired): empty
    /// while the `network.mdns` setting is off or there is no LAN
    /// address.
    pub names: Announcements,
    /// The port each name's `_http._tcp` service carries, by label: the
    /// best port that reaches the app.
    pub ports: BTreeMap<HostLabel, u16>,
    /// Counts the wakes routing has seen. When it changes, the mDNS task
    /// withdraws every name and announces it again, so the LAN hears of
    /// it anew after sleep, as RFC 6762 section 8 asks of a responder
    /// whose network may have changed.
    pub reannounce: u64,
}

impl Wanted {
    fn port(&self, label: &HostLabel) -> u16 {
        self.ports.get(label).copied().unwrap_or(FALLBACK_PORT)
    }
}

/// What the announcer was told to publish and accepted, with each name's
/// service port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    /// The names.
    pub names: Announcements,
    /// Their service ports, by label.
    pub ports: BTreeMap<HostLabel, u16>,
}

impl Published {
    /// Record that `step` succeeded.
    pub fn apply(&mut self, step: &Step) {
        match step {
            Step::Announce(announcement, port) => {
                self.names.apply(&MdnsChange::Add(announcement.clone()));
                self.ports.insert(announcement.label.clone(), *port);
            }
            Step::Withdraw(label) => {
                if let Some(gone) = self.names.get(label).cloned() {
                    self.names.apply(&MdnsChange::Withdraw(gone));
                }
                self.ports.remove(label);
            }
        }
    }
}

/// One call to make on the announcer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Publish a name, or replace it with a new address, interface, or
    /// service port.
    Announce(Announcement, u16),
    /// Stop publishing a name.
    Withdraw(HostLabel),
}

/// The calls that turn `published` into `wanted`: what
/// [`diff`] lists, in its order, then a new announcement for each name
/// whose address stayed but whose service port changed, since `diff`
/// doesn't track ports.
pub fn steps(published: &Published, wanted: &Wanted) -> Vec<Step> {
    let changes = diff(&published.names, &wanted.names);
    let moved: BTreeSet<&HostLabel> = changes.iter().map(MdnsChange::label).collect();
    let repointed = wanted
        .names
        .iter()
        .filter(|a| !moved.contains(&a.label))
        .filter(|a| published.ports.get(&a.label) != Some(&wanted.port(&a.label)))
        .map(|a| Step::Announce(a.clone(), wanted.port(&a.label)));
    changes
        .iter()
        .map(|change| match change {
            MdnsChange::Add(to) | MdnsChange::Readdress { to, .. } => {
                Step::Announce(to.clone(), wanted.port(&to.label))
            }
            MdnsChange::Withdraw(gone) => Step::Withdraw(gone.label.clone()),
        })
        .chain(repointed)
        .collect()
}

/// The calls that withdraw every name in `published`, then announce
/// every name in `wanted` as if nothing were published. An announcement
/// that repeats one does nothing, so this is how a name is heard anew.
pub fn reannounce(published: &Published, wanted: &Wanted) -> Vec<Step> {
    published
        .names
        .iter()
        .map(|a| Step::Withdraw(a.label.clone()))
        .chain(steps(&Published::default(), wanted))
        .collect()
}

/// The names in `published` the responder says are established: those
/// whose `.local` addresses are ready.
pub fn established(published: &Published, states: &RecordStates) -> Announcements {
    published
        .names
        .iter()
        .filter(|a| states.get(&a.label) == Some(&RecordState::Established))
        .cloned()
        .collect()
}

/// The names whose state differs between `before` and `after`, with the
/// new state, for logging. A name that went away is left out.
pub fn changed_states<'a>(
    before: &RecordStates,
    after: &'a RecordStates,
) -> Vec<(&'a HostLabel, &'a RecordState)> {
    after
        .iter()
        .filter(|(label, state)| before.get(label) != Some(*state))
        .collect()
}

/// The report when no announcer could start, as `error` says.
pub fn start_failed(backend: Option<Backend>, error: &str) -> MdnsReport {
    MdnsReport {
        state: MdnsState::Unavailable,
        backend: backend.map(Backend::api),
        message: Some(format!(
            "Alchemy can't start announcing app names on your network ({error}). {STILL_WORKS}"
        )),
        fix: (backend == Some(Backend::Builtin)).then(|| {
            "Install and start avahi-daemon, for example with `sudo apt install avahi-daemon`, then restart Alchemy."
                .to_owned()
        }),
    }
}

/// How the announcer is doing, for `GET /api/network`.
///
/// `wanted` is `None` until routing has said what it wants. `failure` is
/// why the last call to the announcer failed, if it did. Names on
/// another device are no reason to call the whole announcer unavailable:
/// those are app problems.
pub fn report(
    backend: Backend,
    wanted: Option<&Wanted>,
    states: &RecordStates,
    failure: Option<&str>,
) -> MdnsReport {
    let unavailable = |message: String, fix: Option<String>| MdnsReport {
        state: MdnsState::Unavailable,
        backend: Some(backend.api()),
        message: Some(message),
        fix,
    };
    let Some(wanted) = wanted else {
        return MdnsReport {
            state: MdnsState::Starting,
            backend: Some(backend.api()),
            message: None,
            fix: None,
        };
    };
    if states.iter().any(|(_, s)| *s == RecordState::Denied) {
        return unavailable(
            format!(
                "This Mac isn't letting Alchemy announce app names on your network, so other devices can't open them by name. {STILL_WORKS}"
            ),
            Some(
                "Open System Settings, go to Privacy & Security, then Local Network, and turn on Alchemy (it may be listed as alchemyd). Alchemy tries again on its own."
                    .to_owned(),
            ),
        );
    }
    if wanted.lan.is_none() {
        return unavailable(
            format!(
                "Alchemy can't find this computer's address on your local network, so it isn't announcing app names. {STILL_WORKS}"
            ),
            Some(
                "Connect this computer to your Wi-Fi or wired network. Alchemy checks again every few seconds."
                    .to_owned(),
            ),
        );
    }
    let failed = states.iter().find_map(|(_, s)| match s {
        RecordState::Failed(why) => Some(why.as_str()),
        _ => None,
    });
    let any_established = states.iter().any(|(_, s)| *s == RecordState::Established);
    match failure.or(failed) {
        Some(why) if !any_established => unavailable(
            format!("Alchemy can't announce app names on your network ({why}). {STILL_WORKS}"),
            (backend == Backend::Avahi).then(|| {
                "Check that avahi-daemon is running, for example with `systemctl status avahi-daemon`. Alchemy tries again every few seconds."
                    .to_owned()
            }),
        ),
        _ => MdnsReport {
            state: MdnsState::Announcing,
            backend: Some(backend.api()),
            message: None,
            fix: None,
        },
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
    use alchemy_api::network::MdnsBackend;

    use super::*;

    fn label(text: &str) -> HostLabel {
        HostLabel::new(text).unwrap()
    }

    fn lan(addr: &str) -> LanIpv4 {
        LanIpv4::new("en0", addr.parse().unwrap()).unwrap()
    }

    fn ann(name: &str, addr: &str) -> Announcement {
        Announcement {
            label: label(name),
            app: name.into(),
            lan: lan(addr),
        }
    }

    fn wanted(names: &[(&str, &str, u16)]) -> Wanted {
        Wanted {
            lan: names.first().map(|(_, addr, _)| lan(addr)),
            names: names.iter().map(|(n, a, _)| ann(n, a)).collect(),
            ports: names.iter().map(|(n, _, p)| (label(n), *p)).collect(),
            reannounce: 0,
        }
    }

    fn published(names: &[(&str, &str, u16)]) -> Published {
        let mut published = Published::default();
        for (n, a, p) in names {
            published.apply(&Step::Announce(ann(n, a), *p));
        }
        published
    }

    #[test]
    fn steps_table() {
        let ip = "192.168.1.20";
        let new_ip = "192.168.1.30";
        let cases = [
            ("nothing to do", vec![], vec![], vec![]),
            (
                "add",
                vec![],
                vec![("blog", ip, 80)],
                vec![Step::Announce(ann("blog", ip), 80)],
            ),
            (
                "same name, same port",
                vec![("blog", ip, 80)],
                vec![("blog", ip, 80)],
                vec![],
            ),
            (
                "port 80 stopped answering",
                vec![("blog", ip, 80)],
                vec![("blog", ip, 8100)],
                vec![Step::Announce(ann("blog", ip), 8100)],
            ),
            (
                "LAN address changed",
                vec![("blog", ip, 80), ("wiki", ip, 80)],
                vec![("blog", new_ip, 80), ("wiki", new_ip, 8101)],
                vec![
                    Step::Announce(ann("blog", new_ip), 80),
                    Step::Announce(ann("wiki", new_ip), 8101),
                ],
            ),
            (
                "withdraw before anything else",
                vec![("old", ip, 80), ("blog", ip, 80)],
                vec![("blog", ip, 8100), ("new", ip, 80)],
                vec![
                    Step::Withdraw(label("old")),
                    Step::Announce(ann("new", ip), 80),
                    Step::Announce(ann("blog", ip), 8100),
                ],
            ),
            (
                "setting off",
                vec![("blog", ip, 80), ("wiki", ip, 8101)],
                vec![],
                vec![Step::Withdraw(label("blog")), Step::Withdraw(label("wiki"))],
            ),
        ];
        for (name, from, to, want) in cases {
            let from = published(&from);
            let to = wanted(&to);
            let got = steps(&from, &to);
            assert_eq!(got, want, "{name}");
            let mut after = from;
            for step in &got {
                after.apply(step);
            }
            assert_eq!(after.names, to.names, "{name}");
            assert_eq!(after.ports, to.ports, "{name}");
            assert_eq!(steps(&after, &to), [], "{name}: settled");
        }
    }

    #[test]
    fn reannounce_withdraws_everything_then_announces_what_is_wanted() {
        let ip = "192.168.1.20";
        let new_ip = "192.168.1.30";
        let cases = [
            ("nothing", vec![], vec![], vec![]),
            (
                "same names",
                vec![("blog", ip, 80)],
                vec![("blog", ip, 80)],
                vec![
                    Step::Withdraw(label("blog")),
                    Step::Announce(ann("blog", ip), 80),
                ],
            ),
            (
                "the LAN moved and a name went",
                vec![("blog", ip, 80), ("old", ip, 80)],
                vec![("blog", new_ip, 8100)],
                vec![
                    Step::Withdraw(label("blog")),
                    Step::Withdraw(label("old")),
                    Step::Announce(ann("blog", new_ip), 8100),
                ],
            ),
        ];
        for (name, from, to, want) in cases {
            let from = published(&from);
            let to = wanted(&to);
            let got = reannounce(&from, &to);
            assert_eq!(got, want, "{name}");
            let mut after = from;
            for step in &got {
                after.apply(step);
            }
            assert_eq!(after.names, to.names, "{name}");
            assert_eq!(steps(&after, &to), [], "{name}: settled");
        }
    }

    #[test]
    fn a_name_with_no_port_is_announced_on_port_80() {
        let mut to = wanted(&[("blog", "192.168.1.20", 8100)]);
        to.ports.clear();
        assert_eq!(
            steps(&Published::default(), &to),
            [Step::Announce(ann("blog", "192.168.1.20"), 80)]
        );
    }

    #[test]
    fn withdrawing_an_unknown_name_changes_nothing() {
        let mut p = published(&[("blog", "192.168.1.20", 80)]);
        let before = p.clone();
        p.apply(&Step::Withdraw(label("nope")));
        assert_eq!(p, before);
    }

    fn states(entries: &[(&str, RecordState)]) -> RecordStates {
        entries.iter().map(|(l, s)| (label(l), s.clone())).collect()
    }

    #[test]
    fn only_established_names_count_as_announced() {
        let ip = "192.168.1.20";
        let p = published(&[("a", ip, 80), ("b", ip, 80), ("c", ip, 80), ("d", ip, 80)]);
        let s = states(&[
            ("a", RecordState::Established),
            ("b", RecordState::Registering),
            ("c", RecordState::Conflict),
            ("z", RecordState::Established),
        ]);
        let names = established(&p, &s);
        let got: Vec<&str> = names.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(got, ["a"]);
    }

    #[test]
    fn changed_states_lists_new_and_moved_names() {
        let before = states(&[
            ("a", RecordState::Registering),
            ("b", RecordState::Established),
            ("gone", RecordState::Established),
        ]);
        let after = states(&[
            ("a", RecordState::Established),
            ("b", RecordState::Established),
            ("c", RecordState::Conflict),
        ]);
        let got: Vec<(&str, &RecordState)> = changed_states(&before, &after)
            .into_iter()
            .map(|(l, s)| (l.as_str(), s))
            .collect();
        assert_eq!(
            got,
            [
                ("a", &RecordState::Established),
                ("c", &RecordState::Conflict)
            ]
        );
    }

    /// A name, the backend, what routing wants, the record states, why
    /// the last call failed, and the state, message, and whether there is
    /// a fix to expect.
    type ReportCase<'a> = (
        &'a str,
        Backend,
        Option<&'a Wanted>,
        RecordStates,
        Option<&'a str>,
        MdnsState,
        Option<&'a str>,
        bool,
    );

    #[allow(clippy::too_many_lines)]
    #[test]
    fn report_table() {
        let some = wanted(&[("blog", "192.168.1.20", 80)]);
        let no_lan = Wanted::default();
        let failed = || RecordState::Failed("cannot run dns-sd".into());
        let cases: [ReportCase<'_>; 9] = [
            (
                "routing hasn't planned",
                Backend::DnsSd,
                None,
                RecordStates::default(),
                None,
                MdnsState::Starting,
                None,
                false,
            ),
            (
                "announcing",
                Backend::DnsSd,
                Some(&some),
                states(&[("blog", RecordState::Established)]),
                None,
                MdnsState::Announcing,
                None,
                false,
            ),
            (
                "still registering",
                Backend::Avahi,
                Some(&some),
                states(&[("blog", RecordState::Registering)]),
                None,
                MdnsState::Announcing,
                None,
                false,
            ),
            (
                "a conflict is an app problem",
                Backend::Avahi,
                Some(&some),
                states(&[("blog", RecordState::Conflict)]),
                None,
                MdnsState::Announcing,
                None,
                false,
            ),
            (
                "Local Network denied",
                Backend::DnsSd,
                Some(&some),
                states(&[("blog", RecordState::Denied)]),
                None,
                MdnsState::Unavailable,
                Some(
                    "This Mac isn't letting Alchemy announce app names on your network, so other devices can't open them by name. Your apps still work at their IP:port address.",
                ),
                true,
            ),
            (
                "no LAN address",
                Backend::Builtin,
                Some(&no_lan),
                RecordStates::default(),
                None,
                MdnsState::Unavailable,
                Some(
                    "Alchemy can't find this computer's address on your local network, so it isn't announcing app names. Your apps still work at their IP:port address.",
                ),
                true,
            ),
            (
                "every name failed",
                Backend::DnsSd,
                Some(&some),
                states(&[("blog", failed())]),
                None,
                MdnsState::Unavailable,
                Some(
                    "Alchemy can't announce app names on your network (cannot run dns-sd). Your apps still work at their IP:port address.",
                ),
                false,
            ),
            (
                "avahi refused a call",
                Backend::Avahi,
                Some(&some),
                RecordStates::default(),
                Some("avahi failed: org.freedesktop.Avahi.Failure"),
                MdnsState::Unavailable,
                Some(
                    "Alchemy can't announce app names on your network (avahi failed: org.freedesktop.Avahi.Failure). Your apps still work at their IP:port address.",
                ),
                true,
            ),
            (
                "one name failed, another is on the network",
                Backend::DnsSd,
                Some(&some),
                states(&[("blog", RecordState::Established), ("wiki", failed())]),
                None,
                MdnsState::Announcing,
                None,
                false,
            ),
        ];
        for (name, backend, wanted, states, failure, state, message, fix) in cases {
            let got = report(backend, wanted, &states, failure);
            assert_eq!(got.state, state, "{name}");
            assert_eq!(got.backend, Some(backend.api()), "{name}");
            assert_eq!(got.message.as_deref(), message, "{name}");
            assert_eq!(got.fix.is_some(), fix, "{name}");
        }
    }

    #[test]
    fn a_responder_that_cannot_start_is_unavailable() {
        let got = start_failed(Some(Backend::Builtin), "built-in failed: no socket");
        assert_eq!(got.state, MdnsState::Unavailable);
        assert_eq!(got.backend, Some(MdnsBackend::BuiltIn));
        assert_eq!(
            got.message.as_deref(),
            Some(
                "Alchemy can't start announcing app names on your network (built-in failed: no socket). Your apps still work at their IP:port address."
            )
        );
        assert!(got.fix.is_some());
        let got = start_failed(None, "boom");
        assert_eq!(got.backend, None);
        assert_eq!(got.fix, None);
    }

    #[test]
    fn backends_map_to_the_api() {
        for (backend, api) in [
            (Backend::DnsSd, MdnsBackend::DnsSd),
            (Backend::Avahi, MdnsBackend::Avahi),
            (Backend::Builtin, MdnsBackend::BuiltIn),
        ] {
            assert_eq!(backend.api(), api);
        }
    }
}
