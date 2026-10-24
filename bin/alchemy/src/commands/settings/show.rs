//! `alchemy settings show`: where alchemyd looks for apps.

use std::io::Write;

use alchemy_api::apps::AppSettings;

use super::settings_lines;
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "settings show";

/// The route holding the app folder settings.
pub(crate) const PATH: &str = "/api/settings/apps";

/// Arguments to `alchemy settings show`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {}

/// Run `alchemy settings show`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    _args: Args,
) -> Result<(), ClientError> {
    let settings = ctx.get::<AppSettings>(PATH).await?;
    for line in settings_lines(&settings, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
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

    #[tokio::test]
    async fn reads_the_settings_from_alchemyd() {
        let settings = AppSettings {
            home: Some("/srv/apps".into()),
            default_home: None,
            extra: vec![],
        };
        for json in [false, true] {
            let fake = FakeDaemon::new()
                .version("1.2.3", 1)
                .ok(Method::GET, PATH, &settings);
            let mut ctx = Context::buffered(fake, json, NAME);
            run(&mut ctx, Args::default()).await.unwrap();
            let want = settings_lines(&settings, json).join("\n") + "\n";
            assert_eq!(ctx.written(), want);
            assert_eq!(
                ctx.daemon().requests(),
                ["GET /api/version", "GET /api/settings/apps"]
            );
        }
    }
}
