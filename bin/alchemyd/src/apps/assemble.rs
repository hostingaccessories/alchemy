//! Building the [`AppList`] clients see from what the daemon knows: the
//! discovered stacks, what Compose said about each, and the containers.
//!
//! Pure: every input is a value, and the same inputs give the same list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use alchemy_api::apps::{
    App, AppContainer, AppDir, AppDirState, AppList, AppProblem, AppProblemKind, AppService,
    SkipReason as WireSkip, SkippedApp, UnmanagedApp,
};
use alchemy_core::StackNameError;
use alchemy_core::engine::{ContainerState, ContainerStatus, Containers};
use alchemy_core::stack::{
    DirReport, DirState, Discovery, FoundStack, SkipReason, Skipped, StackStatus, stack_status,
};

use crate::compose::{ComposeConfig, ConfigError};

/// What reading one stack's configuration came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigOutcome {
    /// Compose resolved it.
    Ready(ComposeConfig),
    /// It could not be read; `message` says why, for people.
    Failed {
        /// Whether Compose rejected the files or could not run.
        kind: AppProblemKind,
        /// What is wrong.
        message: String,
    },
}

impl ConfigOutcome {
    /// The outcome of reading `compose_file`'s stack.
    pub fn from_result(result: Result<ComposeConfig, ConfigError>, compose_file: &str) -> Self {
        match result {
            Ok(config) => Self::Ready(config),
            Err(ConfigError::Invalid { message, .. }) => Self::Failed {
                kind: AppProblemKind::ComposeError,
                message: format!("Compose can't read {compose_file}: {message}"),
            },
            Err(err) => Self::Failed {
                kind: AppProblemKind::ComposeUnavailable,
                message: format!("Alchemy couldn't run Docker Compose: {err}"),
            },
        }
    }

    /// Whether reading again could give a different answer without the
    /// files changing.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                kind: AppProblemKind::ComposeUnavailable,
                ..
            }
        )
    }
}

/// Everything [`app_list`] combines.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The stacks found and how each folder was read.
    pub discovery: &'a Discovery,
    /// The home folder, to mark it in [`AppList::dirs`].
    pub home: Option<&'a Path>,
    /// What Compose said about each stack, by stack directory. A stack
    /// with no entry has not been read yet.
    pub configs: &'a BTreeMap<PathBuf, ConfigOutcome>,
    /// Every container the engine reported.
    pub containers: &'a Containers,
    /// Whether the engine is ready, so the containers are current.
    pub engine_ready: bool,
}

/// The app list for `inputs`.
pub fn app_list(inputs: &Inputs<'_>) -> AppList {
    let by_project = by_project(inputs.containers);
    let mut apps: Vec<App> = inputs
        .discovery
        .stacks
        .iter()
        .map(|stack| app(stack, inputs, &by_project))
        .collect();
    mark_shared_projects(&mut apps);

    let managed: BTreeSet<&str> = apps.iter().filter_map(|a| a.project.as_deref()).collect();
    let unmanaged = by_project
        .iter()
        .filter(|(project, _)| !managed.contains(project.as_str()))
        .map(|(project, containers)| unmanaged(project, containers, inputs.engine_ready))
        .collect();

    AppList {
        apps,
        unmanaged,
        dirs: inputs
            .discovery
            .dirs
            .iter()
            .map(|report| app_dir(report, inputs.home))
            .collect(),
    }
}

fn by_project(containers: &Containers) -> BTreeMap<String, Vec<&ContainerStatus>> {
    let mut grouped: BTreeMap<String, Vec<&ContainerStatus>> = BTreeMap::new();
    for container in containers.iter() {
        if let Some(project) = &container.project {
            grouped.entry(project.clone()).or_default().push(container);
        }
    }
    grouped
}

