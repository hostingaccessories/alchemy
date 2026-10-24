//! Pure domain model for Alchemy.
//!
//! This crate performs no I/O. Anything that touches the filesystem,
//! sockets, or the Docker engine lives in the binaries and is passed
//! in as plain values.

pub mod engine;
pub mod logs;
pub mod mdns;
pub mod metrics;
pub mod network;
pub mod operation;
pub mod ports;
pub mod privilege;
pub mod route;
pub mod stack;

pub use stack::{StackName, StackNameError};

/// The Alchemy release version, shared by every crate in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
