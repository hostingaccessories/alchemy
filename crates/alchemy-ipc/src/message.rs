//! JSON message codec.

use serde::{Serialize, de::DeserializeOwned};

use crate::error::IpcError;

/// Serialise `value` as a JSON byte vector.
pub fn encode_message<T: Serialize>(value: &T) -> Result<Vec<u8>, IpcError> {
    serde_json::to_vec(value).map_err(IpcError::Encode)
}

/// Deserialise `bytes` as JSON into `T`.
pub fn decode_message<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, IpcError> {
    serde_json::from_slice(bytes).map_err(IpcError::Decode)
}
