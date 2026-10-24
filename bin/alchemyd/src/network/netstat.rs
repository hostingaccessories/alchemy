//! Parsers for macOS `netstat`: sockets from `netstat -anv -p tcp|udp`,
//! routes from `netstat -rn -f inet`.
//!
//! The columns of `-v` change between macOS releases (macOS 12 replaced
//! `pid epid` with `process:pid` and later releases added more), so
//! sockets are read by the header, not by position. Process names are cut
//! to 16 characters by the kernel and may contain spaces, and long IPv6
//! addresses are cut short by `netstat` itself; a row whose address no
//! longer parses is skipped.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::{Listener, Protocol};

use super::{Route, unmapped};

/// The arguments for listing `protocol`'s sockets.
pub fn socket_args(protocol: Protocol) -> [&'static str; 3] {
    ["-anv", "-p", protocol.as_str()]
}

/// The arguments for listing the IPv4 routes.
pub const ROUTE_ARGS: [&str; 3] = ["-rn", "-f", "inet"];

/// The listening sockets in `netstat -anv -p tcp` (with
/// [`Protocol::Tcp`]), or the bound, unconnected ones in
/// `netstat -anv -p udp` (with [`Protocol::Udp`]).
///
/// A wildcard address is `0.0.0.0` on an IPv4 socket and `::` on an IPv6
/// or dual-stack (`tcp46`) one. Output without a `Proto` header line
/// yields nothing.
pub fn parse_sockets(text: &str, protocol: Protocol) -> Vec<Listener> {
    let mut lines = text.lines();
    let Some(header) = lines.by_ref().find(|line| line.starts_with("Proto")) else {
        return Vec::new();
    };
    let layout = Layout::of(header);
    lines
        .filter_map(|line| socket(line, protocol, layout.as_ref()))
        .collect()
}

/// Whether `netstat -anv -p tcp|udp` listed any socket at all, in any
/// state. A Mac always has some, so none means `netstat` was not allowed
/// to see them (see [`lsof`](super::lsof)).
pub fn lists_sockets(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("tcp") || line.starts_with("udp"))
}

/// The IPv4 routes in `netstat -rn`, skipping link-layer (ARP) entries.
///
/// Destinations are as `netstat` abbreviates them: `default`, `10.0.1/24`,
/// `127` (a class-sized `/8`), or a host such as `127.0.0.1` with the `H`
/// flag. Only the `Internet:` table is read when the output has several.
pub fn parse_routes(text: &str) -> Vec<Route> {
    let table = text
        .find("Internet:")
        .and_then(|start| text.get(start..))
        .unwrap_or(text);
    let mut lines = table
        .lines()
        .take_while(|line| !line.starts_with("Internet6:"));
    let Some(header) = lines.by_ref().find(|line| line.starts_with("Destination")) else {
        return Vec::new();
    };
    let columns: Vec<&str> = header.split_whitespace().collect();
    let column = |name: &str| columns.iter().position(|c| *c == name);
    let (Some(destination), Some(gateway), Some(flags), Some(netif)) = (
        column("Destination"),
        column("Gateway"),
        column("Flags"),
        column("Netif"),
    ) else {
        return Vec::new();
    };
    lines
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let flags = *fields.get(flags)?;
            if flags.contains('L') {
                return None;
            }
            Some(Route {
                destination: route_destination(fields.get(destination)?, flags.contains('H'))?,
                gateway: flags
                    .contains('G')
                    .then(|| fields.get(gateway)?.parse().ok())
                    .flatten(),
                interface: (*fields.get(netif)?).to_owned(),
            })
        })
        .collect()
}

/// Where the columns of a socket row are, from its header.
struct Layout {
    /// How many columns come before the owner column.
    before_owner: usize,
    /// How many come after it.
    after_owner: usize,
    /// Whether the owner column is `process:pid` rather than a bare `pid`.
    named: bool,
    /// Whether there is a `(state)` column, which UDP rows leave blank.
    has_state: bool,
}

impl Layout {
    fn of(header: &str) -> Option<Self> {
        let header = header
            .replace("Local Address", "Local-Address")
            .replace("Foreign Address", "Foreign-Address");
        let columns: Vec<&str> = header.split_whitespace().collect();
        let (owner, named) = match columns.iter().position(|c| *c == "process:pid") {
            Some(index) => (index, true),
            None => (columns.iter().position(|c| *c == "pid")?, false),
        };
        Some(Self {
            before_owner: owner,
            after_owner: columns.len() - owner - 1,
            named,
            has_state: columns.contains(&"(state)"),
        })
    }

