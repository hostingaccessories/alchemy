//! The Docker Engine API behind [`DockerApi`], and the only place
//! `bollard` types are mapped to core types.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use alchemy_core::engine::{
    ContainerEvent, ContainerState, ContainerStatus, EngineStatus, Health, flavor_from_platform,
    parse_action,
};
use alchemy_core::metrics::{ContainerSample, DiskIo, Traffic, container_memory};
use bollard::models::{
    ContainerStatsResponse, ContainerSummary, EventMessage, EventMessageTypeEnum, SystemVersion,
};
use bollard::query_parameters::{EventsOptions, ListContainersOptions, StatsOptions};
use bollard::{ClientVersion, Docker};
use futures_util::{Stream, StreamExt as _};

use super::Endpoint;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The Engine API calls the daemon makes.
pub trait DockerApi {
    /// `GET /_ping`: succeeds once the engine is ready for requests.
    fn ping(&self) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// `GET /version`.
    fn version(&self) -> impl Future<Output = Result<EngineVersion, ApiError>> + Send;

    /// `GET /containers/json?all=true`: every container, running or not.
    ///
    /// Containers in a state the core model does not know are left out.
    fn list_containers(
        &self,
    ) -> impl Future<Output = Result<Vec<ContainerStatus>, ApiError>> + Send;

    /// `GET /events` for containers, as a stream that lasts as long as the
    /// connection.
    ///
    /// Events with an action the core model ignores are left out. The
    /// stream ends or yields an error when the engine goes away; callers
    /// reconnect with [`reconnect_delay`](alchemy_core::engine::reconnect_delay).
    fn events(
        &self,
    ) -> impl Stream<Item = Result<ContainerEvent, ApiError>> + Send + Unpin + 'static;
}

/// Why an Engine API call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// No socket exists at the endpoint.
    #[error("no Docker engine socket at {}", .0.display())]
    SocketNotFound(PathBuf),
    /// The socket path cannot be handed to the client because it is not
    /// UTF-8.
    #[error("Docker engine socket path is not UTF-8: {}", .0.display())]
    InvalidSocketPath(PathBuf),
    /// The engine did not respond in time.
    #[error("Docker engine request timed out")]
    Timeout,
    /// The engine answered with an error status.
    #[error("Docker engine returned HTTP {status}: {message}")]
    Server {
        /// HTTP status code.
        status: u16,
        /// The engine's error message.
        message: String,
    },
    /// The engine's response could not be decoded.
    #[error("Docker engine response could not be decoded")]
    Decode(#[source] BoxError),
    /// The connection failed or dropped.
    #[error("Docker engine connection failed")]
    Transport(#[source] BoxError),
}

impl From<bollard::errors::Error> for ApiError {
    fn from(error: bollard::errors::Error) -> Self {
        use bollard::errors::Error as E;
        match error {
            E::SocketNotFoundError(path) => Self::SocketNotFound(PathBuf::from(path)),
            E::RequestTimeoutError => Self::Timeout,
            E::DockerResponseServerError {
                status_code,
                message,
            } => Self::Server {
                status: status_code,
                message,
            },
            e @ (E::JsonDataError { .. } | E::JsonSerdeError { .. } | E::StrParseError { .. }) => {
                Self::Decode(Box::new(e))
            }
            e => Self::Transport(Box::new(e)),
        }
    }
}

/// What `GET /version` reports, reduced to what the daemon shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineVersion {
    /// Engine version, such as `27.3.1`.
    pub version: Option<String>,
    /// Engine API version, such as `1.47`.
    pub api_version: Option<String>,
    /// `Platform.Name`, such as `Docker Desktop 4.34.2 (167172)`.
    pub platform: Option<String>,
}

impl EngineVersion {
    /// Fill the version and flavor fields of `status`.
    pub fn apply_to(&self, status: &mut EngineStatus) {
        let (flavor, desktop_version) =
            flavor_from_platform(self.platform.as_deref().unwrap_or(""));
        status.engine_version.clone_from(&self.version);
        status.api_version.clone_from(&self.api_version);
        status.flavor = Some(flavor);
        status.desktop_version = desktop_version;
    }
}

const PROJECT_LABEL: &str = "com.docker.compose.project";
const SERVICE_LABEL: &str = "com.docker.compose.service";
const WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";

