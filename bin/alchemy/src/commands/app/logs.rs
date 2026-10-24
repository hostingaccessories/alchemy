//! `alchemy app logs NAME [SERVICE]`: an app's logs, followed with `-f`.

use std::fmt::Write as _;
use std::io::Write;

use alchemy_api::logs::{LogLine, LogsDone};

use super::path_segment;
use crate::context::{Context, decode_data};
use crate::daemon::{Daemon, EventStream as _};
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app logs";

/// Arguments to `alchemy app logs`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The app's name: its folder's name.
    pub name: String,

    /// Only this service's lines.
    pub service: Option<String>,

    /// Keep printing new lines as they arrive, until interrupted.
    #[arg(short, long)]
    pub follow: bool,

    /// Start with this many of the last lines (default 200, at most 5000).
    #[arg(long, value_name = "N")]
    pub tail: Option<u32>,
}

/// Run `alchemy app logs`.
///
/// With `--follow` this only returns on an error, such as the daemon
/// stopping; the caller ends it by dropping the future.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    let mut events = ctx.stream(&logs_path(&args)).await?;
    let mut colors = ServiceColors::default();
    loop {
        let Some(raw) = events.next().await? else {
            return Err(ClientError::Disconnected);
        };
        match raw.event.as_str() {
            "log" => {
                let line: LogLine = decode_data(&raw)?;
                let color = ctx.out().color().then(|| colors.of(&line.service));
                let text = log_line(&line, ctx.json(), color);
                ctx.out().line(text)?;
            }
            "done" => {
                let done: LogsDone = decode_data(&raw)?;
                return match done.error {
                    None => Ok(()),
                    Some(error) => Err(ClientError::Daemon(error)),
                };
            }
            _ => {}
        }
    }
}

/// The logs route for `args`.
pub(crate) fn logs_path(args: &Args) -> String {
    let mut path = format!("/api/apps/{}/logs", path_segment(&args.name));
    let mut query = Vec::new();
    if let Some(service) = &args.service {
        query.push(format!("service={}", path_segment(service)));
    }
    if let Some(tail) = args.tail {
        query.push(format!("tail={tail}"));
    }
    if args.follow {
        query.push("follow=true".to_owned());
    }
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    path
}

/// ANSI foreground colours for service names, skipping red, which reads
/// as an error.
const PALETTE: [u8; 5] = [36, 33, 32, 35, 34];

/// A colour per service, in the order services first appear.
#[derive(Debug, Default)]
pub(crate) struct ServiceColors {
    seen: Vec<String>,
}

impl ServiceColors {
    /// The ANSI colour code for `service`.
    pub(crate) fn of(&mut self, service: &str) -> u8 {
        let index = self
            .seen
            .iter()
            .position(|s| s == service)
            .unwrap_or_else(|| {
                self.seen.push(service.to_owned());
                self.seen.len() - 1
            });
        PALETTE.get(index % PALETTE.len()).copied().unwrap_or(36)
    }
}