    /// The owner in `fields`. The owner column may hold several fields,
    /// since a process name can contain spaces.
    fn owner(&self, fields: &[&str], protocol: Protocol) -> Option<(Option<String>, Option<u32>)> {
        let blank_state = usize::from(self.has_state && protocol == Protocol::Udp);
        let start = self.before_owner.checked_sub(blank_state)?;
        let end = fields.len().checked_sub(self.after_owner)?;
        let owner = fields.get(start..end)?.join(" ");
        if !self.named {
            return Some((None, owner.parse().ok()));
        }
        let (name, pid) = owner.rsplit_once(':')?;
        Some((
            (!name.is_empty()).then(|| name.to_owned()),
            pid.parse().ok(),
        ))
    }
}

/// The socket in `line`, if it is listening (TCP) or bound and
/// unconnected (UDP).
fn socket(line: &str, protocol: Protocol, layout: Option<&Layout>) -> Option<Listener> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [proto, _, _, local, foreign, rest @ ..] = fields.as_slice() else {
        return None;
    };
    let family = proto.strip_prefix(protocol.as_str())?;
    let bound = match protocol {
        Protocol::Tcp => rest.first() == Some(&"LISTEN"),
        Protocol::Udp => *foreign == "*.*",
    };
    if !bound {
        return None;
    }
    let (host, port) = local.rsplit_once('.')?;
    let port = port.parse().ok().filter(|p| *p != 0)?;
    let addr = match (host, family) {
        ("*", "4") => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        ("*", "6" | "46") => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ("*", _) => return None,
        (host, _) => unmapped(host_addr(host)?),
    };
    let (process, pid) = layout
        .and_then(|layout| layout.owner(&fields, protocol))
        .unwrap_or_default();
    Some(Listener {
        protocol,
        addr,
        port,
        process,
        pid,
    })
}

/// An address as `netstat` prints it, which may carry an IPv6 zone such
/// as `fe80::1%lo0`.
fn host_addr(host: &str) -> Option<IpAddr> {
    let host = host.split_once('%').map_or(host, |(addr, _zone)| addr);
    host.parse().ok()
}

