//! Opening a file in the person's text editor, for `alchemy app edit`.

use std::future::Future;
use std::path::Path;

use crate::error::ClientError;

/// The editor used when neither `VISUAL` nor `EDITOR` is set.
pub const FALLBACK_EDITOR: &str = "vi";

/// Where a command lets the person edit a file: their editor in the
/// binary, a script in tests.
pub trait Editor {
    /// Let the person edit the file at `path`, returning once they are done.
    fn edit(&mut self, path: &Path) -> impl Future<Output = Result<(), ClientError>>;
}

/// The editor command line from `VISUAL` and `EDITOR`, as a program and its
/// arguments: the first that is set and not blank, split on white space,
/// else [`FALLBACK_EDITOR`].
///
/// Split rather than run through a shell, so a setting such as
/// `code --wait` works but nothing in it is interpreted.
pub fn editor_command(visual: Option<&str>, editor: Option<&str>) -> Vec<String> {
    [visual, editor]
        .into_iter()
        .flatten()
        .map(|value| {
            value
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .find(|words| !words.is_empty())
        .unwrap_or_else(|| vec![FALLBACK_EDITOR.to_owned()])
}

/// Runs the editor from `VISUAL` or `EDITOR` on the terminal.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalEditor;

impl Editor for TerminalEditor {
    /// Waits on its own thread, so the async runtime keeps running.
    fn edit(&mut self, path: &Path) -> impl Future<Output = Result<(), ClientError>> {
        let visual = std::env::var("VISUAL").ok();
        let editor = std::env::var("EDITOR").ok();
        let command = editor_command(visual.as_deref(), editor.as_deref());
        let path = path.to_owned();
        async move {
            let program = command.first().cloned().unwrap_or_default();
            let status = tokio::task::spawn_blocking(move || {
                std::process::Command::new(&program)
                    .args(command.get(1..).unwrap_or_default())
                    .arg(&path)
                    .status()
            })
            .await
            .map_err(|err| ClientError::Runtime(std::io::Error::other(err)))?;
            match status {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(ClientError::Editor(format!(
                    "the editor exited with {status}"
                ))),
                Err(err) => Err(ClientError::Editor(format!(
                    "cannot start the editor: {err}"
                ))),
            }
        }
    }
}

/// A scripted [`Editor`]: each edit replaces the file with the next
/// content in its list, and records what the file held when opened.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeEditor {
    edits: std::collections::VecDeque<String>,
    seen: Vec<String>,
}

#[cfg(test)]
impl FakeEditor {
    /// An editor that saves `edits` in turn, then leaves the file as is.
    pub(crate) fn saving(edits: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            edits: edits.into_iter().map(str::to_owned).collect(),
            seen: Vec::new(),
        }
    }

    /// What the file held each time it was opened.
    pub(crate) fn seen(&self) -> &[String] {
        &self.seen
    }
}

#[cfg(test)]
impl Editor for FakeEditor {
    fn edit(&mut self, path: &Path) -> impl Future<Output = Result<(), ClientError>> {
        let result = std::fs::read_to_string(path)
            .and_then(|before| {
                self.seen.push(before);
                match self.edits.pop_front() {
                    Some(after) => std::fs::write(path, after),
                    None => Ok(()),
                }
            })
            .map_err(|err| ClientError::Editor(err.to_string()));
        std::future::ready(result)
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
    fn picks_visual_then_editor_then_vi() {
        let cases: [(Option<&str>, Option<&str>, &[&str]); 7] = [
            (None, None, &["vi"]),
            (Some("nvim"), Some("nano"), &["nvim"]),
            (None, Some("nano"), &["nano"]),
            (Some(""), Some("nano"), &["nano"]),
            (Some("   "), None, &["vi"]),
            (Some("code --wait"), None, &["code", "--wait"]),
            (Some("vim; rm -rf ~"), None, &["vim;", "rm", "-rf", "~"]),
        ];
        for (visual, editor, want) in cases {
            assert_eq!(
                editor_command(visual, editor),
                want,
                "{visual:?} {editor:?}"
            );
        }
    }
}
