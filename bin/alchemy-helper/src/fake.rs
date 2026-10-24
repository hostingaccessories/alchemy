//! An in-memory [`System`] for unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::system::{CommandOutput, FileInfo, FixedCommand, OpenError, System};

/// A [`System`] method, to make it fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    CurrentExe,
    Open,
    Inspect,
    Xattr,
    SetXattr,
    RemoveXattr,
    WriteFile,
    RemoveFile,
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Regular,
    Symlink,
    Other,
}

/// A file in a [`FakeSystem`].
#[derive(Debug, Clone)]
pub struct FakeFile {
    kind: Kind,
    nosuid: bool,
    xattrs: HashMap<String, Vec<u8>>,
    pub contents: Vec<u8>,
    pub mode: u32,
}

impl FakeFile {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            nosuid: false,
            xattrs: HashMap::new(),
            contents: Vec::new(),
            mode: 0o755,
        }
    }

    pub fn regular() -> Self {
        Self::new(Kind::Regular)
    }

    pub fn symlink() -> Self {
        Self::new(Kind::Symlink)
    }

    /// A directory, device, FIFO, or socket.
    pub fn other() -> Self {
        Self::new(Kind::Other)
    }

    pub fn nosuid(mut self) -> Self {
        self.nosuid = true;
        self
    }

    pub fn with_xattr(mut self, name: &str, value: &[u8]) -> Self {
        self.xattrs.insert(name.to_owned(), value.to_vec());
        self
    }
}

/// An in-memory filesystem and command log.
#[derive(Debug)]
pub struct FakeSystem {
    exe: PathBuf,
    files: RefCell<HashMap<PathBuf, FakeFile>>,
    fail: Option<Call>,
    opened: RefCell<Vec<PathBuf>>,
    commands: RefCell<Vec<FixedCommand>>,
}

impl FakeSystem {
    /// A system whose helper executable is `exe`.
    pub fn new(exe: &str) -> Self {
        Self {
            exe: PathBuf::from(exe),
            files: RefCell::default(),
            fail: None,
            opened: RefCell::default(),
            commands: RefCell::default(),
        }
    }

    pub fn with_file(self, path: &str, file: FakeFile) -> Self {
        self.files.borrow_mut().insert(PathBuf::from(path), file);
        self
    }

    pub fn failing(mut self, call: Call) -> Self {
        self.fail = Some(call);
        self
    }

    pub fn xattr_of(&self, path: &str, name: &str) -> Option<Vec<u8>> {
        self.files
            .borrow()
            .get(Path::new(path))
            .and_then(|f| f.xattrs.get(name).cloned())
    }

    pub fn file(&self, path: &str) -> Option<FakeFile> {
        self.files.borrow().get(Path::new(path)).cloned()
    }

    pub fn opened(&self) -> Vec<PathBuf> {
        self.opened.borrow().clone()
    }

    pub fn commands(&self) -> Vec<FixedCommand> {
        self.commands.borrow().clone()
    }

    fn check(&self, call: Call) -> io::Result<()> {
        if self.fail == Some(call) {
            Err(io::Error::other(format!("{call:?} failed")))
        } else {
            Ok(())
        }
    }

    fn with_open<T>(&self, path: &Path, f: impl FnOnce(&mut FakeFile) -> T) -> io::Result<T> {
        let mut files = self.files.borrow_mut();
        let file = files
            .get_mut(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(f(file))
    }
}

impl System for FakeSystem {
    type File = PathBuf;

    fn current_exe(&self) -> io::Result<PathBuf> {
        self.check(Call::CurrentExe)?;
        Ok(self.exe.clone())
    }

    fn open_nofollow(&self, path: &Path) -> Result<PathBuf, OpenError> {
        self.check(Call::Open)?;
        self.opened.borrow_mut().push(path.to_path_buf());
        match self.files.borrow().get(path).map(|f| f.kind) {
            None => Err(OpenError::NotFound),
            Some(Kind::Symlink) => Err(OpenError::Symlink),
            Some(_) => Ok(path.to_path_buf()),
        }
    }

    fn inspect(&self, file: &PathBuf) -> io::Result<FileInfo> {
        self.check(Call::Inspect)?;
        self.with_open(file, |f| FileInfo {
            regular: f.kind == Kind::Regular,
            nosuid: f.nosuid,
        })
    }

    fn xattr(&self, file: &PathBuf, name: &str) -> io::Result<Option<Vec<u8>>> {
        self.check(Call::Xattr)?;
        self.with_open(file, |f| f.xattrs.get(name).cloned())
    }

    fn set_xattr(&self, file: &PathBuf, name: &str, value: &[u8]) -> io::Result<()> {
        self.check(Call::SetXattr)?;
        self.with_open(file, |f| {
            f.xattrs.insert(name.to_owned(), value.to_vec());
        })
    }

    fn remove_xattr(&self, file: &PathBuf, name: &str) -> io::Result<bool> {
        self.check(Call::RemoveXattr)?;
        self.with_open(file, |f| f.xattrs.remove(name).is_some())
    }

    fn write_file(&self, path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
        self.check(Call::WriteFile)?;
        let file = FakeFile {
            contents: contents.to_vec(),
            mode,
            ..FakeFile::regular()
        };
        self.files.borrow_mut().insert(path.to_path_buf(), file);
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<bool> {
        self.check(Call::RemoveFile)?;
        Ok(self.files.borrow_mut().remove(path).is_some())
    }

    fn run(&self, command: &FixedCommand) -> io::Result<CommandOutput> {
        self.check(Call::Run)?;
        self.commands.borrow_mut().push(command.clone());
        Ok(CommandOutput {
            code: Some(0),
            ..CommandOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_and_commands_are_recorded() {
        let sys = FakeSystem::new("/opt/alchemy/bin/alchemy-helper");
        sys.write_file(Path::new("/etc/unit"), b"x", 0o644).unwrap();
        let file = sys.file("/etc/unit").unwrap();
        assert_eq!((file.contents.as_slice(), file.mode), (&b"x"[..], 0o644));
        assert!(sys.remove_file(Path::new("/etc/unit")).unwrap());
        assert!(!sys.remove_file(Path::new("/etc/unit")).unwrap());

        let command = FixedCommand::new("/usr/sbin/ufw", vec!["status".into()]).unwrap();
        assert!(sys.run(&command).unwrap().success());
        assert_eq!(sys.commands(), [command]);
    }

    #[test]
    fn failing_calls_fail() {
        let path = Path::new("/etc/unit");
        let command = FixedCommand::new("/bin/true", vec![]).unwrap();
        let sys = FakeSystem::new("/x").failing(Call::WriteFile);
        assert!(sys.write_file(path, b"", 0o644).is_err());
        let sys = FakeSystem::new("/x").failing(Call::RemoveFile);
        assert!(sys.remove_file(path).is_err());
        let sys = FakeSystem::new("/x").failing(Call::Run);
        assert!(sys.run(&command).is_err());
        assert_eq!(sys.commands().len(), 0);
    }
}
