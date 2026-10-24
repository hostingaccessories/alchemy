//! The directories Alchemy looks for stacks in.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// How many additional directories [`StackDirs`] accepts besides the home.
pub const MAX_EXTRA_DIRS: usize = 32;

/// Longest accepted directory path, in bytes: Linux's `PATH_MAX`.
const MAX_PATH_LEN: usize = 4096;

/// Where stacks live: the home, where new stacks go, then any additional
/// directories, in the order they are searched.
///
/// Every path is absolute, has no `..`, and is stored normalized (no `.`
/// segments, repeated or trailing slashes), so two spellings of one
/// directory compare equal. No path appears twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackDirs {
    home: PathBuf,
    extra: Vec<PathBuf>,
}

/// Why a [`StackDirs`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum StackDirsError {
    /// A path was not absolute, such as `apps` or `~/apps`.
    #[error("{} is not a full path; use one that starts with /", .0.display())]
    NotAbsolute(PathBuf),
    /// A path had a `..` segment, whose meaning depends on symlinks.
    #[error("{} contains \"..\"; use the folder's full path instead", .0.display())]
    ParentSegment(PathBuf),
    /// A path was longer than the system allows.
    #[error("{} is too long", .0.display())]
    TooLong(PathBuf),
    /// The same directory was listed twice.
    #[error("{} is listed more than once", .0.display())]
    Duplicate(PathBuf),
    /// More than [`MAX_EXTRA_DIRS`] additional directories.
    #[error("at most {MAX_EXTRA_DIRS} additional folders are supported")]
    TooMany,
}

impl StackDirs {
    /// Validate and normalize `home` and `extra`.
    pub fn new(
        home: impl Into<PathBuf>,
        extra: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, StackDirsError> {
        let home = normalize(home.into())?;
        let mut seen = vec![home.clone()];
        let mut dirs = Vec::new();
        for dir in extra {
            if dirs.len() == MAX_EXTRA_DIRS {
                return Err(StackDirsError::TooMany);
            }
            let dir = normalize(dir)?;
            if seen.contains(&dir) {
                return Err(StackDirsError::Duplicate(dir));
            }
            seen.push(dir.clone());
            dirs.push(dir);
        }
        Ok(Self { home, extra: dirs })
    }

    /// The home, where new stacks are created.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The additional directories, in search order.
    pub fn extra(&self) -> &[PathBuf] {
        &self.extra
    }

    /// Every directory in search order: the home first.
    pub fn all(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.home.as_path()).chain(self.extra.iter().map(PathBuf::as_path))
    }
}

fn normalize(dir: PathBuf) -> Result<PathBuf, StackDirsError> {
    if !dir.is_absolute() {
        return Err(StackDirsError::NotAbsolute(dir));
    }
    if dir.as_os_str().len() > MAX_PATH_LEN {
        return Err(StackDirsError::TooLong(dir));
    }
    if dir.components().any(|c| c == Component::ParentDir) {
        return Err(StackDirsError::ParentSegment(dir));
    }
    Ok(dir.components().collect())
}

/// The default home for stacks, `~/Alchemy/apps`, given the user's home
/// directory. `None` when there is no home directory or it is relative.
pub fn default_stack_home(home_dir: Option<&Path>) -> Option<PathBuf> {
    home_dir
        .filter(|h| h.is_absolute())
        .map(|h| h.join("Alchemy").join("apps"))
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

    fn paths(dirs: &[&str]) -> Vec<PathBuf> {
        dirs.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn accepts_and_normalizes_absolute_dirs() {
        let cases: [(&str, &[&str], &str, &[&str]); 4] = [
            ("/home/me/Alchemy/apps", &[], "/home/me/Alchemy/apps", &[]),
            (
                "/srv/apps/",
                &["/opt//stacks", "/data/./more"],
                "/srv/apps",
                &["/opt/stacks", "/data/more"],
            ),
            ("/", &["/srv"], "/", &["/srv"]),
            ("/srv/.apps", &["/srv/..x"], "/srv/.apps", &["/srv/..x"]),
        ];
        for (home, extra, want_home, want_extra) in cases {
            let dirs = StackDirs::new(home, paths(extra)).unwrap();
            assert_eq!(dirs.home(), Path::new(want_home), "{home}");
            assert_eq!(dirs.extra(), paths(want_extra), "{home}");
            let all: Vec<_> = dirs.all().map(Path::to_owned).collect();
            assert_eq!(all.len(), 1 + want_extra.len());
            assert_eq!(all[0], Path::new(want_home));
        }
    }

    #[test]
    fn rejects_unusable_dirs() {
        let long = format!("/{}", "a".repeat(MAX_PATH_LEN));
        let cases: [(&str, Vec<PathBuf>, StackDirsError); 7] = [
            ("apps", vec![], StackDirsError::NotAbsolute("apps".into())),
            (
                "/srv",
                paths(&["~/stacks"]),
                StackDirsError::NotAbsolute("~/stacks".into()),
            ),
            (
                "/srv/../etc",
                vec![],
                StackDirsError::ParentSegment("/srv/../etc".into()),
            ),
            ("", vec![], StackDirsError::NotAbsolute("".into())),
            (
                "/srv",
                paths(&["/srv/"]),
                StackDirsError::Duplicate("/srv".into()),
            ),
            (
                "/srv",
                paths(&["/opt", "/opt/./"]),
                StackDirsError::Duplicate("/opt".into()),
            ),
            (&long, vec![], StackDirsError::TooLong(long.clone().into())),
        ];
        for (home, extra, want) in cases {
            assert_eq!(StackDirs::new(home, extra).unwrap_err(), want, "{home}");
        }
    }

    #[test]
    fn caps_the_number_of_extra_dirs() {
        let extra = |n: usize| (0..n).map(|i| PathBuf::from(format!("/d{i}")));
        assert!(StackDirs::new("/home", extra(MAX_EXTRA_DIRS)).is_ok());
        assert_eq!(
            StackDirs::new("/home", extra(MAX_EXTRA_DIRS + 1)).unwrap_err(),
            StackDirsError::TooMany
        );
    }

    #[test]
    fn default_home_is_under_the_users_home() {
        let cases = [
            (Some("/home/me"), Some("/home/me/Alchemy/apps")),
            (Some("/Users/me/"), Some("/Users/me/Alchemy/apps")),
            (Some("relative"), None),
            (None, None),
        ];
        for (home, want) in cases {
            assert_eq!(
                default_stack_home(home.map(Path::new)),
                want.map(PathBuf::from),
                "{home:?}"
            );
        }
    }
}
