//! End-to-end: `alchemy completions` and `alchemy generate-man` through the
//! real binary, with the man pages rendered by the host's man tools.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

use alchemy::DAEMON_ENV;

/// Commands that exist but must never be advertised.
const HIDDEN: [&str; 2] = ["generate-man", "safety-probe"];

/// Run the binary with a remote `ALCHEMY_DAEMON`, which every command that
/// resolves the daemon target refuses, so success proves these commands
/// never look for alchemyd.
fn alchemy(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alchemy"))
        .args(args)
        .env(DAEMON_ENV, "https://unreachable.invalid")
        .env_remove(alchemy_api::SOCKET_ENV)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{:?}: {stderr}", out.status);
    assert_eq!(stderr, "");
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn every_shell_gets_completions_naming_status() {
    for shell in ["bash", "zsh", "fish"] {
        for args in [
            vec!["completions", shell],
            vec!["--json", "completions", shell],
        ] {
            let script = stdout(&alchemy(&args));
            assert!(!script.trim().is_empty(), "{args:?}");
            assert!(script.contains("status"), "{args:?}");
            for hidden in HIDDEN {
                assert!(!script.contains(hidden), "{args:?} names {hidden}");
            }
        }
    }
}

#[test]
fn an_unknown_shell_is_a_usage_error() {
    let out = alchemy(&["completions", "powershell"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(out.stdout, b"");
}

#[test]
fn help_does_not_list_hidden_commands() {
    let help = stdout(&alchemy(&["--help"]));
    assert!(help.contains("completions"), "{help}");
    for hidden in HIDDEN {
        assert!(!help.contains(hidden), "{help}");
    }
}

#[test]
fn generated_man_pages_render() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("man1");
    stdout(&alchemy(&["generate-man", "--out", out.to_str().unwrap()]));

    let mut names: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "alchemy-app-build.1",
            "alchemy-app-cancel.1",
            "alchemy-app-create.1",
            "alchemy-app-down.1",
            "alchemy-app-edit.1",
            "alchemy-app-list.1",
            "alchemy-app-logs.1",
            "alchemy-app-pull.1",
            "alchemy-app-restart.1",
            "alchemy-app-show.1",
            "alchemy-app-start.1",
            "alchemy-app-stats.1",
            "alchemy-app-stop.1",
            "alchemy-app-up.1",
            "alchemy-app-update.1",
            "alchemy-app.1",
            "alchemy-completions.1",
            "alchemy-machine-autostart.1",
            "alchemy-machine-docker-start.1",
            "alchemy-machine-docker.1",
            "alchemy-machine-ports.1",
            "alchemy-machine-privileges-grant.1",
            "alchemy-machine-privileges-revoke.1",
            "alchemy-machine-privileges.1",
            "alchemy-machine-stats.1",
            "alchemy-machine.1",
            "alchemy-open.1",
            "alchemy-settings-set.1",
            "alchemy-settings-show.1",
            "alchemy-settings.1",
            "alchemy-status.1",
            "alchemy-version.1",
            "alchemy.1"
        ]
    );

    for name in &names {
        let Some((renderer, text)) = render(&out.join(name)) else {
            eprintln!("skipping: no man page renderer (man -l, mandoc, groff) is installed");
            return;
        };
        assert!(text.contains("NAME"), "{renderer} {name}:\n{text}");
        assert!(text.contains("SYNOPSIS"), "{renderer} {name}:\n{text}");
        assert!(text.contains("alchemy"), "{renderer} {name}:\n{text}");
        for hidden in HIDDEN {
            assert!(!text.contains(hidden), "{renderer} {name}:\n{text}");
        }
    }
    let (_, top) = render(&out.join("alchemy.1")).unwrap();
    assert!(top.contains("alchemy-status(1)"), "{top}");
    assert!(top.contains("alchemy-completions(1)"), "{top}");
    assert!(top.contains("alchemy-version(1)"), "{top}");
    assert!(top.contains("alchemy-app(1)"), "{top}");
    assert!(top.contains("alchemy-settings(1)"), "{top}");
}

/// Render `page` with the first renderer installed: `man -l` where it
/// exists (man-db, on Linux), otherwise `mandoc` (macOS) or `groff`.
/// `None` when there is none.
fn render(page: &Path) -> Option<(&'static str, String)> {
    let page = page.to_str().unwrap();
    let mut renderers: Vec<(&'static str, Vec<&str>)> = Vec::new();
    if cfg!(target_os = "linux") {
        renderers.push(("man", vec!["-l", page]));
    }
    renderers.push(("mandoc", vec!["-T", "ascii", "-W", "error", page]));
    renderers.push(("groff", vec!["-man", "-T", "ascii", page]));
    for (program, args) in renderers {
        let out = match Command::new(program)
            .args(&args)
            .env("MANPAGER", "cat")
            .env("PAGER", "cat")
            .env("MANWIDTH", "100")
            .output()
        {
            Ok(out) => out,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => panic!("{program}: {e}"),
        };
        let text = plain(&String::from_utf8_lossy(&out.stdout));
        assert!(
            out.status.success(),
            "{program} {args:?}: {:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !text.trim().is_empty(),
            "{program} {args:?} rendered nothing"
        );
        return Some((program, text));
    }
    None
}

/// `text` without terminal overstrikes (`N\bN` for bold, `_\bx` for
/// underline), with Unicode hyphens and minus signs as ASCII hyphens.
fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\u{8}' {
            plain.pop();
        } else {
            plain.push(c);
        }
    }
    plain.replace(['\u{2010}', '\u{2212}'], "-")
}