fn app(
    stack: &FoundStack,
    inputs: &Inputs<'_>,
    by_project: &BTreeMap<String, Vec<&ContainerStatus>>,
) -> App {
    let mut app = App {
        name: stack.name.clone(),
        status: StackStatus::Unknown,
        dir: display(&stack.dir),
        root: display(&stack.root),
        compose_file: stack.compose_file.to_owned(),
        project: None,
        images: None,
        services: Vec::new(),
        problems: Vec::new(),
    };
    match inputs.configs.get(&stack.dir) {
        Some(ConfigOutcome::Ready(config)) => {
            let containers = by_project.get(&config.name).map_or(&[][..], Vec::as_slice);
            let expected: Vec<&str> = config.services.iter().map(|s| s.name.as_str()).collect();
            app.services = services(&expected, containers);
            app.status = status(&app.services, expected.len(), inputs.engine_ready);
            app.project = Some(config.name.clone());
            app.images = Some(config.images());
        }
        Some(ConfigOutcome::Failed { kind, message }) => app.problems.push(AppProblem {
            kind: *kind,
            message: message.clone(),
        }),
        None => {}
    }
    app
}

/// The expected services in order, then any others that have containers,
/// by name; each with its containers by name.
fn services(expected: &[&str], containers: &[&ContainerStatus]) -> Vec<AppService> {
    let mut by_service: BTreeMap<&str, Vec<AppContainer>> = BTreeMap::new();
    for container in containers {
        if let Some(service) = container.service.as_deref() {
            by_service
                .entry(service)
                .or_default()
                .push(app_container(container));
        }
    }
    let mut services: Vec<AppService> = expected
        .iter()
        .map(|name| AppService {
            name: (*name).to_owned(),
            containers: by_service.remove(name).unwrap_or_default(),
        })
        .collect();
    services.extend(by_service.into_iter().map(|(name, containers)| AppService {
        name: name.to_owned(),
        containers,
    }));
    for service in &mut services {
        service.containers.sort_by(|a, b| a.name.cmp(&b.name));
    }
    services
}

fn app_container(container: &ContainerStatus) -> AppContainer {
    AppContainer {
        id: container.id.clone(),
        name: container.name.clone(),
        image: container.image.clone(),
        state: container.state,
        health: container.health,
    }
}

/// The status from the first `counted` services, the ones Compose expects;
/// leftovers from an older version of the compose file do not count.
fn status(services: &[AppService], counted: usize, engine_ready: bool) -> StackStatus {
    if !engine_ready {
        return StackStatus::Unknown;
    }
    stack_status(services.iter().take(counted).map(|service| {
        service
            .containers
            .iter()
            .any(|c| c.state == ContainerState::Running)
    }))
}

fn mark_shared_projects(apps: &mut [App]) {
    let mut by_project: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for app in apps.iter() {
        if let Some(project) = &app.project {
            by_project
                .entry(project.clone())
                .or_default()
                .push(app.name.to_string());
        }
    }
    for app in apps.iter_mut() {
        let Some(project) = &app.project else {
            continue;
        };
        let others: Vec<&str> = by_project
            .get(project)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|name| *name != app.name.as_str())
            .collect();
        if others.is_empty() {
            continue;
        }
        app.problems.push(AppProblem {
            kind: AppProblemKind::SharedProject,
            message: format!(
                "{} also use{} the Compose project name \"{project}\", so they share containers. Give each app its own name: in its compose file.",
                others.join(", "),
                if others.len() == 1 { "s" } else { "" },
            ),
        });
    }
}

fn unmanaged(project: &str, containers: &[&ContainerStatus], engine_ready: bool) -> UnmanagedApp {
    let services = services(&[], containers);
    let counted = services.len();
    UnmanagedApp {
        project: project.to_owned(),
        dir: containers.iter().find_map(|c| c.working_dir.clone()),
        status: status(&services, counted, engine_ready),
        services,
    }
}

