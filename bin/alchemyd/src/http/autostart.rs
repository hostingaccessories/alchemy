//! `GET /api/machine/autostart` and `PUT /api/machine/autostart`,
//! described in [`alchemy_api::autostart`].

use alchemy_api::ErrorCode;
use alchemy_api::autostart::{AutostartOutcome, AutostartRequest};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::api_error;
use crate::autostart::{Autostart, AutostartError};

fn unavailable() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Internal,
        "this alchemyd can't manage autostart",
    )
}

/// `GET /api/machine/autostart`: the mode and what can be switched to,
/// read afresh, so a service installed or removed by hand shows at once.
pub(super) async fn show(autostart: Option<Extension<Autostart>>) -> Response {
    let Some(Extension(autostart)) = autostart else {
        return unavailable();
    };
    match autostart.read().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot read autostart");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &error.to_string(),
            )
        }
    }
}

/// `PUT /api/machine/autostart`: switch mode, without waiting for a
/// prompt.
pub(super) async fn set(autostart: Option<Extension<Autostart>>, body: Bytes) -> Response {
    let Some(Extension(autostart)) = autostart else {
        return unavailable();
    };
    let request: AutostartRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &format!("invalid autostart request: {error}"),
            );
        }
    };
    match autostart.set(request.mode).await {
        Ok(change) => {
            let status = match change.outcome {
                AutostartOutcome::Pending | AutostartOutcome::Restarting => StatusCode::ACCEPTED,
                _ => StatusCode::OK,
            };
            (status, Json(change)).into_response()
        }
        Err(error @ AutostartError::Busy) => api_error(
            StatusCode::CONFLICT,
            ErrorCode::Conflict,
            &error.to_string(),
        ),
        Err(error @ AutostartError::Unavailable { .. }) => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::Unsupported,
            &error.to_string(),
        ),
        Err(error) => {
            tracing::warn!(%error, "cannot change autostart");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &error.to_string(),
            )
        }
    }
}
