//! The port registry: every host port Alchemy reserved, an app's Compose
//! file declares, or something holds right now.
//!
//! [`PortRegistry`] gathers a [`RegistryInput`] from the daemon's state,
//! the store's reservations, and the host's listeners, for
//! `GET /api/ports` and for anything that must check a port before using
//! it. Reservations are made only through [`PortRegistry::reserve`],
//! which picks the lowest free port with
//! [`alchemy_core::ports::allocate`]. The merge itself is pure and lives
//! in `alchemy_core::ports`; [`registry_input`] is the pure step that
//! turns the daemon's state into its input.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alchemy_api::apps::AppList;
use alchemy_api::ports::PortList;
use alchemy_core::engine::Containers;
use alchemy_core::ports::{
    AllocateError, ContainerPort, Listener, PortRange, Protocol, RegistryInput, Reservation,
    ServicePort, allocate,
};
use tokio::sync::Mutex;

use crate::State;
use crate::apps::ConfigOutcome;
use crate::network::SharedNetwork;
use crate::store::{PortReservation, SharedStore, StoreError};

/// The warning [`PortRegistry::list`] gives when the host's listeners
/// cannot be read.
pub const LISTENERS_UNREAD: &str = "alchemyd couldn't read which programs are listening on this machine, so ports they hold aren't listed.";

/// Docker Desktop's backend as macOS names it: the kernel keeps 16
/// characters of a process name.
const DOCKER_DESKTOP_TRUNCATED: &str = "com.docker.backe";

/// Docker Desktop's backend, which listens on every port its containers
/// publish.
const DOCKER_DESKTOP: &str = "com.docker.backend";

/// What [`PortRegistry::gather`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gathered {
    /// Everything the registry merges.
    pub input: RegistryInput,
    /// What could not be read, in sentences for people.
    pub warnings: Vec<String>,
}

/// Why [`PortRegistry::reserve`] did not reserve a port.
#[derive(Debug, thiserror::Error)]
pub enum ReserveError {
    /// Every port in the range is claimed.
    #[error(transparent)]
    Exhausted(#[from] AllocateError),
    /// The reservations could not be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The chosen port was reserved by someone else between the check and
    /// the write. Only another writer to the same database can cause it.
    #[error("port {port}/{protocol} was reserved by someone else first")]
    Raced {
        /// The port chosen.
        port: u16,
        /// Its protocol.
        protocol: Protocol,
    },
}

/// The daemon's port registry. Cheap to clone: clones share the lock that
/// keeps two reservations from picking the same port.
#[derive(Debug, Clone)]
pub struct PortRegistry {
    state: State,
    store: SharedStore,
    network: SharedNetwork,
    reserving: Arc<Mutex<()>>,
}

impl PortRegistry {
    /// A registry over the daemon's `state`, the reservations in `store`,
    /// and the listeners `network` reports.
    pub fn new(state: State, store: SharedStore, network: SharedNetwork) -> Self {
        Self {
            state,
            store,
            network,
            reserving: Arc::new(Mutex::new(())),
        }
    }

    /// Everything the registry merges, read now.
    ///
    /// A host whose listeners cannot be read is not an error: the input
    /// has none, and a warning says so. Unreadable reservations are.
    pub async fn gather(&self) -> Result<Gathered, StoreError> {
        let reservations = self.store.port_reservations().await?;
        Ok(self.gather_with(&reservations).await)
    }

    /// `GET /api/ports`: every claimed port, merged.
    pub async fn list(&self) -> Result<PortList, StoreError> {
        let gathered = self.gather().await?;
        Ok(PortList {
            ports: gathered.input.merge(),
            warnings: gathered.warnings,
        })
    }

    /// Reserve the lowest free port in `range` for `owner`, on `host_ip`
    /// (`None` for every address), recording `created` as when.
    ///
    /// Every claim counts against it: other reservations, ports apps
    /// declare even while stopped, running containers, and listening
    /// programs. An owner that already holds a port in `range` for the
    /// same protocol and address gets that port again, so asking twice
    /// is harmless.
    pub async fn reserve(
        &self,
        owner: &str,
        range: PortRange,
        protocol: Protocol,
        host_ip: Option<IpAddr>,
        created: i64,
    ) -> Result<u16, ReserveError> {
        let _guard = self.reserving.lock().await;
        let reservations = self.store.port_reservations().await?;
        if let Some(held) = reservations.iter().find(|r| {
            r.owner == owner
                && r.protocol == protocol
                && r.host_ip == host_ip
                && range.contains(r.port)
        }) {
            return Ok(held.port);
        }
        let gathered = self.gather_with(&reservations).await;
        let port = allocate(range, protocol, host_ip, &gathered.input.merge())?;
        let added = self
            .store
            .add_port_reservation(PortReservation {
                owner: owner.to_owned(),
                port,
                protocol,
                host_ip,
                created,
            })
            .await?;
        if !added {
            return Err(ReserveError::Raced { port, protocol });
        }
        tracing::info!(owner, port, %protocol, "port reserved");
        Ok(port)
    }

