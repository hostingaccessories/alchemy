//! Finding stacks in directory listings.
//!
//! The daemon lists each stack directory into a [`ListedDir`] and
//! [`discover`] decides, without touching the filesystem, which entries are
//! stacks and which are skipped and why.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::SystemTime;

use super::{StackName, StackNameError};

/// The compose file names Compose looks for, in its order of preference.
pub const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// The override files Compose merges on top of the compose file.
pub const OVERRIDE_FILES: [&str; 4] = [
    "compose.override.yaml",
    "compose.override.yml",
    "docker-compose.override.yaml",
    "docker-compose.override.yml",
];

/// Every file in a stack directory whose change can change what Compose
/// resolves: the compose files, the overrides, and `.env`. A lister reports
/// these for each subdirectory.
pub const WATCHED_FILES: [&str; 9] = [
    COMPOSE_FILES[0],
    COMPOSE_FILES[1],
    COMPOSE_FILES[2],
    COMPOSE_FILES[3],
    OVERRIDE_FILES[0],
    OVERRIDE_FILES[1],
    OVERRIDE_FILES[2],
    OVERRIDE_FILES[3],
    ".env",
];

/// What a directory entry is, without following symlinks except to say
/// whether a link points at a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symlink to a directory.
    LinkToDir,
    /// Any other symlink, including a dangling one.
    Link,
    /// Anything else, such as a socket or FIFO.
    Other,
}

impl EntryKind {
    fn is_link(self) -> bool {
        matches!(self, Self::Link | Self::LinkToDir)
    }
}

/// One file's type, modification time, and size, as `lstat` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileInfo {
    /// What the file is.
    pub kind: EntryKind,
    /// When it was last modified, if the filesystem says.
    pub modified: Option<SystemTime>,
    /// Its size in bytes.
    pub len: u64,
}

/// One entry of a listed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    /// The entry's file name.
    pub name: OsString,
    /// What it is.
    pub kind: EntryKind,
    /// For a [`EntryKind::Dir`], the [`WATCHED_FILES`] that exist in it, by
    /// name. Empty for anything else.
    pub files: BTreeMap<String, FileInfo>,
}

/// The result of listing one stack directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// The directory's entries, in any order, without `.` and `..`.
    Entries(Vec<ListedEntry>),
    /// Nothing exists at the path.
    Missing,
    /// Something other than a directory exists at the path.
    NotADirectory,
    /// The directory could not be read; the reason, for people.
    Unreadable(String),
}

/// One stack directory and what listing it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedDir {
    /// The directory, as configured.
    pub dir: PathBuf,
    /// What is in it.
    pub listing: Listing,
}

/// A stack found by [`discover`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundStack {
    /// The stack's name: its directory's name.
    pub name: StackName,
    /// The stack directory it was found in.
    pub root: PathBuf,
    /// The stack's own directory, `root` joined with `name`.
    pub dir: PathBuf,
    /// The compose file Compose will use, one of [`COMPOSE_FILES`].
    pub compose_file: &'static str,
    /// The watched files as listed. When these are unchanged, so is what
    /// Compose resolves (barring files the compose file includes).
    pub files: BTreeMap<String, FileInfo>,
}

/// How listing a stack directory went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirState {
    /// It was listed.
    Ok,
    /// It does not exist.
    Missing,
    /// It is not a directory.
    NotADirectory,
    /// It could not be read, for this reason.
    Unreadable(String),
}

/// Why an entry that looks like a stack was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Its directory name is not a valid [`StackName`].
    InvalidName(StackNameError),
    /// An earlier stack directory already has a stack with this name.
    Duplicate {
        /// The directory of the stack that is used instead.
        kept: PathBuf,
    },
    /// The compose file Compose would use is a symlink, which Alchemy
    /// ignores so a stack cannot point it at files elsewhere.
    LinkedComposeFile {
        /// The compose file's name.
        file: &'static str,
    },
    /// The compose file Compose would use is not a regular file.
    ComposeFileNotRegular {
        /// The compose file's name.
        file: &'static str,
    },
    /// The entry is a symlink to a directory. Alchemy only manages stacks
    /// whose directories are really inside a stack directory.
    LinkedDirectory,
}

