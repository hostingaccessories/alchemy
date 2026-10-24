//! `alchemy open`: sign the browser in to alchemyd's web UI and open it.
//!
//! The TCP listener only answers a browser that has signed in. alchemyd
//! issues a one-time launch code on its socket, which only this user can
//! open, and the link `/login?code=CODE` on the TCP listener turns it into
//! a session cookie. The code works once, within a minute.

use std::io::Write;

use alchemy_api::session::LaunchResponse;

use crate::browser::{Browser, SystemBrowser};
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "open";

/// The socket-only route that issues launch codes.
pub(crate) const LAUNCH_PATH: &str = "/api/session/launch";

/// Arguments to `alchemy open`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// Print the sign-in link instead of opening it. It works once,
    /// within a minute.
    #[arg(long)]
    pub print: bool,

    /// Sign in at this origin instead of alchemyd's own, such as
    /// `http://localhost:5173` for the web UI's dev server, which passes
    /// `/login` on to alchemyd.
    #[arg(
        long,
        value_name = "URL",
        help = "Sign in at this origin instead of alchemyd's own, such as \
                http://localhost:5173 for the web UI's dev server, which passes /login on \
                to alchemyd"
    )]
    pub url_base: Option<String>,
}

/// Run `alchemy open` in the system's browser.
pub(crate) async fn run_system<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    let mut browser = SystemBrowser::new(alchemy_api::runtime_dir());
    run(ctx, &mut browser, args).await
}

