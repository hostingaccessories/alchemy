//! HTTP API types shared by `alchemyd` and its web and desktop clients.
//!
//! Every route lives under [`API_PREFIX`]. Like the IPC protocol, these
//! shapes only grow additively; the JSON is pinned by the tests below.

use serde::{Deserialize, Serialize};

/// Path prefix for every API route.
pub const API_PREFIX: &str = "/api";

/// `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Always `"ok"` when the daemon can answer.
    pub status: HealthStatus,
}

/// Coarse daemon health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HealthStatus {
    /// The daemon is serving requests.
    Ok,
}

/// `GET /api/version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResponse {
    /// The daemon's release version.
    pub version: String,
}

/// Body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable detail, safe to show to the user.
    pub error: String,
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

    #[test]
    fn wire_shapes() {
        let health = HealthResponse {
            status: HealthStatus::Ok,
        };
        assert_eq!(
            serde_json::to_string(&health).unwrap(),
            r#"{"status":"ok"}"#
        );

        let version = VersionResponse {
            version: "0.1.0".into(),
        };
        assert_eq!(
            serde_json::to_string(&version).unwrap(),
            r#"{"version":"0.1.0"}"#
        );

        let error = ErrorResponse {
            error: "not found".into(),
        };
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"error":"not found"}"#
        );
    }
}
