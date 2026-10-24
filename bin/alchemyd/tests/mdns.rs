//! mDNS announcers: the fake driven the way the daemon will drive it, the
//! `dns-sd` supervisor against a stand-in script, and, ignored unless
//! asked for, each real backend on this host's LAN.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alchemy_core::mdns::{
    Announcement, Announcements, LanIpv4, MdnsChange, MdnsProblemKind, conflict_problems, diff,
    withdraw_all,
};
use alchemy_core::route::HostLabel;
use alchemyd::mdns::dnssd::DnsSdAnnouncer;
use alchemyd::mdns::{
    AnnounceError, Announcer, Backend, FakeAnnouncer, FakeCall, Record, RecordState, RecordStates,
};
use tokio::sync::watch;

const WAIT: Duration = Duration::from_secs(10);

fn label(text: &str) -> HostLabel {
    HostLabel::new(text).unwrap()
}

fn lan(interface: &str, addr: &str) -> LanIpv4 {
    LanIpv4::new(interface, addr.parse().unwrap()).unwrap()
}

fn ann(name: &str, on: &LanIpv4) -> Announcement {
    Announcement {
        label: label(name),
        app: name.to_owned(),
        lan: on.clone(),
    }
}

async fn wait_for(
    states: &mut watch::Receiver<RecordStates>,
    what: &str,
    mut ok: impl FnMut(&RecordStates) -> bool,
) -> RecordStates {
    let found = match tokio::time::timeout(WAIT, states.wait_for(|s| ok(s))).await {
        Ok(Ok(states)) => Some(states.clone()),
        _ => None,
    };
    found.unwrap_or_else(|| {
        panic!(
            "timed out waiting for {what}; states: {:?}",
            states.borrow()
        )
    })
}

async fn eventually(what: &str, ok: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !ok() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_state(
    states: &mut watch::Receiver<RecordStates>,
    name: &str,
    want: &RecordState,
) -> RecordStates {
    let label = label(name);
    wait_for(states, &format!("{name} to be {want:?}"), |s| {
        s.get(&label) == Some(want)
    })
    .await
}

/// Apply `changes` the way the daemon's mDNS task does: announce or
/// withdraw each, and record it as current only when that worked.
async fn apply(
    announcer: &dyn Announcer,
    current: &mut Announcements,
    changes: Vec<MdnsChange>,
    port: u16,
) {
    for change in changes {
        let done = match &change {
            MdnsChange::Add(to) | MdnsChange::Readdress { to, .. } => {
                announcer.announce(to.clone(), port).await
            }
            MdnsChange::Withdraw(gone) => announcer.withdraw(gone.label.clone()).await,
        };
        if done.is_ok() {
            current.apply(&change);
        }
    }
}

#[tokio::test]
async fn fake_follows_the_diff_and_reports_conflicts_as_problems() {
    let fake = FakeAnnouncer::new(Backend::Avahi);
    let announcer: &dyn Announcer = &fake;
    let mut states = announcer.states();
    let home = lan("en0", "10.0.1.50");
    let mut current = Announcements::default();

    fake.take(&label("plex"));
    let want: Announcements = [ann("immich", &home), ann("plex", &home)]
        .into_iter()
        .collect();
    let changes = diff(&current, &want);
    apply(announcer, &mut current, changes, 80).await;
    assert_eq!(current, want);
    let seen = wait_state(&mut states, "plex", &RecordState::Conflict).await;
    assert_eq!(seen.get(&label("immich")), Some(&RecordState::Established));
    let problems = conflict_problems(&want, seen.conflicts());
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].app, "plex");
    assert_eq!(
        problems[0].kind,
        MdnsProblemKind::NameInUse {
            label: label("plex")
        }
    );

    let moved = lan("en0", "10.0.1.77");
    let want: Announcements = [ann("immich", &moved)].into_iter().collect();
    let changes = diff(&current, &want);
    apply(announcer, &mut current, changes, 80).await;
    assert_eq!(current, want);
    let seen = wait_for(&mut states, "plex to go", |s| s.len() == 1).await;
    assert_eq!(seen.conflicts().count(), 0);
    assert_eq!(
        fake.published(),
        [Record {
            label: label("immich"),
            name: "immich.local".to_owned(),
            interface: "en0".to_owned(),
            addr: Ipv4Addr::new(10, 0, 1, 77),
            port: 80,
        }]
    );

    let changes = withdraw_all(&current);
    apply(announcer, &mut current, changes, 80).await;
    announcer.shutdown().await;
    assert!(current.is_empty());
    assert!(states.borrow().is_empty());
    let calls: Vec<String> = fake
        .calls()
        .iter()
        .map(|call| match call {
            FakeCall::Announce(r) => format!("announce {} {}", r.label, r.addr),
            FakeCall::Withdraw(l) => format!("withdraw {l}"),
            FakeCall::Shutdown => "shutdown".to_owned(),
        })
        .collect();
    assert_eq!(
        calls,
        [
            "announce immich 10.0.1.50",
            "announce plex 10.0.1.50",
            "withdraw plex",
            "announce immich 10.0.1.77",
            "withdraw immich",
            "shutdown",
        ]
    );
    assert_eq!(
        announcer.announce(ann("immich", &home), 80).await,
        Err(AnnounceError::Closed)
    );
}

