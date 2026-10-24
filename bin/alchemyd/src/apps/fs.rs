//! Listing app folders: the [`AppFs`] seam.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use alchemy_core::stack::{EntryKind, FileInfo, ListedEntry, Listing, WATCHED_FILES};

/// Lists an app folder the way [`alchemy_core::stack::discover`] needs it.
///
/// Implementations only read: nothing in an app folder is ever created,
/// changed, or removed by listing it. Calls block, so callers run them off
/// the async runtime.
pub trait AppFs: Send + Sync + fmt::Debug {
    /// List `dir`: every entry with its kind, and for each subdirectory the
    /// [`WATCHED_FILES`] in it.
    fn list(&self, dir: &Path) -> Listing;
}

/// [`AppFs`] over the real filesystem.
///
/// `dir` itself may be a symlink to a directory, since the user chose it.
/// Entries inside it are looked at with `lstat`: a linked entry is reported
/// as a link, never followed into.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemFs;

impl AppFs for SystemFs {
    fn list(&self, dir: &Path) -> Listing {
        match fs::metadata(dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Listing::NotADirectory,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Listing::Missing,
            Err(err) => return Listing::Unreadable(err.to_string()),
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) => return Listing::Unreadable(err.to_string()),
        };
        let listed = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let kind = entry_kind(entry.file_type().ok()?, &path);
                let files = if kind == EntryKind::Dir {
                    watched_files(&path)
                } else {
                    BTreeMap::new()
                };
                Some(ListedEntry {
                    name: entry.file_name(),
                    kind,
                    files,
                })
            })
            .collect();
        Listing::Entries(listed)
    }
}

fn entry_kind(file_type: FileType, path: &Path) -> EntryKind {
    if file_type.is_symlink() {
        if fs::metadata(path).is_ok_and(|m| m.is_dir()) {
            EntryKind::LinkToDir
        } else {
            EntryKind::Link
        }
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn watched_files(dir: &Path) -> BTreeMap<String, FileInfo> {
    WATCHED_FILES
        .iter()
        .filter_map(|name| {
            let meta = fs::symlink_metadata(dir.join(name)).ok()?;
            Some(((*name).to_owned(), file_info(&meta, &dir.join(name))))
        })
        .collect()
}

fn file_info(meta: &Metadata, path: &Path) -> FileInfo {
    FileInfo {
        kind: entry_kind(meta.file_type(), path),
        modified: meta.modified().ok(),
        len: meta.len(),
    }
}

/// [`AppFs`] that answers from a table, for tests. A directory not in the
/// table is [`Listing::Missing`]. Clones share the table.
#[derive(Debug, Clone, Default)]
pub struct FakeFs {
    listings: Arc<Mutex<HashMap<PathBuf, Listing>>>,
}

impl FakeFs {
    /// A filesystem with nothing in it.
    pub fn new() -> Self {
        Self::default()
    }

    /// List `dir` as `listing` from now on.
    pub fn set(&self, dir: impl Into<PathBuf>, listing: Listing) {
        self.listings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(dir.into(), listing);
    }
}

impl AppFs for FakeFs {
    fn list(&self, dir: &Path) -> Listing {
        self.listings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(dir)
            .cloned()
            .unwrap_or(Listing::Missing)
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
    use std::os::unix::fs::symlink;

    use super::*;

    type Entry = (String, EntryKind, Vec<(String, EntryKind)>);

    fn entries(listing: Listing) -> Vec<Entry> {
        let Listing::Entries(mut entries) = listing else {
            panic!("{listing:?}");
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
            .into_iter()
            .map(|e| {
                (
                    e.name.to_string_lossy().into_owned(),
                    e.kind,
                    e.files.into_iter().map(|(n, i)| (n, i.kind)).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn lists_entries_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        let apps = root.path().join("apps");
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir_all(apps.join("web")).unwrap();
        fs::create_dir_all(apps.join("linked")).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("compose.yaml"), "services: {}\n").unwrap();
        fs::write(apps.join("web/compose.yaml"), "services: {}\n").unwrap();
        fs::write(apps.join("web/.env"), "A=1\n").unwrap();
        fs::write(apps.join("web/Dockerfile"), "FROM x\n").unwrap();
        symlink(
            elsewhere.join("compose.yaml"),
            apps.join("linked/compose.yaml"),
        )
        .unwrap();
        symlink(&elsewhere, apps.join("alias")).unwrap();
        symlink(root.path().join("nope"), apps.join("dangling")).unwrap();
        fs::write(apps.join("README.md"), "hi\n").unwrap();

        let before = fs::read_dir(&apps).unwrap().count();
        let got = entries(SystemFs.list(&apps));
        assert_eq!(
            got,
            [
                ("README.md".into(), EntryKind::File, vec![]),
                ("alias".into(), EntryKind::LinkToDir, vec![]),
                ("dangling".into(), EntryKind::Link, vec![]),
                (
                    "linked".into(),
                    EntryKind::Dir,
                    vec![("compose.yaml".into(), EntryKind::Link)]
                ),
                (
                    "web".into(),
                    EntryKind::Dir,
                    vec![
                        (".env".into(), EntryKind::File),
                        ("compose.yaml".into(), EntryKind::File)
                    ]
                ),
            ]
        );
        assert_eq!(fs::read_dir(&apps).unwrap().count(), before);
    }

    #[test]
    fn reports_a_missing_or_odd_directory() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        fs::write(&file, "x").unwrap();
        assert_eq!(SystemFs.list(&root.path().join("nope")), Listing::Missing);
        assert_eq!(SystemFs.list(&file), Listing::NotADirectory);

        let linked = root.path().join("linked");
        symlink(root.path(), &linked).unwrap();
        assert!(matches!(SystemFs.list(&linked), Listing::Entries(_)));
    }

    #[test]
    fn notes_size_and_modification_time() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("web")).unwrap();
        fs::write(root.path().join("web/compose.yaml"), "services: {}\n").unwrap();
        let Listing::Entries(entries) = SystemFs.list(root.path()) else {
            panic!()
        };
        let info = entries[0].files["compose.yaml"];
        assert_eq!(info.len, 13);
        assert!(info.modified.is_some());
    }

    #[test]
    fn fake_answers_from_its_table() {
        let fake = FakeFs::new();
        assert_eq!(fake.list(Path::new("/srv")), Listing::Missing);
        fake.set("/srv", Listing::Entries(vec![]));
        assert_eq!(
            fake.clone().list(Path::new("/srv")),
            Listing::Entries(vec![])
        );
    }
}