    /// Give up every port reserved for `owner`, returning how many.
    pub async fn release(&self, owner: &str) -> Result<u64, StoreError> {
        let _guard = self.reserving.lock().await;
        self.store.remove_port_reservations(owner).await
    }

    async fn gather_with(&self, reservations: &[PortReservation]) -> Gathered {
        let mut warnings = Vec::new();
        let listeners = match self.network.listeners().await {
            Ok(listeners) => listeners,
            Err(error) => {
                tracing::warn!(%error, "cannot read the host's listeners");
                warnings.push(LISTENERS_UNREAD.to_owned());
                Vec::new()
            }
        };
        let input = registry_input(
            &self.state.apps(),
            &self.state.app_configs(),
            &self.state.containers(),
            reservations
                .iter()
                .map(PortReservation::reservation)
                .collect(),
            listeners,
        );
        Gathered { input, warnings }
    }
}

/// The registry's input from what the daemon knows.
///
/// Declared ports come from each app's resolved Compose configuration,
/// and live ones from the containers Docker reports. Both name an app as
/// Alchemy does, by its folder, so a declared port and the running
/// container that holds it merge into one; a Compose project with no app
/// folder keeps its project name. Docker Desktop's backend, which macOS
/// names `com.docker.backe`, gets its full name back.
pub fn registry_input(
    apps: &AppList,
    configs: &BTreeMap<PathBuf, ConfigOutcome>,
    containers: &Containers,
    reservations: Vec<Reservation>,
    listeners: Vec<Listener>,
) -> RegistryInput {
    let mut by_project: BTreeMap<&str, &str> = BTreeMap::new();
    for app in &apps.apps {
        if let Some(project) = app.project.as_deref() {
            by_project.entry(project).or_insert(app.name.as_str());
        }
    }
    let app_for = |project: &str| -> String {
        by_project
            .get(project)
            .map_or_else(|| project.to_owned(), |app| (*app).to_owned())
    };

    let declared = configs
        .iter()
        .filter_map(|(dir, outcome)| match outcome {
            ConfigOutcome::Ready(config) => Some((dir, config)),
            ConfigOutcome::Failed { .. } => None,
        })
        .flat_map(|(dir, config)| {
            let app = apps
                .apps
                .iter()
                .find(|app| Path::new(&app.dir) == dir.as_path())
                .map_or_else(|| app_for(&config.name), |app| app.name.to_string());
            config.services.iter().flat_map(move |service| {
                let app = app.clone();
                service.ports.iter().map(move |port| ServicePort {
                    app: app.clone(),
                    service: service.name.clone(),
                    port: *port,
                })
            })
        })
        .collect();

    let containers = containers
        .iter()
        .flat_map(|c| {
            let app = c.project.as_deref().map(app_for);
            c.ports.iter().map(move |port| ContainerPort {
                name: c.name.clone(),
                app: app.clone(),
                service: c.service.clone(),
                port: *port,
            })
        })
        .collect();

    let listeners = listeners
        .into_iter()
        .map(|mut listener| {
            if listener.process.as_deref() == Some(DOCKER_DESKTOP_TRUNCATED) {
                listener.process = Some(DOCKER_DESKTOP.to_owned());
            }
            listener
        })
        .collect();

    RegistryInput {
        reservations,
        declared,
        containers,
        listeners,
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
    use alchemy_api::apps::{App, StackName, StackStatus};
    use alchemy_core::engine::{ContainerState, ContainerStatus, StopReason};
    use alchemy_core::ports::{PortClaim, PortHolder, PortSource, PublishedPort};

    use super::*;
    use crate::compose::{ComposeConfig, ServiceConfig};
    use crate::network::FakeNetwork;
    use crate::store::MemoryStore;

    fn tcp(port: u16) -> PublishedPort {
        PublishedPort::new(Protocol::Tcp, PortRange::single(port).ok())
    }

    fn app(name: &str, project: Option<&str>) -> App {
        App {
            name: StackName::new(name).unwrap(),
            status: StackStatus::Stopped,
            dir: format!("/srv/apps/{name}"),
            root: "/srv/apps".into(),
            compose_file: "compose.yaml".into(),
            project: project.map(Into::into),
            images: None,
            services: Vec::new(),
            problems: Vec::new(),
            stop_reason: StopReason::default(),
        }
    }

    fn config(project: &str, services: &[(&str, &[u16])]) -> ConfigOutcome {
        ConfigOutcome::Ready(ComposeConfig {
            name: project.into(),
            services: services
                .iter()
                .map(|(name, ports)| ServiceConfig {
                    name: (*name).into(),
                    ports: ports.iter().map(|p| tcp(*p)).collect(),
                    ..ServiceConfig::default()
                })
                .collect(),
            ..ComposeConfig::default()
        })
    }

    fn container(
        name: &str,
        project: Option<&str>,
        service: Option<&str>,
        ports: &[u16],
    ) -> ContainerStatus {
        let mut c = ContainerStatus::new(name, name, ContainerState::Running);
        c.project = project.map(Into::into);
        c.service = service.map(Into::into);
        c.ports = ports.iter().map(|p| tcp(*p)).collect();
        c
    }

    fn listener(port: u16, process: Option<&str>, pid: Option<u32>) -> Listener {
        Listener {
            protocol: Protocol::Tcp,
            addr: "0.0.0.0".parse().unwrap(),
            port,
            process: process.map(Into::into),
            pid,
        }
    }

    fn apps(list: Vec<App>) -> AppList {
        AppList {
            apps: list,
            unmanaged: Vec::new(),
            dirs: Vec::new(),
        }
    }

    fn containers(list: Vec<ContainerStatus>) -> Containers {
        let mut all = Containers::new();
        all.replace_all(list);
        all
    }

    fn rows(claims: &[PortClaim]) -> Vec<String> {
        claims
            .iter()
            .map(|c| {
                let source = match c.source {
                    PortSource::Reserved => "reserved",
                    PortSource::Declared => "declared",
                    PortSource::Live => "live",
                };
                format!("{} {source} {}", c.port, c.holder)
            })
            .collect()
    }

    /// One of each: a stopped app's declared port, a running app whose
    /// declared port is its container's, an app renamed from its project,
    /// a bare container, a reservation, Docker Desktop's listener on a
    /// container port, and `nc`.
    fn everything() -> (AppList, BTreeMap<PathBuf, ConfigOutcome>, Containers) {
        let list = apps(vec![
            app("blog", Some("blog")),
            app("journal", Some("diary")),
            app("wiki", Some("wiki")),
        ]);
        let configs = BTreeMap::from([
            (
                PathBuf::from("/srv/apps/blog"),
                config("blog", &[("web", &[8080])]),
            ),
            (
                PathBuf::from("/srv/apps/journal"),
                config("diary", &[("app", &[3000])]),
            ),
            (
                PathBuf::from("/srv/apps/wiki"),
                config("wiki", &[("web", &[8081])]),
            ),
            (
                PathBuf::from("/srv/apps/broken"),
                ConfigOutcome::Failed {
                    kind: alchemy_api::apps::AppProblemKind::ComposeError,
                    message: "bad".into(),
                },
            ),
        ]);
        let running = containers(vec![
            container("blog-web-1", Some("blog"), Some("web"), &[8080]),
            container("diary-app-1", Some("diary"), Some("app"), &[3000]),
            container("pihole", None, None, &[53]),
            container("legacy-db-1", Some("legacy"), Some("db"), &[5432]),
        ]);
        (list, configs, running)
    }

    #[test]
    fn merges_every_source_with_the_right_holder() {
        let (list, configs, running) = everything();
        let input = registry_input(
            &list,
            &configs,
            &running,
            vec![Reservation {
                owner: "proxy".into(),
                port: tcp(8443),
            }],
            vec![
                listener(8080, Some(DOCKER_DESKTOP_TRUNCATED), Some(7)),
                listener(5432, None, None),
                listener(9000, Some("nc"), Some(42)),
                listener(9001, Some(DOCKER_DESKTOP_TRUNCATED), Some(7)),
            ],
        );
        assert_eq!(
            rows(&input.merge()),
            [
                "53/tcp live container pihole",
                "3000/tcp live service app of app journal",
                "5432/tcp live service db of app legacy",
                "8080/tcp live service web of app blog",
                "8081/tcp declared service web of app wiki",
                "8443/tcp reserved a reservation for proxy",
                "0.0.0.0:9000/tcp live another program (nc, pid 42)",
                "0.0.0.0:9001/tcp live another program (com.docker.backend, pid 7)",
            ]
        );
    }

    #[test]
    fn an_app_is_named_by_folder_or_by_project() {
        let cases = [
            (
                "folder of the config",
                "/srv/apps/journal",
                "diary",
                "journal",
            ),
            ("project of an app", "/elsewhere/diary", "diary", "journal"),
            ("project of no app", "/elsewhere/other", "other", "other"),
        ];
        let list = apps(vec![app("journal", Some("diary"))]);
        for (name, dir, project, want) in cases {
            let configs =
                BTreeMap::from([(PathBuf::from(dir), config(project, &[("web", &[80])]))]);
            let input = registry_input(&list, &configs, &Containers::new(), vec![], vec![]);
            assert_eq!(input.declared.len(), 1, "{name}");
            assert_eq!(input.declared[0].app, want, "{name}");
        }
    }

    fn registry(state: &State, network: &FakeNetwork) -> (PortRegistry, SharedStore) {
        let store: SharedStore = Arc::new(MemoryStore::new());
        let registry = PortRegistry::new(state.clone(), store.clone(), Arc::new(network.clone()));
        (registry, store)
    }

    fn seeded() -> State {
        let (list, configs, running) = everything();
        let state = State::new();
        state.set_app_configs(configs);
        state.set_apps(list);
        state.update_containers(|c| c.replace_all(running.iter().cloned()));
        state
    }

    #[tokio::test]
    async fn lists_without_listeners_when_they_cannot_be_read() {
        let state = seeded();
        let network = FakeNetwork::new();
        network.set_listeners(vec![listener(9000, Some("nc"), Some(42))]);
        let (registry, _) = registry(&state, &network);
        let list = registry.list().await.unwrap();
        assert_eq!(list.ports.len(), 6, "{list:?}");
        assert_eq!(list.warnings, Vec::<String>::new());

        network.set_failing(true);
        let list = registry.list().await.unwrap();
        assert_eq!(list.ports.len(), 5, "{list:?}");
        assert_eq!(list.warnings, [LISTENERS_UNREAD]);
    }

    #[tokio::test]
    async fn reserves_the_lowest_free_port_once_per_owner() {
        let state = seeded();
        let network = FakeNetwork::new();
        network.set_listeners(vec![listener(8082, Some("nc"), Some(42))]);
        let (registry, store) = registry(&state, &network);
        let range = PortRange::new(8080, 8084).unwrap();

        let proxy = registry
            .reserve("proxy", range, Protocol::Tcp, None, 100)
            .await
            .unwrap();
        assert_eq!(proxy, 8083, "8080 live, 8081 declared, 8082 nc");
        let again = registry
            .reserve("proxy", range, Protocol::Tcp, None, 200)
            .await
            .unwrap();
        assert_eq!(again, 8083, "the same owner keeps its port");
        let other = registry
            .reserve("blog", range, Protocol::Tcp, None, 300)
            .await
            .unwrap();
        assert_eq!(other, 8084);
        let udp = registry
            .reserve("proxy", range, Protocol::Udp, None, 400)
            .await
            .unwrap();
        assert_eq!(udp, 8080, "nothing holds UDP ports");

        let err = registry
            .reserve("wiki", range, Protocol::Tcp, None, 500)
            .await
            .unwrap_err();
        assert!(matches!(err, ReserveError::Exhausted(_)), "{err:?}");

        let stored = store.port_reservations().await.unwrap();
        assert_eq!(stored.len(), 3);
        assert_eq!(stored[0].created, 400);
        let listed = registry.list().await.unwrap();
        let held = PortHolder::Reservation {
            owner: "proxy".into(),
        };
        assert!(
            listed
                .ports
                .iter()
                .any(|c| c.holder == held && c.port == tcp(8083))
        );

        assert_eq!(registry.release("proxy").await.unwrap(), 2);
        let freed = registry
            .reserve("wiki", range, Protocol::Tcp, None, 600)
            .await
            .unwrap();
        assert_eq!(freed, 8083);
    }

    #[tokio::test]
    async fn concurrent_reservations_get_different_ports() {
        let state = State::new();
        let (registry, _) = registry(&state, &FakeNetwork::new());
        let range = PortRange::new(9000, 9100).unwrap();
        let tasks: Vec<_> = (0..10)
            .map(|i| {
                let registry = registry.clone();
                tokio::spawn(async move {
                    registry
                        .reserve(&format!("owner{i}"), range, Protocol::Tcp, None, 0)
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut ports = Vec::new();
        for task in tasks {
            ports.push(task.await.unwrap());
        }
        ports.sort_unstable();
        assert_eq!(ports, (9000..9010).collect::<Vec<u16>>());
    }
}