fn app_dir(report: &DirReport, home: Option<&Path>) -> AppDir {
    let is_home = home == Some(report.dir.as_path());
    let (state, message) = match &report.state {
        DirState::Ok => (AppDirState::Ok, None),
        DirState::Missing if is_home => (
            AppDirState::Missing,
            Some(
                "This folder doesn't exist yet. Alchemy creates it when you add your first app."
                    .to_owned(),
            ),
        ),
        DirState::Missing => (
            AppDirState::Missing,
            Some("This folder doesn't exist.".to_owned()),
        ),
        DirState::NotADirectory => (
            AppDirState::NotADirectory,
            Some("Something other than a folder is at this path.".to_owned()),
        ),
        DirState::Unreadable(why) => (
            AppDirState::Unreadable,
            Some(format!("Alchemy can't read this folder: {why}")),
        ),
    };
    AppDir {
        path: display(&report.dir),
        home: is_home,
        state,
        message,
        skipped: report.skipped.iter().map(skipped).collect(),
    }
}

fn skipped(skipped: &Skipped) -> SkippedApp {
    let (reason, message) = match &skipped.reason {
        SkipReason::InvalidName(err) => (
            WireSkip::InvalidName,
            format!(
                "\"{}\" can't be an app name: {}. Rename the folder to use it.",
                skipped.name,
                name_rule(err)
            ),
        ),
        SkipReason::Duplicate { kept } => (
            WireSkip::Duplicate,
            format!(
                "Another app named {} is already in {}, so this one is ignored.",
                skipped.name,
                kept.parent().map_or_else(|| display(kept), display),
            ),
        ),
        SkipReason::LinkedComposeFile { file } => (
            WireSkip::LinkedComposeFile,
            format!("{file} is a link. Alchemy ignores linked compose files; put the file itself here."),
        ),
        SkipReason::ComposeFileNotRegular { file } => (
            WireSkip::ComposeFileNotRegular,
            format!("{file} isn't a regular file."),
        ),
        SkipReason::LinkedDirectory => (
            WireSkip::LinkedFolder,
            "This folder is a link. Alchemy only uses folders that are really inside an app folder; add the folder it points to in Settings instead.".to_owned(),
        ),
    };
    SkippedApp {
        name: skipped.name.clone(),
        path: display(&skipped.path),
        reason,
        message,
    }
}

