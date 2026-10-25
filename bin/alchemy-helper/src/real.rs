//! [`RealSystem`]: the [`System`] backed by the OS.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustix::fs::{FileType, Mode, OFlags, StatVfsMountFlags};
use rustix::io::Errno;

use crate::system::{CommandOutput, FileInfo, FixedCommand, MAX_READ, OpenError, System};

/// The only `PATH` a [`FixedCommand`] sees. Its program is absolute, so
/// this matters only for anything that program runs in turn.
pub const COMMAND_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The [`System`] that makes real system calls.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealSystem;

impl System for RealSystem {
    type File = OwnedFd;

    fn current_exe(&self) -> io::Result<PathBuf> {
        std::fs::canonicalize(std::env::current_exe()?)
    }

    fn open_nofollow(&self, path: &Path) -> Result<OwnedFd, OpenError> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        rustix::fs::open(path, flags, Mode::empty()).map_err(|e| match e {
            Errno::NOENT => OpenError::NotFound,
            Errno::LOOP => OpenError::Symlink,
            e => OpenError::Io(e.into()),
        })
    }

    fn inspect(&self, file: &OwnedFd) -> io::Result<FileInfo> {
        let stat = rustix::fs::fstat(file)?;
        let statvfs = rustix::fs::fstatvfs(file)?;
        Ok(FileInfo {
            regular: FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
            uid: stat.st_uid,
            mode: permission_bits(stat.st_mode),
            nosuid: statvfs.f_flag.contains(StatVfsMountFlags::NOSUID),
        })
    }

    fn xattr(&self, file: &OwnedFd, name: &str) -> io::Result<Option<Vec<u8>>> {
        let len = match rustix::fs::fgetxattr(file, name, &mut [0u8; 0][..]) {
            Ok(len) => len,
            Err(e) if is_no_attr(e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut value = vec![0u8; len];
        let read = rustix::fs::fgetxattr(file, name, &mut value[..])?;
        value.truncate(read);
        Ok(Some(value))
    }

    fn set_xattr(&self, file: &OwnedFd, name: &str, value: &[u8]) -> io::Result<()> {
        rustix::fs::fsetxattr(file, name, value, rustix::fs::XattrFlags::empty())?;
        Ok(())
    }

    fn remove_xattr(&self, file: &OwnedFd, name: &str) -> io::Result<bool> {
        match rustix::fs::fremovexattr(file, name) {
            Ok(()) => Ok(true),
            Err(e) if is_no_attr(e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn read_file(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        let fd = match self.open_nofollow(path) {
            Ok(fd) => fd,
            Err(OpenError::NotFound) => return Ok(None),
            Err(OpenError::Symlink) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "is a symlink"));
            }
            Err(OpenError::Io(e)) => return Err(e),
        };
        if !self.inspect(&fd)?.regular {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "is not a regular file",
            ));
        }
        let mut contents = Vec::new();
        File::from(fd)
            .take(MAX_READ + 1)
            .read_to_end(&mut contents)?;
        if contents.len() as u64 > MAX_READ {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "is too large"));
        }
        Ok(Some(contents))
    }

    fn write_file(&self, path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
        let (dir, temp) = temp_sibling(path)?;
        let result = write_new(&temp, contents, mode)
            .and_then(|()| std::fs::rename(&temp, path))
            .and_then(|()| File::open(dir)?.sync_all());
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn remove_file(&self, path: &Path) -> io::Result<bool> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn run(&self, command: &FixedCommand) -> io::Result<CommandOutput> {
        if !Path::new(command.program()).is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the program is not an absolute path",
            ));
        }
        let output = Command::new(command.program())
            .args(command.args())
            .env_clear()
            .env("PATH", COMMAND_PATH)
            .current_dir("/")
            .stdin(Stdio::null())
            .output()?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// The permission bits of `st_mode`, which is a `u32` on Linux and a
/// `u16` on macOS.
#[allow(clippy::useless_conversion)]
fn permission_bits(st_mode: rustix::fs::RawMode) -> u32 {
    u32::from(st_mode) & 0o7777
}

/// `ENODATA` on Linux and `ENOATTR` on macOS: the attribute is absent.
fn is_no_attr(e: Errno) -> bool {
    #[cfg(target_os = "linux")]
    let absent = Errno::NODATA;
    #[cfg(target_os = "macos")]
    let absent = Errno::NOATTR;
    e == absent
}

/// The directory holding `path`, and a hidden name next to it for a new
/// file that will be renamed over it.
fn temp_sibling(path: &Path) -> io::Result<(&Path, PathBuf)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "not a file path");
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(invalid)?;
    let name = path.file_name().ok_or_else(invalid)?;
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(format!(".alchemy-helper.{}", std::process::id()));
    Ok((dir, dir.join(temp)))
}

