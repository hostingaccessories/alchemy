//! Where `alchemy-helper` lives and whether it is safe to put a password
//! prompt in front of it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The helper's file name.
pub const HELPER: &str = "alchemy-helper";

/// The daemon's file name. The helper acts only on the `alchemyd` in its
/// own directory.
pub const ALCHEMYD: &str = "alchemyd";

/// The canonical, absolute path of an `alchemy-helper` binary, such as
/// `/usr/local/libexec/alchemy/alchemy-helper`.
///
/// It goes into argv and into an `osascript` argument list, so it must be
/// UTF-8 with no control characters, and it has no `.`, `..`, or empty
/// components, so that its ancestors are exactly the directories that
/// lead to it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HelperPath(String);

/// Why a [`HelperPath`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HelperPathError {
    /// The path doesn't start at `/`.
    #[error("the helper path {0:?} isn't absolute")]
    NotAbsolute(String),
    /// The path names no file, or has a `.`, `..`, or empty component.
    #[error("the helper path {0:?} isn't canonical")]
    NotCanonical(String),
    /// The path has a NUL or another control character.
    #[error("the helper path {0:?} has a control character")]
    ControlCharacter(String),
}

impl HelperPath {
    /// Check `path`, the helper's canonical location.
    pub fn new(path: &str) -> Result<Self, HelperPathError> {
        if path.chars().any(char::is_control) {
            return Err(HelperPathError::ControlCharacter(path.to_owned()));
        }
        let Some(rest) = path.strip_prefix('/') else {
            return Err(HelperPathError::NotAbsolute(path.to_owned()));
        };
        if rest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(HelperPathError::NotCanonical(path.to_owned()));
        }
        Ok(Self(path.to_owned()))
    }

    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// The `alchemyd` next to the helper: the only one it grants low ports
    /// to, adds to the macOS firewall, or installs as a service.
    pub fn sibling_alchemyd(&self) -> PathBuf {
        self.as_path().with_file_name(ALCHEMYD)
    }

    /// The helper, then each directory above it up to `/`: every path
    /// [`helper_trust`] needs a [`FileMeta`] for.
    pub fn trust_paths(&self) -> Vec<PathBuf> {
        self.as_path().ancestors().map(Path::to_path_buf).collect()
    }
}

impl fmt::Display for HelperPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for HelperPath {
    type Err = HelperPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for HelperPath {
    type Error = HelperPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<HelperPath> for String {
    fn from(value: HelperPath) -> Self {
        value.0
    }
}

/// What a path is, from `lstat`, so a symbolic link is seen as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// Anything else, such as a socket or a device.
    Other,
}

/// The facts about one path that decide whether it can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileMeta {
    /// What the path is.
    pub kind: FileKind,
    /// The owner's UID.
    pub uid: u32,
    /// `st_mode`; only the permission bits are read.
    pub mode: u32,
}

/// Whether the helper may be run behind a password prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "trust", rename_all = "snake_case")]
pub enum HelperTrust {
    /// The helper and every directory above it are root's alone.
    Trusted,
    /// Something along the way could be changed by a user, so the user
    /// runs the `sudo` command themselves instead.
    Untrusted {
        /// The first problem found, from the helper upward.
        why: Distrust,
    },
}

