//! `COMPOSE_FILE` in a stack's `.env`: which compose files Compose reads.
//!
//! Compose reads a stack's `.env` before it picks the compose files, so a
//! `COMPOSE_FILE` there replaces the files it would find by name. Editing
//! one of those files has to validate against the same list, with the
//! staged copy in place of the file being edited.
//!
//! Only as much of the dotenv format is read as these two variables need:
//! `KEY=value` lines, an optional `export `, single or double quotes on one
//! line, and a ` #` comment after an unquoted value. The last assignment
//! wins. A value Compose would expand, with `$` or a backslash escape, is
//! refused rather than guessed at.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// The variable that lists the compose files.
pub const COMPOSE_FILE_VAR: &str = "COMPOSE_FILE";

/// The variable that sets what separates the entries of
/// [`COMPOSE_FILE_VAR`].
pub const COMPOSE_PATH_SEPARATOR_VAR: &str = "COMPOSE_PATH_SEPARATOR";

/// The separator Compose uses when [`COMPOSE_PATH_SEPARATOR_VAR`] is unset
/// or empty: the OS path list separator, `:` on Linux and macOS.
pub const DEFAULT_PATH_SEPARATOR: &str = ":";

/// Why the compose files a `.env` names could not be worked out, or do not
/// fit the file being edited.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ComposeFileError {
    /// The value uses `$` interpolation or a backslash escape, which
    /// Compose would expand and Alchemy does not.
    #[error(
        "{key} in .env uses a variable or an escape, so Alchemy can't tell which compose files Compose uses; edit this app's files by hand"
    )]
    Unsupported {
        /// The variable whose value it is.
        key: &'static str,
    },
    /// The value opens a quote it does not close on the same line.
    #[error("{key} in .env has a quote that isn't closed on the same line")]
    UnclosedQuote {
        /// The variable whose value it is.
        key: &'static str,
    },
    /// An entry in the list is empty, as in `a.yaml::b.yaml`.
    #[error("{COMPOSE_FILE_VAR} in .env has an empty entry")]
    EmptyEntry,
    /// The first entry, the file the editor opens, is not a file directly
    /// in the stack's folder.
    #[error(
        "{COMPOSE_FILE_VAR} in .env starts with {}, which isn't a file in the app's folder; Alchemy only edits files there", .0.display()
    )]
    NotInFolder(PathBuf),
    /// The file being edited is not among the entries, so Compose would
    /// not read it.
    #[error("Compose doesn't use {file} here: {COMPOSE_FILE_VAR} in .env lists {}", list(.entries))]
    NotListed {
        /// The file being edited.
        file: String,
        /// The entries, as written.
        entries: Vec<PathBuf>,
    },
}

fn list(entries: &[PathBuf]) -> String {
    entries
        .iter()
        .map(|entry| entry.to_string_lossy())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The entries of `COMPOSE_FILE` in the `.env` text `env`, as written, or
/// `None` if it is unset or empty. They are split on
/// `COMPOSE_PATH_SEPARATOR` from the same file, or `:` without one.
pub fn compose_file_entries(env: &str) -> Result<Option<Vec<PathBuf>>, ComposeFileError> {
    let mut files = None;
    let mut separator = None;
    for line in env.lines() {
        let Some((key, raw)) = assignment(line) else {
            continue;
        };
        if key == COMPOSE_FILE_VAR {
            files = Some(value(COMPOSE_FILE_VAR, raw)?);
        } else if key == COMPOSE_PATH_SEPARATOR_VAR {
            separator = Some(value(COMPOSE_PATH_SEPARATOR_VAR, raw)?);
        }
    }
    let Some(files) = files.filter(|files| !files.is_empty()) else {
        return Ok(None);
    };
    let separator = separator
        .filter(|sep| !sep.is_empty())
        .unwrap_or_else(|| DEFAULT_PATH_SEPARATOR.to_owned());
    let entries: Vec<PathBuf> = files.split(separator.as_str()).map(PathBuf::from).collect();
    if entries.iter().any(|entry| entry.as_os_str().is_empty()) {
        return Err(ComposeFileError::EmptyEntry);
    }
    Ok(Some(entries))
}

/// The key and the raw value of an assignment line, or `None` for a blank
/// line, a comment, or anything else.
fn assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start();
    if line.starts_with('#') {
        return None;
    }
    let line = line
        .strip_prefix("export")
        .filter(|rest| rest.starts_with([' ', '\t']))
        .map_or(line, str::trim_start);
    let (key, raw) = line.split_once('=')?;
    Some((key.trim(), raw.trim()))
}

