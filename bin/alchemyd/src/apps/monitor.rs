//! The task that keeps the app list in [`State`] current.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alchemy_api::apps::AppProblemKind;
use alchemy_core::engine::EngineState;
use alchemy_core::stack::{
    Discovery, FileInfo, FoundStack, ListedDir, StackDirs, default_stack_home, discover,
    is_stale_staging,
};
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use super::assemble::{ConfigOutcome, Inputs, app_list};
use super::disk::{AppDisk, SystemDisk};
use super::fs::{AppFs, SystemFs};
use super::settings::{SETTINGS_KEY, StoredDirs, effective};
use crate::State;
use crate::compose::{CliComposeConfig, ComposeConfigReader, ComposeProject};
use crate::engine::{Endpoint, HostEnv};
use crate::store::SharedStore;

/// How often the app folders are listed again by default.
///
/// Listing is cheap: one directory read per folder and a few `lstat`s per
/// app. Compose only runs for an app whose watched files changed.
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// How many `docker compose config` reads run at once.
const MAX_READS: usize = 4;

/// How long before an app whose read failed for a reason other than its
/// files, such as Docker missing, is read again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// What the apps task reads the app folders and compose files through.
#[derive(Debug, Clone)]
pub struct AppsDeps {
    /// Lists the app folders.
    pub fs: Arc<dyn AppFs>,
    /// Reads each app's resolved configuration.
    pub config: Arc<dyn ComposeConfigReader>,
    /// The home used when the settings choose none, usually
    /// `~/Alchemy/apps`. With neither, no folder is searched.
    pub default_home: Option<PathBuf>,
    /// How often to list the folders again.
    pub scan_interval: Duration,
    /// Removes staging folders a stopped daemon left behind. With none,
    /// they are left alone.
    pub disk: Option<Arc<dyn AppDisk>>,
}

impl AppsDeps {
    /// The real filesystem and `docker compose`, with `~/Alchemy/apps` as
    /// the default home, removing staging folders left behind.
    pub fn system() -> Self {
        Self::new(
            Arc::new(SystemFs),
            Arc::new(CliComposeConfig::system()),
            default_stack_home(HostEnv::current().home.as_deref()),
        )
        .with_disk(Arc::new(SystemDisk))
    }

    /// These effects and default home, listed every
    /// [`DEFAULT_SCAN_INTERVAL`], leaving staging folders alone.
    pub fn new(
        fs: Arc<dyn AppFs>,
        config: Arc<dyn ComposeConfigReader>,
        default_home: Option<PathBuf>,
    ) -> Self {
        Self {
            fs,
            config,
            default_home,
            scan_interval: DEFAULT_SCAN_INTERVAL,
            disk: None,
        }
    }

    /// Remove staging folders left behind through `disk`.
    #[must_use]
    pub fn with_disk(mut self, disk: Arc<dyn AppDisk>) -> Self {
        self.disk = Some(disk);
        self
    }

    /// List the folders every `interval` instead.
    #[must_use]
    pub fn with_scan_interval(mut self, interval: Duration) -> Self {
        self.scan_interval = interval;
        self
    }
}

/// A finished read.
#[derive(Debug)]
struct Read {
    /// The stack's directory.
    dir: PathBuf,
    /// The files it was read at.
    files: BTreeMap<String, FileInfo>,
    /// What came of it.
    outcome: ConfigOutcome,
    /// The rescan generation in effect when it started.
    generation: u64,
}

/// When an app was last read, at which files, and in which rescan
/// generation the read started.
#[derive(Debug)]
struct ReadAt {
    files: Option<BTreeMap<String, FileInfo>>,
    at: Instant,
    generation: u64,
}

