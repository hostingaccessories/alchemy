//! Error types for `alchemy-ipc`.

use thiserror::Error;

/// Framing errors produced by [`crate::encode_frame`] and
/// [`crate::FrameDecoder::next_frame`]. Pure (`Clone + Eq`).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// Declared or given size exceeds the configured cap.
    #[error("frame size {size} exceeds max {max}")]
    TooLarge {
        /// The offending size, in bytes.
        size: u64,
        /// The configured cap, in bytes.
        max: u64,
    },

    /// Payload length does not fit in the 4-byte length prefix.
    #[error("payload size {size} overflows the 4-byte length prefix")]
    PayloadOverflowsLengthPrefix {
        /// The offending size, in bytes.
        size: u64,
    },
}

/// Top-level IPC error.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IpcError {
    /// `serde_json` failed to serialise a value.
    #[error("encode: {0}")]
    Encode(#[source] serde_json::Error),

    /// `serde_json` failed to deserialise bytes.
    #[error("decode: {0}")]
    Decode(#[source] serde_json::Error),

    /// A framing error bubbled up.
    #[error("frame: {0}")]
    Frame(#[from] FrameError),

    /// EOF reached mid-frame with `bytes` already buffered.
    #[error("unexpected EOF after {bytes} buffered bytes")]
    UnexpectedEof {
        /// Bytes that were already buffered when EOF arrived.
        bytes: usize,
    },

    /// Underlying I/O failed. Only the [`std::io::ErrorKind`] is kept so
    /// the error stays cheap to move across task boundaries.
    #[error("io: {kind}")]
    Io {
        /// The OS-level error category.
        kind: std::io::ErrorKind,
    },
}

impl From<std::io::Error> for IpcError {
    fn from(e: std::io::Error) -> Self {
        Self::Io { kind: e.kind() }
    }
}