/// The value `raw` stands for, without quotes or a trailing comment.
fn value(key: &'static str, raw: &str) -> Result<String, ComposeFileError> {
    let (text, expands) = if let Some(rest) = raw.strip_prefix('\'') {
        let (inner, _) = rest
            .split_once('\'')
            .ok_or(ComposeFileError::UnclosedQuote { key })?;
        (inner, false)
    } else if let Some(rest) = raw.strip_prefix('"') {
        let (inner, _) = rest
            .split_once('"')
            .ok_or(ComposeFileError::UnclosedQuote { key })?;
        (inner, inner.contains('\\'))
    } else {
        (unquoted(raw), false)
    };
    if expands || (!raw.starts_with('\'') && text.contains('$')) {
        return Err(ComposeFileError::Unsupported { key });
    }
    Ok(text.to_owned())
}

/// An unquoted value up to a `#` that follows white space.
fn unquoted(raw: &str) -> &str {
    let end = raw
        .char_indices()
        .zip(raw.chars().skip(1))
        .find(|((_, c), next)| c.is_whitespace() && *next == '#')
        .map_or(raw.len(), |((i, _), _)| i);
    raw.get(..end).unwrap_or(raw).trim_end()
}

/// The file to open for editing when `COMPOSE_FILE` lists `entries`: the
/// first, which must be a file directly in the stack's folder, such as
/// `compose.yaml` or `./compose.yaml`. Never an absolute path or one with
/// a `..`, so a `.env` cannot point the editor outside the folder.
pub fn edited_compose_file(entries: &[PathBuf]) -> Result<String, ComposeFileError> {
    let first = entries.first().ok_or(ComposeFileError::EmptyEntry)?;
    let mut parts = first.components().filter(|c| *c != Component::CurDir);
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(name)), None) => name
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| ComposeFileError::NotInFolder(first.clone())),
        _ => Err(ComposeFileError::NotInFolder(first.clone())),
    }
}

/// The compose files to validate an edit of `edited` with: each of
/// `entries` resolved against the stack's folder `dir`, as Compose run in
/// that folder resolves them, with `staged` in place of `edited`.
pub fn compose_file_args(
    entries: &[PathBuf],
    dir: &Path,
    edited: &str,
    staged: &Path,
) -> Result<Vec<PathBuf>, ComposeFileError> {
    let live = dir.join(edited);
    let mut found = false;
    let files = entries
        .iter()
        .map(|entry| {
            let path = resolve(dir, entry);
            if path == live {
                found = true;
                staged.to_owned()
            } else {
                path
            }
        })
        .collect();
    if found {
        Ok(files)
    } else {
        Err(ComposeFileError::NotListed {
            file: edited.to_owned(),
            entries: entries.to_vec(),
        })
    }
}