/// Keeps the app list, and the app settings in effect, in [`State`]
/// current until `shutdown` flips to `true` or its sender is dropped.
///
/// It loads the settings from `store` at start and on every
/// [`State::rescan_apps`]; lists the folders every
/// [`AppsDeps::scan_interval`], at start, and on a rescan; reads an app's
/// configuration when it is new, its watched files changed, or a rescan
/// asks; and rebuilds the list whenever any of that, the containers, or
/// the engine status changes. It never creates an app folder: the home is
/// created when the first app is added.
///
/// The only thing it removes is a staging folder a stopped daemon left
/// behind, as [`is_stale_staging`] decides, through [`AppsDeps::disk`]. It
/// looks in each folder, and each app found in it, on the first scan after
/// start and on the first scan after the settings add the folder.
///
/// A read that failed for a reason other than the app's files, such as
/// Docker not being installed, is retried after a while and whenever the
/// engine becomes ready. While a changed app is read again, its previous
/// result stays in the list.
///
/// Once the list holds the results of a rescan, the folders listed after
/// it was asked for and every app read again by a read started after it,
/// the task records its generation with [`State::set_app_scan_done`]. A
/// read already running when a rescan arrives does not count: the app is
/// read once more.
pub async fn run(
    deps: AppsDeps,
    state: State,
    store: SharedStore,
    shutdown: watch::Receiver<bool>,
) {
    Monitor::new(deps, state, store).run(shutdown).await;
}

/// Load the app settings from `store` into `state`, with
/// [`AppsDeps::default_home`] standing in for an unset home, and return
/// the folders they name.
///
/// [`run`] does this at start, but only once its task is scheduled. The
/// daemon calls it before serving, so no request sees the settings
/// before they load.
pub async fn load_settings(
    deps: &AppsDeps,
    state: &State,
    store: &SharedStore,
) -> Option<StackDirs> {
    let stored = match store.setting::<StoredDirs>(SETTINGS_KEY).await {
        Ok(stored) => stored.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(error = %error, "app folder settings unreadable; using the defaults");
            StoredDirs::default()
        }
    };
    let (dirs, settings) = effective(&stored, deps.default_home.as_deref());
    state.set_app_settings(settings);
    dirs
}

struct Monitor {
    deps: AppsDeps,
    state: State,
    store: SharedStore,
    dirs: Option<StackDirs>,
    discovery: Discovery,
    outcomes: BTreeMap<PathBuf, ConfigOutcome>,
    read_at: BTreeMap<PathBuf, ReadAt>,
    reading: BTreeSet<PathBuf>,
    swept: BTreeSet<PathBuf>,
    generation: u64,
    scanned: Option<u64>,
}

