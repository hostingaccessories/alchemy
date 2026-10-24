//! `alchemy settings <verb>`: alchemyd's settings, as on the Settings page.
//!
//! Settings that only change where alchemyd looks, like the app folders,
//! change nothing that can't be changed back, so `set` takes no safety
//! flags and asks no question.

pub mod set;
pub mod show;

use alchemy_api::apps::AppSettings;

use super::app::columns;

/// Arguments to `alchemy settings`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub command: Verb,
}

/// The verbs of `alchemy settings`.
#[derive(Debug, clap::Subcommand)]
pub enum Verb {
    /// Show where alchemyd looks for apps.
    Show(show::Args),

    /// Change where alchemyd looks for apps.
    ///
    /// `app-home` is the folder new apps go in, searched first; `default`
    /// sets it back to ~/Alchemy/apps. `app-folders` replaces the other
    /// folders searched after it, in order; give none to search only the
    /// home. Paths must be full paths.
    Set(set::Args),
}

/// The settings as `show` and `set` print them, or as one JSON line.
pub(crate) fn settings_lines(settings: &AppSettings, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::json!(settings).to_string()];
    }
    let home = match (&settings.home, &settings.default_home) {
        (Some(home), Some(default)) if home == default => format!("{home} (default)"),
        (Some(home), _) => home.clone(),
        (None, _) => "none".to_owned(),
    };
    let mut rows = vec![vec!["App home:".to_owned(), home]];
    let mut extra = settings.extra.iter();
    rows.push(vec![
        "Other app folders:".to_owned(),
        extra.next().cloned().unwrap_or_else(|| "none".to_owned()),
    ]);
    rows.extend(extra.map(|dir| vec![String::new(), dir.clone()]));
    columns(&rows)
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

    pub(crate) fn settings(home: Option<&str>, extra: &[&str]) -> AppSettings {
        AppSettings {
            home: home.map(str::to_owned),
            default_home: Some("/home/me/Alchemy/apps".into()),
            extra: extra.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    #[test]
    fn prints_the_home_and_other_folders() {
        let cases: [(AppSettings, &[&str]); 3] = [
            (
                settings(Some("/home/me/Alchemy/apps"), &[]),
                &[
                    "App home:           /home/me/Alchemy/apps (default)",
                    "Other app folders:  none",
                ],
            ),
            (
                settings(Some("/srv/apps"), &["/opt/stacks", "/data/apps"]),
                &[
                    "App home:           /srv/apps",
                    "Other app folders:  /opt/stacks",
                    "                    /data/apps",
                ],
            ),
            (
                AppSettings {
                    home: None,
                    default_home: None,
                    extra: vec![],
                },
                &["App home:           none", "Other app folders:  none"],
            ),
        ];
        for (settings, want) in cases {
            assert_eq!(settings_lines(&settings, false), want, "{settings:?}");
        }
    }

    #[test]
    fn json_is_the_settings() {
        let s = settings(Some("/srv/apps"), &["/opt/stacks"]);
        let lines = settings_lines(&s, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(serde_json::from_str::<AppSettings>(&lines[0]).unwrap(), s);
    }
}
