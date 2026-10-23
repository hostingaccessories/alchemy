//! HTTP API and web UI.
//!
//! Routes under `/api` return JSON; everything else is served from the
//! embedded web UI build with an `index.html` fallback for client-side
//! routing.

use alchemy_api::{ErrorResponse, HealthResponse, HealthStatus, VersionResponse};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tower_http::cors::CorsLayer;

/// Origins the Tauri shell serves its bundled UI from (macOS and Linux
/// respectively). The browser UI is same-origin and needs no CORS.
const TAURI_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

/// Build the full router: API, CORS for the desktop shell, and the web UI.
pub fn router() -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .fallback(api_not_found);

    Router::new()
        .nest(alchemy_api::API_PREFIX, api)
        .fallback(crate::web::serve)
        .layer(cors())
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(TAURI_ORIGINS.map(HeaderValue::from_static))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE])
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: HealthStatus::Ok,
    })
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: alchemy_core::VERSION.to_owned(),
    })
}

async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: "not found".to_owned(),
        }),
    )
        .into_response()
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
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt as _;

    async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = router()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn health_is_ok() {
        let (status, body) = get_json("/api/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "status": "ok" }));
    }

    #[tokio::test]
    async fn version_reports_crate_version() {
        let (status, body) = get_json("/api/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["version"], alchemy_core::VERSION);
    }

    #[tokio::test]
    async fn unknown_api_route_is_json_404() {
        let (status, body) = get_json("/api/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, serde_json::json!({ "error": "not found" }));
    }

    #[tokio::test]
    async fn cors_allows_tauri_origin_only() {
        for (origin, allowed) in [
            ("tauri://localhost", true),
            ("http://tauri.localhost", true),
            ("https://evil.example", false),
        ] {
            let resp = router()
                .oneshot(
                    Request::get("/api/health")
                        .header(header::ORIGIN, origin)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let got = resp.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN);
            assert_eq!(got.is_some(), allowed, "{origin}");
        }
    }
}
