//! A Compose project on disk.

use std::path::{Path, PathBuf};

/// A Compose project on disk, known by its directory.
///
/// The directory is absolute and free of `.` and `..` components, so the
/// child's working directory never changes what it refers to.
///
/// There is no project name: Compose resolves it from the directory, a
/// `name:` in the compose file, or `COMPOSE_PROJECT_NAME` in the stack's
/// `.env`, exactly as a hand-run `docker compose` in that directory would,
/// so both always target the same project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComposeProject {
    dir: PathBuf,
}

/// Why a [`ComposeProject`] directory was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeProjectError {
    /// The directory was not an absolute path.
    #[error("compose project directory {} is not absolute", dir.display())]
    NotAbsolute {
        /// The rejected directory.
        dir: PathBuf,
    },
    /// The directory contained a `.` or `..` component.
    #[error("compose project directory {} is not normalized", dir.display())]
    NotNormalized {
        /// The rejected directory.
        dir: PathBuf,
    },
}

impl ComposeProject {
    /// The project in `dir`.
    ///
    /// Only the path's shape is checked; whether it exists is up to Compose.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, ComposeProjectError> {
        let dir = dir.into();
        if !dir.is_absolute() {
            return Err(ComposeProjectError::NotAbsolute { dir });
        }
        if has_dot_segment(&dir) {
            return Err(ComposeProjectError::NotNormalized { dir });
        }
        Ok(Self { dir })
    }

    /// The project directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Whether any `/`-separated segment of `dir` is `.` or `..`.
///
/// Checked on the raw bytes because [`Path::components`] silently drops
/// interior `.` segments.
fn has_dot_segment(dir: &Path) -> bool {
    dir.as_os_str()
        .as_encoded_bytes()
        .split(|b| *b == b'/')
        .any(|segment| segment == b"." || segment == b"..")
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
    fn validates_the_project_directory() {
        let cases = [
            ("/srv/blog", None),
            ("/srv/blog/", None),
            ("/", None),
            (
                "srv/blog",
                Some(ComposeProjectError::NotAbsolute {
                    dir: "srv/blog".into(),
                }),
            ),
            (
                "",
                Some(ComposeProjectError::NotAbsolute { dir: "".into() }),
            ),
            (
                "/srv/../etc",
                Some(ComposeProjectError::NotNormalized {
                    dir: "/srv/../etc".into(),
                }),
            ),
            (
                "/srv/blog/.",
                Some(ComposeProjectError::NotNormalized {
                    dir: "/srv/blog/.".into(),
                }),
            ),
            ("/srv/.blog/..x", None),
            (
                "/srv/./blog",
                Some(ComposeProjectError::NotNormalized {
                    dir: "/srv/./blog".into(),
                }),
            ),
        ];
        for (dir, want) in cases {
            let got = ComposeProject::new(dir);
            assert_eq!(got.as_ref().err(), want.as_ref(), "{dir:?}");
            if let Ok(project) = got {
                assert_eq!(project.dir(), Path::new(dir));
            }
        }
    }
}
