//! `alchemy app stats NAME`: one app's usage, in total and per container.

use std::io::Write;

use alchemy_api::MetricsResponse;
use alchemy_core::StackName;

use crate::commands::usage::{self, bytes, percent, rate, table};
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app stats";

/// Arguments to `alchemy app stats`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// The app's name: its folder's name.
    #[arg(value_parser = super::parse_name)]
    pub name: StackName,
}

/// The route for `name`'s figures. Validated names need no escaping.
pub(crate) fn path(name: &StackName) -> String {
    format!("/api/metrics?app={}", name.as_str())
}

/// Run `alchemy app stats NAME`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    let metrics = ctx.get::<MetricsResponse>(&path(&args.name)).await?;
    for line in stats_lines(args.name.as_str(), &metrics, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// What `alchemy app stats` prints: the app's totals, then a table of its
/// containers, or the response as one JSON object. `metrics` holds only
/// the app's Compose project, whatever it is named.
pub(crate) fn stats_lines(name: &str, metrics: &MetricsResponse, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::to_string(metrics).unwrap_or_default()];
    }
    if metrics.at.is_none() {
        return vec!["No usage yet: alchemyd samples it shortly after it starts".to_owned()];
    }
    let Some(app) = metrics.projects.first() else {
        return vec![format!("{name} has no running containers")];
    };
    let containers = if app.containers == 1 {
        "1 container".to_owned()
    } else {
        format!("{} containers", app.containers)
    };
    let mut lines = vec![
        format!(
            "{name}  {containers}  CPU {}  memory {}  in {}  out {}",
            percent(app.cpu),
            bytes(app.memory),
            rate(app.net_rx),
            rate(app.net_tx),
        ),
        String::new(),
    ];
    lines.extend(table(&usage::container_rows(&metrics.containers)));
    lines
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_api::{ContainerUsage, ProjectUsage};
    use clap::Parser as _;

    use super::*;
    use crate::Cli;
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    fn blog() -> MetricsResponse {
        let container = |name: &str, service: &str, cpu| ContainerUsage {
            id: name.into(),
            name: name.into(),
            project: Some("blog".into()),
            service: Some(service.into()),
            cpu,
            memory: 1024 * 1024,
            net_rx: Some(10.0),
            ..ContainerUsage::default()
        };
        MetricsResponse {
            at: Some(60),
            interval: 60,
            containers_in_vm: false,
            machine: None,
            projects: vec![ProjectUsage {
                name: "blog".into(),
                containers: 2,
                cpu: Some(1.5),
                memory: 2 * 1024 * 1024,
                net_rx: Some(20.0),
                net_tx: None,
            }],
            containers: vec![
                container("blog-db-1", "db", None),
                container("blog-web-1", "web", Some(1.5)),
            ],
        }
    }

    #[test]
    fn text_shows_the_app_then_its_containers() {
        assert_eq!(
            stats_lines("blog", &blog(), false),
            [
                "blog  2 containers  CPU 1.5%  memory 2.0 MiB  in 20 B/s  out -",
                "",
                "CONTAINER   SERVICE  CPU   MEMORY   NET IN  NET OUT",
                "blog-db-1   db       -     1.0 MiB  10 B/s  -",
                "blog-web-1  web      1.5%  1.0 MiB  10 B/s  -",
            ]
        );
    }

    #[test]
    fn text_for_a_stopped_app_or_no_sample() {
        let stopped = MetricsResponse {
            at: Some(60),
            interval: 60,
            ..MetricsResponse::default()
        };
        assert_eq!(
            stats_lines("blog", &stopped, false),
            ["blog has no running containers"]
        );
        let empty = MetricsResponse::default();
        assert_eq!(
            stats_lines("blog", &empty, false),
            ["No usage yet: alchemyd samples it shortly after it starts"]
        );
    }

    #[test]
    fn json_is_the_response() {
        let lines = stats_lines("blog", &blog(), true);
        assert_eq!(lines.len(), 1);
        let back: MetricsResponse = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(back, blog());
    }

    #[test]
    fn the_name_must_be_an_app_name() {
        let err = Cli::try_parse_from(["alchemy", "app", "stats", "Not/OK"]).unwrap_err();
        assert!(err.to_string().contains("app name must start"), "{err}");
        let err = Cli::try_parse_from(["alchemy", "app", "stats"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[tokio::test]
    async fn run_asks_for_the_app() {
        let name = StackName::new("blog").unwrap();
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, &path(&name), &blog());
        let mut ctx = Context::buffered(fake, false, NAME);
        run(&mut ctx, Args { name }).await.unwrap();
        assert!(
            ctx.written().starts_with("blog  2 containers"),
            "{}",
            ctx.written()
        );
        assert_eq!(
            ctx.daemon().requests(),
            ["GET /api/version", "GET /api/metrics?app=blog"]
        );
    }

    #[tokio::test]
    async fn run_names_the_app_even_when_its_project_differs() {
        let mut renamed = blog();
        renamed.projects[0].name = "my-blog".into();
        let name = StackName::new("blog").unwrap();
        let nope = StackName::new("nope").unwrap();
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, &path(&name), &renamed)
            .error(
                Method::GET,
                &path(&nope),
                404,
                "app_not_found",
                "no app is named \"nope\"",
            );
        let mut ctx = Context::buffered(fake, false, NAME);
        run(&mut ctx, Args { name }).await.unwrap();
        assert!(
            ctx.written().starts_with("blog  2 containers"),
            "{}",
            ctx.written()
        );
        let err = run(&mut ctx, Args { name: nope }).await.unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.to_string(), "alchemyd: no app is named \"nope\"");
        assert_eq!(
            ctx.daemon().requests(),
            [
                "GET /api/version",
                "GET /api/metrics?app=blog",
                "GET /api/metrics?app=nope"
            ]
        );
    }
}