/// What `alchemy app logs` prints for one line: `service | line`, with
/// the service in `color` when given, or the [`LogLine`] as JSON.
///
/// Control characters other than tab are shown in caret notation, such as
/// `^[` for escape, so a container cannot drive the terminal.
pub(crate) fn log_line(line: &LogLine, json: bool, color: Option<u8>) -> String {
    if json {
        return serde_json::json!(line).to_string();
    }
    let mut text = match color {
        Some(code) => format!("\x1b[{code}m{}\x1b[0m | ", line.service),
        None => format!("{} | ", line.service),
    };
    for c in line.line.chars() {
        match c {
            '\t' => text.push(c),
            c if c.is_ascii_control() => {
                let caret = char::from(u8::try_from(c).unwrap_or(0) ^ 0x40);
                let _ = write!(text, "^{caret}");
            }
            c => text.push(c),
        }
    }
    text
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_api::logs::LogStream;

    use super::*;
    use crate::daemon::fake::{FakeDaemon, sse};
    use crate::sse::SseEvent;

    fn line(service: &str, text: &str) -> LogLine {
        LogLine {
            service: service.into(),
            container: format!("blog-{service}-1"),
            stream: LogStream::Stdout,
            time: Some("2024-05-06T07:08:09.000000000Z".into()),
            line: text.into(),
        }
    }

    fn args(name: &str) -> Args {
        Args {
            name: name.into(),
            ..Args::default()
        }
    }

    #[test]
    fn builds_the_route() {
        let cases = [
            (args("blog"), "/api/apps/blog/logs"),
            (
                Args {
                    service: Some("web".into()),
                    follow: true,
                    tail: Some(10),
                    ..args("blog")
                },
                "/api/apps/blog/logs?service=web&tail=10&follow=true",
            ),
            (
                Args {
                    service: Some("a&b=c".into()),
                    ..args("my app")
                },
                "/api/apps/my%20app/logs?service=a%26b%3Dc",
            ),
            (
                Args {
                    tail: Some(0),
                    ..args("blog")
                },
                "/api/apps/blog/logs?tail=0",
            ),
        ];
        for (args, want) in cases {
            assert_eq!(logs_path(&args), want, "{args:?}");
        }
    }

    #[test]
    fn renders_lines() {
        let cases = [
            ((line("web", "GET /"), false, None), "web | GET /"),
            (
                (line("web", "GET /"), false, Some(36)),
                "\x1b[36mweb\x1b[0m | GET /",
            ),
            (
                (line("web", "\x1b[31mred\x1b[0m\tx\r\x07\x7f"), false, None),
                "web | ^[[31mred^[[0m\tx^M^G^?",
            ),
            (
                (line("db", "é ok"), true, Some(36)),
                r#"{"container":"blog-db-1","line":"é ok","service":"db","stream":"stdout","time":"2024-05-06T07:08:09.000000000Z"}"#,
            ),
        ];
        for ((input, json, color), want) in cases {
            assert_eq!(log_line(&input, json, color), want, "{input:?}");
        }
    }

    #[test]
    fn colours_services_in_order_of_appearance() {
        let mut colors = ServiceColors::default();
        let got: Vec<u8> = ["web", "db", "web", "a", "b", "c", "d"]
            .iter()
            .map(|s| colors.of(s))
            .collect();
        assert_eq!(got, [36, 33, 36, 32, 35, 34, 36]);
    }

    fn log(service: &str, text: &str) -> SseEvent {
        sse("log", &serde_json::to_string(&line(service, text)).unwrap())
    }

    type RunCase<'a> = (Vec<Result<SseEvent, ClientError>>, bool, &'a str, &'a str);

    #[tokio::test]
    async fn prints_lines_until_done() {
        let path = "/api/apps/blog/logs?follow=true";
        let cases: Vec<RunCase> = vec![
            (
                vec![
                    Ok(log("web", "one")),
                    Ok(sse("following", "{}")),
                    Ok(sse("future", "{}")),
                    Ok(log("db", "two")),
                    Ok(sse("done", r#"{"error":null}"#)),
                ],
                false,
                "web | one\ndb | two\n",
                "ok",
            ),
            (
                vec![
                    Ok(log("web", "one")),
                    Ok(sse("done", r#"{"error":"Docker isn't running"}"#)),
                ],
                false,
                "web | one\n",
                "1 daemon: alchemyd: Docker isn't running",
            ),
            (
                vec![Ok(log("web", "one"))],
                false,
                "web | one\n",
                "69 disconnected: alchemyd closed the connection",
            ),
            (vec![Ok(sse("log", "nope"))], false, "", "76 protocol"),
            (
                vec![Ok(log("web", "one")), Ok(sse("done", r#"{"error":null}"#))],
                true,
                "{\"container\":\"blog-web-1\",\"line\":\"one\",\"service\":\"web\",\"stream\":\"stdout\",\"time\":\"2024-05-06T07:08:09.000000000Z\"}\n",
                "ok",
            ),
        ];
        for (items, json, want_out, want) in cases {
            let fake = FakeDaemon::new().version("1.2.3", 1).stream(path, items);
            let mut ctx = Context::buffered(fake, json, NAME);
            let result = run(
                &mut ctx,
                Args {
                    follow: true,
                    ..args("blog")
                },
            )
            .await;
            let got = match result {
                Ok(()) => "ok".to_owned(),
                Err(err) => format!("{} {}: {err}", err.exit_code(), err.kind()),
            };
            assert!(got.starts_with(want), "{got} != {want}");
            assert_eq!(ctx.written(), want_out, "{want}");
            assert_eq!(
                ctx.daemon().requests(),
                ["GET /api/version", format!("GET {path}").as_str()]
            );
        }
    }

    #[tokio::test]
    async fn refusals_are_errors() {
        let cases = [
            (404, "app_not_found", "no app is named \"nope\"", "1 daemon"),
            (404, "not_found", "not found", "76 daemon_too_old"),
            (
                400,
                "bad_request",
                "the app nope has no service named \"x\"",
                "1 daemon",
            ),
        ];
        for (status, code, error, want) in cases {
            let body =
                serde_json::to_vec(&serde_json::json!({ "error": error, "code": code })).unwrap();
            let fake = FakeDaemon::new().version("1.2.3", 1).refuse_stream(
                "/api/apps/nope/logs",
                status,
                body,
            );
            let mut ctx = Context::buffered(fake, false, NAME);
            let err = run(&mut ctx, args("nope")).await.unwrap_err();
            assert_eq!(format!("{} {}", err.exit_code(), err.kind()), want, "{err}");
        }
    }
}
