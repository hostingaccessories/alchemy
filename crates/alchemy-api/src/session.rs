//! Signing in to the TCP listener.
//!
//! The Unix socket's `0o600` mode is its only access control, and it needs
//! no sign-in. On the TCP listener every route under `/api` except
//! `GET /api/health` and `GET /api/version` answers 401 with
//! [`ErrorCode::Unauthorized`](crate::ErrorCode::Unauthorized) unless the
//! request carries one of:
//!
//! - `Authorization: Bearer TOKEN`, where `TOKEN` is the random token
//!   alchemyd writes to `tcp-token` in its state directory, readable only
//!   by the user it runs as, each time it starts. The desktop shell and
//!   tools send this;
//! - the session cookie the browser gets from `GET /login?code=CODE`,
//!   where `CODE` is a launch code from `POST /api/session/launch`.
//!
//! `POST /api/session/launch` is served on the socket only, and answers a
//! [`LaunchResponse`]; on the TCP listener it is refused. A code works
//! once, for [`LaunchResponse::expires_in`] seconds. Tokens and sessions
//! last until alchemyd stops.
//!
//! `GET /api/session` answers a [`SessionResponse`] saying how the request
//! was let in, so a client can tell it is signed in.

use serde::{Deserialize, Serialize};

/// `POST /api/session/launch`, on the socket: a one-time code to sign a
/// browser in to the TCP listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchResponse {
    /// The launch code, for `GET /login?code=CODE` on the TCP listener.
    pub code: String,
    /// The sign-in link on the address alchemyd listens on, with the code.
    pub url: String,
    /// Seconds the code stays valid.
    pub expires_in: u32,
}

/// `GET /api/session`: how this request was let in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResponse {
    /// The credential the request carried.
    pub auth: SessionAuth,
}

/// How a request was let in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionAuth {
    /// It came over the Unix socket, which needs no sign-in.
    Socket,
    /// It carried the daemon's token as a bearer token.
    Token,
    /// It carried a session cookie from a launch code.
    Cookie,
    /// A kind this build does not know, sent by a newer daemon.
    #[serde(other)]
    Unknown,
}
