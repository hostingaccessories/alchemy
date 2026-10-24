//! Parser for `lsof -F` output, which macOS falls back to when `netstat`
//! shows no Internet sockets at all.
//!
//! Recent macOS releases hide every Internet socket from `netstat` when it
//! is started by a process that is not a platform binary, such as an
//! unsigned `alchemyd`. `lsof` still sees the sockets of the processes the
//! daemon's user owns, though not those of root or other users.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use alchemy_core::ports::{Listener, Protocol};

use super::unmapped;

/// The arguments for listing listening TCP and bound UDP sockets, one
/// field per line: pid, full command name, address family, protocol,
/// address, and TCP state.
pub const ARGS: [&str; 9] = [
    "-nP",
    "-w",
    "+c",
    "0",
    "-iTCP",
    "-sTCP:LISTEN",
    "-iUDP",
    "-F",
    "pcPtnT",
];

/// The listening TCP sockets and bound, unconnected UDP sockets in the
/// output of `lsof` with [`ARGS`].
pub fn parse(text: &str) -> Vec<Listener> {
    let mut listeners = Vec::new();
    let mut process = Process::default();
    let mut file: Option<File> = None;
    for line in text.lines() {
        let Some((field, value)) = line.split_at_checked(1) else {
            continue;
        };
        match field {
            "p" | "f" => {
                listeners.extend(file.take().and_then(|f| f.listener(&process)));
                if field == "p" {
                    process = Process {
                        pid: value.parse().ok(),
                        command: None,
                    };
                } else {
                    file = Some(File::default());
                }
            }
            "c" => process.command = Some(value.to_owned()),
            "t" | "P" | "n" | "T" => {
                if let Some(file) = file.as_mut() {
                    file.set(field, value);
                }
            }
            _ => {}
        }
    }
    listeners.extend(file.and_then(|f| f.listener(&process)));
    listeners
}

#[derive(Debug, Default)]
struct Process {
    pid: Option<u32>,
    command: Option<String>,
}

/// The fields of one open file.
#[derive(Debug, Default)]
struct File {
    family: String,
    protocol: String,
    name: String,
    state: Option<String>,
}

impl File {
    fn set(&mut self, field: &str, value: &str) {
        match field {
            "t" => value.clone_into(&mut self.family),
            "P" => value.clone_into(&mut self.protocol),
            "n" => value.clone_into(&mut self.name),
            _ => {
                if let Some(state) = value.strip_prefix("ST=") {
                    self.state = Some(state.to_owned());
                }
            }
        }
    }

    fn listener(self, process: &Process) -> Option<Listener> {
        let protocol = match self.protocol.as_str() {
            "TCP" if self.state.as_deref().is_none_or(|s| s == "LISTEN") => Protocol::Tcp,
            "UDP" => Protocol::Udp,
            _ => return None,
        };
        if self.name.contains("->") {
            return None;
        }
        let (host, port) = self.name.rsplit_once(':')?;
        let port = port.parse().ok().filter(|p| *p != 0)?;
        let addr = match (host, self.family.as_str()) {
            ("*", "IPv4") => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ("*", "IPv6") => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            ("*", _) => return None,
            (host, _) => {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let host = host.split_once('%').map_or(host, |(addr, _zone)| addr);
                unmapped(host.parse().ok()?)
            }
        };
        Some(Listener {
            protocol,
            addr,
            port,
            process: process.command.clone(),
            pid: process.pid,
        })
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

    fn listener(protocol: Protocol, addr: &str, port: u16, owner: (&str, u32)) -> Listener {
        Listener {
            protocol,
            addr: addr.parse().unwrap(),
            port,
            process: Some(owner.0.to_owned()),
            pid: Some(owner.1),
        }
    }

    /// Captured on macOS 27.
    #[test]
    fn parse_reads_real_capture() {
        use Protocol::{Tcp, Udp};
        let got = parse(include_str!("fixtures/macos-lsof.txt"));
        for want in [
            listener(Tcp, "0.0.0.0", 7000, ("ControlCenter", 663)),
            listener(Tcp, "::", 7000, ("ControlCenter", 663)),
            listener(Tcp, "0.0.0.0", 62439, ("rapportd", 670)),
            listener(Udp, "::", 3722, ("rapportd", 670)),
            listener(Udp, "0.0.0.0", 3722, ("rapportd", 670)),
            listener(Tcp, "127.0.0.1", 3306, ("mysqld", 53726)),
            listener(Udp, "127.0.0.1", 1053, ("yerdd", 53630)),
        ] {
            assert!(got.contains(&want), "{want:?} missing from {got:#?}");
        }
        assert!(
            got.iter().all(|l| l.port != 0 && l.pid.is_some()),
            "{got:#?}"
        );
        assert_eq!(got.len(), 22, "{got:#?}");
    }

    #[test]
    fn parse_handles_addresses_and_skips_what_is_not_bound() {
        use Protocol::{Tcp, Udp};
        let text = "p1
claunchd
f7
tIPv6
PTCP
n[::1]:8021
TST=LISTEN
f8
tIPv6
PTCP
n[fe80::1%lo0]:631
TST=LISTEN
f9
tIPv6
PTCP
n[::ffff:127.0.0.1]:8081
p4242
cCode Helper
f20
tIPv4
PUDP
n*:*
f21
tIPv4
PUDP
n10.0.1.50:5353->224.0.0.251:5353
f22
tIPv4
PTCP
n127.0.0.1:9000
TST=ESTABLISHED
f23
tIPv4
PUDP
n*:5353
p77
f3
tIPv4
PTCP
n127.0.0.1:22
TST=LISTEN
";
        let got = parse(text);
        assert_eq!(
            got,
            [
                listener(Tcp, "::1", 8021, ("launchd", 1)),
                listener(Tcp, "fe80::1", 631, ("launchd", 1)),
                listener(Tcp, "127.0.0.1", 8081, ("launchd", 1)),
                listener(Udp, "0.0.0.0", 5353, ("Code Helper", 4242)),
                Listener {
                    process: None,
                    ..listener(Tcp, "127.0.0.1", 22, ("", 77))
                },
            ]
        );
        assert_eq!(parse(""), []);
    }
}
