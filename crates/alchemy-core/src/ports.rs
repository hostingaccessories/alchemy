//! Host ports: what is published, who holds it, and what would collide.
//!
//! Everything here is a pure decision over plain values. The daemon reads
//! Compose files, Docker containers, host listeners, and reservations,
//! then passes them in.

mod conflict;
mod published;
mod registry;

pub use conflict::{AllocateError, PortConflict, allocate, find_conflicts};
pub use published::{
    Listener, PortRange, PortRangeError, Protocol, ProtocolError, PublishedPort, host_ips_overlap,
};
pub use registry::{
    ContainerPort, PortClaim, PortHolder, PortSource, RegistryInput, Reservation, ServicePort,
};