#[tokio::test]
async fn fake_failure_leaves_the_change_for_the_next_tick() {
    let fake = FakeAnnouncer::default();
    let home = lan("eth0", "192.168.1.10");
    let want: Announcements = [ann("immich", &home)].into_iter().collect();
    let mut current = Announcements::default();

    fake.set_failing(Some("avahi went away"));
    let changes = diff(&current, &want);
    apply(&fake, &mut current, changes, 80).await;
    assert!(current.is_empty());

    fake.set_failing(None);
    let changes = diff(&current, &want);
    apply(&fake, &mut current, changes, 80).await;
    assert_eq!(current, want);
    assert_eq!(
        fake.states().borrow().get(&label("immich")),
        Some(&RecordState::Established)
    );
}

/// A stand-in for `dns-sd` in `dir` that appends its PID to `starts`,
/// writes its arguments to `args`, and then runs `body`, where `$8` is
/// the name.
fn stand_in(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("dns-sd");
    let script = format!(
        "#!/bin/sh\necho $$ >> '{dir}/starts'\necho \"$@\" > '{dir}/args'\n{body}\n",
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

const REGISTERED: &str =
    "echo \"12:00:00.000  Got a reply for record $8: Name now registered and active\"";
const IN_USE: &str =
    "echo \"12:00:00.000  Got a reply for record $8: Name in use, please choose another\"";

fn starts(dir: &Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join("starts"))
        .unwrap_or_default()
        .lines()
        .map(|pid| pid.parse().unwrap())
        .collect()
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn dns_sd(binary: &Path) -> DnsSdAnnouncer {
    DnsSdAnnouncer::new(binary).with_backoff(Duration::from_millis(20), Duration::from_millis(80))
}

#[tokio::test]
async fn dns_sd_runs_one_child_per_name_and_kills_it_on_withdraw() {
    let dir = tempfile::tempdir().unwrap();
    let binary = stand_in(dir.path(), &format!("{REGISTERED}\nexec sleep 600"));
    let announcer = dns_sd(&binary);
    let mut states = announcer.states();
    let home = lan("en0", "10.0.1.50");

    announcer
        .announce(ann("immich", &home), 8101)
        .await
        .unwrap();
    wait_state(&mut states, "immich", &RecordState::Established).await;
    let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
    assert_eq!(
        args.trim(),
        "-i en0 -P immich _http._tcp local 8101 immich.local 10.0.1.50"
    );

    announcer
        .announce(ann("immich", &home), 8101)
        .await
        .unwrap();
    assert_eq!(starts(dir.path()).len(), 1, "a repeat changes nothing");

    let first = starts(dir.path())[0];
    announcer
        .announce(ann("immich", &lan("en0", "10.0.1.77")), 8101)
        .await
        .unwrap();
    assert!(!alive(first), "the old child is gone");
    eventually("the new child", || starts(dir.path()).len() == 2).await;
    wait_state(&mut states, "immich", &RecordState::Established).await;
    let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
    assert!(args.trim().ends_with("immich.local 10.0.1.77"), "{args}");

    let second = starts(dir.path())[1];
    announcer.withdraw(label("immich")).await.unwrap();
    assert!(!alive(second));
    assert!(states.borrow().is_empty());
    announcer.withdraw(label("immich")).await.unwrap();
}

#[tokio::test]
async fn dns_sd_reports_a_conflict_and_does_not_restart() {
    let dir = tempfile::tempdir().unwrap();
    let binary = stand_in(
        dir.path(),
        &format!("{REGISTERED}\nsleep 0.2\n{IN_USE}\nexit 255"),
    );
    let announcer = dns_sd(&binary);
    let mut states = announcer.states();

    announcer
        .announce(ann("immich", &lan("en0", "10.0.1.50")), 80)
        .await
        .unwrap();
    wait_state(&mut states, "immich", &RecordState::Conflict).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(starts(dir.path()).len(), 1);
    assert_eq!(
        states.borrow().conflicts().collect::<Vec<_>>(),
        [&label("immich")]
    );

    announcer
        .announce(ann("immich", &lan("en0", "10.0.1.50")), 80)
        .await
        .unwrap();
    assert_eq!(
        starts(dir.path()).len(),
        1,
        "a conflict is not retried as is"
    );

    announcer
        .announce(ann("immich", &lan("en0", "10.0.1.50")), 8101)
        .await
        .unwrap();
    eventually("a new child", || starts(dir.path()).len() == 2).await;
    announcer.shutdown().await;
}

#[tokio::test]
async fn dns_sd_restarts_a_child_that_exits() {
    let dir = tempfile::tempdir().unwrap();
    let binary = stand_in(dir.path(), "exit 1");
    let announcer = dns_sd(&binary);
    let states = announcer.states();

    announcer
        .announce(ann("immich", &lan("en0", "10.0.1.50")), 80)
        .await
        .unwrap();
    eventually("three starts", || starts(dir.path()).len() >= 3).await;
    let seen = states.borrow().get(&label("immich")).cloned();
    assert!(
        matches!(&seen, Some(RecordState::Failed(why)) if why.contains("dns-sd exited")),
        "{seen:?}"
    );

    announcer.withdraw(label("immich")).await.unwrap();
    let after = starts(dir.path()).len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(starts(dir.path()).len(), after, "withdrawn names stay down");
}

#[tokio::test]
async fn dns_sd_reports_denied_and_missing_binary() {
    let dir = tempfile::tempdir().unwrap();
    let binary = stand_in(
        dir.path(),
        "echo \"12:00:00.000  Got a reply for record $8: Error -65570\"\nexec sleep 600",
    );
    let announcer = dns_sd(&binary);
    let mut states = announcer.states();
    announcer
        .announce(ann("immich", &lan("en0", "10.0.1.50")), 80)
        .await
        .unwrap();
    wait_state(&mut states, "immich", &RecordState::Denied).await;
    announcer.shutdown().await;
    assert!(!alive(starts(dir.path())[0]));
    assert_eq!(
        announcer.withdraw(label("immich")).await,
        Err(AnnounceError::Closed)
    );

    let missing = dns_sd(&dir.path().join("no-such-dns-sd"));
    let mut states = missing.states();
    missing
        .announce(ann("immich", &lan("en0", "10.0.1.50")), 80)
        .await
        .unwrap();
    let label = label("immich");
    let seen = wait_for(&mut states, "a failure", |s| {
        matches!(s.get(&label), Some(RecordState::Failed(_)))
    })
    .await;
    assert!(
        matches!(seen.get(&label), Some(RecordState::Failed(why)) if why.contains("cannot run")),
        "{seen:?}"
    );
    missing.shutdown().await;
}

/// Real backends on this host's LAN. Each publishes a name unique to the
/// test run, `alchemy-test-<pid>-<n>.local`, and withdraws it.
mod real {
    use super::*;
    use alchemyd::network::{HostNetwork as _, SystemNetwork};

    pub(super) async fn host_lan() -> LanIpv4 {
        let addresses = SystemNetwork::new().lan_addresses().await.unwrap();
        let first = addresses.first().expect("a LAN address");
        LanIpv4::new(&first.interface, first.addr).unwrap()
    }

    pub(super) fn test_name(n: u32) -> String {
        format!("alchemy-test-{}-{n}", std::process::id())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::real::{host_lan, test_name};
    use super::*;
    use alchemyd::mdns::dnssd::DNS_SD;

    /// An address on no network, for a rival to claim a name with.
    const RIVAL_IP: &str = "192.0.2.1";

    #[tokio::test]
    async fn system_uses_dns_sd() {
        let announcer = alchemyd::mdns::system().await.unwrap();
        assert_eq!(announcer.backend(), Backend::DnsSd);
        assert!(announcer.states().borrow().is_empty());
    }

    async fn resolves(name: &str) -> String {
        let out = tokio::process::Command::new("/usr/bin/dscacheutil")
            .args(["-q", "host", "-a", "name", name])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Whether `dns-sd -G v4 name` reports `addr` within a few seconds,
    /// as a resolver that asks mDNSResponder directly would.
    async fn dns_sd_resolves(name: &str, addr: &str) -> bool {
        use tokio::io::{AsyncBufReadExt as _, BufReader};

        let mut child = tokio::process::Command::new(DNS_SD)
            .args(["-G", "v4", name])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains(" Add ") && line.split_whitespace().any(|word| word == addr) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        child.kill().await.unwrap();
        found
    }

    #[tokio::test]
    #[ignore = "publishes on the LAN through mDNSResponder"]
    async fn dns_sd_announces_resolves_and_withdraws() {
        let lan = host_lan().await;
        let name = test_name(1);
        let host = format!("{name}.local");
        let announcer = DnsSdAnnouncer::new(DNS_SD);
        let mut states = announcer.states();

        announcer.announce(ann(&name, &lan), 80).await.unwrap();
        wait_state(&mut states, &name, &RecordState::Established).await;
        let found = resolves(&host).await;
        assert!(found.contains(&lan.addr().to_string()), "{found}");
        assert!(
            dns_sd_resolves(&host, &lan.addr().to_string()).await,
            "dns-sd -G v4 {host} did not report {}",
            lan.addr()
        );

        announcer.withdraw(label(&name)).await.unwrap();
        let mut gone = false;
        for _ in 0..20 {
            if !resolves(&host).await.contains(&lan.addr().to_string()) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(gone, "{host} still resolves after withdraw");
        announcer.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "publishes on the LAN through mDNSResponder"]
    async fn dns_sd_reports_a_conflict() {
        let lan = host_lan().await;
        let name = test_name(2);
        let host = format!("{name}.local");
        let mut rival = tokio::process::Command::new(DNS_SD)
            .args([
                "-i",
                lan.interface(),
                "-P",
                "rival",
                "_http._tcp",
                "local",
                "80",
            ])
            .args([&host, RIVAL_IP])
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;

        let announcer = DnsSdAnnouncer::new(DNS_SD);
        let mut states = announcer.states();
        announcer.announce(ann(&name, &lan), 80).await.unwrap();
        wait_state(&mut states, &name, &RecordState::Conflict).await;

        announcer.shutdown().await;
        rival.kill().await.unwrap();
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;

    use super::real::{host_lan, test_name};
    use super::*;
    use alchemyd::mdns::avahi::AvahiAnnouncer;
    use alchemyd::mdns::builtin::BuiltinAnnouncer;
    use mdns_sd::{DaemonEvent, HostnameResolutionEvent, IfKind, ServiceDaemon, ServiceInfo};

    async fn avahi_resolves(name: &str) -> Option<String> {
        let out = tokio::process::Command::new("avahi-resolve")
            .args(["-4", "-n", name])
            .output()
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        out.status
            .success()
            .then_some(text)
            .filter(|t| !t.is_empty())
    }

    /// A second `mdns-sd` responder on `lan`'s interface, as another
    /// device would be.
    fn peer(lan: &LanIpv4) -> ServiceDaemon {
        let daemon = ServiceDaemon::new().unwrap();
        daemon.disable_interface(IfKind::All).unwrap();
        daemon
            .enable_interface(IfKind::Name(lan.interface().to_owned()))
            .unwrap();
        daemon
    }

    /// Another address on `lan`'s subnet: `mdns-sd` publishes only
    /// addresses on the interface's subnet.
    fn rival_addr(lan: &LanIpv4) -> Ipv4Addr {
        let [a, b, c, d] = lan.addr().octets();
        Ipv4Addr::new(a, b, c, if d == 250 { 251 } else { 250 })
    }

    /// A peer that already answers for `name`.local with another
    /// address, as another device on the LAN would. Avahi doesn't see two
    /// of its own clients publishing one name as a conflict, so the rival
    /// is never avahi-publish.
    async fn rival(lan: &LanIpv4, name: &str) -> ServiceDaemon {
        let rival = peer(lan);
        let events = rival.monitor().unwrap();
        let mut info = ServiceInfo::new(
            "_http._tcp.local.",
            "rival",
            &format!("{name}.local."),
            rival_addr(lan).to_string().as_str(),
            80,
            None::<HashMap<String, String>>,
        )
        .unwrap();
        info.set_interfaces(vec![IfKind::Name(lan.interface().to_owned())]);
        rival.register(info).unwrap();
        tokio::time::timeout(WAIT, async {
            while let Ok(event) = events.recv_async().await {
                if matches!(event, DaemonEvent::Announce(..)) {
                    break;
                }
            }
        })
        .await
        .expect("the rival announced");
        rival
    }

    async fn peer_resolves(peer: &ServiceDaemon, host: &str) -> Vec<String> {
        let events = peer
            .resolve_hostname(&format!("{host}."), Some(3000))
            .unwrap();
        let mut found = Vec::new();
        while let Ok(event) = events.recv_async().await {
            match event {
                HostnameResolutionEvent::AddressesFound(_, addrs) => {
                    found.extend(addrs.iter().map(|a| a.to_ip_addr().to_string()));
                }
                HostnameResolutionEvent::SearchTimeout(_)
                | HostnameResolutionEvent::SearchStopped(_) => break,
                _ => {}
            }
        }
        drop(peer.stop_resolve_hostname(&format!("{host}.")));
        found
    }

    #[tokio::test]
    #[ignore = "publishes on the LAN through avahi-daemon"]
    async fn avahi_announces_resolves_and_withdraws() {
        let lan = host_lan().await;
        let name = test_name(1);
        let host = format!("{name}.local");
        let announcer = AvahiAnnouncer::connect()
            .await
            .unwrap()
            .expect("avahi-daemon on the system bus");
        let mut states = announcer.states();

        announcer.announce(ann(&name, &lan), 80).await.unwrap();
        wait_state(&mut states, &name, &RecordState::Established).await;
        let found = avahi_resolves(&host).await.unwrap_or_default();
        assert!(found.contains(&lan.addr().to_string()), "{found}");
        let peer = peer(&lan);
        let found = peer_resolves(&peer, &host).await;
        assert!(found.contains(&lan.addr().to_string()), "{found:?}");

        announcer.withdraw(label(&name)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(avahi_resolves(&host).await, None);
        let found = peer_resolves(&peer, &host).await;
        assert!(found.is_empty(), "{found:?}");
        announcer.shutdown().await;
        drop(peer.shutdown());
    }

    #[tokio::test]
    #[ignore = "publishes on the LAN through avahi-daemon"]
    async fn avahi_reports_a_conflict() {
        let lan = host_lan().await;
        let name = test_name(2);
        let rival = rival(&lan, &name).await;

        let announcer = AvahiAnnouncer::connect().await.unwrap().unwrap();
        let mut states = announcer.states();
        announcer.announce(ann(&name, &lan), 80).await.unwrap();
        wait_state(&mut states, &name, &RecordState::Conflict).await;

        announcer.shutdown().await;
        drop(rival.shutdown());
    }

    #[tokio::test]
    #[ignore = "publishes on the LAN with the built-in responder"]
    async fn builtin_announces_resolves_and_withdraws() {
        let lan = host_lan().await;
        let name = test_name(3);
        let host = format!("{name}.local");
        let announcer = BuiltinAnnouncer::new().unwrap();
        let mut states = announcer.states();

        announcer.announce(ann(&name, &lan), 80).await.unwrap();
        wait_state(&mut states, &name, &RecordState::Established).await;
        let peer = peer(&lan);
        let found = peer_resolves(&peer, &host).await;
        assert!(found.contains(&lan.addr().to_string()), "{found:?}");

        announcer.withdraw(label(&name)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let found = peer_resolves(&peer, &host).await;
        assert!(found.is_empty(), "{found:?}");
        announcer.shutdown().await;
        drop(peer.shutdown());
    }

    #[tokio::test]
    #[ignore = "publishes on the LAN with the built-in responder"]
    async fn builtin_reports_a_conflict() {
        let lan = host_lan().await;
        let name = test_name(4);
        let rival = rival(&lan, &name).await;

        let announcer = BuiltinAnnouncer::new().unwrap();
        let mut states = announcer.states();
        announcer.announce(ann(&name, &lan), 80).await.unwrap();
        wait_state(&mut states, &name, &RecordState::Conflict).await;

        announcer.shutdown().await;
        drop(rival.shutdown());
    }
}