/// Run `alchemy open`, opening the link in `browser`.
pub(crate) async fn run<D: Daemon, W: Write, B: Browser>(
    ctx: &mut Context<D, W>,
    browser: &mut B,
    args: Args,
) -> Result<(), ClientError> {
    let base = args.url_base.as_deref().map(url_base).transpose()?;
    let launch: LaunchResponse = ctx.post(LAUNCH_PATH, &serde_json::Map::new()).await?;
    let url = match &base {
        Some(base) => format!("{base}/login?code={}", launch.code),
        None => launch.url.clone(),
    };
    let opened = if args.print {
        false
    } else {
        match browser.open(&url).await {
            Ok(()) => true,
            Err(reason) => {
                ctx.out().progress(format!(
                    "cannot open a browser: {reason}\n\
                     Open this link in your browser within {} seconds; it works once:",
                    launch.expires_in
                ));
                false
            }
        }
    };
    for line in open_lines(&url, opened, launch.expires_in, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// `--url-base` without a trailing `/`, if it is an `http` or `https`
/// origin: a scheme and a host, with no path, query, or fragment.
pub(crate) fn url_base(base: &str) -> Result<String, ClientError> {
    let trimmed = base.strip_suffix('/').unwrap_or(base);
    let authority = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"));
    match authority {
        Some(authority)
            if !authority.is_empty() && !authority.contains(['/', '?', '#', '@', ' ']) =>
        {
            Ok(trimmed.to_owned())
        }
        _ => Err(ClientError::Usage {
            message: format!(
                "invalid --url-base {base:?}: expected an origin such as http://localhost:5173"
            ),
            hint: None,
        }),
    }
}

/// What `alchemy open` prints: the link, unless the browser opened it, or
/// one JSON object with the link either way.
pub(crate) fn open_lines(url: &str, opened: bool, expires_in: u32, json: bool) -> Vec<String> {
    if json {
        let doc = serde_json::json!({
            "url": url,
            "opened": opened,
            "expires_in": expires_in,
        });
        return vec![doc.to_string()];
    }
    if opened {
        let origin = url.split_once("/login?").map_or(url, |(origin, _)| origin);
        vec![format!("Opened {origin} in your browser")]
    } else {
        vec![url.to_owned()]
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
    use crate::browser::FakeBrowser;
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    const URL: &str = "http://127.0.0.1:7240/login?code=c0de";

    fn launch() -> LaunchResponse {
        LaunchResponse {
            code: "c0de".into(),
            url: URL.into(),
            expires_in: 60,
        }
    }

    fn fake() -> FakeDaemon {
        FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::POST, LAUNCH_PATH, &launch())
    }

    #[test]
    fn url_base_table() {
        let cases = [
            ("http://localhost:5173", Some("http://localhost:5173")),
            ("http://localhost:5173/", Some("http://localhost:5173")),
            ("https://alchemy.lan", Some("https://alchemy.lan")),
            ("http://[::1]:7240", Some("http://[::1]:7240")),
            ("http://", None),
            ("localhost:5173", None),
            ("ftp://localhost", None),
            ("http://localhost:5173/app", None),
            ("http://localhost:5173?x=1", None),
            ("http://localhost:5173#x", None),
            ("http://user@localhost", None),
            ("javascript:alert(1)", None),
            ("", None),
        ];
        for (base, want) in cases {
            let got = url_base(base).ok();
            assert_eq!(got.as_deref(), want, "{base:?}");
        }
        let err = url_base("nope").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn lines_per_format() {
        let cases = [
            (true, false, "Opened http://127.0.0.1:7240 in your browser"),
            (false, false, URL),
        ];
        for (opened, json, want) in cases {
            assert_eq!(open_lines(URL, opened, 60, json), [want], "{opened}");
        }
        for opened in [true, false] {
            let lines = open_lines(URL, opened, 60, true);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&lines[0]).unwrap(),
                serde_json::json!({ "url": URL, "opened": opened, "expires_in": 60 })
            );
        }
    }

    #[tokio::test]
    async fn opens_the_link_from_the_socket() {
        let mut browser = FakeBrowser::working();
        let mut ctx = Context::buffered(fake(), false, NAME);
        run(&mut ctx, &mut browser, Args::default()).await.unwrap();
        assert_eq!(browser.opened(), [URL]);
        assert_eq!(
            ctx.written(),
            "Opened http://127.0.0.1:7240 in your browser\n"
        );
        assert_eq!(
            ctx.daemon().requests(),
            ["GET /api/version", "POST /api/session/launch"]
        );
    }

    #[tokio::test]
    async fn print_only_prints() {
        for json in [false, true] {
            let mut browser = FakeBrowser::working();
            let mut ctx = Context::buffered(fake(), json, NAME);
            let args = Args {
                print: true,
                ..Args::default()
            };
            run(&mut ctx, &mut browser, args).await.unwrap();
            assert_eq!(browser.opened(), [] as [String; 0], "{json}");
            let want = open_lines(URL, false, 60, json);
            assert_eq!(ctx.written(), format!("{}\n", want[0]), "{json}");
        }
    }

    #[tokio::test]
    async fn prints_the_link_when_no_browser_opens() {
        let mut browser = FakeBrowser::failing("no program to open it with was found");
        let mut ctx = Context::buffered(fake(), false, NAME);
        run(&mut ctx, &mut browser, Args::default()).await.unwrap();
        assert_eq!(ctx.written(), format!("{URL}\n"));
        let progress = ctx.progress_written();
        assert!(
            progress.starts_with("cannot open a browser: no program to open it with was found\n"),
            "{progress}"
        );
        assert!(progress.contains("within 60 seconds"), "{progress}");
    }

    #[tokio::test]
    async fn url_base_swaps_the_origin() {
        let mut browser = FakeBrowser::working();
        let mut ctx = Context::buffered(fake(), true, NAME);
        let args = Args {
            url_base: Some("http://localhost:5173/".into()),
            ..Args::default()
        };
        run(&mut ctx, &mut browser, args).await.unwrap();
        assert_eq!(browser.opened(), ["http://localhost:5173/login?code=c0de"]);
    }

    #[tokio::test]
    async fn a_bad_url_base_contacts_nothing() {
        let mut browser = FakeBrowser::working();
        let mut ctx = Context::buffered(FakeDaemon::new(), false, NAME);
        let args = Args {
            url_base: Some("localhost:5173".into()),
            ..Args::default()
        };
        let err = run(&mut ctx, &mut browser, args).await.unwrap_err();
        assert!(matches!(err, ClientError::Usage { .. }), "{err:?}");
        assert_eq!(ctx.daemon().requests(), [] as [String; 0]);
        assert_eq!(browser.opened(), [] as [String; 0]);
    }

    #[tokio::test]
    async fn an_old_daemon_is_too_old() {
        let fake = FakeDaemon::new().version("0.1.0", 1).error(
            Method::POST,
            LAUNCH_PATH,
            404,
            "not_found",
            "not found",
        );
        let mut browser = FakeBrowser::working();
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(&mut ctx, &mut browser, Args::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::DaemonTooOld { .. }), "{err:?}");
        assert_eq!(browser.opened(), [] as [String; 0]);
    }
}