fn name_rule(err: &StackNameError) -> String {
    match err {
        StackNameError::TooLong => "names can be at most 63 characters".to_owned(),
        StackNameError::InvalidChar(c) => {
            format!("names can only use lowercase letters, numbers, - and _, not {c:?}")
        }
        _ => "names must start with a lowercase letter or number".to_owned(),
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_api::apps::{ImageSource, StackName};
    use alchemy_core::engine::Health;
    use alchemy_core::stack::{EntryKind, FileInfo, ListedDir, ListedEntry, Listing, discover};

    use super::*;
    use crate::compose::ServiceConfig;

    fn stack_entry(name: &str) -> ListedEntry {
        ListedEntry {
            name: name.into(),
            kind: EntryKind::Dir,
            files: [(
                "compose.yaml".to_owned(),
                FileInfo {
                    kind: EntryKind::File,
                    modified: None,
                    len: 1,
                },
            )]
            .into(),
        }
    }

    fn discovery(dirs: &[(&str, &[&str])]) -> Discovery {
        discover(
            &dirs
                .iter()
                .map(|(dir, names)| ListedDir {
                    dir: (*dir).into(),
                    listing: Listing::Entries(names.iter().map(|n| stack_entry(n)).collect()),
                })
                .collect::<Vec<_>>(),
        )
    }

    fn config(name: &str, services: &[(&str, bool)]) -> ConfigOutcome {
        ConfigOutcome::Ready(ComposeConfig {
            name: name.to_owned(),
            services: services
                .iter()
                .map(|(s, builds)| ServiceConfig {
                    name: (*s).to_owned(),
                    builds: *builds,
                })
                .collect(),
        })
    }

    fn container(id: &str, project: &str, service: &str, state: ContainerState) -> ContainerStatus {
        let mut c = ContainerStatus::new(id, format!("{project}-{service}-{id}"), state);
        c.project = Some(project.to_owned());
        c.service = Some(service.to_owned());
        c
    }

    fn containers(list: Vec<ContainerStatus>) -> Containers {
        let mut c = Containers::new();
        c.replace_all(list);
        c
    }

    fn list(
        discovery: &Discovery,
        configs: &BTreeMap<PathBuf, ConfigOutcome>,
        containers: &Containers,
        engine_ready: bool,
    ) -> AppList {
        app_list(&Inputs {
            discovery,
            home: Some(Path::new("/srv/apps")),
            configs,
            containers,
            engine_ready,
        })
    }

    use ContainerState::{Exited, Running};

    #[test]
    fn status_counts_the_services_compose_expects() {
        let found = discovery(&[("/srv/apps", &["blog"])]);
        let configs: BTreeMap<PathBuf, ConfigOutcome> = [(
            PathBuf::from("/srv/apps/blog"),
            config("blog", &[("db", false), ("web", false)]),
        )]
        .into();
        let cases: [(&str, Vec<ContainerStatus>, bool, StackStatus); 6] = [
            ("nothing created", vec![], true, StackStatus::Stopped),
            (
                "all running",
                vec![
                    container("1", "blog", "db", Running),
                    container("2", "blog", "web", Running),
                ],
                true,
                StackStatus::Running,
            ),
            (
                "one stopped",
                vec![
                    container("1", "blog", "db", Running),
                    container("2", "blog", "web", Exited),
                ],
                true,
                StackStatus::Partial,
            ),
            (
                "all stopped",
                vec![
                    container("1", "blog", "db", Exited),
                    container("2", "blog", "web", Exited),
                ],
                true,
                StackStatus::Stopped,
            ),
            (
                "orphan running does not count",
                vec![
                    container("1", "blog", "db", Running),
                    container("2", "blog", "web", Running),
                    container("3", "blog", "old", Exited),
                ],
                true,
                StackStatus::Running,
            ),
            (
                "engine not ready",
                vec![container("1", "blog", "db", Running)],
                false,
                StackStatus::Unknown,
            ),
        ];
        for (name, list_of, ready, want) in cases {
            let got = list(&found, &configs, &containers(list_of), ready);
            assert_eq!(got.apps[0].status, want, "{name}");
            assert_eq!(got.apps[0].project.as_deref(), Some("blog"), "{name}");
            assert!(got.unmanaged.is_empty(), "{name}");
        }
    }

    #[test]
    fn apps_carry_services_containers_and_image_source() {
        let found = discovery(&[("/srv/apps", &["blog"])]);
        let configs: BTreeMap<PathBuf, ConfigOutcome> = [(
            PathBuf::from("/srv/apps/blog"),
            config("fancy", &[("api", true), ("db", false)]),
        )]
        .into();
        let mut db = container("1", "fancy", "db", Running);
        db.image = Some("postgres:16".into());
        db.health = Some(Health::Healthy);
        let got = list(
            &found,
            &configs,
            &containers(vec![
                db,
                container("2", "fancy", "old", Exited),
                container("3", "blog", "web", Running),
            ]),
            true,
        );
        let app = &got.apps[0];
        assert_eq!(app.name, StackName::new("blog").unwrap());
        assert_eq!(app.dir, "/srv/apps/blog");
        assert_eq!(app.root, "/srv/apps");
        assert_eq!(app.compose_file, "compose.yaml");
        assert_eq!(app.project.as_deref(), Some("fancy"));
        assert_eq!(app.images, Some(ImageSource::Build));
        assert_eq!(app.status, StackStatus::Partial);
        let services: Vec<_> = app
            .services
            .iter()
            .map(|s| (s.name.as_str(), s.containers.len()))
            .collect();
        assert_eq!(services, [("api", 0), ("db", 1), ("old", 1)]);
        assert_eq!(
            app.services[1].containers[0],
            AppContainer {
                id: "1".into(),
                name: "fancy-db-1".into(),
                image: Some("postgres:16".into()),
                state: Running,
                health: Some(Health::Healthy),
            }
        );
        let unmanaged: Vec<_> = got.unmanaged.iter().map(|u| u.project.as_str()).collect();
        assert_eq!(unmanaged, ["blog"], "the directory name is not the project");
    }

    #[test]
    fn unread_and_failed_configs_are_unknown() {
        let found = discovery(&[("/srv/apps", &["bad", "new", "nodocker"])]);
        let configs: BTreeMap<PathBuf, ConfigOutcome> = [
            (
                PathBuf::from("/srv/apps/bad"),
                ConfigOutcome::from_result(
                    Err(ConfigError::Invalid {
                        code: Some(1),
                        message: "yaml: line 4: oops".into(),
                    }),
                    "compose.yaml",
                ),
            ),
            (
                PathBuf::from("/srv/apps/nodocker"),
                ConfigOutcome::from_result(Err(ConfigError::NoDocker), "compose.yaml"),
            ),
        ]
        .into();
        let got = list(
            &found,
            &configs,
            &containers(vec![container("1", "bad", "web", Running)]),
            true,
        );
        let summary: Vec<_> = got
            .apps
            .iter()
            .map(|a| {
                (
                    a.name.as_str(),
                    a.status,
                    a.project.clone(),
                    a.images,
                    a.problems.iter().map(|p| p.kind).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "bad",
                    StackStatus::Unknown,
                    None,
                    None,
                    vec![AppProblemKind::ComposeError]
                ),
                ("new", StackStatus::Unknown, None, None, vec![]),
                (
                    "nodocker",
                    StackStatus::Unknown,
                    None,
                    None,
                    vec![AppProblemKind::ComposeUnavailable]
                ),
            ]
        );
        assert_eq!(
            got.apps[0].problems[0].message,
            "Compose can't read compose.yaml: yaml: line 4: oops"
        );
        assert_eq!(
            got.apps[2].problems[0].message,
            "Alchemy couldn't run Docker Compose: the docker command wasn't found"
        );
        assert_eq!(got.unmanaged.len(), 1, "unmatched containers stay visible");
    }

    #[test]
    fn transient_failures_are_retried() {
        let invalid = ConfigOutcome::from_result(
            Err(ConfigError::Invalid {
                code: Some(1),
                message: "x".into(),
            }),
            "compose.yaml",
        );
        assert!(!invalid.is_transient());
        assert!(
            ConfigOutcome::from_result(Err(ConfigError::NoDocker), "compose.yaml").is_transient()
        );
        assert!(!config("a", &[]).is_transient());
    }

    #[test]
    fn unmanaged_projects_are_grouped_with_their_folder() {
        let found = discovery(&[("/srv/apps", &[])]);
        let mut legacy = container("1", "legacy", "web", Running);
        legacy.working_dir = Some("/home/me/legacy".into());
        let mut plain = ContainerStatus::new("9", "standalone", Running);
        plain.image = Some("redis".into());
        let got = list(
            &found,
            &BTreeMap::new(),
            &containers(vec![legacy, container("2", "legacy", "db", Exited), plain]),
            true,
        );
        assert_eq!(got.apps.len(), 0);
        assert_eq!(got.unmanaged.len(), 1);
        let legacy = &got.unmanaged[0];
        assert_eq!(legacy.project, "legacy");
        assert_eq!(legacy.dir.as_deref(), Some("/home/me/legacy"));
        assert_eq!(legacy.status, StackStatus::Partial);
        let services: Vec<_> = legacy.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(services, ["db", "web"]);
    }

    #[test]
    fn apps_sharing_a_project_are_flagged() {
        let found = discovery(&[("/srv/apps", &["a", "b", "c"])]);
        let configs: BTreeMap<PathBuf, ConfigOutcome> = [
            (
                PathBuf::from("/srv/apps/a"),
                config("shared", &[("web", false)]),
            ),
            (
                PathBuf::from("/srv/apps/b"),
                config("shared", &[("web", false)]),
            ),
            (PathBuf::from("/srv/apps/c"), config("c", &[("web", false)])),
        ]
        .into();
        let got = list(&found, &configs, &Containers::new(), true);
        let problems: Vec<_> = got
            .apps
            .iter()
            .map(|a| {
                a.problems
                    .iter()
                    .map(|p| p.message.as_str())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            problems,
            [
                vec![
                    "b also uses the Compose project name \"shared\", so they share containers. Give each app its own name: in its compose file."
                ],
                vec![
                    "a also uses the Compose project name \"shared\", so they share containers. Give each app its own name: in its compose file."
                ],
                vec![],
            ]
        );
    }

    #[test]
    fn dirs_report_state_and_skips_in_plain_words() {
        let found = discover(&[
            ListedDir {
                dir: "/srv/apps".into(),
                listing: Listing::Missing,
            },
            ListedDir {
                dir: "/opt/stacks".into(),
                listing: Listing::Entries(vec![
                    stack_entry("My App"),
                    stack_entry("x.y"),
                    ListedEntry {
                        name: "alias".into(),
                        kind: EntryKind::LinkToDir,
                        files: BTreeMap::new(),
                    },
                ]),
            },
            ListedDir {
                dir: "/gone".into(),
                listing: Listing::Missing,
            },
            ListedDir {
                dir: "/locked".into(),
                listing: Listing::Unreadable("Permission denied (os error 13)".into()),
            },
        ]);
        let got = list(&found, &BTreeMap::new(), &Containers::new(), true);
        let dirs: Vec<_> = got
            .dirs
            .iter()
            .map(|d| (d.path.as_str(), d.home, d.state, d.message.as_deref()))
            .collect();
        assert_eq!(
            dirs,
            [
                (
                    "/srv/apps",
                    true,
                    AppDirState::Missing,
                    Some(
                        "This folder doesn't exist yet. Alchemy creates it when you add your first app."
                    )
                ),
                ("/opt/stacks", false, AppDirState::Ok, None),
                (
                    "/gone",
                    false,
                    AppDirState::Missing,
                    Some("This folder doesn't exist.")
                ),
                (
                    "/locked",
                    false,
                    AppDirState::Unreadable,
                    Some("Alchemy can't read this folder: Permission denied (os error 13)")
                ),
            ]
        );
        let skipped: Vec<_> = got.dirs[1]
            .skipped
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.path.as_str(),
                    s.reason,
                    s.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            skipped,
            [
                (
                    "My App",
                    "/opt/stacks/My App",
                    WireSkip::InvalidName,
                    "\"My App\" can't be an app name: names must start with a lowercase letter or number. Rename the folder to use it."
                ),
                (
                    "alias",
                    "/opt/stacks/alias",
                    WireSkip::LinkedFolder,
                    "This folder is a link. Alchemy only uses folders that are really inside an app folder; add the folder it points to in Settings instead."
                ),
                (
                    "x.y",
                    "/opt/stacks/x.y",
                    WireSkip::InvalidName,
                    "\"x.y\" can't be an app name: names can only use lowercase letters, numbers, - and _, not '.'. Rename the folder to use it."
                ),
            ]
        );
    }

    #[test]
    fn skip_messages_name_the_duplicate_and_the_file() {
        let found = discovery(&[("/srv/apps", &["web"]), ("/opt/stacks", &["web"])]);
        let got = list(&found, &BTreeMap::new(), &Containers::new(), true);
        assert_eq!(
            got.dirs[1].skipped[0].message,
            "Another app named web is already in /srv/apps, so this one is ignored."
        );
        let linked = skipped(&Skipped {
            name: "web".into(),
            path: "/srv/apps/web".into(),
            reason: SkipReason::LinkedComposeFile {
                file: "compose.yaml",
            },
        });
        assert_eq!(linked.reason, WireSkip::LinkedComposeFile);
        assert_eq!(
            linked.message,
            "compose.yaml is a link. Alchemy ignores linked compose files; put the file itself here."
        );
        let odd = skipped(&Skipped {
            name: "web".into(),
            path: "/srv/apps/web".into(),
            reason: SkipReason::ComposeFileNotRegular {
                file: "compose.yml",
            },
        });
        assert_eq!(odd.message, "compose.yml isn't a regular file.");
        assert_eq!(
            name_rule(&StackNameError::TooLong),
            "names can be at most 63 characters"
        );
    }
}