/// An entry [`discover`] skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The entry's name, with invalid UTF-8 replaced.
    pub name: String,
    /// The entry's path.
    pub path: PathBuf,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// What [`discover`] found in one stack directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirReport {
    /// The directory.
    pub dir: PathBuf,
    /// How listing it went.
    pub state: DirState,
    /// Entries that look like stacks but were skipped, by name.
    pub skipped: Vec<Skipped>,
}

/// Every stack in every directory, and what was skipped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Discovery {
    /// The stacks, in directory order, then by name.
    pub stacks: Vec<FoundStack>,
    /// One report per directory, in the order given.
    pub dirs: Vec<DirReport>,
}

/// Decide which entries of `dirs` are stacks.
///
/// A stack is a subdirectory holding one of [`COMPOSE_FILES`]. Like
/// Compose, only the first of those that exists counts, so Alchemy and a
/// hand-run `docker compose` always read the same file: when it is a
/// symlink or not a regular file, the stack is skipped rather than falling
/// back to a later name.
///
/// When several directories hold a stack of the same name, the first
/// directory wins and the others are skipped as duplicates. Invalid names,
/// linked compose files, and linked directories are skipped and reported.
/// Entries whose names start with `.`, files, and subdirectories without a
/// compose file are not stacks and are ignored silently.
pub fn discover(dirs: &[ListedDir]) -> Discovery {
    let mut discovery = Discovery::default();
    let mut kept: BTreeMap<StackName, PathBuf> = BTreeMap::new();
    for listed in dirs {
        let mut report = DirReport {
            dir: listed.dir.clone(),
            state: DirState::Ok,
            skipped: Vec::new(),
        };
        match &listed.listing {
            Listing::Entries(entries) => {
                let mut entries: Vec<&ListedEntry> = entries.iter().collect();
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                for entry in entries {
                    match classify(listed, entry, &kept) {
                        Some(Ok(stack)) => {
                            kept.insert(stack.name.clone(), stack.dir.clone());
                            discovery.stacks.push(stack);
                        }
                        Some(Err(skipped)) => report.skipped.push(skipped),
                        None => {}
                    }
                }
            }
            Listing::Missing => report.state = DirState::Missing,
            Listing::NotADirectory => report.state = DirState::NotADirectory,
            Listing::Unreadable(why) => report.state = DirState::Unreadable(why.clone()),
        }
        discovery.dirs.push(report);
    }
    discovery
}

