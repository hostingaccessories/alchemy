//! Creating apps and editing their files.
//!
//! - `POST /api/apps` takes a [`NewApp`] and returns 201 with the new
//!   [`App`](super::App).
//! - `GET /api/apps/{name}/files/{compose|env}` returns an [`AppFile`],
//!   with its [`hash`](AppFile::hash) also sent as a quoted `ETag`.
//! - `PUT /api/apps/{name}/files/{compose|env}` takes an
//!   [`AppFileUpdate`] and returns the saved [`AppFile`].
//!
//! Every write is checked (at most [`MAX_FILE_LEN`] bytes, no NUL) and
//! validated with `docker compose config` before anything reaches the
//! app's folder. Compose rejecting it is 422
//! [`ErrorCode::Invalid`](crate::ErrorCode::Invalid), with Compose's own
//! message. A folder that already exists is 409
//! [`ErrorCode::Conflict`](crate::ErrorCode::Conflict), and so is a file
//! that changed on disk since it was read: an update carries the
//! [`hash`](AppFile::hash) it started from in
//! [`base_hash`](AppFileUpdate::base_hash), in the body rather than an
//! `If-Match` header, so the CLI and the web UI send it the same way.
//!
//! With `dry_run`, either write is checked and validated, conflicts
//! included, and answers as it would (200 instead of 201 for a create),
//! but nothing is written.
//!
//! With `start`, a write that saved also starts the app with
//! [`Action::Up`](crate::operations::Action::Up) once alchemyd has read
//! it again, and the answer is a [`Saved`] carrying the operation. A start
//! that is refused does not undo the save: the answer is still 201 or
//! 200, with the refusal in [`Saved::start_error`]. Asking for `start`
//! with `dry_run` is 400 [`ErrorCode::BadRequest`](crate::ErrorCode::BadRequest),
//! since a dry run saves nothing to start.

pub use alchemy_core::stack::{MAX_FILE_LEN, StackFile as AppFileKind};
use serde::{Deserialize, Serialize};

use crate::ErrorResponse;
use crate::operations::Operation;

/// The body of `POST /api/apps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewApp {
    /// The app's name, which becomes its folder's name. Checked as a
    /// [`StackName`](super::StackName); a bad one is 422.
    pub name: String,
    /// The app folder to create it in, as a full path: the home when
    /// `None`. Must be one of the configured folders.
    #[serde(default)]
    pub folder: Option<String>,
    /// The compose file, saved as `compose.yaml`.
    pub compose: String,
    /// The `.env` file, saved readable by its owner alone. `None` writes
    /// none.
    #[serde(default)]
    pub env: Option<String>,
    /// Check and validate without creating anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Start the app once it is created. Sent only when set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub start: bool,
}

/// One of an app's files, from `GET /api/apps/{name}/files/{kind}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppFile {
    /// Which file it is.
    pub kind: AppFileKind,
    /// Its name inside the app's folder, such as `compose.yaml` or `.env`.
    pub name: String,
    /// What it holds. Empty when the file does not exist.
    pub content: String,
    /// The SHA-256 of its bytes in lowercase hex, to send back as
    /// [`AppFileUpdate::base_hash`]. `None` when the file does not exist,
    /// as a `.env` may not.
    pub hash: Option<String>,
}

/// The body of `PUT /api/apps/{name}/files/{kind}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppFileUpdate {
    /// The file's new content.
    pub content: String,
    /// The [`AppFile::hash`] this edit started from. If the file on disk
    /// no longer matches, the update is 409 and nothing is written. `None`
    /// or absent means the file did not exist.
    #[serde(default)]
    pub base_hash: Option<String>,
    /// Check and validate without saving.
    #[serde(default)]
    pub dry_run: bool,
    /// Start the app once the file is saved. Sent only when set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub start: bool,
}

/// The answer to a create or an update: the created [`App`](super::App)
/// or the saved [`AppFile`] as `T`, with the fields of `T` at the top
/// level, and how starting it went when the request asked to.
///
/// Without `start`, both extra fields are left out, so the body is `T`'s
/// alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved<T> {
    /// What was saved.
    #[serde(flatten)]
    pub saved: T,
    /// The operation started on the app, running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<Operation>,
    /// Why the app was saved but not started, with the code and message
    /// `POST /api/apps/{name}/actions` would have refused with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_error: Option<ErrorResponse>,
}

impl<T> Saved<T> {
    /// `saved`, with nothing started.
    pub fn new(saved: T) -> Self {
        Self {
            saved,
            operation: None,
            start_error: None,
        }
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}
