//! `POST /api/engine/start`, described in [`alchemy_api::engine`], and
//! `GET /api/reconcile`, described in [`alchemy_api::reconcile`].

use alchemy_api::ErrorCode;
use alchemy_api::engine::EngineStartResult;
use alchemy_api::reconcile::ReconcileStatus;
use axum::extract;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::api_error;
use crate::State;
use crate::desktop::{self, LaunchError, SharedLauncher};

/// `POST /api/engine/start`: start Docker Desktop if it isn't running,
/// without waiting for it to be ready. The body is ignored.
pub(super) async fn start_engine(
    launcher: Option<Extension<SharedLauncher>>,
    extract::State(state): extract::State<State>,
) -> Response {
    let Some(Extension(launcher)) = launcher else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal,
            "this alchemyd can't start Docker Desktop",
        );
    };
    match desktop::start_docker(&*launcher, desktop::host_os(), &state.engine()).await {
        Ok(answer) => {
            let status = if answer.result == EngineStartResult::Starting {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            };
            (status, Json(answer)).into_response()
        }
        Err(error @ LaunchError::NotInstalled) => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::Unsupported,
            &error.to_string(),
        ),
        Err(error) => {
            tracing::warn!(%error, "cannot start Docker Desktop");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &error.to_string(),
            )
        }
    }
}

/// `GET /api/reconcile`: the last reconcile and how each app is doing.
pub(super) async fn reconcile(
    extract::State(state): extract::State<State>,
) -> Json<ReconcileStatus> {
    Json(state.reconcile())
}
