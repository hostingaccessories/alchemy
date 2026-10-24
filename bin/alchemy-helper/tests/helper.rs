//! End-to-end tests of the `alchemy-helper` binary: exit codes, the result
//! line, and, when run as root on Linux, a real grant and revoke on a
//! copy of the helper with a stand-in alchemyd next to it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::{Command, Output};

const HELPER: &str = env!("CARGO_BIN_EXE_alchemy-helper");

fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Run `exe`. A freshly copied executable can briefly fail with
/// `ETXTBSY` while a child forked by a concurrent test still holds the
/// copy's write descriptor, so that is retried.
fn helper(exe: &Path, args: &[&str]) -> (i32, String, String) {
    let mut attempts = 0;
    let Output {
        status,
        stdout,
        stderr,
    } = loop {
        match Command::new(exe).args(args).output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 100 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => break result.unwrap(),
        }
    };
    (
        status.code().unwrap(),
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

fn ok_line(operation: &str, changed: bool) -> String {
    format!("{{\"result\":\"ok\",\"operation\":\"{operation}\",\"changed\":{changed}}}\n")
}

fn error_line(operation: &str, error: &str) -> String {
    format!("{{\"result\":\"error\",\"operation\":\"{operation}\",\"error\":\"{error}\"}}\n")
}

#[test]
fn invalid_arguments_are_usage_errors() {
    let usage = "{\"result\":\"error\",\"operation\":null,\"error\":\"usage\"}\n";
    for args in [
        &[][..],
        &["bogus"],
        &["grant-low-ports", "/bin/sh"],
        &["check", "--force"],
        &["help"],
        &["install-service"],
        &["install-service", "--uid", "0"],
        &["install-service", "--uid", "alice"],
        &["remove-service", "--uid", "-1"],
    ] {
        let (code, stdout, stderr) = helper(Path::new(HELPER), args);
        assert_eq!(code, 64, "{args:?}");
        assert_eq!(stdout, usage, "{args:?}");
        assert!(
            stderr.contains("alchemy-helper") || stderr.contains("--uid"),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn help_and_version_print_to_stdout() {
    let (code, stdout, _) = helper(Path::new(HELPER), &["--version"]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        format!("alchemy-helper {}\n", env!("CARGO_PKG_VERSION"))
    );
    let (code, stdout, _) = helper(Path::new(HELPER), &["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("grant-low-ports"), "{stdout}");
}

#[test]
fn every_operation_needs_root() {
    if is_root() {
        let (code, stdout, stderr) = helper(Path::new(HELPER), &["check"]);
        assert_eq!(
            (code, stdout, stderr),
            (0, ok_line("check", false), String::new())
        );
        return;
    }
    for args in [
        &["check"][..],
        &["grant-low-ports"],
        &["revoke-low-ports"],
        &["install-service", "--uid", "1000"],
        &["remove-service", "--uid", "1000"],
    ] {
        let operation = args[0];
        let (code, stdout, stderr) = helper(Path::new(HELPER), args);
        assert_eq!(code, 77, "{operation}");
        assert_eq!(stdout, error_line(operation, "not_root"));
        assert_eq!(stderr, "alchemy-helper: must be run as root\n");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn low_ports_are_unsupported_on_macos() {
    if !is_root() {
        return;
    }
    for operation in ["grant-low-ports", "revoke-low-ports"] {
        let (code, stdout, _) = helper(Path::new(HELPER), &[operation]);
        assert_eq!(code, 69, "{operation}");
        assert_eq!(stdout, error_line(operation, "unsupported"));
    }
}

#[cfg(target_os = "linux")]
mod linux_root {
    use std::path::PathBuf;

    use alchemy_helper::{CAPABILITY_XATTR, NET_BIND_SERVICE_EP};
    use rustix::fs::StatVfsMountFlags;

    use super::*;

    /// A temporary directory holding a copy of the helper, so the real
    /// grant lands on a stand-in alchemyd and never the build's own.
    fn install() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let helper = dir.path().join("alchemy-helper");
        std::fs::copy(HELPER, &helper).unwrap();
        let daemon = dir.path().join("alchemyd");
        (dir, helper, daemon)
    }

    fn capability(path: &Path) -> Option<Vec<u8>> {
        let mut buf = [0u8; 64];
        match rustix::fs::lgetxattr(path, CAPABILITY_XATTR, &mut buf[..]) {
            Ok(len) => Some(buf[..len].to_vec()),
            Err(rustix::io::Errno::NODATA) => None,
            Err(e) => panic!("getxattr {}: {e}", path.display()),
        }
    }

    fn nosuid(path: &Path) -> bool {
        rustix::fs::statvfs(path)
            .unwrap()
            .f_flag
            .contains(StatVfsMountFlags::NOSUID)
    }

    #[test]
    fn grant_and_revoke_the_sibling_daemon() {
        if !is_root() {
            return;
        }
        let (dir, helper, daemon) = install();
        std::fs::write(&daemon, b"stand-in").unwrap();
        if nosuid(dir.path()) {
            let (code, stdout, _) = helper_run(&helper, "grant-low-ports");
            assert_eq!(
                (code, stdout),
                (69, error_line("grant-low-ports", "unsupported"))
            );
            return;
        }

        let steps = [
            ("grant-low-ports", true, Some(NET_BIND_SERVICE_EP.to_vec())),
            ("grant-low-ports", false, Some(NET_BIND_SERVICE_EP.to_vec())),
            ("revoke-low-ports", true, None),
            ("revoke-low-ports", false, None),
        ];
        for (operation, changed, want) in steps {
            let (code, stdout, stderr) = helper_run(&helper, operation);
            assert_eq!(code, 0, "{operation}: {stderr}");
            assert_eq!(stdout, ok_line(operation, changed));
            assert_eq!(capability(&daemon), want, "{operation}");
        }
    }

    #[test]
    fn refuses_anything_but_a_regular_sibling() {
        if !is_root() {
            return;
        }
        let (dir, helper, daemon) = install();
        let target = dir.path().join("target");
        std::fs::write(&target, b"other").unwrap();

        for operation in ["grant-low-ports", "revoke-low-ports"] {
            let (code, stdout, _) = helper_run(&helper, operation);
            assert_eq!((code, stdout), (66, error_line(operation, "not_found")));
        }

        std::os::unix::fs::symlink(&target, &daemon).unwrap();
        for operation in ["grant-low-ports", "revoke-low-ports"] {
            let (code, stdout, stderr) = helper_run(&helper, operation);
            assert_eq!((code, stdout), (65, error_line(operation, "refused")));
            assert!(stderr.contains("symlink"), "{stderr}");
        }
        assert_eq!(capability(&target), None);

        std::fs::remove_file(&daemon).unwrap();
        std::fs::create_dir(&daemon).unwrap();
        let (code, stdout, _) = helper_run(&helper, "grant-low-ports");
        assert_eq!(
            (code, stdout),
            (65, error_line("grant-low-ports", "refused"))
        );
    }

    #[test]
    fn follows_a_symlinked_helper_to_its_real_directory() {
        if !is_root() {
            return;
        }
        let (dir, helper, daemon) = install();
        std::fs::write(&daemon, b"stand-in").unwrap();
        if nosuid(dir.path()) {
            return;
        }
        let elsewhere = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let decoy = elsewhere.path().join("alchemyd");
        std::fs::write(&decoy, b"decoy").unwrap();
        let link = elsewhere.path().join("alchemy-helper");
        std::os::unix::fs::symlink(&helper, &link).unwrap();

        let (code, _, stderr) = helper_run(&link, "grant-low-ports");
        assert_eq!(code, 0, "{stderr}");
        assert_eq!(capability(&daemon), Some(NET_BIND_SERVICE_EP.to_vec()));
        assert_eq!(capability(&decoy), None);
    }

    fn helper_run(helper: &Path, operation: &str) -> (i32, String, String) {
        super::helper(helper, &[operation])
    }
}