fn engine_version(version: SystemVersion) -> EngineVersion {
    EngineVersion {
        version: version.version,
        api_version: version.api_version,
        platform: version.platform.map(|p| p.name),
    }
}

/// Docker 29 reports `stopping` while a stop is in progress. The core
/// model treats the container as running until the `stop` or `die` event.
fn container_state(state: &str) -> Option<ContainerState> {
    match state {
        "stopping" => Some(ContainerState::Running),
        other => ContainerState::from_docker(other),
    }
}

fn container_status(summary: ContainerSummary) -> Option<ContainerStatus> {
    let id = summary.id.filter(|id| !id.is_empty())?;
    let state = container_state(summary.state?.as_ref())?;
    let name = summary
        .names
        .unwrap_or_default()
        .into_iter()
        .next()
        .map(|n| n.trim_start_matches('/').to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| id.clone());
    let mut labels = summary.labels.unwrap_or_default();
    let mut status = ContainerStatus::new(id, name, state);
    status.image = summary.image.filter(|i| !i.is_empty());
    status.project = labels.remove(PROJECT_LABEL);
    status.service = labels.remove(SERVICE_LABEL);
    status.working_dir = labels.remove(WORKING_DIR_LABEL);
    status.health = summary
        .health
        .and_then(|h| h.status)
        .and_then(|s| Health::from_docker(s.as_ref()));
    Some(status)
}

fn container_event(message: EventMessage) -> Option<ContainerEvent> {
    if message.typ != Some(EventMessageTypeEnum::CONTAINER) {
        return None;
    }
    let action = parse_action(message.action.as_deref()?)?;
    let actor = message.actor?;
    let id = actor.id.filter(|id| !id.is_empty())?;
    let mut attributes = actor.attributes.unwrap_or_default();
    let mut event = ContainerEvent::new(id, action);
    event.name = attributes.remove("name");
    event.image = attributes.remove("image");
    event.project = attributes.remove(PROJECT_LABEL);
    event.service = attributes.remove(SERVICE_LABEL);
    event.working_dir = attributes.remove(WORKING_DIR_LABEL);
    Some(event)
}

/// The API version the client is configured with.
///
/// bollard 0.21 builds request URLs that drop the version prefix, so the
/// engine answers at its own current version. If a later bollard sends the
/// prefix, 1.44 keeps working: it is the oldest version Docker 29 serves and
/// covers Docker Engine 25 and later, and every endpoint used here is far
/// older than that.
const API_VERSION: ClientVersion = ClientVersion {
    major_version: 1,
    minor_version: 44,
};

/// [`DockerApi`] over the engine's Unix socket, using `bollard`.
#[derive(Debug, Clone)]
pub struct BollardApi {
    docker: Docker,
}

impl BollardApi {
    /// Per-request timeout used by [`Self::connect`] callers that have no
    /// better value. Streams are only bounded until the response starts.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

    /// A client for `endpoint`.
    ///
    /// This checks that the socket exists but does not connect; the first
    /// request does. `timeout` is rounded up to whole seconds.
    pub fn connect(endpoint: &Endpoint, timeout: Duration) -> Result<Self, ApiError> {
        let socket = endpoint.socket();
        let path = socket
            .to_str()
            .ok_or_else(|| ApiError::InvalidSocketPath(socket.to_owned()))?;
        let secs = timeout.as_secs() + u64::from(timeout.subsec_nanos() > 0);
        let docker = Docker::connect_with_unix(path, secs.max(1), &API_VERSION)?;
        Ok(Self { docker })
    }
}

impl DockerApi for BollardApi {
    async fn ping(&self) -> Result<(), ApiError> {
        self.docker.ping().await?;
        Ok(())
    }

    async fn version(&self) -> Result<EngineVersion, ApiError> {
        Ok(engine_version(self.docker.version().await?))
    }

    async fn list_containers(&self) -> Result<Vec<ContainerStatus>, ApiError> {
        let options = ListContainersOptions {
            all: true,
            ..ListContainersOptions::default()
        };
        let summaries = self.docker.list_containers(Some(options)).await?;
        Ok(summaries.into_iter().filter_map(container_status).collect())
    }

