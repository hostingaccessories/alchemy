//! Host network wiring: `SystemNetwork` reading this machine's real
//! sockets, routes, and interfaces.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::{IpAddr, Ipv4Addr};

use alchemy_core::ports::{Listener, Protocol};
use alchemyd::network::{HostNetwork, SharedNetwork, SystemNetwork, preferred_lan_address};

fn held_by_us(listeners: &[Listener], protocol: Protocol, port: u16) -> Option<&Listener> {
    listeners.iter().find(|l| {
        l.protocol == protocol && l.port == port && l.addr == IpAddr::V4(Ipv4Addr::LOCALHOST)
    })
}

#[tokio::test]
async fn sees_sockets_this_test_opened_with_their_owner() {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tcp_port = tcp.local_addr().unwrap().port();
    let udp_port = udp.local_addr().unwrap().port();
    let network: SharedNetwork = std::sync::Arc::new(SystemNetwork::new());

    let listeners = network.listeners().await.unwrap();

    let pid = Some(std::process::id());
    let tcp_listener = held_by_us(&listeners, Protocol::Tcp, tcp_port)
        .unwrap_or_else(|| panic!("no TCP listener on {tcp_port} in {listeners:#?}"));
    assert_eq!(tcp_listener.pid, pid, "{tcp_listener:?}");
    let udp_listener = held_by_us(&listeners, Protocol::Udp, udp_port)
        .unwrap_or_else(|| panic!("no UDP socket on {udp_port} in {listeners:#?}"));
    assert_eq!(udp_listener.pid, pid, "{udp_listener:?}");

    drop(tcp);
    let listeners = network.listeners().await.unwrap();
    assert!(
        held_by_us(&listeners, Protocol::Tcp, tcp_port).is_none(),
        "closed listener still listed"
    );
}

#[tokio::test]
async fn reads_routes_and_lan_subnets() {
    let network = SystemNetwork::new();
    let routes = network.routes().await.unwrap();
    let lan = network.lan_subnets().await.unwrap();
    for subnet in &lan {
        assert!(
            !subnet.network().is_loopback() && !subnet.network().is_link_local(),
            "{subnet} is not a LAN subnet"
        );
    }
    assert!(
        routes.iter().all(|route| !route.interface.is_empty()),
        "{routes:#?}"
    );
}

#[tokio::test]
async fn lan_addresses_are_on_the_lan_subnets() {
    let network = SystemNetwork::new();
    let addresses = network.lan_addresses().await.unwrap();
    let mut subnets: Vec<_> = addresses.iter().map(|a| a.subnet).collect();
    subnets.sort_unstable();
    subnets.dedup();
    assert_eq!(subnets, network.lan_subnets().await.unwrap());
    for address in &addresses {
        assert!(address.subnet.contains(address.addr), "{address:?}");
        assert!(!address.addr.is_loopback(), "{address:?}");
    }
    let routes = network.routes().await.unwrap();
    let preferred = preferred_lan_address(&addresses, &routes);
    assert_eq!(preferred.is_some(), !addresses.is_empty(), "{addresses:?}");
}
