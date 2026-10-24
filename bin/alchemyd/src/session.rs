//! The TCP listener's live sign-in state: this start's token, the launch
//! codes issued on the socket, and the browser sessions they became.
//!
//! The rules are in [`crate::auth`]; this adds randomness from the OS,
//! the clock, and the token file. A token, and every code and session
//! made from it, lasts only as long as this daemon, so stopping alchemyd
//! signs everything out.

use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use alchemy_api::session::{LaunchResponse, SessionAuth};
use axum::http::HeaderMap;

use crate::auth::{self, LaunchCodes, SECRET_BYTES, Sessions};
use crate::secure_fs;

/// A fresh 256-bit secret from the OS's random source, as lowercase hex.
pub fn random_secret() -> io::Result<String> {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(auth::hex(&bytes))
}

/// Sign-in state for the TCP listener, shared by both front doors: the
/// socket issues launch codes, the TCP listener redeems them and checks
/// every request. Clones share the same state.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

struct Inner {
    token: String,
    cookie: String,
    origin: String,
    codes: Mutex<LaunchCodes>,
    sessions: Mutex<Sessions>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("cookie", &self.inner.cookie)
            .field("origin", &self.inner.origin)
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// A new random token for a TCP listener on `listen`.
    pub fn generate(listen: SocketAddr) -> io::Result<Self> {
        Ok(Self::with_token(random_secret()?, listen))
    }

    /// Sign-in state for a TCP listener on `listen` whose token is
    /// `token`, for tests that need to know it.
    pub fn with_token(token: String, listen: SocketAddr) -> Self {
        Self {
            inner: Arc::new(Inner {
                token,
                cookie: auth::cookie_name(listen.port()),
                origin: auth::launch_origin(listen),
                codes: Mutex::new(LaunchCodes::new()),
                sessions: Mutex::new(Sessions::new()),
            }),
        }
    }

    /// The token, as written to the token file and sent as a bearer token.
    pub fn token(&self) -> &str {
        &self.inner.token
    }

    /// The session cookie's name.
    pub fn cookie_name(&self) -> &str {
        &self.inner.cookie
    }

    /// How a request with `headers` may be let in, or `None`.
    pub fn authenticate(&self, headers: &HeaderMap) -> Option<SessionAuth> {
        let sessions = lock(&self.inner.sessions);
        auth::authenticate(headers, &self.inner.token, &self.inner.cookie, &sessions)
    }

    /// Issue a launch code, valid from now.
    pub fn launch(&self) -> io::Result<LaunchResponse> {
        let code = random_secret()?;
        lock(&self.inner.codes).issue(&code, Instant::now());
        Ok(LaunchResponse {
            url: auth::login_url(&self.inner.origin, &code),
            code,
            expires_in: u32::try_from(auth::LAUNCH_CODE_TTL.as_secs()).unwrap_or(u32::MAX),
        })
    }

    /// Redeem launch `code` now and start a session, returning its
    /// `Set-Cookie` value. `Ok(None)` when the code is unknown, used, or
    /// expired.
    pub fn login(&self, code: &str) -> io::Result<Option<String>> {
        if !lock(&self.inner.codes).redeem(code, Instant::now()) {
            return Ok(None);
        }
        let id = random_secret()?;
        lock(&self.inner.sessions).insert(&id);
        Ok(Some(auth::set_cookie(&self.inner.cookie, &id)))
    }
}

/// A poisoned lock still holds consistent data here: every update is one
/// call that cannot panic halfway.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The token file for this start. Dropping it removes the file, unless it
/// no longer holds this start's token.
#[derive(Debug)]
pub struct TokenFile {
    path: PathBuf,
    token: String,
}

impl TokenFile {
    /// Write `token` to `path`, readable only by this user, replacing any
    /// file left by an earlier start in one rename, so a reader sees the
    /// old token or the new one and never part of one.
    pub fn write(path: &Path, token: &str) -> io::Result<Self> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("the token file has no parent directory"))?;
        let mut staged = tempfile::Builder::new()
            .prefix(".tcp-token-")
            .tempfile_in(dir)?;
        secure_fs::restrict_to_owner(staged.path())?;
        staged.write_all(token.as_bytes())?;
        staged.as_file().sync_all()?;
        staged.persist(path).map_err(|e| e.error)?;
        Ok(Self {
            path: path.to_owned(),
            token: token.to_owned(),
        })
    }

    /// Where the token was written.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        let ours = std::fs::read(&self.path).is_ok_and(|held| held == self.token.as_bytes());
        if ours && let Err(e) = std::fs::remove_file(&self.path) {
            tracing::debug!(error = %e, path = %self.path.display(), "token file removal failed");
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
    use super::*;
    use axum::http::{HeaderValue, header};

    const LISTEN: &str = "127.0.0.1:7240";

    fn bearer(token: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        map
    }

    #[test]
    fn secrets_are_random_hex() {
        let a = random_secret().unwrap();
        let b = random_secret().unwrap();
        assert!(auth::is_secret(&a), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn the_token_lets_a_bearer_in() {
        let auth = Auth::generate(LISTEN.parse().unwrap()).unwrap();
        assert_eq!(
            auth.authenticate(&bearer(auth.token())),
            Some(SessionAuth::Token)
        );
        assert_eq!(auth.authenticate(&bearer(&random_secret().unwrap())), None);
        assert_eq!(auth.authenticate(&HeaderMap::new()), None);
    }

    #[test]
    fn a_launch_code_becomes_a_session_once() {
        let auth = Auth::generate(LISTEN.parse().unwrap()).unwrap();
        let launch = auth.launch().unwrap();
        assert_eq!(launch.expires_in, 60);
        assert_eq!(
            launch.url,
            format!("http://127.0.0.1:7240/login?code={}", launch.code)
        );

        let set_cookie = auth.login(&launch.code).unwrap().unwrap();
        assert!(auth.login(&launch.code).unwrap().is_none());
        let pair = set_cookie.split(';').next().unwrap();
        assert!(pair.starts_with("alchemy_session_7240="), "{set_cookie}");
        assert!(!set_cookie.contains(auth.token()));

        let mut cookie = HeaderMap::new();
        cookie.insert(header::COOKIE, HeaderValue::from_str(pair).unwrap());
        assert_eq!(auth.authenticate(&cookie), Some(SessionAuth::Cookie));
    }

    #[test]
    fn debug_never_shows_the_token() {
        let auth = Auth::with_token("s3cret".into(), LISTEN.parse().unwrap());
        assert!(!format!("{auth:?}").contains("s3cret"));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_private_replaced_and_removed() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tcp-token");
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let file = TokenFile::write(&path, "fresh").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn dropping_leaves_a_newer_token_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tcp-token");
        let file = TokenFile::write(&path, "first").unwrap();
        std::fs::write(&path, b"second").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }
}
