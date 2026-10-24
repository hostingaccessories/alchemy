//! `GET /api/ports`: the port registry, described in
//! [`alchemy_api::ports`].

use alchemy_api::ErrorCode;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::api_error;
use crate::ports::PortRegistry;

/// `GET /api/ports`: every claimed host port and who holds it.
pub(super) async fn list(registry: Option<Extension<PortRegistry>>) -> Response {
    let Some(Extension(registry)) = registry else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "this alchemyd can't list ports",
        );
    };
    match registry.list().await {
        Ok(list) => Json(list).into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot read port reservations");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "cannot read port reservations from the state database",
            )
        }
    }
}
