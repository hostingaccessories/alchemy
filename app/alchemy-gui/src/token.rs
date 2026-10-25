//! alchemyd's TCP token, which signs the shell in to its TCP listener.
//!
//! The file is alchemyd's, `0o600` in its state directory, and this shell
//! runs as the same user. It is read on every call, so a restarted
//! daemon's new token is picked up the next time one is needed.

use std::ffi::OsString;
use std::path::PathBuf;

/// Read the token alchemyd wrote when it last started, or say why not in
/// a sentence safe to show.
pub fn read() -> Result<String, String> {
    let dir = state_dir(
        std::env::var_os(alchemy_api::STATE_DIR_ENV),
        alchemy_api::default_state_dir(),
    )
    .ok_or("cannot find alchemyd's state directory: set HOME or ALCHEMY_STATE_DIR")?;
    let path = alchemy_api::tcp_token_path(&dir);
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read alchemyd's token at {}: {e}", path.display()))?;
    token(&contents).ok_or_else(|| format!("{} does not hold a token", path.display()))
}

/// The state directory alchemyd uses by default: `ALCHEMY_STATE_DIR` as
/// `env`, when set and not empty, else the per-user `default`.
fn state_dir(env: Option<OsString>, default: Option<PathBuf>) -> Option<PathBuf> {
    match env {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => default,
    }
}

/// The token in the token file's `contents`, if it is one: 64 lowercase
/// hex digits, as alchemyd writes it.
fn token(contents: &str) -> Option<String> {
    let token = contents.trim();
    let hex = token
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (token.len() == 64 && hex).then(|| token.to_owned())
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

    #[test]
    fn state_dir_table() {
        let default = || Some(PathBuf::from("/home/u/.local/state/alchemy"));
        let cases = [
            (Some("/tmp/s"), default(), Some("/tmp/s")),
            (Some(""), default(), Some("/home/u/.local/state/alchemy")),
            (None, default(), Some("/home/u/.local/state/alchemy")),
            (Some("/tmp/s"), None, Some("/tmp/s")),
            (None, None, None),
        ];
        for (env, default, want) in cases {
            assert_eq!(
                state_dir(env.map(OsString::from), default),
                want.map(PathBuf::from),
                "{env:?}"
            );
        }
    }

    #[test]
    fn token_table() {
        let good = "0123456789abcdef".repeat(4);
        let cases = [
            (good.clone(), Some(good.clone())),
            (format!("{good}\n"), Some(good.clone())),
            (good.to_uppercase(), None),
            (good[1..].to_owned(), None),
            (format!("{good}0"), None),
            ("not a token".to_owned(), None),
            (String::new(), None),
        ];
        for (contents, want) in cases {
            assert_eq!(token(&contents), want, "{contents:?}");
        }
    }
}
