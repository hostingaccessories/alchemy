//! `alchemy app list`: every app and its status.

use std::io::Write;

use alchemy_api::apps::{AppDirState, AppList};

use super::{app_status, columns, is_running, unmanaged_status};
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app list";

/// The route it reads.
const PATH: &str = "/api/apps";

/// Arguments to `alchemy app list`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {}

/// Run `alchemy app list`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    _args: Args,
) -> Result<(), ClientError> {
    let apps = ctx.get::<AppList>(PATH).await?;
    for line in list_lines(&apps, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// What `alchemy app list` prints: a table of apps, then the Compose
/// projects Alchemy doesn't manage, then anything worth a look, such as
/// folders that were skipped. With `json`, the [`AppList`] as one line.
pub(crate) fn list_lines(apps: &AppList, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::json!(apps).to_string()];
    }
    let mut lines = Vec::new();
    if apps.apps.is_empty() {
        lines.push(empty_line(apps));
    } else {
        let mut rows = vec![vec![
            "NAME".to_owned(),
            "STATUS".to_owned(),
            "SERVICES".to_owned(),
            "FOLDER".to_owned(),
        ]];
        rows.extend(apps.apps.iter().map(|app| {
            let services = if app.project.is_some() {
                let running = app.services.iter().filter(|s| is_running(s)).count();
                format!("{running}/{}", app.services.len())
            } else {
                "-".to_owned()
            };
            vec![
                app.name.to_string(),
                app_status(app.status, app.stop_reason),
                services,
                app.dir.clone(),
            ]
        }));
        lines.extend(columns(&rows));
    }

    if !apps.unmanaged.is_empty() {
        lines.push(String::new());
        lines.push("Not managed by Alchemy:".to_owned());
        let rows: Vec<Vec<String>> = apps
            .unmanaged
            .iter()
            .map(|project| {
                vec![
                    project.project.clone(),
                    unmanaged_status(project.status, project.stop_reason),
                    project.dir.clone().unwrap_or_default(),
                ]
            })
            .collect();
        lines.extend(columns(&rows).into_iter().map(|row| format!("  {row}")));
    }

    let notes = notes(apps);
    if !notes.is_empty() {
        lines.push(String::new());
        lines.extend(notes);
    }
    lines
}

fn empty_line(apps: &AppList) -> String {
    match apps.dirs.iter().find(|dir| dir.home) {
        Some(home) => format!(
            "No apps yet. Alchemy looks for apps in {}; each app is a folder with a compose file.",
            home.path
        ),
        None => {
            "No apps yet. Choose a home folder with `alchemy settings set app-home`.".to_owned()
        }
    }
}