/// Create `path`, which must not exist, as `root:root` with `mode`, write
/// `contents`, and sync it. It starts `0o600` and is chowned before the
/// final mode, which a chown could otherwise clear setuid bits from.
fn write_new(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    std::os::unix::fs::fchown(&file, Some(0), Some(0))?;
    file.set_permissions(Permissions::from_mode(mode))?;
    file.write_all(contents)?;
    file.sync_all()
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

    fn is_root() -> bool {
        rustix::process::geteuid().is_root()
    }

    #[test]
    fn open_nofollow_classifies_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("missing"), &dangling).unwrap();

        let sys = RealSystem;
        assert!(matches!(
            sys.open_nofollow(&tmp.path().join("missing")),
            Err(OpenError::NotFound)
        ));
        assert!(matches!(sys.open_nofollow(&link), Err(OpenError::Symlink)));
        assert!(matches!(
            sys.open_nofollow(&dangling),
            Err(OpenError::Symlink)
        ));

        let fd = sys.open_nofollow(&file).unwrap();
        assert!(sys.inspect(&fd).unwrap().regular);
        let dir = sys.open_nofollow(tmp.path()).unwrap();
        assert!(!sys.inspect(&dir).unwrap().regular);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_nofollow_does_not_block_on_a_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            FileType::Fifo,
            Mode::from_bits_truncate(0o600),
            0,
        )
        .unwrap();
        let sys = RealSystem;
        let fd = sys.open_nofollow(&fifo).unwrap();
        assert!(!sys.inspect(&fd).unwrap().regular);
    }

    #[test]
    fn xattr_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let sys = RealSystem;
        let fd = sys.open_nofollow(&file).unwrap();
        let name = "user.alchemy-helper-test";
        match sys.set_xattr(&fd, name, b"value") {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(Errno::NOTSUP.raw_os_error()) => return,
            Err(e) => panic!("set_xattr: {e}"),
        }
        assert_eq!(
            sys.xattr(&fd, name).unwrap().as_deref(),
            Some(&b"value"[..])
        );
        sys.set_xattr(&fd, name, b"").unwrap();
        assert_eq!(sys.xattr(&fd, name).unwrap().as_deref(), Some(&b""[..]));
        assert!(sys.remove_xattr(&fd, name).unwrap());
        assert!(!sys.remove_xattr(&fd, name).unwrap());
        assert_eq!(sys.xattr(&fd, name).unwrap(), None);
    }

    #[test]
    fn temp_sibling_is_hidden_in_the_same_directory() {
        let (dir, temp) = temp_sibling(Path::new("/etc/systemd/system/alchemyd.service")).unwrap();
        assert_eq!(dir, Path::new("/etc/systemd/system"));
        assert_eq!(temp.parent(), Some(dir));
        let name = temp.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(".alchemyd.service.alchemy-helper."),
            "{name}"
        );
        for bad in ["/", "file", ""] {
            assert!(temp_sibling(Path::new(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn write_file_replaces_atomically_as_root() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("unit");
        let sys = RealSystem;
        if !is_root() {
            assert!(sys.write_file(&path, b"new", 0o644).is_err());
            assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
            return;
        }
        std::fs::write(&path, b"old").unwrap();
        sys.write_file(&path, b"new", 0o644).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
        assert_eq!(std::os::unix::fs::MetadataExt::uid(&meta), 0);
        assert_eq!(std::os::unix::fs::MetadataExt::gid(&meta), 0);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn read_file_reports_a_missing_file_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        let sys = RealSystem;
        assert_eq!(sys.read_file(&path).unwrap(), None);
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(sys.read_file(&path).unwrap().as_deref(), Some(&b"x"[..]));
        assert!(sys.read_file(tmp.path()).is_err());
    }

    #[test]
    fn read_file_reads_only_small_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"contents").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let limit = usize::try_from(MAX_READ).unwrap();
        let big = tmp.path().join("big");
        std::fs::write(&big, vec![b'x'; limit + 1]).unwrap();
        let fits = tmp.path().join("fits");
        std::fs::write(&fits, vec![b'x'; limit]).unwrap();

        let sys = RealSystem;
        assert_eq!(
            sys.read_file(&file).unwrap().as_deref(),
            Some(&b"contents"[..])
        );
        assert_eq!(sys.read_file(&fits).unwrap().map(|c| c.len()), Some(limit));
        for bad in [&link, &big] {
            assert!(sys.read_file(bad).is_err(), "{}", bad.display());
        }
    }

    #[test]
    fn inspect_reports_owner_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, Permissions::from_mode(0o640)).unwrap();
        let sys = RealSystem;
        let info = sys.inspect(&sys.open_nofollow(&file).unwrap()).unwrap();
        assert_eq!(info.mode, 0o640);
        assert_eq!(info.uid, rustix::process::geteuid().as_raw());
    }

    #[test]
    fn remove_file_reports_whether_it_existed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let sys = RealSystem;
        assert!(sys.remove_file(&path).unwrap());
        assert!(!sys.remove_file(&path).unwrap());
    }

    #[test]
    fn run_uses_a_fixed_environment_and_no_shell() {
        let sys = RealSystem;
        let env = FixedCommand::new("/usr/bin/env", vec![]).unwrap();
        let out = sys.run(&env).unwrap();
        assert!(out.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("PATH={COMMAND_PATH}\n")
        );

        let literal = FixedCommand::new("/usr/bin/env", vec!["$HOME;".into()]).unwrap();
        let out = sys.run(&literal).unwrap();
        assert!(!out.success());
        assert!(String::from_utf8(out.stderr).unwrap().contains("$HOME;"));
    }
}