/// Why the helper isn't trusted: a path that a user other than root could
/// swap or change just before the password prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Distrust {
    /// No metadata was given for the path, such as when it couldn't be
    /// read.
    #[error("Alchemy couldn't check {}.", path.display())]
    Unchecked {
        /// The path.
        path: PathBuf,
    },
    /// The path is a symbolic link.
    #[error("{} is a symbolic link.", path.display())]
    Symlink {
        /// The path.
        path: PathBuf,
    },
    /// The helper isn't a regular file.
    #[error("{} isn't a regular file.", path.display())]
    NotAFile {
        /// The path.
        path: PathBuf,
    },
    /// A path above the helper isn't a directory.
    #[error("{} isn't a directory.", path.display())]
    NotADirectory {
        /// The path.
        path: PathBuf,
    },
    /// The path isn't owned by root.
    #[error("{} is owned by UID {uid}, not root.", path.display())]
    NotRootOwned {
        /// The path.
        path: PathBuf,
        /// Its owner.
        uid: u32,
    },
    /// The path's group can write to it.
    #[error("{} is writable by its group.", path.display())]
    GroupWritable {
        /// The path.
        path: PathBuf,
    },
    /// Anyone can write to the path.
    #[error("{} is writable by everyone.", path.display())]
    WorldWritable {
        /// The path.
        path: PathBuf,
    },
}

/// Whether `helper` can be trusted, from `meta`, the `lstat` facts for
/// each of its [`trust_paths`](HelperPath::trust_paths).
///
/// The helper must be a regular file and each ancestor a directory, all
/// owned by root and writable by neither group nor others. There are no
/// exceptions: a sticky world-writable directory such as `/tmp`, or
/// macOS's admin-writable `/Applications`, still lets a user rename what
/// is inside it. Dev builds fail this, which is the point.
pub fn helper_trust(helper: &HelperPath, meta: &BTreeMap<PathBuf, FileMeta>) -> HelperTrust {
    let first_problem = helper
        .trust_paths()
        .into_iter()
        .enumerate()
        .find_map(|(i, path)| {
            let want = if i == 0 {
                FileKind::File
            } else {
                FileKind::Dir
            };
            let Some(facts) = meta.get(&path) else {
                return Some(Distrust::Unchecked { path });
            };
            distrust(path, facts, want)
        });
    match first_problem {
        Some(why) => HelperTrust::Untrusted { why },
        None => HelperTrust::Trusted,
    }
}

