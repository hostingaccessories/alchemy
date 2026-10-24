//! `alchemy app <verb>`: the apps Alchemy runs. An app is a Compose
//! project in one of the app folders, named after its folder.
//!
//! Each verb is a module with its own `Args`, `NAME`, and `run`; add one by
//! adding its module and a [`Verb`] variant.

pub mod list;
pub mod show;
pub mod stats;

use alchemy_api::apps::{AppService, ContainerState, StackStatus};
use alchemy_core::StackName;

/// Arguments to `alchemy app`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub command: Verb,
}

/// The verbs of `alchemy app`.
#[derive(Debug, clap::Subcommand)]
pub enum Verb {
    /// List every app, its status, and its folder, then any running
    /// Compose projects Alchemy doesn't manage.
    List(list::Args),

    /// Show one app: its folder, Compose project, services, and anything
    /// wrong with it.
    Show(show::Args),

    /// Show an app's CPU, memory, and network use, in total and per
    /// container.
    ///
    /// Figures are from alchemyd's latest sample, taken every minute. CPU is
    /// a share of the whole machine Docker runs on, or of its VM with Docker
    /// Desktop, so an app's containers add up to the machine's figure.
    Stats(stats::Args),
}

/// Parse an app name as Compose would accept it, in the CLI's words.
pub(crate) fn parse_name(name: &str) -> Result<StackName, String> {
    StackName::new(name).map_err(|e| e.to_string().replacen("stack name", "app name", 1))
}

/// The word for `status`, as the web UI uses it.
pub(crate) fn status_word(status: StackStatus) -> &'static str {
    match status {
        StackStatus::Running => "running",
        StackStatus::Partial => "partly running",
        StackStatus::Stopped => "stopped",
        _ => "unknown",
    }
}

/// Whether any of `service`'s containers is running.
pub(crate) fn is_running(service: &AppService) -> bool {
    service
        .containers
        .iter()
        .any(|c| c.state == ContainerState::Running)
}

/// `name` as one URL path segment: everything but unreserved characters
/// percent-encoded, so no name can reach another route.
pub(crate) fn path_segment(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Rows of cells padded into columns two spaces apart, without trailing
/// spaces.
pub(crate) fn columns(rows: &[Vec<String>]) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(i) {
                Some(w) => *w = (*w).max(width),
                None => widths.push(width),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                }
                line.push_str(cell);
                let pad = widths.get(i).copied().unwrap_or(0) - cell.chars().count();
                line.extend(std::iter::repeat_n(' ', pad));
            }
            line.trim_end().to_owned()
        })
        .collect()
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
    fn app_names_follow_compose() {
        let cases = [
            ("blog", Ok("blog")),
            ("my-app_2", Ok("my-app_2")),
            ("", Err("app name must not be empty")),
            (
                "Blog",
                Err("app name must start with a lowercase letter or digit"),
            ),
            ("a/b", Err("app name contains invalid character '/'")),
        ];
        for (name, want) in cases {
            let got = parse_name(name);
            match want {
                Ok(want) => assert_eq!(got.unwrap().as_str(), want, "{name}"),
                Err(want) => assert_eq!(got.unwrap_err(), want, "{name}"),
            }
        }
    }

    #[test]
    fn encodes_names_as_one_segment() {
        let cases = [
            ("blog", "blog"),
            ("my_app-2", "my_app-2"),
            ("My App", "My%20App"),
            ("../etc", "..%2Fetc"),
            ("a?b#c", "a%3Fb%23c"),
            ("é", "%C3%A9"),
        ];
        for (name, want) in cases {
            assert_eq!(path_segment(name), want, "{name}");
        }
    }

    #[test]
    fn pads_columns() {
        let rows = vec![
            vec!["NAME".into(), "STATUS".into(), "FOLDER".into()],
            vec!["blog".into(), "running".into(), "/srv/blog".into()],
            vec!["a".into(), "x".into(), String::new()],
        ];
        assert_eq!(
            columns(&rows),
            [
                "NAME  STATUS   FOLDER",
                "blog  running  /srv/blog",
                "a     x",
            ]
        );
        assert_eq!(columns(&[]), [] as [String; 0]);
    }

    #[test]
    fn status_words_match_the_web_ui() {
        let cases = [
            (StackStatus::Running, "running"),
            (StackStatus::Partial, "partly running"),
            (StackStatus::Stopped, "stopped"),
            (StackStatus::Unknown, "unknown"),
        ];
        for (status, want) in cases {
            assert_eq!(status_word(status), want);
        }
    }
}
