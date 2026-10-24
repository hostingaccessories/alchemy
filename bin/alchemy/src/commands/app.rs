//! `alchemy app`: the apps Alchemy runs. An app is a Compose project, named
//! as Compose names it.

pub mod stats;

use alchemy_core::StackName;

/// Arguments to `alchemy app`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// What to do with the app.
    #[command(subcommand)]
    pub command: Verb,
}

/// The `alchemy app` verbs.
#[derive(Debug, clap::Subcommand)]
pub enum Verb {
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
}
