//! `alchemy settings set KEY VALUE...`: change where alchemyd looks for
//! apps.

use std::io::Write;

use alchemy_api::apps::{AppSettings, AppSettingsUpdate};

use super::settings_lines;
use super::show::PATH;
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "settings set";

/// The value of `app-home` that means the default home.
const DEFAULT: &str = "default";

/// A setting `alchemy settings set` changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Key {
    /// The folder new apps go in, searched first.
    AppHome,
    /// The other folders searched, in order.
    AppFolders,
}

/// Arguments to `alchemy settings set`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// The setting to change.
    #[arg(value_enum)]
    pub key: Key,

    /// The new value: one folder, or `default`, for app-home; any number of
    /// folders for app-folders.
    #[arg(value_name = "VALUE")]
    pub values: Vec<String>,
}

/// Run `alchemy settings set`, then print the settings now in effect.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    check(args.key, &args.values)?;
    let current = ctx.get::<AppSettings>(PATH).await?;
    let update = update(&current, args.key, args.values)?;
    let settings = ctx.put::<_, AppSettings>(PATH, &update).await?;
    for line in settings_lines(&settings, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// Refuse a command line that gives `key` the wrong number of values,
/// before anything is sent.
fn check(key: Key, values: &[String]) -> Result<(), ClientError> {
    if key == Key::AppHome && values.len() != 1 {
        return Err(ClientError::Usage {
            message: "app-home takes one folder, or `default`".to_owned(),
            hint: Some("for example: alchemy settings set app-home /srv/apps".to_owned()),
        });
    }
    Ok(())
}

/// The update that changes `key` to `values` and keeps the rest of
/// `current`. A home that is the default stays the default.
pub(crate) fn update(
    current: &AppSettings,
    key: Key,
    values: Vec<String>,
) -> Result<AppSettingsUpdate, ClientError> {
    let kept_home = current
        .home
        .clone()
        .filter(|home| current.default_home.as_ref() != Some(home));
    match key {
        Key::AppHome => {
            check(key, &values)?;
            let home = values.into_iter().next().unwrap_or_default();
            Ok(AppSettingsUpdate {
                home: (home != DEFAULT).then_some(home),
                extra: current.extra.clone(),
            })
        }
        Key::AppFolders => Ok(AppSettingsUpdate {
            home: kept_home,
            extra: values,
        }),
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
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    const HOME: &str = "/home/me/Alchemy/apps";

    fn settings(home: &str, extra: &[&str]) -> AppSettings {
        AppSettings {
            home: Some(home.to_owned()),
            default_home: Some(HOME.to_owned()),
            extra: extra.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    fn values(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn want(home: Option<&str>, extra: &[&str]) -> AppSettingsUpdate {
        AppSettingsUpdate {
            home: home.map(str::to_owned),
            extra: values(extra),
        }
    }

    #[test]
    fn changes_one_setting_and_keeps_the_other() {
        let cases = [
            (
                settings(HOME, &["/opt/stacks"]),
                Key::AppHome,
                values(&["/srv/apps"]),
                want(Some("/srv/apps"), &["/opt/stacks"]),
            ),
            (
                settings("/srv/apps", &["/opt/stacks"]),
                Key::AppHome,
                values(&["default"]),
                want(None, &["/opt/stacks"]),
            ),
            (
                settings(HOME, &["/opt/stacks"]),
                Key::AppFolders,
                values(&["/a", "/b"]),
                want(None, &["/a", "/b"]),
            ),
            (
                settings("/srv/apps", &["/opt/stacks"]),
                Key::AppFolders,
                values(&[]),
                want(Some("/srv/apps"), &[]),
            ),
        ];
        for (current, key, values, want) in cases {
            assert_eq!(update(&current, key, values).unwrap(), want, "{key:?}");
        }
    }

    #[test]
    fn app_home_takes_exactly_one_value() {
        for given in [values(&[]), values(&["/a", "/b"])] {
            let err = update(&settings(HOME, &[]), Key::AppHome, given).unwrap_err();
            assert_eq!(err.kind(), "usage", "{err}");
            assert_eq!(err.exit_code(), 2);
        }
    }

    #[tokio::test]
    async fn a_usage_error_sends_nothing() {
        let mut ctx = Context::buffered(FakeDaemon::new(), false, NAME);
        let err = run(
            &mut ctx,
            Args {
                key: Key::AppHome,
                values: values(&[]),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), "usage");
        assert_eq!(ctx.daemon().requests(), [] as [String; 0]);
    }

    #[tokio::test]
    async fn puts_the_update_and_prints_the_result() {
        let after = settings(HOME, &["/opt/stacks"]);
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &settings(HOME, &[]))
            .ok(Method::PUT, PATH, &after);
        let mut ctx = Context::buffered(fake, false, NAME);
        run(
            &mut ctx,
            Args {
                key: Key::AppFolders,
                values: values(&["/opt/stacks"]),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            ctx.written(),
            settings_lines(&after, false).join("\n") + "\n"
        );
        assert_eq!(
            ctx.daemon().requests(),
            [
                "GET /api/version",
                "GET /api/settings/apps",
                "PUT /api/settings/apps"
            ]
        );
        assert_eq!(
            ctx.daemon().bodies(),
            [r#"{"home":null,"extra":["/opt/stacks"]}"#]
        );
    }

    #[tokio::test]
    async fn reports_what_alchemyd_refused() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &settings(HOME, &[]))
            .error(
                Method::PUT,
                PATH,
                422,
                "invalid",
                "stacks is not a full path; use one that starts with /",
            );
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(
            &mut ctx,
            Args {
                key: Key::AppFolders,
                values: values(&["stacks"]),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            err.to_string(),
            "alchemyd: stacks is not a full path; use one that starts with /"
        );
        assert_eq!(ctx.written(), "");
    }
}