/// `entry` joined to `dir` and cleaned of `.` and `..` by its text alone,
/// as Go's `filepath.Abs` does.
fn resolve(dir: &Path, entry: &Path) -> PathBuf {
    let mut path = PathBuf::new();
    for part in dir.join(entry).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            other => path.push(other),
        }
    }
    path
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

    type Entries = Result<Option<&'static [&'static str]>, ComposeFileError>;
    type Args = Result<&'static [&'static str], ComposeFileError>;

    fn paths(entries: &[&str]) -> Vec<PathBuf> {
        entries.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn reads_compose_file_entries() {
        let cases: [(&str, Entries); 20] = [
            ("", Ok(None)),
            ("A=1\n# COMPOSE_FILE=x.yaml\n", Ok(None)),
            ("COMPOSE_FILE=\n", Ok(None)),
            ("COMPOSE_FILE=a.yaml\nCOMPOSE_FILE=\n", Ok(None)),
            ("COMPOSE_FILE=compose.yaml", Ok(Some(&["compose.yaml"]))),
            (
                "COMPOSE_FILE=compose.yaml:compose.prod.yaml\n",
                Ok(Some(&["compose.yaml", "compose.prod.yaml"])),
            ),
            (
                "COMPOSE_FILE=a.yaml\nexport COMPOSE_FILE=b.yaml\n",
                Ok(Some(&["b.yaml"])),
            ),
            ("  COMPOSE_FILE = a.yaml  \n", Ok(Some(&["a.yaml"]))),
            ("COMPOSE_FILE='a b.yaml' # c\n", Ok(Some(&["a b.yaml"]))),
            (
                "COMPOSE_FILE=\"a.yaml:b.yaml\"\n",
                Ok(Some(&["a.yaml", "b.yaml"])),
            ),
            ("COMPOSE_FILE=a.yaml # main\n", Ok(Some(&["a.yaml"]))),
            ("COMPOSE_FILE=a#b.yaml\n", Ok(Some(&["a#b.yaml"]))),
            ("COMPOSE_FILE='$a.yaml'\n", Ok(Some(&["$a.yaml"]))),
            (
                "COMPOSE_FILE=a.yaml;b.yaml\nCOMPOSE_PATH_SEPARATOR=;\n",
                Ok(Some(&["a.yaml", "b.yaml"])),
            ),
            (
                "COMPOSE_PATH_SEPARATOR=\nCOMPOSE_FILE=a.yaml:b.yaml\n",
                Ok(Some(&["a.yaml", "b.yaml"])),
            ),
            (
                "F=a.yaml\nCOMPOSE_FILE=${F}\n",
                Err(ComposeFileError::Unsupported {
                    key: COMPOSE_FILE_VAR,
                }),
            ),
            (
                "COMPOSE_FILE=\"$F:b.yaml\"\n",
                Err(ComposeFileError::Unsupported {
                    key: COMPOSE_FILE_VAR,
                }),
            ),
            (
                "COMPOSE_PATH_SEPARATOR=\"\\t\"\nCOMPOSE_FILE=a.yaml\n",
                Err(ComposeFileError::Unsupported {
                    key: COMPOSE_PATH_SEPARATOR_VAR,
                }),
            ),
            (
                "COMPOSE_FILE='a.yaml\n",
                Err(ComposeFileError::UnclosedQuote {
                    key: COMPOSE_FILE_VAR,
                }),
            ),
            (
                "COMPOSE_FILE=a.yaml::b.yaml\n",
                Err(ComposeFileError::EmptyEntry),
            ),
        ];
        for (env, want) in cases {
            let want = want.map(|entries| entries.map(paths));
            assert_eq!(compose_file_entries(env), want, "{env:?}");
        }
    }

    #[test]
    fn ignores_other_keys_and_their_values() {
        let env = "SECRET='unclosed\nPASS=${X}\nexported=1\nCOMPOSE_FILE=a.yaml\n";
        assert_eq!(compose_file_entries(env), Ok(Some(paths(&["a.yaml"]))));
        assert_eq!(
            compose_file_entries("exportCOMPOSE_FILE=a.yaml\n"),
            Ok(None)
        );
    }

    #[test]
    fn edits_the_first_entry_only_if_it_is_in_the_folder() {
        let cases: [(&[&str], Result<&str, ComposeFileError>); 7] = [
            (&["compose.yaml", "compose.prod.yaml"], Ok("compose.yaml")),
            (&["./base.yml"], Ok("base.yml")),
            (
                &["/etc/compose.yaml"],
                Err(ComposeFileError::NotInFolder("/etc/compose.yaml".into())),
            ),
            (
                &["../other/compose.yaml"],
                Err(ComposeFileError::NotInFolder(
                    "../other/compose.yaml".into(),
                )),
            ),
            (
                &["sub/compose.yaml"],
                Err(ComposeFileError::NotInFolder("sub/compose.yaml".into())),
            ),
            (&["."], Err(ComposeFileError::NotInFolder(".".into()))),
            (&[], Err(ComposeFileError::EmptyEntry)),
        ];
        for (entries, want) in cases {
            assert_eq!(
                edited_compose_file(&paths(entries)),
                want.map(str::to_owned),
                "{entries:?}"
            );
        }
    }

    #[test]
    fn substitutes_the_staged_copy_for_the_edited_file() {
        let dir = Path::new("/srv/apps/blog");
        let staged = Path::new("/srv/apps/blog/.alchemy-tmp-blog-1/compose.yaml");
        let cases: [(&[&str], &str, Args); 5] = [
            (
                &["compose.yaml", "compose.prod.yaml"],
                "compose.yaml",
                Ok(&[
                    "/srv/apps/blog/.alchemy-tmp-blog-1/compose.yaml",
                    "/srv/apps/blog/compose.prod.yaml",
                ]),
            ),
            (
                &["../shared/base.yaml", "./compose.yaml"],
                "compose.yaml",
                Ok(&[
                    "/srv/apps/shared/base.yaml",
                    "/srv/apps/blog/.alchemy-tmp-blog-1/compose.yaml",
                ]),
            ),
            (
                &["/srv/apps/blog/compose.yaml"],
                "compose.yaml",
                Ok(&["/srv/apps/blog/.alchemy-tmp-blog-1/compose.yaml"]),
            ),
            (
                &["a.yaml", "b.yaml"],
                "compose.yaml",
                Err(ComposeFileError::NotListed {
                    file: "compose.yaml".into(),
                    entries: paths(&["a.yaml", "b.yaml"]),
                }),
            ),
            (
                &["../blog2/compose.yaml"],
                "compose.yaml",
                Err(ComposeFileError::NotListed {
                    file: "compose.yaml".into(),
                    entries: paths(&["../blog2/compose.yaml"]),
                }),
            ),
        ];
        for (entries, edited, want) in cases {
            assert_eq!(
                compose_file_args(&paths(entries), dir, edited, staged),
                want.map(paths),
                "{entries:?}"
            );
        }
    }

    #[test]
    fn says_which_files_compose_uses_instead() {
        let err = ComposeFileError::NotListed {
            file: "compose.yaml".into(),
            entries: paths(&["a.yaml", "b.yaml"]),
        };
        assert_eq!(
            err.to_string(),
            "Compose doesn't use compose.yaml here: COMPOSE_FILE in .env lists a.yaml, b.yaml"
        );
    }
}
