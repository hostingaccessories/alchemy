//! IPC protocol, framing, and codec used between `alchemyd` and its
//! local clients (the `alchemy` CLI and `alchemy-helper`).
//!
//! The browser and the Tauri shell do not use this crate; they talk to
//! the daemon's HTTP API (see `alchemy-api`).
//!
//! The default build is pure: no sockets, no async, no I/O. Enable the
//! `transport` feature for the `tokio`-based helpers and the socket-path
//! resolution shared by the daemon and its clients, re-exported from
//! `alchemy-api`.
//!
//! ## Wire format
//!
//! Every message is a length-prefixed JSON frame: a 4-byte big-endian
//! `u32` length followed by `length` bytes of UTF-8 JSON. The JSON shape
//! is pinned in `tests/wire_stability.rs`; the framing edges are pinned
//! in `tests/frame_codec.rs`.
//!
//! ## Compatibility
//!
//! The protocol only grows additively. [`Request`] and [`Response`] are
//! `#[non_exhaustive]`; renaming or removing a variant is a breaking
//! change that requires bumping [`PROTOCOL_VERSION`].

mod error;
mod frame;
mod message;
mod request;
mod response;

#[cfg(feature = "transport")]
mod transport;

/// The current IPC protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

pub use error::{FrameError, IpcError};
pub use frame::{DEFAULT_MAX_FRAME, FrameDecoder, encode_frame};
pub use message::{decode_message, encode_message};
pub use request::Request;
pub use response::{ErrorCode, Response};

#[cfg(feature = "transport")]
pub use alchemy_api::{SOCKET_ENV, SOCKET_FILE, default_socket_path, runtime_dir};
#[cfg(feature = "transport")]
pub use transport::{read_frame, read_message, write_message};
