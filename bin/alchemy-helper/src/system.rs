//! The [`System`] trait: every side effect the helper performs.
//!
//! Operations are pure functions over a `System`, so tests drive them
//! with a fake and only [`RealSystem`](crate::RealSystem) touches the OS.
//! The trait offers no way to run a shell or to follow a symlink into a
//! file it changes.

use std::io;
use std::path::{Path, PathBuf};

/// The side effects an operation may perform.
///
/// Files that get an extended attribute are opened once, without
/// following a final symlink, and every later check and change goes
/// through that handle, so they all hit the same inode.
pub trait System {
    /// An open file, from [`open_nofollow`](Self::open_nofollow).
    type File;

    /// The helper's own executable, with every symlink resolved.
    fn current_exe(&self) -> io::Result<PathBuf>;

    /// Open `path` read-only without following a final symlink, and
    /// without blocking on a FIFO.
    fn open_nofollow(&self, path: &Path) -> Result<Self::File, OpenError>;

    /// What `file` is and how its filesystem is mounted.
    fn inspect(&self, file: &Self::File) -> io::Result<FileInfo>;

    /// The extended attribute `name` on `file`, or `None` if it has none.
    fn xattr(&self, file: &Self::File, name: &str) -> io::Result<Option<Vec<u8>>>;

    /// Create or replace the extended attribute `name` on `file`.
    fn set_xattr(&self, file: &Self::File, name: &str, value: &[u8]) -> io::Result<()>;

    /// Remove the extended attribute `name` from `file`. Returns whether
    /// it was there.
    fn remove_xattr(&self, file: &Self::File, name: &str) -> io::Result<bool>;

    /// The contents of the regular file at `path`, or `None` if nothing
    /// is there. The final component is not followed, so a symlink is an
    /// error, as is anything but a regular file or one larger than
    /// [`MAX_READ`].
    fn read_file(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;

    /// Atomically replace `path` with a `root:root` file holding
    /// `contents` with permission bits `mode`.
    ///
    /// The contents go to a new file in the same directory, are synced,
    /// and are renamed over `path`, so readers see the old file or the
    /// new one and never a partial write.
    fn write_file(&self, path: &Path, contents: &[u8], mode: u32) -> io::Result<()>;

    /// Remove `path`. Returns whether it existed. A symlink is removed
    /// itself, never its target.
    fn remove_file(&self, path: &Path) -> io::Result<bool>;

    /// Run `command` and wait for it, capturing its output.
    fn run(&self, command: &FixedCommand) -> io::Result<CommandOutput>;
}

/// Why [`System::open_nofollow`] failed.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Nothing exists at the path.
    #[error("no such file")]
    NotFound,
    /// The path is a symlink.
    #[error("is a symlink")]
    Symlink,
    /// Any other failure.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The largest file [`System::read_file`] reads: the helper only reads
/// back small configuration files it wrote itself.
pub const MAX_READ: u64 = 64 * 1024;

/// What [`System::inspect`] reports about an open file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    /// The file is a regular file, not a directory, device, FIFO, or socket.
    pub regular: bool,
    /// The owner's UID.
    pub uid: u32,
    /// The permission bits, such as `0o755`.
    pub mode: u32,
    /// The file's filesystem is mounted `nosuid`, so the kernel ignores
    /// its setuid bit and file capabilities on exec.
    pub nosuid: bool,
}

/// A program at a fixed absolute path with its arguments, run directly
/// and never through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedCommand {
    program: &'static str,
    args: Vec<String>,
}

impl FixedCommand {
    /// A command running `program`, which must be an absolute path, with
    /// `args`. Returns `None` for a relative `program`.
    pub fn new(program: &'static str, args: Vec<String>) -> Option<Self> {
        Path::new(program)
            .is_absolute()
            .then_some(Self { program, args })
    }

    /// The absolute path of the program.
    pub fn program(&self) -> &'static str {
        self.program
    }

    /// The arguments, not including the program.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// How a [`FixedCommand`] ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// The exit code, or `None` if a signal ended it.
    pub code: Option<i32>,
    /// Everything it wrote to stdout.
    pub stdout: Vec<u8>,
    /// Everything it wrote to stderr.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// Whether the command exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
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
    fn fixed_command_requires_an_absolute_program() {
        let cases = [
            ("/usr/sbin/ufw", true),
            ("/bin/true", true),
            ("ufw", false),
            ("./ufw", false),
            ("", false),
        ];
        for (program, ok) in cases {
            assert_eq!(
                FixedCommand::new(program, vec![]).is_some(),
                ok,
                "{program}"
            );
        }
    }

    #[test]
    fn command_output_success_is_exit_zero() {
        let cases = [(Some(0), true), (Some(1), false), (None, false)];
        for (code, ok) in cases {
            let out = CommandOutput {
                code,
                ..CommandOutput::default()
            };
            assert_eq!(out.success(), ok, "{code:?}");
        }
    }
}
