//! `alchemy app show NAME`: one app in detail.

use std::io::Write;

use alchemy_api::apps::{App, Health, ImageSource};

use super::{columns, is_running, path_segment, status_word};
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app show";

/// Arguments to `alchemy app show`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The app's name: its folder's name.
    pub name: String,
}

/// Run `alchemy app show`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    let path = format!("/api/apps/{}", path_segment(&args.name));
    let app = ctx.get::<App>(&path).await?;
    for line in show_lines(&app, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// What `alchemy app show` prints: the app's status and where it lives,
/// its services, and its problems. With `json`, the [`App`] as one line.
pub(crate) fn show_lines(app: &App, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::json!(app).to_string()];
    }
    let images = match app.images {
        Some(ImageSource::Build) => "built from the app's files",
        Some(ImageSource::Registry) => "downloaded",
        None => "unknown",
    };
    let mut lines = vec![format!("{}: {}", app.name, status_word(app.status))];
    lines.extend(columns(&[
        vec!["Folder:".into(), app.dir.clone()],
        vec!["Compose file:".into(), app.compose_file.clone()],
        vec![
            "Project:".into(),
            app.project.clone().unwrap_or_else(|| "unknown".into()),
        ],
        vec!["Images:".into(), images.into()],
    ]));
    if !app.services.is_empty() {
        lines.push("Services:".into());
        let rows: Vec<Vec<String>> = app
            .services
            .iter()
            .map(|service| {
                let containers: Vec<String> = service
                    .containers
                    .iter()
                    .map(|c| match c.health {
                        Some(Health::Healthy) => format!("{} (healthy)", c.name),
                        Some(Health::Unhealthy) => format!("{} (unhealthy)", c.name),
                        Some(Health::Starting) => format!("{} (starting)", c.name),
                        _ => c.name.clone(),
                    })
                    .collect();
                vec![
                    service.name.clone(),
                    if is_running(service) {
                        "running"
                    } else {
                        "stopped"
                    }
                    .into(),
                    containers.join(", "),
                ]
            })
            .collect();
        lines.extend(columns(&rows).into_iter().map(|row| format!("  {row}")));
    }
    if !app.problems.is_empty() {
        lines.push("Problems:".into());
        lines.extend(app.problems.iter().map(|p| format!("  {}", p.message)));
    }
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
    use alchemy_api::apps::{AppProblem, AppProblemKind, StackStatus};

    use super::*;
    use crate::commands::app::list::tests::blog;
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    #[test]
    fn shows_where_the_app_lives_and_its_services() {
        let mut app = blog();
        app.services[0].containers[0].health = Some(Health::Healthy);
        assert_eq!(
            show_lines(&app, false),
            [
                "blog: partly running",
                "Folder:        /srv/apps/blog",
                "Compose file:  compose.yaml",
                "Project:       blog",
                "Images:        downloaded",
                "Services:",
                "  db   running  blog-db-1 (healthy)",
                "  web  stopped  blog-web-1",
            ]
        );
    }

    #[test]
    fn shows_problems_and_what_is_unknown() {
        let mut app = blog();
        app.status = StackStatus::Unknown;
        app.project = None;
        app.images = None;
        app.services = vec![];
        app.problems = vec![AppProblem {
            kind: AppProblemKind::ComposeUnavailable,
            message: "Alchemy couldn't run Docker Compose".into(),
        }];
        assert_eq!(
            show_lines(&app, false),
            [
                "blog: unknown",
                "Folder:        /srv/apps/blog",
                "Compose file:  compose.yaml",
                "Project:       unknown",
                "Images:        unknown",
                "Problems:",
                "  Alchemy couldn't run Docker Compose",
            ]
        );
    }

    #[test]
    fn json_is_the_app() {
        let lines = show_lines(&blog(), true);
        assert_eq!(lines.len(), 1);
        assert_eq!(serde_json::from_str::<App>(&lines[0]).unwrap(), blog());
    }

    #[tokio::test]
    async fn asks_for_the_app_by_its_encoded_name() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, "/api/apps/blog", &blog())
            .error(
                Method::GET,
                "/api/apps/My%20App",
                404,
                "app_not_found",
                "no app is named \"My App\"",
            );
        let mut ctx = Context::buffered(fake, false, NAME);
        run(
            &mut ctx,
            Args {
                name: "blog".into(),
            },
        )
        .await
        .unwrap();
        assert!(ctx.written().starts_with("blog: partly running\n"));
        let err = run(
            &mut ctx,
            Args {
                name: "My App".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.to_string(), "alchemyd: no app is named \"My App\"");
        assert_eq!(
            ctx.daemon().requests(),
            [
                "GET /api/version",
                "GET /api/apps/blog",
                "GET /api/apps/My%20App"
            ]
        );
    }
}
