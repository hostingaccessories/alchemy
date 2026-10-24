//! `POST /api/apps` and `/api/apps/{name}/files/{compose|env}`: creating
//! apps and editing their files through the [`AppEditor`], described in
//! [`alchemy_api::apps::files`].

use std::sync::Arc;

use alchemy_api::ErrorCode;
use alchemy_api::apps::files::MAX_FILE_LEN;
use alchemy_api::apps::{AppFileKind, AppFileUpdate, NewApp};
use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection};
use axum::extract::{self, DefaultBodyLimit};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use super::api_error;
use crate::State;
use crate::apps::{AppEditor, EditError};

/// The largest body these routes accept: a compose file and a `.env` at
/// their limit, even if every byte needs a six-byte JSON escape.
pub(super) const MAX_BODY: usize = 2 * 6 * MAX_FILE_LEN + 64 * 1024;

/// The body limit for these routes.
pub(super) fn body_limit() -> DefaultBodyLimit {
    DefaultBodyLimit::max(MAX_BODY)
}

type Editor = Option<Extension<Arc<AppEditor>>>;
type FilePath = Result<extract::Path<(String, String)>, PathRejection>;

/// `POST /api/apps`: 201 with the new app, or 200 for a dry run.
pub(super) async fn create(
    editor: Editor,
    extract::State(state): extract::State<State>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Some(Extension(editor)) = editor else {
        return unavailable();
    };
    let new: NewApp = match decode(body) {
        Ok(new) => new,
        Err(response) => return *response,
    };
    let dry_run = new.dry_run;
    match editor.create(&state, new).await {
        Ok(app) if dry_run => Json(app).into_response(),
        Ok(app) => (StatusCode::CREATED, Json(app)).into_response(),
        Err(err) => refused(&err),
    }
}

/// `GET /api/apps/{name}/files/{kind}`, with the hash as an `ETag`.
pub(super) async fn read(
    editor: Editor,
    extract::State(state): extract::State<State>,
    path: FilePath,
) -> Response {
    let Some(Extension(editor)) = editor else {
        return unavailable();
    };
    let (name, kind) = match file_path(path) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    match editor.read(&state, &name, kind).await {
        Ok(file) => {
            let etag = file
                .hash
                .as_deref()
                .and_then(|hash| HeaderValue::from_str(&format!("\"{hash}\"")).ok());
            let mut response = Json(file).into_response();
            if let Some(etag) = etag {
                response.headers_mut().insert(header::ETAG, etag);
            }
            response
        }
        Err(err) => refused(&err),
    }
}

/// `PUT /api/apps/{name}/files/{kind}`: the saved file.
pub(super) async fn write(
    editor: Editor,
    extract::State(state): extract::State<State>,
    path: FilePath,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Some(Extension(editor)) = editor else {
        return unavailable();
    };
    let (name, kind) = match file_path(path) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let update: AppFileUpdate = match decode(body) {
        Ok(update) => update,
        Err(response) => return *response,
    };
    match editor.write(&state, &name, kind, update).await {
        Ok(file) => Json(file).into_response(),
        Err(err) => refused(&err),
    }
}

/// A request refused before it reaches the editor, boxed because a
/// [`Response`] is large.
type Early = Box<Response>;

fn file_path(path: FilePath) -> Result<(String, AppFileKind), Early> {
    let extract::Path((name, file)) = path.map_err(|rejection| {
        Box::new(api_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::BadRequest,
            &rejection.body_text(),
        ))
    })?;
    let kind = AppFileKind::from_segment(&file).ok_or_else(|| {
        Box::new(api_error(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "not found: an app's files are compose and env",
        ))
    })?;
    Ok((name, kind))
}

fn decode<T: DeserializeOwned>(body: Result<Bytes, BytesRejection>) -> Result<T, Early> {
    let body = body.map_err(|rejection| {
        Box::new(api_error(
            rejection.status(),
            ErrorCode::BadRequest,
            &format!(
                "the request is too large or unreadable; files are at most {} KiB",
                MAX_FILE_LEN / 1024
            ),
        ))
    })?;
    serde_json::from_slice(&body).map_err(|err| {
        Box::new(api_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::BadRequest,
            &format!("invalid request body: {err}"),
        ))
    })
}

fn refused(err: &EditError) -> Response {
    let status = match err {
        EditError::AppNotFound(_) => StatusCode::NOT_FOUND,
        EditError::Invalid(_) | EditError::ComposeFile(_) => StatusCode::UNPROCESSABLE_ENTITY,
        EditError::Conflict(_) => StatusCode::CONFLICT,
        EditError::ComposeUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::warn!(error = %err, "an app file request failed");
    }
    api_error(status, err.code(), &err.to_string())
}

fn unavailable() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Internal,
        "this alchemyd can't create or edit apps",
    )
}