impl Monitor {
    fn new(deps: AppsDeps, state: State, store: SharedStore) -> Self {
        Self {
            deps,
            state,
            store,
            dirs: None,
            discovery: Discovery::default(),
            outcomes: BTreeMap::new(),
            read_at: BTreeMap::new(),
            reading: BTreeSet::new(),
            swept: BTreeSet::new(),
            generation: 0,
            scanned: None,
        }
    }

    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut containers = self.state.watch_containers();
        let mut engine = self.state.watch_engine();
        let mut network = self.state.watch_network();
        let mut routing = self.state.watch_routing();
        let mut rescans = self.state.watch_app_rescans();
        self.generation = *rescans.borrow_and_update();
        let mut tick = tokio::time::interval(self.deps.scan_interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut reads: FuturesUnordered<BoxFuture<'static, Read>> = FuturesUnordered::new();
        let mut reload = true;
        loop {
            let mut scan = false;
            let mut reread = false;
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => return,
                Some(read) = reads.next(), if !reads.is_empty() => self.finish(read),
                Ok(()) = rescans.changed() => {
                    self.generation = *rescans.borrow_and_update();
                    (reload, scan, reread) = (true, true, true);
                }
                _ = tick.tick() => scan = true,
                Ok(()) = engine.changed() => {
                    if engine.borrow_and_update().state == EngineState::Ready {
                        self.retry_transient();
                    }
                }
                Ok(()) = containers.changed() => {}
                Ok(()) = network.changed() => {}
                Ok(()) = routing.changed() => {}
            }
            if reload {
                self.load_settings().await;
                reload = false;
            }
            if scan {
                self.scan(reread).await;
                self.sweep().await;
            }
            self.start_reads(&mut reads);
            self.publish();
            if let Some(done) = self.done() {
                self.state.set_app_scan_done(done);
            }
        }
    }

    async fn load_settings(&mut self) {
        self.dirs = load_settings(&self.deps, &self.state, &self.store).await;
    }

    /// List every folder and work out the stacks. With `reread`, every
    /// app's configuration is read again.
    async fn scan(&mut self, reread: bool) {
        let paths: Vec<PathBuf> = self
            .dirs
            .iter()
            .flat_map(StackDirs::all)
            .map(Path::to_owned)
            .collect();
        let fs = self.deps.fs.clone();
        let listed = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|dir| ListedDir {
                    listing: fs.list(&dir),
                    dir,
                })
                .collect::<Vec<_>>()
        })
        .await;
        let listed = match listed {
            Ok(listed) => listed,
            Err(error) => {
                tracing::warn!(error = %error, "listing the app folders failed");
                return;
            }
        };
        self.discovery = discover(&listed);
        let found: BTreeSet<&Path> = self
            .discovery
            .stacks
            .iter()
            .map(|s| s.dir.as_path())
            .collect();
        self.outcomes.retain(|dir, _| found.contains(dir.as_path()));
        self.read_at.retain(|dir, _| found.contains(dir.as_path()));
        if reread {
            for read in self.read_at.values_mut() {
                read.files = None;
            }
        }
        self.scanned = Some(self.generation);
    }

    /// The newest rescan generation the list holds every result of, or
    /// `None` before the first scan.
    fn done(&self) -> Option<u64> {
        let scanned = self.scanned?;
        Some(
            self.discovery
                .stacks
                .iter()
                .map(|stack| self.read_at.get(&stack.dir).map_or(0, |r| r.generation))
                .fold(scanned, u64::min),
        )
    }

    /// Remove staging folders left behind in each folder not yet swept,
    /// and in the apps found in it.
    async fn sweep(&mut self) {
        let roots: BTreeSet<PathBuf> = self
            .dirs
            .iter()
            .flat_map(StackDirs::all)
            .map(Path::to_owned)
            .collect();
        self.swept.retain(|root| roots.contains(root));
        let Some(disk) = self.deps.disk.clone() else {
            return;
        };
        let new: BTreeSet<PathBuf> = roots.difference(&self.swept).cloned().collect();
        if new.is_empty() {
            return;
        }
        let apps = self
            .discovery
            .stacks
            .iter()
            .filter(|stack| new.contains(&stack.root))
            .map(|stack| stack.dir.clone());
        let folders: Vec<PathBuf> = new.iter().cloned().chain(apps).collect();
        self.swept.extend(new);
        let uid = rustix::process::getuid().as_raw();
        let now = SystemTime::now();
        let swept = tokio::task::spawn_blocking(move || {
            for folder in &folders {
                sweep_folder(&*disk, folder, uid, now);
            }
        })
        .await;
        if let Err(error) = swept {
            tracing::warn!(error = %error, "removing leftover staging folders failed");
        }
    }

    fn retry_transient(&mut self) {
        for (dir, read) in &mut self.read_at {
            if self
                .outcomes
                .get(dir)
                .is_some_and(ConfigOutcome::is_transient)
            {
                read.files = None;
            }
        }
    }

    fn is_stale(&self, stack: &FoundStack) -> bool {
        let Some(read) = self.read_at.get(&stack.dir) else {
            return true;
        };
        let transient = self
            .outcomes
            .get(&stack.dir)
            .is_some_and(ConfigOutcome::is_transient);
        read.files.as_ref() != Some(&stack.files) || (transient && read.at.elapsed() >= RETRY_AFTER)
    }

    fn start_reads(&mut self, reads: &mut FuturesUnordered<BoxFuture<'static, Read>>) {
        let endpoint = self
            .state
            .engine()
            .endpoint
            .and_then(|e| Endpoint::parse(&e).ok())
            .unwrap_or_else(Endpoint::system);
        let stale: Vec<FoundStack> = self
            .discovery
            .stacks
            .iter()
            .filter(|stack| !self.reading.contains(&stack.dir) && self.is_stale(stack))
            .take(MAX_READS.saturating_sub(reads.len()))
            .cloned()
            .collect();
        for stack in stale {
            self.reading.insert(stack.dir.clone());
            reads.push(Box::pin(read(
                self.deps.config.clone(),
                stack,
                endpoint.clone(),
                self.generation,
            )));
        }
    }

    fn finish(&mut self, read: Read) {
        self.reading.remove(&read.dir);
        if !self.discovery.stacks.iter().any(|s| s.dir == read.dir) {
            return;
        }
        let current = read.generation >= self.generation;
        self.read_at.insert(
            read.dir.clone(),
            ReadAt {
                files: current.then_some(read.files),
                at: Instant::now(),
                generation: read.generation,
            },
        );
        self.outcomes.insert(read.dir, read.outcome);
    }

    fn publish(&self) {
        let containers = self.state.containers();
        let network = self.state.network();
        let routing = self.state.routing();
        let list = app_list(&Inputs {
            discovery: &self.discovery,
            home: self.dirs.as_ref().map(StackDirs::home),
            configs: &self.outcomes,
            containers: &containers,
            engine_ready: self.state.engine().state == EngineState::Ready,
            network: &network,
            routing: &routing.apps,
        });
        self.state.set_app_configs(self.outcomes.clone());
        self.state.set_apps(list);
    }
}