fn route_destination(text: &str, host: bool) -> Option<Ipv4Net> {
    if text == "default" {
        return Ipv4Net::new(Ipv4Addr::UNSPECIFIED, 0).ok();
    }
    let (addr, prefix) = match text.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix.parse().ok()?)),
        None => (text, None),
    };
    let parts: Vec<u8> = addr
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut octets = [0_u8; 4];
    octets
        .iter_mut()
        .zip(&parts)
        .for_each(|(octet, part)| *octet = *part);
    let prefix = match prefix {
        Some(prefix) => prefix,
        None if host => 32,
        None => u8::try_from(parts.len() * 8).ok()?,
    };
    Ipv4Net::new(Ipv4Addr::from(octets), prefix).ok()
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

    fn listener(protocol: Protocol, addr: &str, port: u16, owner: Option<(&str, u32)>) -> Listener {
        Listener {
            protocol,
            addr: addr.parse().unwrap(),
            port,
            process: owner.map(|(name, _)| name.to_owned()),
            pid: owner.map(|(_, pid)| pid),
        }
    }

    fn route(destination: &str, gateway: Option<&str>, interface: &str) -> Route {
        Route {
            destination: destination.parse().unwrap(),
            gateway: gateway.map(|g| g.parse().unwrap()),
            interface: interface.into(),
        }
    }

    /// Captured on macOS 27 with remote addresses replaced.
    #[test]
    fn parse_sockets_reads_real_tcp_capture() {
        use Protocol::Tcp;
        let got = parse_sockets(include_str!("fixtures/macos-netstat-tcp.txt"), Tcp);
        let rapportd = Some(("rapportd", 670));
        let yerdd = Some(("yerdd", 53630));
        let control = Some(("ControlCenter", 663));
        assert_eq!(
            got,
            [
                listener(Tcp, "::", 62439, rapportd),
                listener(Tcp, "0.0.0.0", 62439, rapportd),
                listener(Tcp, "127.0.0.1", 7265, Some(("node", 64830))),
                listener(Tcp, "127.0.0.1", 3306, Some(("mysqld", 53726))),
                listener(Tcp, "::", 33060, Some(("mysqld", 53726))),
                listener(Tcp, "127.0.0.1", 2304, yerdd),
                listener(Tcp, "127.0.0.1", 2525, yerdd),
                listener(Tcp, "127.0.0.1", 1053, yerdd),
                listener(Tcp, "127.0.0.1", 8443, yerdd),
                listener(Tcp, "127.0.0.1", 8080, yerdd),
                listener(Tcp, "::", 5000, control),
                listener(Tcp, "0.0.0.0", 5000, control),
                listener(Tcp, "::", 7000, control),
                listener(Tcp, "0.0.0.0", 7000, control),
            ]
        );
    }

    /// Captured on macOS 27 with remote addresses replaced.
    #[test]
    fn parse_sockets_reads_real_udp_capture() {
        use Protocol::Udp;
        let got = parse_sockets(include_str!("fixtures/macos-netstat-udp.txt"), Udp);
        let rapportd = Some(("rapportd", 670));
        assert_eq!(
            got,
            [
                listener(Udp, "0.0.0.0", 3722, Some(("remotepairingd", 1714))),
                listener(Udp, "::", 50975, Some(("sharingd", 731))),
                listener(Udp, "0.0.0.0", 3722, rapportd),
                listener(Udp, "::", 3722, rapportd),
                listener(Udp, "127.0.0.1", 1053, Some(("yerdd", 53630))),
                listener(Udp, "::", 547, Some(("dhcp6d", 56678))),
            ]
        );
    }

    #[test]
    fn parse_sockets_handles_other_layouts_and_odd_rows() {
        use Protocol::{Tcp, Udp};
        let header = "Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)          rxbytes      txbytes  rhiwat  shiwat          process:pid    state  options           gencnt    flags   flags1 usecnt rtncnt fltrs";
        let current = format!(
            "{header}
tcp4       0      0  *.8080                 *.*                    LISTEN                 0            0  131072  131072      Code Helper:4242   00100 00000006 00000000006842fa 00000001 00000800      1      0 000000
tcp6       0      0  ::1.8021               *.*                    LISTEN                 0            0  131072  131072          launchd:1      00180 00000006 000000000000066f 00000000 00000800      1      0 000000
tcp6       0      0  fe80::1%lo0.631        *.*                    LISTEN                 0            0  131072  131072            cupsd:300    00180 00000006 000000000000066f 00000000 00000800      1      0 000000
tcp6       0      0  fe80::147c:74bc:.51793 *.*                    LISTEN                 0            0  131072  131072      truncated:9    00182 00000000 0000000000793023 00000081 04000900      2      0 000000
tcp4       0      0  10.0.1.50.443          *.*                    LISTEN                 0            0  131072  131072 com.docker.backe:18875  00180 00000006 000000000000066f 00000000 00000800      1      0 000000
"
        );
        let big_sur = "Active Internet connections (including servers)
Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)     rhiwat shiwat    pid   epid  state    options
tcp4       0      0  127.0.0.1.631          *.*                    LISTEN      131072 131072    345      0 0x0080 0x00000006
tcp4       0      0  10.0.1.50.52011        203.0.113.10.443       ESTABLISHED 131072 131768    512      0 0x0102 0x00000008
udp4       0      0  *.5353                 *.*                                786896   9216    210      0 0x0100 0x00000000
";
        let no_owner = "Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)
