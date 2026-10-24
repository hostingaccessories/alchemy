//! IPv4 networks: subnets, Docker's address pools, and how they collide
//! with the home network.
//!
//! Everything here is a pure decision over plain values. The daemon reads
//! LAN subnets, routes, and Docker networks, then passes them in.

mod ipv4;

pub use ipv4::{Ipv4Net, Ipv4NetError, Subnets};
