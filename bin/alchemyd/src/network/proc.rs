//! Parsers for Linux's `/proc/net` tables.
//!
//! The kernel prints each IPv4 address, and each 32-bit word of an IPv6
//! address, as a hex number in host byte order. These parsers read them
//! as little-endian, which every supported Linux host (`x86_64` and `arm64`)
//! is.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::{Listener, Protocol};

use super::{Route, unmapped};

/// `st` of a listening TCP socket (`TCP_LISTEN`).
const TCP_LISTEN: &str = "0A";
/// `st` of an unconnected UDP socket (`TCP_CLOSE`, reused by UDP).
const UDP_UNCONNECTED: &str = "07";
/// `RTF_UP` in a route's flags.
const RTF_UP: u16 = 0x1;
/// `RTF_GATEWAY` in a route's flags.
const RTF_GATEWAY: u16 = 0x2;

/// A socket from a `/proc/net` table and the inode that names it, which
/// `/proc/<pid>/fd` links to as `socket:[<inode>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSocket {
    /// The socket, without its owner, which the table does not show.
    pub listener: Listener,
    /// The socket's inode, `0` for one no process holds any more.
    pub inode: u64,
}

/// The listening sockets in `/proc/net/tcp` or `tcp6` (with
/// [`Protocol::Tcp`]), or the bound, unconnected ones in `/proc/net/udp`
/// or `udp6` (with [`Protocol::Udp`]).
///
/// IPv4-mapped IPv6 addresses come back as IPv4. Malformed lines and the
/// header are skipped.
pub fn parse_sockets(text: &str, protocol: Protocol) -> Vec<ProcSocket> {
    let wanted = match protocol {
        Protocol::Tcp => TCP_LISTEN,
        Protocol::Udp => UDP_UNCONNECTED,
    };
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [slot, local, _remote, state, _, _, _, _, _, inode, ..] = fields.as_slice() else {
                return None;
            };
            if !slot.ends_with(':') || *state != wanted {
                return None;
            }
            let (addr, port) = local.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok().filter(|p| *p != 0)?;
            Some(ProcSocket {
                listener: Listener {
                    protocol,
                    addr: unmapped(hex_addr(addr)?),
                    port,
                    process: None,
                    pid: None,
                },
                inode: inode.parse().ok()?,
            })
        })
        .collect()
}

/// The routes in `/proc/net/route` that are up.
///
/// The file shows the main routing table only, so routes in other tables,
/// such as Tailscale's policy routes, are not here.
pub fn parse_route(text: &str) -> Vec<Route> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [interface, destination, gateway, flags, _, _, _, mask, ..] = fields.as_slice()
            else {
                return None;
            };
            let flags = u16::from_str_radix(flags, 16).ok()?;
            if flags & RTF_UP == 0 {
                return None;
            }
            let destination =
                Ipv4Net::from_netmask(hex_ipv4(destination)?, hex_ipv4(mask)?).ok()?;
            let gateway = (flags & RTF_GATEWAY != 0)
                .then(|| hex_ipv4(gateway))
                .flatten();
            Some(Route {
                destination,
                gateway,
                interface: (*interface).to_owned(),
            })
        })
        .collect()
}

/// The inode in a `/proc/<pid>/fd` link to a socket, such as
/// `socket:[18853]`.
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// A process holding a socket: its pid and, when readable, its name from
/// `/proc/<pid>/comm`.
pub type Owner = (u32, Option<String>);

/// The listeners of `sockets`, each with its owner from `owners` (keyed by
/// inode) when known.
pub fn with_owners<S: BuildHasher>(
    sockets: Vec<ProcSocket>,
    owners: &HashMap<u64, Owner, S>,
) -> Vec<Listener> {
    sockets
        .into_iter()
        .map(|socket| {
            let mut listener = socket.listener;
            if let Some((pid, name)) = owners.get(&socket.inode) {
                listener.pid = Some(*pid);
                listener.process.clone_from(name);
            }
            listener
        })
        .collect()
}