fn classify(
    listed: &ListedDir,
    entry: &ListedEntry,
    kept: &BTreeMap<StackName, PathBuf>,
) -> Option<Result<FoundStack, Skipped>> {
    let name = entry.name.to_string_lossy().into_owned();
    if name.starts_with('.') {
        return None;
    }
    let path = listed.dir.join(&entry.name);
    let skip = |reason| {
        Some(Err(Skipped {
            name: name.clone(),
            path: path.clone(),
            reason,
        }))
    };
    match entry.kind {
        EntryKind::Dir => {}
        EntryKind::LinkToDir => return skip(SkipReason::LinkedDirectory),
        EntryKind::File | EntryKind::Link | EntryKind::Other => return None,
    }
    let (file, info) = COMPOSE_FILES
        .iter()
        .find_map(|file| entry.files.get(*file).map(|info| (*file, info)))?;
    if info.kind.is_link() {
        return skip(SkipReason::LinkedComposeFile { file });
    }
    if info.kind != EntryKind::File {
        return skip(SkipReason::ComposeFileNotRegular { file });
    }
    let stack_name = match StackName::new(name.clone()) {
        Ok(stack_name) => stack_name,
        Err(err) => return skip(SkipReason::InvalidName(err)),
    };
    if let Some(dir) = kept.get(&stack_name) {
        return skip(SkipReason::Duplicate { kept: dir.clone() });
    }
    let watched: BTreeSet<&str> = WATCHED_FILES.into_iter().collect();
    Some(Ok(FoundStack {
        name: stack_name,
        root: listed.dir.clone(),
        dir: path,
        compose_file: file,
        files: entry
            .files
            .iter()
            .filter(|(name, _)| watched.contains(name.as_str()))
            .map(|(name, info)| (name.clone(), *info))
            .collect(),
    }))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn info(kind: EntryKind) -> FileInfo {
        FileInfo {
            kind,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
            len: 10,
        }
    }

    fn dir(name: &str, files: &[(&str, EntryKind)]) -> ListedEntry {
        ListedEntry {
            name: name.into(),
            kind: EntryKind::Dir,
            files: files
                .iter()
                .map(|(f, k)| ((*f).to_owned(), info(*k)))
                .collect(),
        }
    }

    fn entry(name: &str, kind: EntryKind) -> ListedEntry {
        ListedEntry {
            name: name.into(),
            kind,
            files: BTreeMap::new(),
        }
    }

    fn listed(path: &str, entries: Vec<ListedEntry>) -> ListedDir {
        ListedDir {
            dir: path.into(),
            listing: Listing::Entries(entries),
        }
    }

    fn names(discovery: &Discovery) -> Vec<(String, &'static str)> {
        discovery
            .stacks
            .iter()
            .map(|s| (s.dir.display().to_string(), s.compose_file))
            .collect()
    }

    fn skips(report: &DirReport) -> Vec<(&str, SkipReason)> {
        report
            .skipped
            .iter()
            .map(|s| (s.name.as_str(), s.reason.clone()))
            .collect()
    }

    use EntryKind::{Dir, File, Link, LinkToDir, Other};

    type Files = &'static [(&'static str, EntryKind)];

    #[test]
    fn picks_compose_files_in_composes_order() {
        let cases: [(Files, Option<&str>); 6] = [
            (&[("compose.yaml", File)], Some("compose.yaml")),
            (
                &[("docker-compose.yml", File), ("compose.yml", File)],
                Some("compose.yml"),
            ),
            (
                &[("docker-compose.yaml", File), ("docker-compose.yml", File)],
                Some("docker-compose.yaml"),
            ),
            (
                &[("docker-compose.yml", File), (".env", File)],
                Some("docker-compose.yml"),
            ),
            (&[("compose.override.yaml", File), (".env", File)], None),
            (&[], None),
        ];
        for (files, want) in cases {
            let found = discover(&[listed("/srv", vec![dir("web", files)])]);
            assert_eq!(
                found.stacks.first().map(|s| s.compose_file),
                want,
                "{files:?}"
            );
            assert!(found.dirs[0].skipped.is_empty(), "{files:?}");
        }
    }

    #[test]
    fn skips_and_reports_what_looks_like_a_stack_but_cannot_be_one() {
        let found = discover(&[listed(
            "/srv",
            vec![
                dir("linked", &[("compose.yaml", Link), ("compose.yml", File)]),
                dir("odd", &[("compose.yaml", Dir)]),
                dir("odd2", &[("compose.yaml", Other)]),
                dir("My App", &[("compose.yaml", File)]),
                dir("Web", &[("compose.yaml", File)]),
                entry("alias", LinkToDir),
                dir("ok", &[("compose.yaml", File)]),
            ],
        )]);
        assert_eq!(names(&found), [("/srv/ok".to_owned(), "compose.yaml")]);
        assert_eq!(
            skips(&found.dirs[0]),
            [
                (
                    "My App",
                    SkipReason::InvalidName(StackNameError::InvalidStart)
                ),
                ("Web", SkipReason::InvalidName(StackNameError::InvalidStart)),
                ("alias", SkipReason::LinkedDirectory),
                (
                    "linked",
                    SkipReason::LinkedComposeFile {
                        file: "compose.yaml"
                    }
                ),
                (
                    "odd",
                    SkipReason::ComposeFileNotRegular {
                        file: "compose.yaml"
                    }
                ),
                (
                    "odd2",
                    SkipReason::ComposeFileNotRegular {
                        file: "compose.yaml"
                    }
                ),
            ]
        );
        assert_eq!(found.dirs[0].skipped[2].path, PathBuf::from("/srv/alias"));
    }

    #[test]
    fn ignores_hidden_entries_files_and_plain_folders() {
        let found = discover(&[listed(
            "/srv",
            vec![
                dir(".git", &[("compose.yaml", File)]),
                entry(".trash", LinkToDir),
                entry("README.md", File),
                entry("notes", Link),
                entry("sock", Other),
                dir("backups", &[(".env", File)]),
            ],
        )]);
        assert_eq!(found.stacks.len(), 0);
        assert_eq!(found.dirs[0].skipped.len(), 0);
        assert_eq!(found.dirs[0].state, DirState::Ok);
    }

    #[test]
    fn first_directory_wins_a_duplicate_name() {
        let found = discover(&[
            listed(
                "/home/me/Alchemy/apps",
                vec![dir("web", &[("compose.yaml", File)])],
            ),
            listed(
                "/opt/stacks",
                vec![
                    dir("web", &[("docker-compose.yml", File)]),
                    dir("db", &[("compose.yaml", File)]),
                ],
            ),
        ]);
        assert_eq!(
            names(&found),
            [
                ("/home/me/Alchemy/apps/web".to_owned(), "compose.yaml"),
                ("/opt/stacks/db".to_owned(), "compose.yaml"),
            ]
        );
        assert_eq!(found.dirs[0].skipped.len(), 0);
        assert_eq!(
            skips(&found.dirs[1]),
            [(
                "web",
                SkipReason::Duplicate {
                    kept: "/home/me/Alchemy/apps/web".into()
                }
            )]
        );
        assert_eq!(found.stacks[1].root, PathBuf::from("/opt/stacks"));
    }

    #[test]
    fn a_skipped_stack_does_not_claim_its_name() {
        let found = discover(&[
            listed("/a", vec![dir("web", &[("compose.yaml", Link)])]),
            listed("/b", vec![dir("web", &[("compose.yaml", File)])]),
        ]);
        assert_eq!(names(&found), [("/b/web".to_owned(), "compose.yaml")]);
    }

    #[test]
    fn reports_directories_that_could_not_be_listed() {
        let found = discover(&[
            ListedDir {
                dir: "/missing".into(),
                listing: Listing::Missing,
            },
            ListedDir {
                dir: "/file".into(),
                listing: Listing::NotADirectory,
            },
            ListedDir {
                dir: "/locked".into(),
                listing: Listing::Unreadable("permission denied".into()),
            },
        ]);
        let states: Vec<_> = found.dirs.iter().map(|d| d.state.clone()).collect();
        assert_eq!(
            states,
            [
                DirState::Missing,
                DirState::NotADirectory,
                DirState::Unreadable("permission denied".into())
            ]
        );
        assert_eq!(found.stacks.len(), 0);
    }

    #[test]
    fn keeps_only_watched_files_as_the_fingerprint() {
        let mut web = dir(
            "web",
            &[
                ("compose.yaml", File),
                (".env", File),
                ("compose.override.yml", File),
            ],
        );
        web.files.insert("Dockerfile".into(), info(File));
        let found = discover(&[listed("/srv", vec![web])]);
        let files: Vec<_> = found.stacks[0].files.keys().cloned().collect();
        assert_eq!(files, [".env", "compose.override.yml", "compose.yaml"]);
    }

    #[test]
    fn orders_stacks_by_name_within_a_directory() {
        let found = discover(&[listed(
            "/srv",
            vec![
                dir("zeta", &[("compose.yaml", File)]),
                dir("alpha", &[("compose.yaml", File)]),
            ],
        )]);
        let got: Vec<_> = found.stacks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(got, ["alpha", "zeta"]);
    }
}
