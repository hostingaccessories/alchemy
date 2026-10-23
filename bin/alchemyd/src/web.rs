//! Embedded web UI (`app/alchemy-web/dist`).
//!
//! Release builds embed the files at compile time; debug builds read them
//! from disk so a rebuilt UI shows up without recompiling the daemon. A
//! missing build is allowed so the Rust workspace compiles without Node.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../app/alchemy-web/dist"]
#[allow_missing = true]
struct Assets;

const INDEX: &str = "index.html";

/// Serve a UI asset, falling back to `index.html` for extension-less
/// paths so client-side routes deep-link correctly.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    if let Some(resp) = asset(path) {
        return resp;
    }
    if path.contains('.') {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset(INDEX).unwrap_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "web UI is not built; run `vp run alchemy-web#build`",
        )
            .into_response()
    })
}

fn asset(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
                (header::CACHE_CONTROL, cache.to_owned()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            ],
            file.data,
        )
            .into_response(),
    )
}
