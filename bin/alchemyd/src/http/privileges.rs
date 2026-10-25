//! `GET /api/machine/privileges` and
//! `POST /api/machine/privileges/{action}`, described in
//! [`alchemy_api::privileges`].

use alchemy_api::ErrorCode;
use alchemy_api::privileges::{ChangeOutcome, PrivilegeRequest};
use alchemy_core::privilege::Action;
use axum::body::Bytes;
use axum::extract::{self, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::api_error;
use crate::State;
use crate::privileges::{ChangeError, Privileges};

fn unavailable() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Internal,
        "this alchemyd can't manage privileges",
    )
}

/// `GET /api/machine/privileges`: every privilege and where it stands,
/// read afresh, so a change made from a terminal shows at once.
pub(super) async fn list(
    privileges: Option<Extension<Privileges>>,
    extract::State(state): extract::State<State>,
) -> Response {
    let Some(Extension(privileges)) = privileges else {
        return unavailable();
    };
    privileges.refresh().await;
    Json(state.privileges()).into_response()
}

/// `POST /api/machine/privileges/{action}`: grant or undo it, without
/// waiting for the prompt.
pub(super) async fn change(
    privileges: Option<Extension<Privileges>>,
    Path(action): Path<String>,
    body: Bytes,
) -> Response {
    let Some(Extension(privileges)) = privileges else {
        return unavailable();
    };
    let action: Action = match action.parse() {
        Ok(action) => action,
        Err(error) => {
            return api_error(
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                &error.to_string(),
            );
        }
    };
    let request: PrivilegeRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                &format!("invalid privilege request: {error}"),
            );
        }
    };
    match privileges.change(action, request.grant).await {
        Ok(change) => {
            let status = match change.outcome {
                ChangeOutcome::Pending | ChangeOutcome::Restarting => StatusCode::ACCEPTED,
                _ => StatusCode::OK,
            };
            (status, Json(change)).into_response()
        }
        Err(error @ ChangeError::Busy) => api_error(
            StatusCode::CONFLICT,
            ErrorCode::Conflict,
            &error.to_string(),
        ),
        Err(error @ ChangeError::Unsupported(_)) => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::Unsupported,
            &error.to_string(),
        ),
        Err(error) => {
            tracing::warn!(%error, "cannot change a privilege");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &error.to_string(),
            )
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::sync::Arc;

    use alchemy_api::network::{ListenerState, ProxyListener, ProxyReport};
    use alchemy_api::privileges::{PrivilegeAction, PrivilegeChange, PrivilegeState};
    use alchemy_core::privilege::ProxyPorts;
    use alchemy_core::route::HostOs;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, header};
    use tokio::sync::watch;
    use tower::ServiceExt as _;

    use crate::State;
    use crate::privileges::{FakeElevator, FakePrivilegeSystem, PrivilegeDeps, Privileges};
    use crate::routing::Routing;

    fn router(state: &State) -> Router {
        let store: crate::store::SharedStore = Arc::new(crate::store::MemoryStore::new());
        let ports = crate::ports::PortRegistry::new(
            state.clone(),
            store.clone(),
            Arc::new(crate::network::FakeNetwork::new()),
        );
        let operations = crate::operations::Operations::new(
            Arc::new(crate::compose::FakeComposeRunner::new()),
            state.clone(),
            store.clone(),
            ports,
        );
        let (_tx, rx) = watch::channel(false);
        super::super::api_router(
            state.clone(),
            store,
            rx,
            Arc::new(crate::logs::FakeLogSource::new()),
            operations,
            crate::session::Auth::with_token("t".repeat(64), "127.0.0.1:7240".parse().unwrap()),
        )
    }

    async fn post(app: Router, uri: &str, body: &str) -> (u16, String) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_grant_already_on_disk_restarts_alchemyd() {
        let state = State::new();
        state.set_routing(Routing {
            proxy: ProxyReport {
                host: ProxyListener {
                    port: 80,
                    app: None,
                    state: ListenerState::Unavailable,
                    message: None,
                    fix: None,
                    privilege: Some(PrivilegeAction::LowPorts),
                },
                ..ProxyReport::default()
            },
            ..Routing::default()
        });
        let system = FakePrivilegeSystem::plain();
        system.update(|f| {
            f.file_capable = true;
            f.helper = Some(crate::privileges::HelperReading {
                path: alchemy_core::privilege::HelperPath::new(
                    "/usr/libexec/alchemy/alchemy-helper",
                )
                .unwrap(),
                meta: std::collections::BTreeMap::new(),
            });
        });
        let elevator = FakeElevator::new();
        let privileges = Privileges::new(
            PrivilegeDeps::new(Arc::new(system), Arc::new(elevator.clone())).with_os(HostOs::Linux),
            state.clone(),
            ProxyPorts::DEFAULT,
        );
        let app = super::super::with_privileges(router(&state), privileges.clone());
        let (status, body) = post(
            app,
            "/api/machine/privileges/low-ports",
            r#"{"grant":true}"#,
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let change: PrivilegeChange = serde_json::from_str(&body).unwrap();
        assert_eq!(
            change.outcome,
            alchemy_api::privileges::ChangeOutcome::Restarting
        );
        assert_eq!(change.privilege.state, PrivilegeState::Pending);
        assert!(change.privilege.restarts);
        assert!(privileges.restart_requested());
        assert_eq!(elevator.commands(), Vec::new());
    }

    #[tokio::test]
    async fn without_privileges_the_routes_are_unavailable() {
        let state = State::new();
        let (status, body) = post(
            router(&state),
            "/api/machine/privileges/low-ports",
            r#"{"grant":true}"#,
        )
        .await;
        assert_eq!(status, 503, "{body}");
        let response = router(&state)
            .oneshot(
                Request::builder()
                    .uri("/api/machine/privileges")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 503);
    }
}
