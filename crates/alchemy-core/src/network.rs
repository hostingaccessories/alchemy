//! IPv4 networks: subnets, Docker's address pools, and how they collide
//! with the home network, and where Alchemy puts its own network.
//!
//! Everything here is a pure decision over plain values. The daemon reads
//! LAN subnets, routes, and Docker networks, then passes them in.

mod collision;
mod ipv4;
mod pools;
mod shared;

pub use collision::{Collision, HostSide, NetworkSubnet, NetworkSurvey, Subject};
pub use ipv4::{Ipv4Net, Ipv4NetError, Subnets};
pub use pools::{
    AddressPool, PICKED_PREFIX, default_address_pools, default_bridge, fallback_range, pick_subnet,
    subnet_candidates,
};
pub use shared::{
    DaemonJsonFix, MANAGED_LABEL, SHARED_NETWORK, SUGGESTED_POOL_SIZE, SharedNetworkPlan,
    plan_shared_network, suggest_daemon_json,
};