fn distrust(path: PathBuf, facts: &FileMeta, want: FileKind) -> Option<Distrust> {
    if facts.kind == FileKind::Symlink {
        return Some(Distrust::Symlink { path });
    }
    if facts.kind != want {
        return Some(match want {
            FileKind::Dir => Distrust::NotADirectory { path },
            _ => Distrust::NotAFile { path },
        });
    }
    if facts.uid != 0 {
        return Some(Distrust::NotRootOwned {
            path,
            uid: facts.uid,
        });
    }
    if facts.mode & 0o002 != 0 {
        return Some(Distrust::WorldWritable { path });
    }
    if facts.mode & 0o020 != 0 {
        return Some(Distrust::GroupWritable { path });
    }
    None
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

    const FILE: FileMeta = FileMeta {
        kind: FileKind::File,
        uid: 0,
        mode: 0o100_755,
    };
    const DIR: FileMeta = FileMeta {
        kind: FileKind::Dir,
        uid: 0,
        mode: 0o040_755,
    };

    fn helper() -> HelperPath {
        HelperPath::new("/usr/libexec/alchemy/alchemy-helper").unwrap()
    }

    fn safe() -> BTreeMap<PathBuf, FileMeta> {
        [
            ("/usr/libexec/alchemy/alchemy-helper", FILE),
            ("/usr/libexec/alchemy", DIR),
            ("/usr/libexec", DIR),
            ("/usr", DIR),
            ("/", DIR),
        ]
        .into_iter()
        .map(|(p, m)| (PathBuf::from(p), m))
        .collect()
    }

    #[test]
    fn helper_paths() {
        type Case = (&'static str, Result<(), fn(String) -> HelperPathError>);
        let cases: [Case; 10] = [
            ("/usr/libexec/alchemy/alchemy-helper", Ok(())),
            ("/opt/Alchemy Tools/alchemy-helper", Ok(())),
            ("/alchemy-helper", Ok(())),
            ("alchemy-helper", Err(HelperPathError::NotAbsolute)),
            ("", Err(HelperPathError::NotAbsolute)),
            ("/", Err(HelperPathError::NotCanonical)),
            ("/usr//alchemy-helper", Err(HelperPathError::NotCanonical)),
            (
                "/usr/../tmp/alchemy-helper",
                Err(HelperPathError::NotCanonical),
            ),
            ("/usr/./alchemy-helper", Err(HelperPathError::NotCanonical)),
            (
                "/usr/bin/\nalchemy-helper",
                Err(HelperPathError::ControlCharacter),
            ),
        ];
        for (path, want) in cases {
            let got = HelperPath::new(path);
            match want {
                Ok(()) => assert_eq!(got.unwrap().as_str(), path, "{path:?}"),
                Err(err) => assert_eq!(got, Err(err(path.to_owned())), "{path:?}"),
            }
        }
        assert!(HelperPath::new("/usr/bin/").is_err());
        assert!(HelperPath::new("/usr/bin/alchemy\0helper").is_err());
    }

    #[test]
    fn sibling_and_trust_paths() {
        assert_eq!(
            helper().sibling_alchemyd(),
            PathBuf::from("/usr/libexec/alchemy/alchemyd")
        );
        assert_eq!(
            helper().trust_paths(),
            [
                "/usr/libexec/alchemy/alchemy-helper",
                "/usr/libexec/alchemy",
                "/usr/libexec",
                "/usr",
                "/",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn helper_path_serde() {
        let path = helper();
        let json = serde_json::to_value(&path).unwrap();
        assert_eq!(json, "/usr/libexec/alchemy/alchemy-helper");
        assert_eq!(serde_json::from_value::<HelperPath>(json).unwrap(), path);
        assert!(serde_json::from_value::<HelperPath>("relative/helper".into()).is_err());
        assert_eq!(path.to_string(), path.as_str());
        assert_eq!(
            "/x/alchemy-helper".parse::<HelperPath>().unwrap().as_str(),
            "/x/alchemy-helper"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn trust() {
        let p = |s: &str| PathBuf::from(s);
        let with = |path: &str, meta: FileMeta| {
            let mut all = safe();
            all.insert(p(path), meta);
            all
        };
        let without = |path: &str| {
            let mut all = safe();
            all.remove(&p(path));
            all
        };
        let cases: Vec<(&str, BTreeMap<PathBuf, FileMeta>, HelperTrust)> = vec![
            ("root's alone", safe(), HelperTrust::Trusted),
            (
                "setuid and sticky bits don't matter",
                with(
                    "/usr/libexec/alchemy/alchemy-helper",
                    FileMeta {
                        mode: 0o104_755,
                        ..FILE
                    },
                ),
                HelperTrust::Trusted,
            ),
            (
                "dev build owned by the user",
                with(
                    "/usr/libexec/alchemy/alchemy-helper",
                    FileMeta { uid: 501, ..FILE },
                ),
                HelperTrust::Untrusted {
                    why: Distrust::NotRootOwned {
                        path: p("/usr/libexec/alchemy/alchemy-helper"),
                        uid: 501,
                    },
                },
            ),
            (
                "group-writable helper",
                with(
                    "/usr/libexec/alchemy/alchemy-helper",
                    FileMeta {
                        mode: 0o100_775,
                        ..FILE
                    },
                ),
                HelperTrust::Untrusted {
                    why: Distrust::GroupWritable {
                        path: p("/usr/libexec/alchemy/alchemy-helper"),
                    },
                },
            ),
            (
                "world-writable helper",
                with(
                    "/usr/libexec/alchemy/alchemy-helper",
                    FileMeta {
                        mode: 0o100_757,
                        ..FILE
                    },
                ),
                HelperTrust::Untrusted {
                    why: Distrust::WorldWritable {
                        path: p("/usr/libexec/alchemy/alchemy-helper"),
                    },
                },
            ),
            (
                "helper is a symlink",
                with(
                    "/usr/libexec/alchemy/alchemy-helper",
                    FileMeta {
                        kind: FileKind::Symlink,
                        ..FILE
                    },
                ),
                HelperTrust::Untrusted {
                    why: Distrust::Symlink {
                        path: p("/usr/libexec/alchemy/alchemy-helper"),
                    },
                },
            ),
            (
                "helper is a directory",
                with("/usr/libexec/alchemy/alchemy-helper", DIR),
                HelperTrust::Untrusted {
                    why: Distrust::NotAFile {
                        path: p("/usr/libexec/alchemy/alchemy-helper"),
                    },
                },
            ),
            (
                "ancestor is a file",
                with("/usr/libexec", FILE),
                HelperTrust::Untrusted {
                    why: Distrust::NotADirectory {
                        path: p("/usr/libexec"),
                    },
                },
            ),
            (
                "ancestor is a symlink",
                with(
                    "/usr",
                    FileMeta {
                        kind: FileKind::Symlink,
                        ..DIR
                    },
                ),
                HelperTrust::Untrusted {
                    why: Distrust::Symlink { path: p("/usr") },
                },
            ),
            (
                "user-owned ancestor, like Homebrew's prefix",
                with("/usr/libexec", FileMeta { uid: 501, ..DIR }),
                HelperTrust::Untrusted {
                    why: Distrust::NotRootOwned {
                        path: p("/usr/libexec"),
                        uid: 501,
                    },
                },
            ),
            (
                "admin-writable ancestor, like /Applications",
                with(
                    "/usr/libexec/alchemy",
                    FileMeta {
                        mode: 0o040_775,
                        uid: 0,
                        ..DIR
                    },
                ),
                HelperTrust::Untrusted {
                    why: Distrust::GroupWritable {
                        path: p("/usr/libexec/alchemy"),
                    },
                },
            ),
            (
                "sticky world-writable root",
                with(
                    "/",
                    FileMeta {
                        mode: 0o041_777,
                        ..DIR
                    },
                ),
                HelperTrust::Untrusted {
                    why: Distrust::WorldWritable { path: p("/") },
                },
            ),
            (
                "the closest problem wins",
                {
                    let mut all = with("/usr", FileMeta { uid: 501, ..DIR });
                    all.insert(
                        p("/usr/libexec/alchemy"),
                        FileMeta {
                            mode: 0o040_777,
                            ..DIR
                        },
                    );
                    all
                },
                HelperTrust::Untrusted {
                    why: Distrust::WorldWritable {
                        path: p("/usr/libexec/alchemy"),
                    },
                },
            ),
            (
                "an ancestor wasn't checked",
                without("/usr"),
                HelperTrust::Untrusted {
                    why: Distrust::Unchecked { path: p("/usr") },
                },
            ),
            (
                "nothing was checked",
                BTreeMap::new(),
                HelperTrust::Untrusted {
                    why: Distrust::Unchecked {
                        path: p("/usr/libexec/alchemy/alchemy-helper"),
                    },
                },
            ),
        ];
        for (name, meta, want) in cases {
            assert_eq!(helper_trust(&helper(), &meta), want, "{name}");
        }
    }

    #[test]
    fn distrust_text_and_wire() {
        let why = Distrust::NotRootOwned {
            path: PathBuf::from("/home/me/alchemy/target/debug/alchemy-helper"),
            uid: 1000,
        };
        assert_eq!(
            why.to_string(),
            "/home/me/alchemy/target/debug/alchemy-helper is owned by UID 1000, not root."
        );
        assert_eq!(
            serde_json::to_value(HelperTrust::Untrusted { why }).unwrap(),
            serde_json::json!({
                "trust": "untrusted",
                "why": {
                    "kind": "not_root_owned",
                    "path": "/home/me/alchemy/target/debug/alchemy-helper",
                    "uid": 1000,
                },
            })
        );
        assert_eq!(
            serde_json::to_value(HelperTrust::Trusted).unwrap(),
            serde_json::json!({"trust": "trusted"})
        );
    }
}
