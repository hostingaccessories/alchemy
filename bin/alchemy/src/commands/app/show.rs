//! `alchemy app show NAME`: one app in detail.

use std::io::Write;

use alchemy_api::apps::{App, Health, ImageSource};

use super::{address_status, app_status, columns, path_segment, service_status};
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
/// its web addresses, its services, and its problems. With `json`, the
/// [`App`] as one line.
pub(crate) fn show_lines(app: &App, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::json!(app).to_string()];
    }
    let images = match app.images {
        Some(ImageSource::Build) => "built from the app's files",
        Some(ImageSource::Registry) => "downloaded",
        None => "unknown",
    };
    let mut lines = vec![format!(
        "{}: {}",
        app.name,
        app_status(app.status, app.stop_reason)
    )];
    lines.extend(columns(&[
        vec!["Folder:".into(), app.dir.clone()],
        vec!["Compose file:".into(), app.compose_file.clone()],
        vec![
            "Project:".into(),
            app.project.clone().unwrap_or_else(|| "unknown".into()),
        ],
        vec!["Images:".into(), images.into()],
    ]));
    if !app.addresses.is_empty() {
        lines.push("Addresses:".into());
        let rows: Vec<Vec<String>> = app
            .addresses
            .iter()
            .map(|address| vec![address.url.clone(), address_status(address)])
            .collect();
        lines.extend(columns(&rows).into_iter().map(|row| format!("  {row}")));
    }
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
                    service_status(service),
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
                "Addresses:",
                "  http://192.168.1.20:8100/  not running: Service web isn't running.",
                "Services:",
                "  db   running  blog-db-1 (healthy)",
                "  web  stopped  blog-web-1",
            ]
        );
    }

    #[test]
    fn shows_addresses_and_why_they_do_not_answer() {
        use alchemy_api::apps::{AddressKind, AddressState, AppAddress};
        let mut app = blog();
        let address = |kind, url: &str, state, message: Option<&str>| AppAddress {
            kind,
            url: url.into(),
            state,
            service: "web".into(),
            port: 80,
            message: message.map(Into::into),
        };
        app.addresses = vec![
            address(
                AddressKind::Port,
                "http://192.168.1.20:8100/",
                AddressState::Ready,
                None,
            ),
            address(
                AddressKind::Host,
                "http://blog.localhost/",
                AddressState::Unavailable,
                Some("Alchemy doesn't have permission to listen on port 80."),
            ),
        ];
        assert_eq!(
            show_lines(&app, false)[5..8],
            [
                "Addresses:",
                "  http://192.168.1.20:8100/  ready",
                "  http://blog.localhost/     unavailable: Alchemy doesn't have permission to listen on port 80.",
            ]
        );
    }

    #[test]
    fn shows_local_addresses_best_first_and_a_name_conflict() {
        use alchemy_api::apps::{AddressKind, AddressState, AppAddress};
        let mut app = blog();
        let in_use = "Another device on your network uses blog.local.";
        let address = |kind, url: &str, state, message: Option<&str>| AppAddress {
            kind,
            url: url.into(),
            state,
            service: "web".into(),
            port: 80,
            message: message.map(Into::into),
        };
        app.addresses = vec![
            address(
                AddressKind::Port,
                "http://192.168.1.20:8100/",
                AddressState::Ready,
                None,
            ),
            address(
                AddressKind::Host,
                "http://blog.localhost/",
                AddressState::Ready,
                None,
            ),
            address(
                AddressKind::Host,
                "http://blog.local/",
                AddressState::Unavailable,
                Some(in_use),
            ),
            address(
                AddressKind::Port,
                "http://blog.local:8100/",
                AddressState::Pending,
                Some("Alchemy is announcing blog.local on your network."),
            ),
        ];
        app.problems = vec![AppProblem {
            kind: AppProblemKind::Route,
            message: format!("{in_use} Set a hostname in x-alchemy."),
        }];
        let lines = show_lines(&app, false);
        assert_eq!(
            lines[5..10],
            [
                "Addresses:",
                "  http://192.168.1.20:8100/  ready",
                "  http://blog.localhost/     ready",
                "  http://blog.local/         unavailable: Another device on your network uses blog.local.",
                "  http://blog.local:8100/    starting: Alchemy is announcing blog.local on your network.",
            ]
        );
        assert_eq!(
            lines[lines.len() - 2..],
            [
                "Problems:",
                "  Another device on your network uses blog.local. Set a hostname in x-alchemy.",
            ]
        );
    }

    #[test]
    fn says_why_services_are_not_running() {
        use alchemy_api::apps::StopReason;
        let mut app = blog();
        app.addresses.clear();
        let crashed = StopReason::Crashed {
            exit_code: Some(137),
            oom: true,
        };
        app.stop_reason = crashed;
        app.services[1].containers[0].stop_reason = crashed;
        let lines = show_lines(&app, false);
        assert_eq!(lines[0], "blog: partly running, crashed (out of memory)");
        assert_eq!(lines[7], "  web  crashed (out of memory)  blog-web-1");
        app.services[1].containers[0].stop_reason = StopReason::Finished;
        assert_eq!(show_lines(&app, false)[7], "  web  finished  blog-web-1");
        app.services[1].containers.clear();
        assert_eq!(show_lines(&app, false)[7], "  web  not running");
    }

    #[test]
    fn shows_problems_and_what_is_unknown() {
        let mut app = blog();
        app.status = StackStatus::Unknown;
        app.project = None;
        app.images = None;
        app.services = vec![];
        app.addresses = vec![];
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
