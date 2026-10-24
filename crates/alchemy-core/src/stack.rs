//! Compose stacks: their names, where they live, and how they are doing.
//!
//! The UI and CLI call a stack an app. Everything here is a pure decision
//! over plain values: the daemon lists directories and reads Compose's
//! resolved configuration, then passes the results in.

mod dirs;
mod discovery;
mod files;
mod status;

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use dirs::{MAX_EXTRA_DIRS, StackDirs, StackDirsError, default_stack_home};
pub use discovery::{
    COMPOSE_FILES, DirReport, DirState, Discovery, EntryKind, FileInfo, FoundStack, ListedDir,
    ListedEntry, Listing, OVERRIDE_FILES, SkipReason, Skipped, WATCHED_FILES, discover,
};
pub use files::{
    CreateRootError, ENV_FILE, FileContentError, MAX_FILE_LEN, MAX_MESSAGE_LEN, NEW_COMPOSE_FILE,
    STAGING_PREFIX, StackFile, check_content, create_root, scrub, staging_prefix,
};
pub use status::{ImageSource, ServiceRun, StackStatus, stack_status};

/// Maximum accepted stack name length, in bytes.
const MAX_LEN: usize = 63;

/// A validated Compose project name.
///
/// Mirrors Docker Compose's own rule: lowercase ASCII letters, digits,
/// `-` and `_`, starting with a letter or digit. The stricter length cap
/// keeps names usable as directory names and DNS labels.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StackName(String);

/// Why a candidate [`StackName`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum StackNameError {
    /// The name was empty.
    #[error("stack name must not be empty")]
    Empty,
    /// The name exceeded [`MAX_LEN`] bytes.
    #[error("stack name must be at most {MAX_LEN} characters")]
    TooLong,
    /// The first character was not a lowercase letter or digit.
    #[error("stack name must start with a lowercase letter or digit")]
    InvalidStart,
    /// The name contained a character outside `[a-z0-9_-]`.
    #[error("stack name contains invalid character {0:?}")]
    InvalidChar(char),
}

impl StackName {
    /// Validate and wrap `name`.
    pub fn new(name: impl Into<String>) -> Result<Self, StackNameError> {
        let name = name.into();
        let mut chars = name.chars();
        let first = chars.next().ok_or(StackNameError::Empty)?;
        if name.len() > MAX_LEN {
            return Err(StackNameError::TooLong);
        }
        if !is_alnum(first) {
            return Err(StackNameError::InvalidStart);
        }
        if let Some(bad) = chars.find(|&c| !(is_alnum(c) || c == '-' || c == '_')) {
            return Err(StackNameError::InvalidChar(bad));
        }
        Ok(Self(name))
    }

    /// The validated name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_alnum(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

impl fmt::Display for StackName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for StackName {
    type Error = StackNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<StackName> for String {
    fn from(value: StackName) -> Self {
        value.0
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

    #[test]
    fn accepts_valid_names() {
        for name in ["web", "my-app", "app_2", "0day", "a"] {
            assert_eq!(StackName::new(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn rejects_invalid_names() {
        let cases = [
            ("", StackNameError::Empty),
            ("-web", StackNameError::InvalidStart),
            ("_web", StackNameError::InvalidStart),
            ("Web", StackNameError::InvalidStart),
            ("web app", StackNameError::InvalidChar(' ')),
            ("web.app", StackNameError::InvalidChar('.')),
            ("wEb", StackNameError::InvalidChar('E')),
            ("../etc", StackNameError::InvalidStart),
            ("a/b", StackNameError::InvalidChar('/')),
        ];
        for (name, want) in cases {
            assert_eq!(StackName::new(name).unwrap_err(), want, "{name:?}");
        }
    }

    #[test]
    fn rejects_overlong_names() {
        assert!(StackName::new("a".repeat(MAX_LEN)).is_ok());
        assert_eq!(
            StackName::new("a".repeat(MAX_LEN + 1)).unwrap_err(),
            StackNameError::TooLong
        );
    }

    #[test]
    fn serde_validates_on_deserialize() {
        let ok: StackName = serde_json::from_str("\"web\"").unwrap();
        assert_eq!(ok.as_str(), "web");
        assert!(serde_json::from_str::<StackName>("\"../x\"").is_err());
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"web\"");
    }
}