    fn events(
        &self,
    ) -> impl Stream<Item = Result<ContainerEvent, ApiError>> + Send + Unpin + 'static {
        let filters = HashMap::from([("type".to_owned(), vec!["container".to_owned()])]);
        let options = EventsOptions {
            filters: Some(filters),
            ..EventsOptions::default()
        };
        self.docker
            .events(Some(options))
            .filter_map(|item| {
                std::future::ready(match item {
                    Ok(message) => container_event(message).map(Ok),
                    Err(error) => Some(Err(ApiError::from(error))),
                })
            })
            .boxed()
    }
}

impl BollardApi {
    /// `GET /containers/{id}/stats?stream=false&one-shot=true`: one reading
    /// of `container`'s raw counters, without waiting for the engine to
    /// compute a delta of its own.
    pub async fn container_stats(
        &self,
        container: &ContainerStatus,
    ) -> Result<ContainerSample, ApiError> {
        let options = StatsOptions {
            stream: false,
            one_shot: true,
        };
        let stats = self
            .docker
            .stats(&container.id, Some(options))
            .next()
            .await
            .ok_or_else(|| ApiError::Decode("the engine sent no stats".into()))??;
        Ok(container_sample(container, stats))
    }
}

/// The raw counters in `stats`, labelled with `container`'s identity.
fn container_sample(container: &ContainerStatus, stats: ContainerStatsResponse) -> ContainerSample {
    let cpu = stats.cpu_stats.unwrap_or_default();
    let memory = stats.memory_stats.unwrap_or_default();
    let cache = memory.stats.unwrap_or_default();
    let net = stats.networks.map(|networks| {
        networks
            .values()
            .fold(Traffic::default(), |total, network| Traffic {
                rx: total.rx.saturating_add(network.rx_bytes.unwrap_or(0)),
                tx: total.tx.saturating_add(network.tx_bytes.unwrap_or(0)),
            })
    });
    let block = stats
        .blkio_stats
        .and_then(|blkio| blkio.io_service_bytes_recursive)
        .map(|entries| {
            entries.iter().fold(DiskIo::default(), |total, entry| {
                let value = entry.value.unwrap_or(0);
                match entry.op.as_deref().map(str::to_ascii_lowercase).as_deref() {
                    Some("read") => DiskIo {
                        read: total.read.saturating_add(value),
                        ..total
                    },
                    Some("write") => DiskIo {
                        written: total.written.saturating_add(value),
                        ..total
                    },
                    _ => total,
                }
            })
        });
    ContainerSample {
        id: container.id.clone(),
        name: container.name.clone(),
        project: container.project.clone(),
        service: container.service.clone(),
        cpu_usage: cpu.cpu_usage.and_then(|u| u.total_usage).unwrap_or(0),
        system_cpu: cpu.system_cpu_usage,
        memory: container_memory(
            memory.usage.unwrap_or(0),
            cache.get("total_inactive_file").copied(),
            cache.get("inactive_file").copied(),
        ),
        net,
        block,
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
    use alchemy_core::engine::{ContainerAction, EngineFlavor, EngineState};
    use bollard::models::{
        ContainerSummaryHealth, ContainerSummaryHealthStatusEnum, ContainerSummaryStateEnum,
        EventActor, SystemVersionPlatform,
    };

    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn summary(id: &str, name: &str, state: ContainerSummaryStateEnum) -> ContainerSummary {
        ContainerSummary {
            id: Some(id.into()),
            names: Some(vec![name.into()]),
            state: Some(state),
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn container_status_maps_summaries() {
        use ContainerSummaryStateEnum as S;
        let full = ContainerSummary {
            image: Some("postgres:16".into()),
            labels: Some(labels(&[
                (PROJECT_LABEL, "blog"),
                (SERVICE_LABEL, "db"),
                (WORKING_DIR_LABEL, "/srv/blog"),
                ("other", "x"),
            ])),
            health: Some(ContainerSummaryHealth {
                status: Some(ContainerSummaryHealthStatusEnum::HEALTHY),
                ..ContainerSummaryHealth::default()
            }),
            ..summary("abc", "/blog-db-1", S::RUNNING)
        };
        let mut want = ContainerStatus::new("abc", "blog-db-1", ContainerState::Running);
        want.image = Some("postgres:16".into());
        want.project = Some("blog".into());
        want.service = Some("db".into());
        want.working_dir = Some("/srv/blog".into());
        want.health = Some(Health::Healthy);
        let no_health = ContainerSummary {
            health: Some(ContainerSummaryHealth {
                status: Some(ContainerSummaryHealthStatusEnum::NONE),
                ..ContainerSummaryHealth::default()
            }),
            ..summary("abc", "/web", S::EXITED)
        };
        let cases = [
            (full, Some(want)),
            (
                no_health,
                Some(ContainerStatus::new("abc", "web", ContainerState::Exited)),
            ),
            (
                summary("abc", "/web", S::STOPPING),
                Some(ContainerStatus::new("abc", "web", ContainerState::Running)),
            ),
            (
                ContainerSummary {
                    names: None,
                    ..summary("abc", "", S::CREATED)
                },
                Some(ContainerStatus::new("abc", "abc", ContainerState::Created)),
            ),
            (
                summary("abc", "/", S::DEAD),
                Some(ContainerStatus::new("abc", "abc", ContainerState::Dead)),
            ),
            (summary("abc", "/web", S::EMPTY), None),
            (summary("", "/web", S::RUNNING), None),
            (
                ContainerSummary {
                    state: None,
                    ..summary("abc", "/web", S::RUNNING)
                },
                None,
            ),
        ];
        for (input, want) in cases {
            assert_eq!(container_status(input.clone()), want, "{input:?}");
        }
    }

    fn message(typ: EventMessageTypeEnum, action: &str, id: &str) -> EventMessage {
        EventMessage {
            typ: Some(typ),
            action: Some(action.into()),
            actor: Some(EventActor {
                id: Some(id.into()),
                attributes: Some(labels(&[
                    ("name", "blog-db-1"),
                    ("image", "postgres:16"),
                    (PROJECT_LABEL, "blog"),
                    (SERVICE_LABEL, "db"),
                    (WORKING_DIR_LABEL, "/srv/blog"),
                ])),
            }),
            ..EventMessage::default()
        }
    }

    #[test]
    fn container_event_maps_messages() {
        use EventMessageTypeEnum::{CONTAINER, NETWORK};
        let mut start = ContainerEvent::new("abc", ContainerAction::Start);
        start.name = Some("blog-db-1".into());
        start.image = Some("postgres:16".into());
        start.project = Some("blog".into());
        start.service = Some("db".into());
        start.working_dir = Some("/srv/blog".into());
        let mut healthy = start.clone();
        healthy.action = ContainerAction::Health(Health::Healthy);
        let bare = EventMessage {
            actor: Some(EventActor {
                id: Some("abc".into()),
                attributes: None,
            }),
            ..message(CONTAINER, "die", "abc")
        };
        let cases = [
            (message(CONTAINER, "start", "abc"), Some(start)),
            (
                message(CONTAINER, "health_status: healthy", "abc"),
                Some(healthy),
            ),
            (bare, Some(ContainerEvent::new("abc", ContainerAction::Die))),
            (message(CONTAINER, "exec_start: sh", "abc"), None),
            (message(NETWORK, "start", "abc"), None),
            (message(CONTAINER, "start", ""), None),
            (
                EventMessage {
                    actor: None,
                    ..message(CONTAINER, "start", "abc")
                },
                None,
            ),
            (
                EventMessage {
                    typ: None,
                    ..message(CONTAINER, "start", "abc")
                },
                None,
            ),
        ];
        for (input, want) in cases {
            assert_eq!(container_event(input.clone()), want, "{input:?}");
        }
    }

    #[test]
    fn engine_version_fills_status() {
        let cases = [
            (
                SystemVersion {
                    version: Some("27.3.1".into()),
                    api_version: Some("1.47".into()),
                    platform: Some(SystemVersionPlatform {
                        name: "Docker Desktop 4.34.2 (167172)".into(),
                    }),
                    ..SystemVersion::default()
                },
                Some("27.3.1"),
                Some("1.47"),
                EngineFlavor::DockerDesktop,
                Some("4.34.2"),
            ),
            (
                SystemVersion {
                    version: Some("28.0.0".into()),
                    platform: Some(SystemVersionPlatform {
                        name: "Docker Engine - Community".into(),
                    }),
                    ..SystemVersion::default()
                },
                Some("28.0.0"),
                None,
                EngineFlavor::DockerEngine,
                None,
            ),
            (
                SystemVersion::default(),
                None,
                None,
                EngineFlavor::Other,
                None,
            ),
        ];
        for (input, version, api, flavor, desktop) in cases {
            let mut status = EngineStatus::new(EngineState::Ready);
            status.desktop_version = Some("stale".into());
            engine_version(input).apply_to(&mut status);
            assert_eq!(status.engine_version.as_deref(), version);
            assert_eq!(status.api_version.as_deref(), api);
            assert_eq!(status.flavor, Some(flavor));
            assert_eq!(status.desktop_version.as_deref(), desktop);
        }
    }

    #[test]
    fn bollard_errors_map_to_api_errors() {
        use bollard::errors::Error as E;
        let server = ApiError::from(E::DockerResponseServerError {
            status_code: 404,
            message: "no such container".into(),
        });
        assert!(
            matches!(server, ApiError::Server { status: 404, ref message } if message == "no such container")
        );
        assert!(matches!(
            ApiError::from(E::RequestTimeoutError),
            ApiError::Timeout
        ));
        assert!(matches!(
            ApiError::from(E::SocketNotFoundError("/x.sock".into())),
            ApiError::SocketNotFound(ref p) if p == &PathBuf::from("/x.sock")
        ));
        assert!(matches!(
            ApiError::from(E::HttpHeaderNotFoundError("x".into())),
            ApiError::Transport(_)
        ));
    }

    #[test]
    fn connect_requires_an_existing_socket() {
        let endpoint = Endpoint::unix("/nonexistent/alchemy-test/docker.sock").unwrap();
        assert!(matches!(
            BollardApi::connect(&endpoint, BollardApi::DEFAULT_TIMEOUT),
            Err(ApiError::SocketNotFound(_))
        ));
    }

    #[test]
    fn container_sample_maps_raw_stats() {
        use bollard::models::{
            ContainerBlkioStatEntry, ContainerBlkioStats, ContainerCpuStats, ContainerCpuUsage,
            ContainerMemoryStats, ContainerNetworkStats,
        };
        let entry = |op: &str, value| ContainerBlkioStatEntry {
            op: Some(op.into()),
            value: Some(value),
            ..ContainerBlkioStatEntry::default()
        };
        let network = |rx, tx| ContainerNetworkStats {
            rx_bytes: Some(rx),
            tx_bytes: Some(tx),
            ..ContainerNetworkStats::default()
        };
        let full = ContainerStatsResponse {
            cpu_stats: Some(ContainerCpuStats {
                cpu_usage: Some(ContainerCpuUsage {
                    total_usage: Some(500),
                    ..ContainerCpuUsage::default()
                }),
                system_cpu_usage: Some(10_000),
                ..ContainerCpuStats::default()
            }),
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(1_000),
                stats: Some(HashMap::from([("inactive_file".to_owned(), 300)])),
                ..ContainerMemoryStats::default()
            }),
            networks: Some(HashMap::from([
                ("eth0".to_owned(), network(100, 10)),
                ("eth1".to_owned(), network(5, 1)),
            ])),
            blkio_stats: Some(ContainerBlkioStats {
                io_service_bytes_recursive: Some(vec![
                    entry("read", 40),
                    entry("write", 7),
                    entry("Read", 2),
                    entry("discard", 99),
                ]),
                ..ContainerBlkioStats::default()
            }),
            ..ContainerStatsResponse::default()
        };
        let mut container = ContainerStatus::new("abc", "blog-db-1", ContainerState::Running);
        container.project = Some("blog".into());
        container.service = Some("db".into());

        let got = container_sample(&container, full);
        assert_eq!(
            got,
            ContainerSample {
                id: "abc".into(),
                name: "blog-db-1".into(),
                project: Some("blog".into()),
                service: Some("db".into()),
                cpu_usage: 500,
                system_cpu: Some(10_000),
                memory: 700,
                net: Some(Traffic { rx: 105, tx: 11 }),
                block: Some(DiskIo {
                    read: 42,
                    written: 7
                }),
            }
        );

        let empty = container_sample(&container, ContainerStatsResponse::default());
        assert_eq!(
            (empty.cpu_usage, empty.system_cpu, empty.memory),
            (0, None, 0)
        );
        assert_eq!((empty.net, empty.block), (None, None), "host network");
    }
}
