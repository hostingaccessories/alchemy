//! `alchemy generate-man`: man pages for packaging, generated from the
//! command tree.
//!
//! Hidden from help, completions, and the man pages themselves. It writes
//! `alchemy.1` plus one page per visible subcommand, such as
//! `alchemy-status.1`, and never contacts alchemyd.

use std::path::{Path, PathBuf};

use clap_mangen::Man;

use crate::error::ClientError;

/// Arguments to `alchemy generate-man`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Directory to write the pages to, created if missing. Existing pages
    /// are overwritten.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
}

/// One rendered man page.
#[derive(Debug)]
pub(crate) struct Page {
    /// The page's file name, such as `alchemy-status.1`.
    pub(crate) file_name: String,
    /// The page's roff source.
    pub(crate) roff: Vec<u8>,
}

/// Run `alchemy generate-man`, writing every page into `args.out`.
pub(crate) fn run(args: &Args) -> Result<(), ClientError> {
    let pages = pages()?;
    std::fs::create_dir_all(&args.out).map_err(at(&args.out))?;
    for page in pages {
        let path = args.out.join(&page.file_name);
        std::fs::write(&path, &page.roff).map_err(at(&path))?;
    }
    Ok(())
}

/// The man pages for `alchemy` and each visible subcommand, leaving out
/// hidden commands and clap's `help` subcommand.
pub(crate) fn pages() -> Result<Vec<Page>, ClientError> {
    let mut cmd = super::visible_tree().disable_help_subcommand(true);
    cmd.build();
    let source = format!("{} {}", cmd.get_name(), env!("CARGO_PKG_VERSION"));
    let mut pages = Vec::new();
    collect(cmd, &source, &mut pages)?;
    Ok(pages)
}

fn collect(cmd: clap::Command, source: &str, pages: &mut Vec<Page>) -> Result<(), ClientError> {
    for sub in cmd.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        collect(sub.clone(), source, pages)?;
    }
    let man = Man::new(cmd).source(source);
    let mut roff = Vec::new();
    man.render(&mut roff)?;
    pages.push(Page {
        file_name: man.get_filename(),
        roff,
    });
    Ok(())
}

fn at(path: &Path) -> impl FnOnce(std::io::Error) -> ClientError {
    move |err| {
        ClientError::Output(std::io::Error::new(
            err.kind(),
            format!("{}: {err}", path.display()),
        ))
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

    fn page(name: &str) -> String {
        let pages = pages().unwrap();
        let page = pages.iter().find(|p| p.file_name == name).unwrap();
        String::from_utf8(page.roff.clone()).unwrap()
    }

    #[test]
    fn one_page_per_visible_command() {
        let mut names: Vec<_> = pages().unwrap().into_iter().map(|p| p.file_name).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "alchemy-app-list.1",
                "alchemy-app-show.1",
                "alchemy-app-stats.1",
                "alchemy-app.1",
                "alchemy-completions.1",
                "alchemy-machine-stats.1",
                "alchemy-machine.1",
                "alchemy-settings-set.1",
                "alchemy-settings-show.1",
                "alchemy-settings.1",
                "alchemy-status.1",
                "alchemy-version.1",
                "alchemy.1"
            ]
        );
    }

    #[test]
    fn the_top_page_lists_visible_commands_only() {
        let top = page("alchemy.1");
        assert!(top.starts_with(".ie \\n(.g .ds Aq"), "{top}");
        assert!(top.contains(".TH alchemy 1"), "{top}");
        assert!(top.contains("alchemy\\-status(1)"), "{top}");
        assert!(!top.contains("generate"), "{top}");
    }

    #[test]
    fn subcommand_pages_are_named_after_the_full_command() {
        let status = page("alchemy-status.1");
        assert!(status.contains(".TH alchemy-status 1"), "{status}");
        assert!(status.contains("\\-\\-watch"), "{status}");
        assert!(
            status.contains(&format!("\"alchemy {}\"", env!("CARGO_PKG_VERSION"))),
            "{status}"
        );
    }

    #[test]
    fn run_writes_the_pages_into_a_new_directory() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("man/man1");
        run(&Args { out: out.clone() }).unwrap();
        for page in pages().unwrap() {
            assert_eq!(std::fs::read(out.join(&page.file_name)).unwrap(), page.roff);
        }
    }

    #[test]
    fn a_write_failure_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("taken");
        std::fs::write(&file, "").unwrap();
        let err = run(&Args { out: file.clone() }).unwrap_err();
        assert_eq!(err.exit_code(), 74, "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write output: {}: ", file.display())),
            "{err}"
        );
    }
}
