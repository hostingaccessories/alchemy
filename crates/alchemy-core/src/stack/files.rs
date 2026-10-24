//! The files Alchemy writes in a stack directory, and the rules a new one
//! must pass before it is staged.
//!
//! Alchemy only ever writes a stack's compose file and its `.env`. Each is
//! checked here, then staged in a temporary directory inside the same
//! stack folder and validated with Compose before it is renamed into
//! place, so a rejected file never reaches the stack.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{COMPOSE_FILES, StackDirs, StackName};

/// The most bytes a compose file or `.env` may hold.
pub const MAX_FILE_LEN: usize = 256 * 1024;

/// The name of a stack's environment file, which Compose reads from the
/// project directory.
pub const ENV_FILE: &str = ".env";

/// The compose file name a new stack is created with.
pub const NEW_COMPOSE_FILE: &str = COMPOSE_FILES[0];

/// The start of every staging directory's name. The leading dot hides it
/// from discovery, which ignores hidden folders.
pub const STAGING_PREFIX: &str = ".alchemy-tmp-";

/// The most bytes of a validation message kept for people to read.
pub const MAX_MESSAGE_LEN: usize = 4 * 1024;

/// One of the files Alchemy writes in a stack directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackFile {
    /// The compose file Compose picks, such as `compose.yaml`.
    Compose,
    /// The `.env` file beside it.
    Env,
}

impl StackFile {
    /// The file as an API path segment: `compose` or `env`.
    pub fn segment(self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Env => "env",
        }
    }

    /// The file named by the path segment `segment`.
    pub fn from_segment(segment: &str) -> Option<Self> {
        match segment {
            "compose" => Some(Self::Compose),
            "env" => Some(Self::Env),
            _ => None,
        }
    }

    /// The permission bits a newly written copy gets. `.env` often holds
    /// secrets, so it is readable by its owner alone, whatever the umask.
    pub fn new_mode(self) -> u32 {
        match self {
            Self::Compose => 0o644,
            Self::Env => 0o600,
        }
    }

    /// The permission bits to write over a file whose current bits are
    /// `current`: the compose file keeps its own, and `.env` is always
    /// [`Self::new_mode`].
    pub fn replacement_mode(self, current: u32) -> u32 {
        match self {
            Self::Compose => current & 0o777,
            Self::Env => self.new_mode(),
        }
    }
}

/// Why a file's content was refused before staging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FileContentError {
    /// It is longer than [`MAX_FILE_LEN`].
    #[error("is larger than {} KiB", MAX_FILE_LEN / 1024)]
    TooLarge,
    /// It is not valid UTF-8.
    #[error("isn't UTF-8 text")]
    NotUtf8,
    /// It contains a NUL byte.
    #[error("contains a NUL byte")]
    Nul,
    /// A compose file with nothing but white space in it.
    #[error("is empty")]
    Empty,
}

/// Check `bytes` as the content of `file`, returning it as text.
pub fn check_content(file: StackFile, bytes: &[u8]) -> Result<&str, FileContentError> {
    if bytes.len() > MAX_FILE_LEN {
        return Err(FileContentError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| FileContentError::NotUtf8)?;
    if text.contains('\0') {
        return Err(FileContentError::Nul);
    }
    if file == StackFile::Compose && text.trim().is_empty() {
        return Err(FileContentError::Empty);
    }
    Ok(text)
}

/// The prefix of the staging directory for `name`, made inside the stack
/// folder so that renaming out of it stays on one filesystem.
pub fn staging_prefix(name: &StackName) -> String {
    format!("{STAGING_PREFIX}{name}-")
}

/// Why no folder was chosen for a new stack.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CreateRootError {
    /// The requested folder is not a full path, or has a `..` in it.
    #[error("{} is not a full path; use one that starts with /", .0.display())]
    NotAbsolute(PathBuf),
    /// The requested folder is not one of the stack folders.
    #[error("{} isn't one of your app folders; add it in Settings first", .0.display())]
    NotConfigured(PathBuf),
}

/// Where a new stack goes: `folder` if it is one of `dirs`, else the
/// home. Two spellings of one folder, such as with a trailing slash, match.
pub fn create_root<'a>(
    dirs: &'a StackDirs,
    folder: Option<&Path>,
) -> Result<&'a Path, CreateRootError> {
    let Some(folder) = folder else {
        return Ok(dirs.home());
    };
    if !folder.is_absolute() || folder.components().any(|c| c == Component::ParentDir) {
        return Err(CreateRootError::NotAbsolute(folder.to_owned()));
    }
    let normalized: PathBuf = folder.components().collect();
    dirs.all()
        .find(|dir| *dir == normalized)
        .ok_or_else(|| CreateRootError::NotConfigured(folder.to_owned()))
}