/// Remove the staging folders in `folder` that [`is_stale_staging`] says
/// were left behind, for the daemon running as `uid` at `now`.
fn sweep_folder(disk: &dyn AppDisk, folder: &Path, uid: u32, now: SystemTime) {
    let entries = match disk.list_entries(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(%error, dir = %folder.display(), "listing a folder for leftover staging folders failed");
            return;
        }
    };
    for entry in entries.iter().filter(|e| is_stale_staging(e, uid, now)) {
        let dir = folder.join(&entry.name);
        match disk.remove_left_staging(&dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "removed a staging folder left by an earlier run");
            }
            Err(error) => {
                tracing::warn!(%error, dir = %dir.display(), "removing a leftover staging folder failed");
            }
        }
    }
}

async fn read(
    config: Arc<dyn ComposeConfigReader>,
    stack: FoundStack,
    endpoint: Endpoint,
    generation: u64,
) -> Read {
    let outcome = match ComposeProject::new(&stack.dir) {
        Ok(project) => {
            ConfigOutcome::from_result(config.read(&project, &endpoint).await, stack.compose_file)
        }
        Err(error) => ConfigOutcome::Failed {
            kind: AppProblemKind::ComposeUnavailable,
            message: error.to_string(),
        },
    };
    Read {
        dir: stack.dir,
        files: stack.files,
        outcome,
        generation,
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
    use std::fs::{self, File};
    use std::os::unix::fs::symlink;

    use alchemy_api::apps::{AppList, StackStatus};
    use alchemy_core::engine::{ContainerState, ContainerStatus, EngineStatus};
    use alchemy_core::stack::{EntryKind, ListedEntry, Listing, STAGING_MAX_AGE};

    use super::*;
    use crate::apps::FakeFs;
    use crate::compose::{FakeComposeConfig, FakeConfig};
    use crate::store::MemoryStore;

    const HOME: &str = "/home/me/Alchemy/apps";
    const WAIT: Duration = Duration::from_secs(5);

    struct Harness {
        state: State,
        store: SharedStore,
        fs: FakeFs,
        config: FakeComposeConfig,
        stop: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    fn start() -> Harness {
        let state = State::new();
        let store: SharedStore = Arc::new(MemoryStore::new());
        let fs = FakeFs::new();
        let config = FakeComposeConfig::new();
        let deps = AppsDeps::new(
            Arc::new(fs.clone()),
            Arc::new(config.clone()),
            Some(HOME.into()),
        )
        .with_scan_interval(Duration::from_millis(10));
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(deps, state.clone(), store.clone(), stop_rx));
        Harness {
            state,
            store,
            fs,
            config,
            stop,
            task,
        }
    }

    fn stacks(names: &[(&str, u64)]) -> Listing {
        Listing::Entries(
            names
                .iter()
                .map(|(name, len)| ListedEntry {
                    name: (*name).into(),
                    kind: EntryKind::Dir,
                    files: [(
                        "compose.yaml".to_owned(),
                        FileInfo {
                            kind: EntryKind::File,
                            modified: None,
                            len: *len,
                        },
                    )]
                    .into(),
                })
                .collect(),
        )
    }

    async fn until(state: &State, what: &str, done: impl Fn(&AppList) -> bool) -> AppList {
        let mut rx = state.watch_apps();
        let found = tokio::time::timeout(WAIT, rx.wait_for(|apps| done(apps))).await;
        match found {
            Ok(Ok(apps)) => apps.clone(),
            _ => panic!("timed out waiting for {what}: {:?}", state.apps()),
        }
    }

    fn running(id: &str, project: &str, service: &str) -> ContainerStatus {
        let mut c = ContainerStatus::new(id, id, ContainerState::Running);
        c.project = Some(project.into());
        c.service = Some(service.into());
        c
    }

    fn status_of(apps: &AppList, name: &str) -> Option<StackStatus> {
        apps.apps
            .iter()
            .find(|a| a.name.as_str() == name)
            .map(|a| a.status)
    }

    fn resolved(apps: &AppList) -> bool {
        apps.apps.first().is_some_and(|a| a.project.is_some())
    }

    #[tokio::test]
    async fn lists_apps_in_the_default_home_and_reads_each_once() {
        let h = start();
        let blog = format!("{HOME}/blog");
        h.state.set_engine(EngineStatus::new(EngineState::Ready));
        h.config.project(&blog, "fancy", &["web"]);
        h.fs.set(HOME, stacks(&[("blog", 1)]));

        let apps = until(&h.state, "blog read", resolved).await;
        assert_eq!(apps.apps[0].project.as_deref(), Some("fancy"));
        assert_eq!(apps.apps[0].status, StackStatus::Stopped);
        assert_eq!(apps.dirs[0].path, HOME);
        assert!(apps.dirs[0].home);
        assert_eq!(h.state.app_settings().home.as_deref(), Some(HOME));
        match h.state.app_config(Path::new(&blog)) {
            Some(ConfigOutcome::Ready(config)) => assert_eq!(config.name, "fancy"),
            other => panic!("expected the cached config, got {other:?}"),
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.config.reads(Path::new(&blog)), 1);

        h.state
            .update_containers(|c| c.replace_all([running("1", "fancy", "web")]));
        until(&h.state, "blog running", |a| {
            status_of(a, "blog") == Some(StackStatus::Running)
        })
        .await;
        assert_eq!(h.config.reads(Path::new(&blog)), 1);

        h.stop.send_replace(true);
        tokio::time::timeout(WAIT, h.task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rereads_an_app_when_its_files_change() {
        let h = start();
        let blog = format!("{HOME}/blog");
        h.config.project(&blog, "blog", &["web"]);
        h.fs.set(HOME, stacks(&[("blog", 1)]));
        until(&h.state, "blog read", resolved).await;

        h.config.project(&blog, "renamed", &["web"]);
        h.fs.set(HOME, stacks(&[("blog", 2)]));
        until(&h.state, "blog reread", |a| {
            a.apps.first().and_then(|a| a.project.as_deref()) == Some("renamed")
        })
        .await;
        assert_eq!(h.config.reads(Path::new(&blog)), 2);

        h.fs.set(HOME, stacks(&[]));
        until(&h.state, "blog gone", |a| a.apps.is_empty()).await;
        assert_eq!(h.state.app_configs(), BTreeMap::new());
    }

    #[tokio::test]
    async fn a_rescan_reloads_settings_and_rereads_every_app() {
        let h = start();
        let db = Path::new("/opt/stacks/db");
        h.config.project(db, "db", &["postgres"]);
        h.fs.set(HOME, stacks(&[]));
        h.fs.set("/opt/stacks", stacks(&[("db", 1)]));
        until(&h.state, "home listed", |a| a.dirs.len() == 1).await;

        h.store
            .set_setting(
                SETTINGS_KEY,
                &StoredDirs {
                    home: None,
                    extra: vec!["/opt/stacks".into()],
                },
            )
            .await
            .unwrap();
        h.state.rescan_apps();
        let apps = until(&h.state, "db read", resolved).await;
        assert_eq!(apps.dirs.len(), 2);
        assert_eq!(apps.apps[0].root, "/opt/stacks");
        assert_eq!(h.state.app_settings().extra, ["/opt/stacks"]);
        assert_eq!(h.config.reads(db), 1);

        h.state.rescan_apps();
        let deadline = Instant::now() + WAIT;
        while h.config.reads(db) < 2 {
            assert!(Instant::now() < deadline, "no reread after a rescan");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_done_rescan_has_every_app_read_again() {
        let h = start();
        h.config.project(format!("{HOME}/blog"), "blog", &["web"]);
        h.fs.set(HOME, stacks(&[("blog", 1)]));
        until(&h.state, "blog read", resolved).await;

        let project = |apps: &AppList, name: &str| {
            apps.apps
                .iter()
                .find(|a| a.name.as_str() == name)
                .and_then(|a| a.project.clone())
        };
        let blog_and_wiki = [("blog", 1), ("wiki", 1)];
        let cases = [
            (
                "an edit with the same files",
                "blog",
                &blog_and_wiki[..1],
                "edited",
            ),
            ("a new app", "wiki", &blog_and_wiki[..], "wiki"),
        ];
        for (what, name, listed, want) in cases {
            h.config.project(format!("{HOME}/{name}"), want, &["web"]);
            h.fs.set(HOME, stacks(listed));
            let ticket = h.state.rescan_apps();
            tokio::time::timeout(WAIT, h.state.app_scan_done(ticket))
                .await
                .unwrap_or_else(|_| panic!("{what}: rescan {ticket} never done"));
            assert_eq!(
                project(&h.state.apps(), name).as_deref(),
                Some(want),
                "{what}"
            );
        }
    }

    #[tokio::test]
    async fn retries_a_read_that_failed_without_the_files_changing() {
        let h = start();
        let blog = format!("{HOME}/blog");
        h.config.set(&blog, FakeConfig::NoDocker);
        h.fs.set(HOME, stacks(&[("blog", 1)]));
        until(&h.state, "blog failed", |a| {
            a.apps.first().is_some_and(|a| !a.problems.is_empty())
        })
        .await;

        h.config.project(&blog, "blog", &["web"]);
        h.state.set_engine(EngineStatus::new(EngineState::Ready));
        let apps = until(&h.state, "blog read", resolved).await;
        assert_eq!(apps.apps[0].problems, []);
    }

    #[tokio::test]
    async fn loading_the_settings_publishes_them_without_the_task() {
        let state = State::new();
        let store: SharedStore = Arc::new(MemoryStore::new());
        let deps = AppsDeps::new(
            Arc::new(FakeFs::new()),
            Arc::new(FakeComposeConfig::new()),
            Some(HOME.into()),
        );
        let dirs = load_settings(&deps, &state, &store).await.unwrap();
        assert_eq!(dirs.home(), Path::new(HOME));
        assert_eq!(state.app_settings().home.as_deref(), Some(HOME));
    }

    #[tokio::test]
    async fn searches_nothing_without_a_home() {
        let state = State::new();
        let fs = FakeFs::new();
        fs.set("/", stacks(&[("blog", 1)]));
        let deps = AppsDeps::new(Arc::new(fs), Arc::new(FakeComposeConfig::new()), None)
            .with_scan_interval(Duration::from_millis(10));
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            deps,
            state.clone(),
            Arc::new(MemoryStore::new()),
            stop_rx,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(state.apps(), AppList::default());
        assert_eq!(state.app_settings().home, None);
        drop(stop);
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    }

    /// Make the directory `dir`, with everything in `files`, last changed
    /// `age` ago.
    fn staging(dir: &Path, files: &[&str], age: Duration) {
        fs::create_dir_all(dir).unwrap();
        for file in files {
            fs::write(dir.join(file), "x").unwrap();
        }
        File::open(dir)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    const OLD: Duration = Duration::from_hours(2);

    #[test]
    fn sweeps_only_staging_folders_left_behind() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let outside = dir.join("outside");
        staging(&outside, &["data"], OLD);
        let uid = rustix::process::getuid().as_raw();
        let left = [".alchemy-tmp-blog-a1B2c3", ".alchemy-tmp-my-app-Zz9"];
        for name in left {
            staging(&dir.join(name), &["compose.yaml"], OLD);
        }
        staging(&dir.join(".alchemy-tmp-blog-fresh1"), &[], Duration::ZERO);
        staging(&dir.join(".alchemy-tmp-blog-"), &[], OLD);
        staging(&dir.join(".alchemy-tmp-Blog-a1B2c3"), &[], OLD);
        staging(&dir.join(".alchemy-tmp-blog-a1.b2"), &[], OLD);
        staging(&dir.join(".alchemy-cache"), &[], OLD);
        staging(&dir.join("blog"), &["compose.yaml"], OLD);
        symlink(&outside, dir.join(".alchemy-tmp-blog-d4E5f6")).unwrap();
        fs::write(dir.join(".alchemy-tmp-blog-g7H8i9"), "keep").unwrap();
        let kept: BTreeSet<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| !left.iter().any(|l| name == l))
            .collect();

        sweep_folder(&SystemDisk, dir, uid, SystemTime::now());
        let now: BTreeSet<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(now, kept);
        assert!(outside.join("data").exists());

        sweep_folder(
            &SystemDisk,
            dir,
            uid.wrapping_add(1),
            SystemTime::now() + OLD,
        );
        sweep_folder(&SystemDisk, dir, uid, SystemTime::UNIX_EPOCH);
        sweep_folder(&SystemDisk, &dir.join("missing"), uid, SystemTime::now());
        assert!(dir.join(".alchemy-tmp-blog-fresh1").exists());
        sweep_folder(&SystemDisk, dir, uid, SystemTime::now() + OLD);
        assert!(!dir.join(".alchemy-tmp-blog-fresh1").exists());
        assert!(OLD > STAGING_MAX_AGE);
    }

    async fn until_gone(path: &Path) {
        let deadline = Instant::now() + WAIT;
        while fs::symlink_metadata(path).is_ok() {
            assert!(Instant::now() < deadline, "{} was kept", path.display());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn sweeps_each_folder_and_its_apps_once() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("apps");
        let extra = root.path().join("stacks");
        staging(&home.join("blog"), &["compose.yaml"], Duration::ZERO);
        let create = home.join(".alchemy-tmp-shop-a1B2c3");
        let edit = home.join("blog/.alchemy-tmp-blog-d4E5f6");
        let fresh = home.join("blog/.alchemy-tmp-blog-g7H8i9");
        staging(&create, &[], OLD);
        staging(&edit, &["compose.yaml"], OLD);
        staging(&fresh, &[], Duration::ZERO);
        staging(&extra.join("db"), &["compose.yaml"], Duration::ZERO);
        let later = extra.join("db/.alchemy-tmp-db-j1K2l3");
        staging(&later, &[], OLD);

        let state = State::new();
        let store: SharedStore = Arc::new(MemoryStore::new());
        let deps = AppsDeps::new(
            Arc::new(SystemFs),
            Arc::new(FakeComposeConfig::new()),
            Some(home.clone()),
        )
        .with_scan_interval(Duration::from_millis(10))
        .with_disk(Arc::new(SystemDisk));
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(deps, state.clone(), store.clone(), stop_rx));
        until_gone(&create).await;
        until_gone(&edit).await;
        assert!(fresh.exists());

        let again = home.join(".alchemy-tmp-shop-m4N5o6");
        staging(&again, &[], OLD);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(again.exists(), "a folder already swept was swept again");
        assert!(later.exists(), "a folder not in the settings was swept");

        store
            .set_setting(
                SETTINGS_KEY,
                &StoredDirs {
                    home: None,
                    extra: vec![extra.clone()],
                },
            )
            .await
            .unwrap();
        state.rescan_apps();
        until_gone(&later).await;
        assert!(again.exists());
        assert!(home.join("blog/compose.yaml").exists());
        assert!(extra.join("db/compose.yaml").exists());

        stop.send_replace(true);
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    }
}