fn hex_addr(hex: &str) -> Option<IpAddr> {
    match hex.len() {
        8 => hex_ipv4(hex).map(IpAddr::V4),
        32 => {
            let words = (0..4)
                .map(|word| {
                    let word = hex.get(word * 8..word * 8 + 8)?;
                    u32::from_str_radix(word, 16).ok().map(u32::to_le_bytes)
                })
                .collect::<Option<Vec<_>>>()?;
            let octets: [u8; 16] = words.concat().try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

fn hex_ipv4(hex: &str) -> Option<Ipv4Addr> {
    (hex.len() == 8)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
        .map(|word| Ipv4Addr::from(word.to_le_bytes()))
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

    fn socket(protocol: Protocol, addr: &str, port: u16, inode: u64) -> ProcSocket {
        ProcSocket {
            listener: Listener {
                protocol,
                addr: addr.parse().unwrap(),
                port,
                process: None,
                pid: None,
            },
            inode,
        }
    }

    fn route(destination: &str, gateway: Option<&str>, interface: &str) -> Route {
        Route {
            destination: destination.parse().unwrap(),
            gateway: gateway.map(|g| g.parse().unwrap()),
            interface: interface.into(),
        }
    }

    /// Captured on Docker Desktop's Linux VM.
    #[test]
    fn parse_sockets_reads_real_captures() {
        use Protocol::{Tcp, Udp};
        let cases = [
            (
                "tcp",
                include_str!("fixtures/linux-proc-net-tcp.txt"),
                Tcp,
                vec![
                    socket(Tcp, "0.0.0.0", 111, 10247),
                    socket(Tcp, "0.0.0.0", 56457, 4259),
                ],
            ),
            (
                "tcp6",
                include_str!("fixtures/linux-proc-net-tcp6.txt"),
                Tcp,
                vec![
                    socket(Tcp, "::", 111, 10249),
                    socket(Tcp, "::", 59923, 4263),
                ],
            ),
            (
                "udp",
                include_str!("fixtures/linux-proc-net-udp.txt"),
                Udp,
                vec![
                    socket(Udp, "0.0.0.0", 56345, 4257),
                    socket(Udp, "0.0.0.0", 111, 7188),
                    socket(Udp, "127.0.0.1", 820, 10258),
                ],
            ),
            (
                "udp6",
                include_str!("fixtures/linux-proc-net-udp6.txt"),
                Udp,
                vec![
                    socket(Udp, "::", 60687, 4261),
                    socket(Udp, "::", 111, 10248),
                ],
            ),
        ];
        for (name, text, protocol, want) in cases {
            assert_eq!(parse_sockets(text, protocol), want, "{name}");
        }
    }

    #[test]
    fn parse_sockets_keeps_listening_and_unconnected_sockets_only() {
        use Protocol::{Tcp, Udp};
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 3500007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   101        0 18853 1 0000000000000000 100 0 0 10 5
   1: 0501A8C0:0016 0A01A8C0:D2F4 01 00000000:00000000 02:000A7B2F 00000000     0        0 31337 4 0000000000000000 20 4 30 10 -1
   2: 0501A8C0:9C40 00000000:0000 06 00000000:00000000 03:00000F2A 00000000     0        0 0 3 0000000000000000
   3: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 40001 1 0000000000000000 100 0 0 10 0
   4: garbage
";
        let tcp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0277 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 21500 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000100007F:1F91 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 40002 1 0000000000000000 100 0 0 10 0
   2: 000080FE00000000FF7F1A02FE6B0C8D:0016 000080FE00000000FF7F1A02FE1B3A44:C350 01 00000000:00000000 02:00000A1E 00000000     0        0 31340 2 0000000000000000 20 4 30 10 -1
";
        let udp = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  347: 3500007F:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000   101        0 18852 2 0000000000000000 0
  362: 0501A8C0:0044 0101A8C0:0043 01 00000000:00000000 00:00000000 00000000     0        0 22001 2 0000000000000000 0
  500: 0501A8C0:0000 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 22002 2 0000000000000000 0
";
        let cases = [
            (
                "tcp: LISTEN only, not established or time-wait",
                tcp,
                Tcp,
                vec![
                    socket(Tcp, "127.0.0.53", 53, 18853),
                    socket(Tcp, "127.0.0.1", 8080, 40001),
                ],
            ),
            (
                "tcp6: loopback and an IPv4-mapped address",
                tcp6,
                Tcp,
                vec![
                    socket(Tcp, "::1", 631, 21500),
                    socket(Tcp, "127.0.0.1", 8081, 40002),
                ],
            ),
            (
                "udp: unconnected with a port only",
                udp,
                Udp,
                vec![socket(Udp, "127.0.0.53", 53, 18852)],
            ),
            ("tcp table read as udp", tcp, Udp, vec![]),
            ("empty", "", Tcp, vec![]),
        ];
        for (name, text, protocol, want) in cases {
            assert_eq!(parse_sockets(text, protocol), want, "{name}");
        }
    }

    #[test]
    fn parse_route_reads_routes_that_are_up() {
        let host =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t0000A8C0\t00000000\t0001\t0\t0\t100\t0000FFFF\t0\t0\t0
wg0\t0000080A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
eth1\t0000C80A\t00000000\t0000\t0\t0\t0\t0000FFFF\t0\t0\t0
eth2\t0000C90A\t00000000\t0001\t0\t0\t0\t00FF00FF\t0\t0\t0
";
        let cases = [
            (
                "Docker Desktop's VM, captured",
                include_str!("fixtures/linux-proc-net-route.txt"),
                vec![
                    route("127.0.0.0/8", None, "lo"),
                    route("172.17.0.0/16", None, "docker0"),
                    route("172.18.0.0/16", None, "br-fadc7d999f41"),
                    route("192.168.65.0/24", None, "eth0"),
                    route("192.168.65.7/32", None, "services1"),
                ],
            ),
            (
                "a host with a default route and a VPN; down and non-contiguous skipped",
                host,
                vec![
                    route("0.0.0.0/0", Some("192.168.1.1"), "eth0"),
                    route("192.168.0.0/16", None, "eth0"),
                    route("10.8.0.0/24", None, "wg0"),
                ],
            ),
            ("empty", "", vec![]),
        ];
        for (name, text, want) in cases {
            assert_eq!(parse_route(text), want, "{name}");
        }
    }

    #[test]
    fn with_owners_fills_in_known_owners() {
        let sockets = vec![
            socket(Protocol::Tcp, "0.0.0.0", 22, 100),
            socket(Protocol::Tcp, "127.0.0.1", 8080, 200),
            socket(Protocol::Udp, "0.0.0.0", 68, 300),
            socket(Protocol::Tcp, "::", 443, 0),
        ];
        let owners = HashMap::from([
            (200, (4242, Some("node".to_owned()))),
            (300, (77, None)),
            (999, (1, Some("init".to_owned()))),
        ]);
        let got = with_owners(sockets.clone(), &owners);
        let want = |index: usize, pid: Option<u32>, process: Option<&str>| Listener {
            pid,
            process: process.map(str::to_owned),
            ..sockets[index].listener.clone()
        };
        assert_eq!(
            got,
            [
                want(0, None, None),
                want(1, Some(4242), Some("node")),
                want(2, Some(77), None),
                want(3, None, None),
            ]
        );
    }

    #[test]
    fn socket_inode_reads_socket_links_only() {
        let cases = [
            ("socket:[18853]", Some(18853)),
            ("socket:[0]", Some(0)),
            ("pipe:[18853]", None),
            ("/dev/null", None),
            ("socket:[abc]", None),
            ("socket:[18853", None),
        ];
        for (link, want) in cases {
            assert_eq!(socket_inode(link), want, "{link}");
        }
    }
}
