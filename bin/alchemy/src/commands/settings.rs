//! `alchemy settings <verb>`: alchemyd's settings, as on the Settings page.
//!
//! Settings that only change where alchemyd looks, like the app folders,
//! or what it announces, like apps' `.local` names, change nothing that
//! can't be changed back, so `set` takes no safety flags and asks no
//! question.

pub mod set;
pub mod show;

use alchemy_api::apps::AppSettings;
use alchemy_api::network::NetworkSettings;

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
    /// Show where alchemyd looks for apps, and whether it announces their
    /// .local names.
    Show(show::Args),

    /// Change where alchemyd looks for apps, or whether it announces their
    /// .local names.
    ///
    /// `app-home` is the folder new apps go in, searched first; `default`
    /// sets it back to ~/Alchemy/apps. `app-folders` replaces the other
    /// folders searched after it, in order; give none to search only the
    /// home. Paths must be full paths. `mdns` is `on` or `off`: whether
    /// apps answer at NAME.local on your network, announced over mDNS.
    /// While it is off, apps have no .local address.
    Set(set::Args),
}

/// The settings as `show` and `set` print them, or as one JSON line: the
/// app settings, with the network settings under `network`.
pub(crate) fn settings_lines(
    settings: &AppSettings,
    network: NetworkSettings,
    json: bool,
) -> Vec<String> {
    if json {
        let mut value = serde_json::json!(settings);
        if let Some(object) = value.as_object_mut() {
            object.insert("network".to_owned(), serde_json::json!(network));
        }
        return vec![value.to_string()];
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
    rows.push(vec![
        "Local names (mDNS):".to_owned(),
        on_off(network.mdns).to_owned(),
    ]);
    columns(&rows)
}

/// How a switch is typed and shown.
pub(crate) fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
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

    const ON: NetworkSettings = NetworkSettings { mdns: true };

    #[test]
    fn prints_the_home_other_folders_and_local_names() {
        let cases: [(AppSettings, NetworkSettings, &[&str]); 3] = [
            (
                settings(Some("/home/me/Alchemy/apps"), &[]),
                ON,
                &[
                    "App home:            /home/me/Alchemy/apps (default)",
                    "Other app folders:   none",
                    "Local names (mDNS):  on",
                ],
            ),
            (
                settings(Some("/srv/apps"), &["/opt/stacks", "/data/apps"]),
                NetworkSettings { mdns: false },
                &[
                    "App home:            /srv/apps",
                    "Other app folders:   /opt/stacks",
                    "                     /data/apps",
                    "Local names (mDNS):  off",
                ],
            ),
            (
                AppSettings {
                    home: None,
                    default_home: None,
                    extra: vec![],
                },
                ON,
                &[
                    "App home:            none",
                    "Other app folders:   none",
                    "Local names (mDNS):  on",
                ],
            ),
        ];
        for (settings, network, want) in cases {
            assert_eq!(
                settings_lines(&settings, network, false),
                want,
                "{settings:?}"
            );
        }
    }

    #[test]
    fn json_is_the_app_settings_with_the_network_settings() {
        let s = settings(Some("/srv/apps"), &["/opt/stacks"]);
        let lines = settings_lines(&s, NetworkSettings { mdns: false }, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(serde_json::from_str::<AppSettings>(&lines[0]).unwrap(), s);
        let value: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(value["network"], serde_json::json!({"mdns": false}));
    }
}