/// Problems with apps, unusable folders, and skipped folders, one line
/// each. A missing home is normal before the first app, so it isn't one.
fn notes(apps: &AppList) -> Vec<String> {
    let mut notes = Vec::new();
    for app in &apps.apps {
        for problem in &app.problems {
            notes.push(format!("{}: {}", app.name, problem.message));
        }
    }
    for dir in &apps.dirs {
        let normal =
            dir.state == AppDirState::Ok || (dir.home && dir.state == AppDirState::Missing);
        if !normal {
            let message = dir
                .message
                .as_deref()
                .unwrap_or("Alchemy can't use this folder.");
            notes.push(format!("{}: {message}", dir.path));
        }
        for skipped in &dir.skipped {
            notes.push(format!("{}: {}", skipped.path, skipped.message));
        }
    }
    notes
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
pub(crate) mod tests {
    use alchemy_api::apps::{
        App, AppContainer, AppDir, AppProblem, AppProblemKind, AppService, ContainerState,
        ImageSource, SkipReason, SkippedApp, StackName, StackStatus, StopReason, UnmanagedApp,
    };

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    fn service(name: &str, state: Option<ContainerState>) -> AppService {
        AppService {
            name: name.into(),
            containers: state
                .map(|state| AppContainer {
                    id: format!("{name}1"),
                    name: format!("blog-{name}-1"),
                    image: Some("nginx".into()),
                    state,
                    health: None,
                    stop_reason: if state == ContainerState::Running {
                        StopReason::Running
                    } else {
                        StopReason::Unknown
                    },
                })
                .into_iter()
                .collect(),
        }
    }

    pub(crate) fn blog() -> App {
        App {
            name: StackName::new("blog").unwrap(),
            status: StackStatus::Partial,
            dir: "/srv/apps/blog".into(),
            root: "/srv/apps".into(),
            compose_file: "compose.yaml".into(),
            project: Some("blog".into()),
            images: Some(ImageSource::Registry),
            services: vec![
                service("db", Some(ContainerState::Running)),
                service("web", Some(ContainerState::Exited)),
            ],
            problems: vec![],
            stop_reason: StopReason::Unknown,
            addresses: vec![],
        }
    }

    fn home(state: AppDirState) -> AppDir {
        AppDir {
            path: "/srv/apps".into(),
            home: true,
            state,
            message: (state != AppDirState::Ok).then(|| "This folder doesn't exist yet.".into()),
            skipped: vec![],
        }
    }

    pub(crate) fn full_list() -> AppList {
        let mut broken = blog();
        broken.name = StackName::new("wiki").unwrap();
        broken.dir = "/opt/stacks/wiki".into();
        broken.status = StackStatus::Unknown;
        broken.project = None;
        broken.images = None;
        broken.services = vec![];
        broken.problems = vec![AppProblem {
            kind: AppProblemKind::ComposeError,
            message: "Compose can't read compose.yaml: yaml: line 4".into(),
        }];
        AppList {
            apps: vec![blog(), broken],
            unmanaged: vec![
                UnmanagedApp {
                    project: "legacy".into(),
                    dir: Some("/home/me/legacy".into()),
                    status: StackStatus::Running,
                    services: vec![],
                    stop_reason: StopReason::Running,
                },
                UnmanagedApp {
                    project: "worker".into(),
                    dir: None,
                    status: StackStatus::Stopped,
                    services: vec![],
                    stop_reason: StopReason::Crashed {
                        exit_code: Some(1),
                        oom: false,
                    },
                },
                UnmanagedApp {
                    project: "tools".into(),
                    dir: Some("/home/me/tools".into()),
                    status: StackStatus::Stopped,
                    services: vec![],
                    stop_reason: StopReason::StoppedOutside,
                },
            ],
            dirs: vec![
                home(AppDirState::Missing),
                AppDir {
                    path: "/opt/stacks".into(),
                    home: false,
                    state: AppDirState::Ok,
                    message: None,
                    skipped: vec![SkippedApp {
                        name: "My App".into(),
                        path: "/opt/stacks/My App".into(),
                        reason: SkipReason::InvalidName,
                        message: "\"My App\" can't be an app name.".into(),
                    }],
                },
                AppDir {
                    path: "/gone".into(),
                    home: false,
                    state: AppDirState::Missing,
                    message: Some("This folder doesn't exist.".into()),
                    skipped: vec![],
                },
            ],
        }
    }

    #[test]
    fn lists_apps_unmanaged_projects_and_notes() {
        assert_eq!(
            list_lines(&full_list(), false),
            [
                "NAME  STATUS          SERVICES  FOLDER",
                "blog  partly running  1/2       /srv/apps/blog",
                "wiki  unknown         -         /opt/stacks/wiki",
                "",
                "Not managed by Alchemy:",
                "  legacy  running                         /home/me/legacy",
                "  worker  stopped, crashed (exit code 1)",
                "  tools   stopped                         /home/me/tools",
                "",
                "wiki: Compose can't read compose.yaml: yaml: line 4",
                "/opt/stacks/My App: \"My App\" can't be an app name.",
                "/gone: This folder doesn't exist.",
            ]
        );
    }

    #[test]
    fn says_where_to_put_the_first_app() {
        let empty = AppList {
            dirs: vec![home(AppDirState::Missing)],
            ..AppList::default()
        };
        assert_eq!(
            list_lines(&empty, false),
            [
                "No apps yet. Alchemy looks for apps in /srv/apps; each app is a folder with a compose file."
            ]
        );
        assert_eq!(
            list_lines(&AppList::default(), false),
            ["No apps yet. Choose a home folder with `alchemy settings set app-home`."]
        );
    }

    #[test]
    fn json_is_the_app_list() {
        let lines = list_lines(&full_list(), true);
        assert_eq!(lines.len(), 1);
        let back: AppList = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(back, full_list());
    }

    #[tokio::test]
    async fn reads_the_list_from_alchemyd() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &full_list());
        let mut ctx = Context::buffered(fake, false, NAME);
        run(&mut ctx, Args::default()).await.unwrap();
        assert!(
            ctx.written().starts_with("NAME  STATUS"),
            "{}",
            ctx.written()
        );
        assert_eq!(
            ctx.daemon().requests(),
            ["GET /api/version", "GET /api/apps"]
        );
    }

    #[tokio::test]
    async fn an_older_alchemyd_is_too_old() {
        let fake = FakeDaemon::new().version("0.1.0", 1).error(
            Method::GET,
            PATH,
            404,
            "not_found",
            "not found",
        );
        let mut ctx = Context::buffered(fake, true, NAME);
        let err = run(&mut ctx, Args::default()).await.unwrap_err();
        assert_eq!(err.kind(), "daemon_too_old", "{err}");
        assert_eq!(err.to_string(), "alchemyd 0.1.0 is too old for `app list`");
        assert_eq!(ctx.written(), "");
    }
}