/// `message` with every mention of `staging` replaced: a path inside it
/// loses the prefix, so `<staging>/compose.yaml` reads `compose.yaml`, and
/// the directory itself reads as `shown_as`. Kept to [`MAX_MESSAGE_LEN`].
pub fn scrub(message: &str, staging: &Path, shown_as: &Path) -> String {
    let staging = staging.to_string_lossy();
    let scrubbed = if staging.is_empty() {
        message.to_owned()
    } else {
        message
            .replace(&format!("{staging}/"), "")
            .replace(staging.as_ref(), &shown_as.to_string_lossy())
    };
    truncate(scrubbed.trim(), MAX_MESSAGE_LEN)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let end = (0..=max)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}…", text.get(..end).unwrap_or_default())
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
    fn files_round_trip_their_segments() {
        for file in [StackFile::Compose, StackFile::Env] {
            assert_eq!(StackFile::from_segment(file.segment()), Some(file));
        }
        for bad in ["", "Compose", "compose.yaml", ".env", "override"] {
            assert_eq!(StackFile::from_segment(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn modes_keep_env_private() {
        let cases = [
            (StackFile::Compose, 0o100_640, 0o644, 0o640),
            (StackFile::Compose, 0o100_755, 0o644, 0o755),
            (StackFile::Env, 0o100_644, 0o600, 0o600),
            (StackFile::Env, 0o100_666, 0o600, 0o600),
        ];
        for (file, current, new, replacement) in cases {
            assert_eq!(file.new_mode(), new, "{file:?}");
            assert_eq!(file.replacement_mode(current), replacement, "{file:?}");
        }
    }

    #[test]
    fn checks_content() {
        let big = vec![b'a'; MAX_FILE_LEN + 1];
        let full = vec![b'a'; MAX_FILE_LEN];
        let cases: [(StackFile, &[u8], Result<(), FileContentError>); 10] = [
            (StackFile::Compose, b"services: {}\n", Ok(())),
            (StackFile::Compose, b"", Err(FileContentError::Empty)),
            (StackFile::Compose, b" \n\t\n", Err(FileContentError::Empty)),
            (StackFile::Env, b"", Ok(())),
            (StackFile::Env, b"A=1\n", Ok(())),
            (StackFile::Env, b"A=\0", Err(FileContentError::Nul)),
            (
                StackFile::Compose,
                b"x: \xff",
                Err(FileContentError::NotUtf8),
            ),
            (StackFile::Env, &big, Err(FileContentError::TooLarge)),
            (StackFile::Env, &full, Ok(())),
            (StackFile::Compose, "名前: ok".as_bytes(), Ok(())),
        ];
        for (file, bytes, want) in cases {
            assert_eq!(
                check_content(file, bytes).map(|_| ()),
                want,
                "{file:?} {}",
                bytes.len()
            );
        }
    }

    #[test]
    fn staging_names_start_hidden() {
        let name = StackName::new("blog").unwrap();
        assert_eq!(staging_prefix(&name), ".alchemy-tmp-blog-");
    }

    #[test]
    fn chooses_where_a_new_stack_goes() {
        let dirs = StackDirs::new("/home/me/apps", [PathBuf::from("/opt/stacks")]).unwrap();
        let cases: [(Option<&str>, Result<&str, CreateRootError>); 7] = [
            (None, Ok("/home/me/apps")),
            (Some("/home/me/apps"), Ok("/home/me/apps")),
            (Some("/opt/stacks"), Ok("/opt/stacks")),
            (Some("/opt/stacks/"), Ok("/opt/stacks")),
            (Some("/opt//stacks/."), Ok("/opt/stacks")),
            (
                Some("/opt/other"),
                Err(CreateRootError::NotConfigured("/opt/other".into())),
            ),
            (
                Some("/opt/stacks/../x"),
                Err(CreateRootError::NotAbsolute("/opt/stacks/../x".into())),
            ),
        ];
        for (folder, want) in cases {
            let got = create_root(&dirs, folder.map(Path::new));
            assert_eq!(got, want.map(Path::new), "{folder:?}");
        }
        assert_eq!(
            create_root(&dirs, Some(Path::new("stacks"))),
            Err(CreateRootError::NotAbsolute("stacks".into()))
        );
    }

    #[test]
    fn scrubs_staging_paths() {
        let staging = Path::new("/srv/apps/.alchemy-tmp-blog-x1/blog");
        let shown = Path::new("/srv/apps/blog");
        let cases = [
            ("", ""),
            (
                "/srv/apps/.alchemy-tmp-blog-x1/blog/compose.yaml: yaml: line 2: bad\n",
                "compose.yaml: yaml: line 2: bad",
            ),
            (
                "env file /srv/apps/.alchemy-tmp-blog-x1/blog/.env not found",
                "env file .env not found",
            ),
            (
                "project directory /srv/apps/.alchemy-tmp-blog-x1/blog is odd",
                "project directory /srv/apps/blog is odd",
            ),
            ("nothing to hide", "nothing to hide"),
        ];
        for (message, want) in cases {
            assert_eq!(scrub(message, staging, shown), want, "{message:?}");
        }
        assert_eq!(scrub(" x ", Path::new(""), shown), "x");
    }

    #[test]
    fn keeps_messages_short_on_a_character_boundary() {
        let long = "é".repeat(MAX_MESSAGE_LEN);
        let kept = scrub(&long, Path::new("/tmp/s"), Path::new("/x"));
        assert!(kept.ends_with('…'));
        assert!(kept.len() <= MAX_MESSAGE_LEN + '…'.len_utf8());
        assert!(kept.trim_end_matches('…').chars().all(|c| c == 'é'));
    }
}