tcp4       0      0  *.22                   *.*                    LISTEN
";
        let cases = [
            (
                "current layout",
                current.as_str(),
                Tcp,
                vec![
                    listener(Tcp, "0.0.0.0", 8080, Some(("Code Helper", 4242))),
                    listener(Tcp, "::1", 8021, Some(("launchd", 1))),
                    listener(Tcp, "fe80::1", 631, Some(("cupsd", 300))),
                    listener(Tcp, "10.0.1.50", 443, Some(("com.docker.backe", 18875))),
                ],
            ),
            (
                "macOS 11, tcp",
                big_sur,
                Tcp,
                vec![Listener {
                    pid: Some(345),
                    ..listener(Tcp, "127.0.0.1", 631, None)
                }],
            ),
            (
                "macOS 11, udp",
                big_sur,
                Udp,
                vec![Listener {
                    pid: Some(210),
                    ..listener(Udp, "0.0.0.0", 5353, None)
                }],
            ),
            (
                "no owner column",
                no_owner,
                Tcp,
                vec![listener(Tcp, "0.0.0.0", 22, None)],
            ),
            ("no header", "tcp4 0 0 *.22 *.* LISTEN\n", Tcp, vec![]),
            ("empty", "", Tcp, vec![]),
        ];
        for (name, text, protocol, want) in cases {
            assert_eq!(parse_sockets(text, protocol), want, "{name}");
        }
    }

    #[test]
    fn lists_sockets_sees_any_internet_row() {
        let header = "Active Internet connections (including servers)\nProto Recv-Q Send-Q  Local Address          Foreign Address        (state)\n";
        let cases = [
            (
                "tcp capture",
                include_str!("fixtures/macos-netstat-tcp.txt"),
                true,
            ),
            (
                "udp capture",
                include_str!("fixtures/macos-netstat-udp.txt"),
                true,
            ),
            ("header only", header, false),
            ("nothing, as when hidden", "", false),
        ];
        for (name, text, want) in cases {
            assert_eq!(lists_sockets(text), want, "{name}");
        }
    }

    /// Captured on macOS 27 with link-layer addresses replaced.
    #[test]
    fn parse_routes_reads_real_capture() {
        assert_eq!(
            parse_routes(include_str!("fixtures/macos-netstat-rn.txt")),
            [
                route("0.0.0.0/0", Some("10.0.1.1"), "en0"),
                route("10.0.1.0/24", None, "en0"),
                route("10.0.1.1/32", None, "en0"),
                route("10.0.1.50/32", None, "en0"),
                route("127.0.0.0/8", None, "lo0"),
                route("127.0.0.1/32", None, "lo0"),
                route("169.254.0.0/16", None, "en0"),
                route("224.0.0.0/4", None, "en0"),
                route("255.255.255.255/32", None, "en0"),
            ]
        );
    }

    #[test]
    fn parse_routes_handles_vpns_both_families_and_old_layouts() {
        let both = "Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            192.168.1.1        UGScg                 en0
100.64/10          100.101.102.103    UGSc                utun4
10.200             link#30            UCS               bridge100      !
192.168.1          link#12            UCS                   en0      !

Internet6:
Destination                             Gateway                                 Flags               Netif Expire
default                                 fe80::%utun0                            UGcIg               utun0
";
        let catalina = "Routing tables

Internet:
Destination        Gateway            Flags        Refs      Use   Netif Expire
default            172.16.0.1         UGSc           77        0     en0
172.16/12          link#4             UCS             2        0     en0      !
";
        let cases = [
            (
                "VPN and bridge routes, IPv6 table ignored",
                both,
                vec![
                    route("0.0.0.0/0", Some("192.168.1.1"), "en0"),
                    route("100.64.0.0/10", Some("100.101.102.103"), "utun4"),
                    route("10.200.0.0/16", None, "bridge100"),
                    route("192.168.1.0/24", None, "en0"),
                ],
            ),
            (
                "macOS 10.15 with Refs and Use",
                catalina,
                vec![
                    route("0.0.0.0/0", Some("172.16.0.1"), "en0"),
                    route("172.16.0.0/12", None, "en0"),
                ],
            ),
            ("no header", "default 10.0.1.1 UGScg en0\n", vec![]),
            ("empty", "", vec![]),
        ];
        for (name, text, want) in cases {
            assert_eq!(parse_routes(text), want, "{name}");
        }
    }

    #[test]
    fn route_destinations_expand_netstat_abbreviations() {
        let cases = [
            ("default", false, Some("0.0.0.0/0")),
            ("10.0.1/24", false, Some("10.0.1.0/24")),
            ("127", false, Some("127.0.0.0/8")),
            ("169.254", false, Some("169.254.0.0/16")),
            ("192.168.1", false, Some("192.168.1.0/24")),
            ("10.0.1.1", true, Some("10.0.1.1/32")),
            ("10.0.1.1/32", false, Some("10.0.1.1/32")),
            ("224.0.0/4", false, Some("224.0.0.0/4")),
            ("10.0.1/33", false, None),
            ("fe80::1", false, None),
            ("1.2.3.4.5", false, None),
            ("", false, None),
        ];
        for (text, host, want) in cases {
            assert_eq!(
                route_destination(text, host),
                want.map(|w| w.parse().unwrap()),
                "{text}"
            );
        }
    }
}
