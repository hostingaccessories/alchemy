//! Writing app folders: the [`AppDisk`] seam.
//!
//! The only writes Alchemy makes in an app folder go through here, and
//! [`AppEditor`](super::AppEditor) decides which: a staging directory, the
//! files staged in it, and one rename into place. The apps task also
//! lists folders and removes staging directories a stopped daemon left
//! behind through it. Nothing here follows a symlink at the path it is
//! given.

use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use alchemy_core::stack::{EntryKind, EntryStat};
use rustix::fs::{CWD, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

/// A regular file read by [`AppDisk::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRead {
    /// Its bytes, at most the `max` asked for plus one.
    pub bytes: Vec<u8>,
    /// Its permission bits.
    pub mode: u32,
    /// Its owner.
    pub uid: u32,
    /// Its group.
    pub gid: u32,
}

/// What [`AppDisk::read`] found at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Nothing is there.
    Missing,
    /// A symlink is there, which is not followed.
    Linked,
    /// Something other than a regular file is there.
    NotRegular,
    /// A regular file.
    File(FileRead),
}

/// The filesystem operations an [`AppEditor`](super::AppEditor) needs.
///
/// Calls block, so callers run them off the async runtime.
pub trait AppDisk: Send + Sync + fmt::Debug {
    /// What is at `path`, without following a symlink there, or `None` if
    /// nothing is.
    fn kind(&self, path: &Path) -> io::Result<Option<EntryKind>>;

    /// Read the regular file at `path`, keeping at most `max + 1` bytes so
    /// the caller can tell a file that is too large.
    fn read(&self, path: &Path, max: usize) -> io::Result<ReadOutcome>;

    /// Create `dir` and any missing parents with the usual permissions.
    /// Directories that already exist are left as they are.
    fn create_dirs(&self, dir: &Path) -> io::Result<()>;

    /// Make a new directory in `root`, readable by its owner alone, whose
    /// name starts with `prefix`, and return its path.
    fn make_staging(&self, root: &Path, prefix: &str) -> io::Result<PathBuf>;

    /// Make the directory `dir`, whose parent exists, with exactly `mode`.
    fn make_dir(&self, dir: &Path, mode: u32) -> io::Result<()>;

    /// Create the file `path`, which must not exist, write `content` to it,
    /// flush it to disk, and set exactly `mode`, whatever the umask.
    fn write_new(&self, path: &Path, content: &[u8], mode: u32) -> io::Result<()>;

    /// The owner and group of `path`, without following a symlink.
    fn owner(&self, path: &Path) -> io::Result<(u32, u32)>;

    /// Give `path` this owner and group.
    fn set_owner(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()>;

    /// Rename `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`]
    /// if anything is at `to`.
    fn rename_new(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Rename `from` over `to` in one step, replacing whatever is there.
    fn rename_over(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Remove the staging directory `dir` and everything in it.
    fn remove_staging(&self, dir: &Path) -> io::Result<()>;

    /// Remove the directory `dir` if it is empty, and never anything in
    /// it. Anything but a real directory at `dir`, such as a symlink to
    /// one, fails with [`io::ErrorKind::NotADirectory`] and is left alone.
    fn remove_empty_dir(&self, dir: &Path) -> io::Result<()>;

    /// Every entry in `dir`, each looked at without following a symlink.
    /// `dir` itself may be a symlink to a directory, since the user chose
    /// it. An entry that vanishes while listing is left out.
    fn list_entries(&self, dir: &Path) -> io::Result<Vec<EntryStat>>;

    /// Remove the staging directory `dir`, left behind by an earlier run,
    /// and everything in it. Anything but a real directory at `dir`, such
    /// as a symlink to one, fails with [`io::ErrorKind::NotADirectory`]
    /// and is left alone.
    fn remove_left_staging(&self, dir: &Path) -> io::Result<()>;
}

/// [`AppDisk`] over the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDisk;

impl AppDisk for SystemDisk {
    fn kind(&self, path: &Path) -> io::Result<Option<EntryKind>> {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(kind_of(&meta))),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Opened with `O_NOFOLLOW`, and `O_NONBLOCK` so a FIFO put in the
    /// file's place cannot hang the open. Opening a socket fails with
    /// `ENXIO` (Linux) or `EOPNOTSUPP` (macOS), which reads as not a
    /// regular file.
    fn read(&self, path: &Path, max: usize) -> io::Result<ReadOutcome> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let fd = match rustix::fs::openat(CWD, path, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(ReadOutcome::Missing),
            Err(Errno::LOOP) => return Ok(ReadOutcome::Linked),
            Err(Errno::NXIO | Errno::OPNOTSUPP) => return Ok(ReadOutcome::NotRegular),
            Err(err) => return Err(err.into()),
        };
        let file = File::from(fd);
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Ok(ReadOutcome::NotRegular);
        }
        let mut bytes = Vec::new();
        let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
        file.take(limit).read_to_end(&mut bytes)?;
        Ok(ReadOutcome::File(FileRead {
            bytes,
            mode: meta.permissions().mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
        }))
    }

    fn create_dirs(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
    }

    fn make_staging(&self, root: &Path, prefix: &str) -> io::Result<PathBuf> {
        Ok(tempfile::Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(root)?
            .keep())
    }

    fn make_dir(&self, dir: &Path, mode: u32) -> io::Result<()> {
        fs::create_dir(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(mode))
    }

    fn write_new(&self, path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)?;
        file.write_all(content)?;
        file.sync_all()?;
        file.set_permissions(fs::Permissions::from_mode(mode))
    }

    fn owner(&self, path: &Path) -> io::Result<(u32, u32)> {
        let meta = fs::symlink_metadata(path)?;
        Ok((meta.uid(), meta.gid()))
    }

    fn set_owner(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))
    }

    /// `RENAME_NOREPLACE` on Linux and `RENAME_EXCL` on macOS, so nothing
    /// created at `to` after it was checked is replaced. On a filesystem
    /// without them it falls back to checking, then renaming.
    fn rename_new(&self, from: &Path, to: &Path) -> io::Result<()> {
        match rustix::fs::renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
            Ok(()) => {}
            Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {
                if self.kind(to)?.is_some() {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                fs::rename(from, to)?;
            }
            Err(err) => return Err(err.into()),
        }
        sync_parent(to);
        Ok(())
    }

    fn rename_over(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)?;
        sync_parent(to);
        Ok(())
    }

    fn remove_staging(&self, dir: &Path) -> io::Result<()> {
        fs::remove_dir_all(dir)
    }

    fn remove_empty_dir(&self, dir: &Path) -> io::Result<()> {
        if self.kind(dir)? != Some(EntryKind::Dir) {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        fs::remove_dir(dir)
    }

    fn list_entries(&self, dir: &Path) -> io::Result<Vec<EntryStat>> {
        let mut listed = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            listed.push(EntryStat {
                name: entry.file_name(),
                kind: kind_of(&meta),
                uid: meta.uid(),
                modified: meta.modified().ok(),
            });
        }
        Ok(listed)
    }

    /// The check before removing narrows, but cannot close, the window in
    /// which `dir` could be swapped for a symlink. That costs no more than
    /// the symlink: [`fs::remove_dir_all`] removes a symlink at `dir`
    /// itself and never follows one, there or anywhere below it.
    fn remove_left_staging(&self, dir: &Path) -> io::Result<()> {
        if self.kind(dir)? != Some(EntryKind::Dir) {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        fs::remove_dir_all(dir)
    }
}

/// What `meta`, from `lstat`, says the entry is.
fn kind_of(meta: &Metadata) -> EntryKind {
    let kind = meta.file_type();
    if kind.is_symlink() {
        EntryKind::Link
    } else if kind.is_dir() {
        EntryKind::Dir
    } else if kind.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// Flush the directory holding `path`, so a rename into it survives a
/// crash. Best effort: the rename itself has already happened.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent()
        && let Err(error) = File::open(parent).and_then(|dir| dir.sync_all())
    {
        tracing::debug!(%error, dir = %parent.display(), "flushing the folder failed");
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

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn reads_regular_files_and_never_follows_links() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("compose.yaml");
        fs::write(&file, "services: {}\n").unwrap();
        symlink(&file, root.path().join("linked")).unwrap();
        symlink(root.path().join("nope"), root.path().join("dangling")).unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(root.path().join("sock")).unwrap();

        let ReadOutcome::File(read) = SystemDisk.read(&file, 1024).unwrap() else {
            panic!()
        };
        assert_eq!(read.bytes, b"services: {}\n");
        let cases = [
            ("linked", ReadOutcome::Linked),
            ("dangling", ReadOutcome::Linked),
            ("missing", ReadOutcome::Missing),
            ("dir", ReadOutcome::NotRegular),
            ("sock", ReadOutcome::NotRegular),
        ];
        for (name, want) in cases {
            assert_eq!(
                SystemDisk.read(&root.path().join(name), 1024).unwrap(),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn reads_one_byte_past_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("big");
        fs::write(&file, vec![b'a'; 100]).unwrap();
        let ReadOutcome::File(read) = SystemDisk.read(&file, 10).unwrap() else {
            panic!()
        };
        assert_eq!(read.bytes.len(), 11);
    }

    #[test]
    fn reports_kinds_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("file"), "").unwrap();
        symlink(root.path().join("dir"), root.path().join("link")).unwrap();
        let cases = [
            ("dir", Some(EntryKind::Dir)),
            ("file", Some(EntryKind::File)),
            ("link", Some(EntryKind::Link)),
            ("missing", None),
        ];
        for (name, want) in cases {
            assert_eq!(
                SystemDisk.kind(&root.path().join(name)).unwrap(),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn writes_new_files_with_exact_modes() {
        let root = tempfile::tempdir().unwrap();
        let env = root.path().join(".env");
        SystemDisk.write_new(&env, b"A=1\n", 0o600).unwrap();
        assert_eq!(mode(&env), 0o600);
        let compose = root.path().join("compose.yaml");
        SystemDisk.write_new(&compose, b"x", 0o644).unwrap();
        assert_eq!(mode(&compose), 0o644);
        assert_eq!(fs::read(&env).unwrap(), b"A=1\n");

        let err = SystemDisk.write_new(&env, b"B=2\n", 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&env).unwrap(), b"A=1\n");

        let target = root.path().join("target");
        fs::write(&target, "keep").unwrap();
        symlink(&target, root.path().join("link")).unwrap();
        assert!(
            SystemDisk
                .write_new(&root.path().join("link"), b"x", 0o644)
                .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn stages_privately_and_makes_dirs_with_exact_modes() {
        let root = tempfile::tempdir().unwrap();
        let staging = SystemDisk
            .make_staging(root.path(), ".alchemy-tmp-blog-")
            .unwrap();
        assert!(
            staging
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".alchemy-tmp-blog-")
        );
        assert_eq!(staging.parent(), Some(root.path()));
        assert_eq!(mode(&staging), 0o700);
        SystemDisk.make_dir(&staging.join("blog"), 0o755).unwrap();
        assert_eq!(mode(&staging.join("blog")), 0o755);
        SystemDisk.remove_staging(&staging).unwrap();
        assert!(!staging.exists());
    }

    #[test]
    fn removes_only_empty_real_dirs() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty");
        let full = root.path().join("full");
        fs::create_dir(&empty).unwrap();
        fs::create_dir(&full).unwrap();
        fs::write(full.join("data"), "keep").unwrap();
        fs::write(root.path().join("file"), "keep").unwrap();
        symlink(&empty, root.path().join("link")).unwrap();
        let cases = [
            ("full", Some(io::ErrorKind::DirectoryNotEmpty)),
            ("file", Some(io::ErrorKind::NotADirectory)),
            ("link", Some(io::ErrorKind::NotADirectory)),
            ("missing", Some(io::ErrorKind::NotADirectory)),
            ("empty", None),
        ];
        for (name, want) in cases {
            let got = SystemDisk.remove_empty_dir(&root.path().join(name));
            assert_eq!(got.err().map(|e| e.kind()), want, "{name}");
        }
        assert!(!empty.exists());
        assert!(full.join("data").exists());
        assert!(root.path().join("file").exists());
        assert!(fs::symlink_metadata(root.path().join("link")).is_ok());
    }

    #[test]
    fn lists_entries_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("file"), "").unwrap();
        symlink(root.path().join("dir"), root.path().join("link")).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        File::open(root.path().join("dir"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let (uid, _) = SystemDisk.owner(root.path()).unwrap();

        let mut listed = SystemDisk.list_entries(root.path()).unwrap();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        let kinds: Vec<_> = listed
            .iter()
            .map(|e| (e.name.to_str().unwrap(), e.kind, e.uid))
            .collect();
        assert_eq!(
            kinds,
            [
                ("dir", EntryKind::Dir, uid),
                ("file", EntryKind::File, uid),
                ("link", EntryKind::Link, uid),
            ]
        );
        assert_eq!(listed[0].modified, Some(old));
        let err = SystemDisk
            .list_entries(&root.path().join("missing"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn removes_left_staging_only_when_a_real_dir() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("data"), "keep").unwrap();
        let staging = root.path().join(".alchemy-tmp-blog-a1B2c3");
        fs::create_dir_all(staging.join("blog")).unwrap();
        fs::write(staging.join("blog/compose.yaml"), "x").unwrap();
        symlink(&outside, staging.join("blog/linked")).unwrap();
        let linked = root.path().join(".alchemy-tmp-blog-d4E5f6");
        symlink(&outside, &linked).unwrap();
        let file = root.path().join(".alchemy-tmp-blog-g7H8i9");
        fs::write(&file, "keep").unwrap();

        for path in [&linked, &file, &root.path().join("missing")] {
            let err = SystemDisk.remove_left_staging(path).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{path:?}");
        }
        SystemDisk.remove_left_staging(&staging).unwrap();
        assert!(!staging.exists());
        assert!(fs::symlink_metadata(&linked).is_ok());
        assert!(file.exists());
        assert_eq!(fs::read(outside.join("data")).unwrap(), b"keep");
    }

    #[test]
    fn rename_new_never_replaces() {
        let root = tempfile::tempdir().unwrap();
        let from = root.path().join("from");
        fs::create_dir(&from).unwrap();
        fs::write(from.join("compose.yaml"), "x").unwrap();
        for existing in ["empty", "full", "file"] {
            let to = root.path().join(existing);
            match existing {
                "empty" => fs::create_dir(&to).unwrap(),
                "full" => {
                    fs::create_dir(&to).unwrap();
                    fs::write(to.join("data"), "keep").unwrap();
                }
                _ => fs::write(&to, "keep").unwrap(),
            }
            let err = SystemDisk.rename_new(&from, &to).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{existing}");
        }
        assert!(root.path().join("full/data").exists());
        let to = root.path().join("new");
        SystemDisk.rename_new(&from, &to).unwrap();
        assert_eq!(fs::read(to.join("compose.yaml")).unwrap(), b"x");
        assert!(!from.exists());
    }

    #[test]
    fn rename_over_replaces_a_file_in_one_step() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("compose.yaml");
        let staged = root.path().join("staged");
        fs::write(&live, "old").unwrap();
        fs::write(&staged, "new").unwrap();
        SystemDisk.rename_over(&staged, &live).unwrap();
        assert_eq!(fs::read(&live).unwrap(), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn keeps_an_owner_it_already_has() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("f");
        fs::write(&file, "").unwrap();
        let (uid, gid) = SystemDisk.owner(&file).unwrap();
        SystemDisk.set_owner(&file, uid, gid).unwrap();
        assert_eq!(SystemDisk.owner(&file).unwrap(), (uid, gid));
    }
}
