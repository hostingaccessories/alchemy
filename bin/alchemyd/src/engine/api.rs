//! The Docker Engine API behind [`DockerApi`], and the only place
//! `bollard` types are mapped to core types.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::time::Duration;

use alchemy_api::logs::LogStream;
use alchemy_core::engine::{
    ContainerEvent, ContainerState, ContainerStatus, EngineStatus, Health, InspectedExit,
    flavor_from_platform, parse_action, parse_timestamp_ms,
};
use alchemy_core::metrics::{ContainerSample, DiskIo, Traffic, container_memory};
use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::{PortRange, Protocol, PublishedPort};
use bollard::container::LogOutput;
use bollard::models::{
    ContainerInspectResponse, ContainerStatsResponse, ContainerSummary, EventMessage,
    EventMessageTypeEnum, Ipam, IpamConfig, Network, NetworkCreateRequest, PortSummary,
    PortSummaryTypeEnum, SystemInfo, SystemVersion,
};
use bollard::query_parameters::{
    EventsOptions, ListContainersOptions, ListNetworksOptions, LogsOptions, StatsOptions,
};
use bollard::{ClientVersion, Docker};
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt as _};

use super::Endpoint;
use crate::logs::{LogChunk, LogRequest};

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

    /// `GET /containers/{id}/json`, reduced to how its last run ended, or
    /// `None` if the engine did not say.
    ///
    /// The default reports nothing, for engines whose tests do not care.
    fn inspect_exit(
        &self,
        id: String,
    ) -> impl Future<Output = Result<Option<InspectedExit>, ApiError>> + Send {
        drop(id);
        std::future::ready(Ok(None))
    }

    /// `GET /networks`: every network the engine knows.
    ///
    /// The default reports none, for engines whose tests do not care.
    fn list_networks(&self) -> impl Future<Output = Result<Vec<DockerNetwork>, ApiError>> + Send {
        std::future::ready(Ok(Vec::new()))
    }

    /// `POST /networks/create`: a bridge network as `spec` describes,
    /// returning its ID.
    ///
    /// The default refuses, as an engine without network support would.
    fn create_network(
        &self,
        spec: NetworkSpec,
    ) -> impl Future<Output = Result<String, ApiError>> + Send {
        drop(spec);
        std::future::ready(Err(ApiError::Server {
            status: 501,
            message: "creating networks is not supported".to_owned(),
        }))
    }

    /// The IPv4 subnets of the default `bridge` network, from
    /// `GET /networks/bridge`. `/info` does not report them.
    ///
    /// The default reports none, for engines whose tests do not care.
    fn bridge_subnets(&self) -> impl Future<Output = Result<Vec<Ipv4Net>, ApiError>> + Send {
        std::future::ready(Ok(Vec::new()))
    }

    /// The address pools the engine was configured with, from
    /// `GET /info`. Empty when it uses Docker's built-in defaults.
    ///
    /// The default reports none, for engines whose tests do not care.
    fn address_pools(&self) -> impl Future<Output = Result<Vec<AddressPool>, ApiError>> + Send {
        std::future::ready(Ok(Vec::new()))
    }
}

/// A Docker network, reduced to what overlap checks need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerNetwork {
    /// Full network ID.
    pub id: String,
    /// Network name, such as `bridge` or `blog_default`.
    pub name: String,
    /// Driver, such as `bridge`, `host`, or `overlay`.
    pub driver: Option<String>,
    /// The IPv4 subnets from its IPAM config. IPv6 subnets are left out.
    pub subnets: Vec<Ipv4Net>,
    /// Its labels, without the Compose project label.
    pub labels: BTreeMap<String, String>,
    /// The Compose project that created it, from the
    /// `com.docker.compose.project` label.
    pub project: Option<String>,
}

/// A bridge network for [`DockerApi::create_network`] to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSpec {
    /// Network name.
    pub name: String,
    /// The one IPv4 subnet its IPAM config holds.
    pub subnet: Ipv4Net,
    /// Labels to set on it.
    pub labels: BTreeMap<String, String>,
}

/// One of the engine's `default-address-pools`: networks Docker creates
/// without a subnet are carved from `base` in blocks of `/size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressPool {
    /// The range blocks are carved from.
    pub base: Ipv4Net,
    /// The prefix length of each block.
    pub size: u8,
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
    status.ports = published_ports(summary.ports.unwrap_or_default());
    Some(status)
}

/// The host ports in a container listing's `Ports`, in a stable order.
///
/// Ports that are only exposed, and SCTP ports, are left out. Docker lists
/// a port published on every address twice, on `0.0.0.0` and on `::`; the
/// pair becomes one port with no host IP.
fn published_ports(ports: Vec<PortSummary>) -> Vec<PublishedPort> {
    let found: Vec<PublishedPort> = ports
        .into_iter()
        .filter_map(|port| {
            let protocol = match port.typ? {
                PortSummaryTypeEnum::TCP => Protocol::Tcp,
                PortSummaryTypeEnum::UDP => Protocol::Udp,
                PortSummaryTypeEnum::EMPTY | PortSummaryTypeEnum::SCTP => return None,
            };
            let published = PortRange::single(port.public_port?).ok()?;
            let host_ip = port.ip.as_deref().and_then(|ip| ip.parse().ok());
            Some(
                PublishedPort::new(protocol, Some(published))
                    .with_host_ip(host_ip)
                    .with_target(PortRange::single(port.private_port).ok()),
            )
        })
        .collect();
    let on = |port: &PublishedPort, ip: IpAddr| PublishedPort {
        host_ip: Some(ip),
        ..*port
    };
    let any_v4 = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let any_v6 = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    let mut merged: Vec<PublishedPort> = found
        .iter()
        .filter_map(|port| {
            let paired = |other: IpAddr| found.contains(&on(port, other));
            match port.host_ip {
                Some(ip) if ip == any_v4 && paired(any_v6) => Some(port.with_host_ip(None)),
                Some(ip) if ip == any_v6 && paired(any_v4) => None,
                _ => Some(*port),
            }
        })
        .collect();
    merged.sort_by_key(|p| (p.published, p.protocol, p.host_ip, p.target));
    merged.dedup();
    merged
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
    event.exit_code = attributes.remove("exitCode").and_then(|c| c.parse().ok());
    event.signal = attributes.remove("signal").and_then(|s| s.parse().ok());
    event.at = message
        .time_nano
        .map(|nanos| nanos / 1_000_000)
        .or_else(|| message.time.map(|secs| secs.saturating_mul(1_000)));
    Some(event)
}

fn inspected_exit(response: ContainerInspectResponse) -> Option<InspectedExit> {
    let state = response.state?;
    let time = |t: Option<String>| t.as_deref().and_then(parse_timestamp_ms);
    Some(InspectedExit {
        exit_code: state.exit_code,
        oom_killed: state.oom_killed.unwrap_or(false),
        started_at: time(state.started_at),
        finished_at: time(state.finished_at),
        restart_count: response
            .restart_count
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or(0),
    })
}

/// The IPv4 subnets in `ipam`'s config; IPv6 and unreadable ones are
/// left out.
fn ipv4_subnets(ipam: Option<Ipam>) -> Vec<Ipv4Net> {
    ipam.and_then(|ipam| ipam.config)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|config| config.subnet?.parse().ok())
        .collect()
}

fn docker_network(network: Network) -> Option<DockerNetwork> {
    let id = network.id.filter(|id| !id.is_empty())?;
    let name = network.name.filter(|name| !name.is_empty())?;
    let mut labels: BTreeMap<String, String> =
        network.labels.unwrap_or_default().into_iter().collect();
    Some(DockerNetwork {
        id,
        name,
        driver: network.driver.filter(|d| !d.is_empty()),
        subnets: ipv4_subnets(network.ipam),
        project: labels.remove(PROJECT_LABEL),
        labels,
    })
}

fn network_request(spec: NetworkSpec) -> NetworkCreateRequest {
    NetworkCreateRequest {
        name: spec.name,
        driver: Some("bridge".to_owned()),
        ipam: Some(Ipam {
            driver: Some("default".to_owned()),
            config: Some(vec![IpamConfig {
                subnet: Some(spec.subnet.to_string()),
                ..IpamConfig::default()
            }]),
            options: None,
        }),
        labels: Some(spec.labels.into_iter().collect()),
        ..NetworkCreateRequest::default()
    }
}

/// The IPv4 pools in `info`; IPv6 pools and unreadable entries are left
/// out.
fn address_pools(info: SystemInfo) -> Vec<AddressPool> {
    info.default_address_pools
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pool| {
            let base = pool.base?.parse().ok()?;
            let size = u8::try_from(pool.size?).ok().filter(|size| *size <= 32)?;
            Some(AddressPool { base, size })
        })
        .collect()
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

    async fn inspect_exit(&self, id: String) -> Result<Option<InspectedExit>, ApiError> {
        let response = self.docker.inspect_container(&id, None).await?;
        Ok(inspected_exit(response))
    }

    async fn list_networks(&self) -> Result<Vec<DockerNetwork>, ApiError> {
        let networks = self
            .docker
            .list_networks(None::<ListNetworksOptions>)
            .await?;
        Ok(networks.into_iter().filter_map(docker_network).collect())
    }

    async fn create_network(&self, spec: NetworkSpec) -> Result<String, ApiError> {
        Ok(self.docker.create_network(network_request(spec)).await?.id)
    }

    async fn bridge_subnets(&self) -> Result<Vec<Ipv4Net>, ApiError> {
        let bridge = self.docker.inspect_network("bridge", None).await?;
        Ok(ipv4_subnets(bridge.ipam))
    }

    async fn address_pools(&self) -> Result<Vec<AddressPool>, ApiError> {
        Ok(address_pools(self.docker.info().await?))
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

    /// `GET /containers/{id}/logs` for stdout and stderr, with Docker's
    /// timestamp before every line, as raw chunks in the order the engine
    /// sends them.
    ///
    /// Without `follow` the stream ends after the lines asked for; with
    /// it, when the container stops or the connection drops.
    pub fn logs(
        &self,
        id: &str,
        request: &LogRequest,
    ) -> BoxStream<'static, Result<LogChunk, ApiError>> {
        let options = LogsOptions {
            follow: request.follow,
            stdout: true,
            stderr: true,
            since: request
                .since
                .map_or(0, |secs| i32::try_from(secs.max(0)).unwrap_or(i32::MAX)),
            until: 0,
            timestamps: true,
            tail: request
                .tail
                .map_or_else(|| "all".to_owned(), |n| n.to_string()),
        };
        self.docker
            .logs(id, Some(options))
            .filter_map(|item| {
                std::future::ready(match item {
                    Ok(output) => log_chunk(output).map(Ok),
                    Err(error) => Some(Err(ApiError::from(error))),
                })
            })
            .boxed()
    }
}

/// A log frame as a [`LogChunk`]. A container with a terminal writes
/// everything to its console, which is reported as stdout; stdin echoes
/// are dropped.
fn log_chunk(output: LogOutput) -> Option<LogChunk> {
    let (stream, bytes) = match output {
        LogOutput::StdOut { message } | LogOutput::Console { message } => {
            (LogStream::Stdout, message)
        }
        LogOutput::StdErr { message } => (LogStream::Stderr, message),
        LogOutput::StdIn { .. } => return None,
    };
    Some(LogChunk {
        stream,
        bytes: bytes.to_vec(),
    })
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
    fn published_ports_keep_host_bindings() {
        use PortSummaryTypeEnum::{SCTP, TCP, UDP};
        let port = |ip: Option<&str>, private: u16, public: Option<u16>, typ| PortSummary {
            ip: ip.map(Into::into),
            private_port: private,
            public_port: public,
            typ: Some(typ),
        };
        let want = |s: &str, ip: Option<&str>, target: &str| {
            let (range, proto) = s.split_once('/').unwrap();
            PublishedPort::new(proto.parse().unwrap(), Some(range.parse().unwrap()))
                .with_host_ip(ip.map(|ip| ip.parse().unwrap()))
                .with_target(Some(target.parse().unwrap()))
        };
        let cases = [
            (
                vec![
                    port(Some("::"), 80, Some(8080), TCP),
                    port(Some("0.0.0.0"), 80, Some(8080), TCP),
                ],
                vec![want("8080/tcp", None, "80")],
            ),
            (
                vec![
                    port(Some("127.0.0.1"), 443, Some(8443), TCP),
                    port(Some("0.0.0.0"), 53, Some(53), UDP),
                    port(Some("::1"), 6000, Some(6000), TCP),
                ],
                vec![
                    want("53/udp", Some("0.0.0.0"), "53"),
                    want("6000/tcp", Some("::1"), "6000"),
                    want("8443/tcp", Some("127.0.0.1"), "443"),
                ],
            ),
            (
                vec![
                    port(None, 6379, None, TCP),
                    port(Some("0.0.0.0"), 9, Some(9), SCTP),
                    port(Some("0.0.0.0"), 1, Some(0), TCP),
                    PortSummary {
                        typ: None,
                        ..port(Some("0.0.0.0"), 1, Some(1), TCP)
                    },
                ],
                vec![],
            ),
            (
                vec![
                    port(Some("::"), 5000, Some(5000), UDP),
                    port(None, 5000, Some(5000), UDP),
                    port(None, 5000, Some(5000), UDP),
                ],
                vec![
                    want("5000/udp", None, "5000"),
                    want("5000/udp", Some("::"), "5000"),
                ],
            ),
        ];
        for (input, want) in cases {
            assert_eq!(published_ports(input.clone()), want, "{input:?}");
        }
    }

    #[test]
    fn container_status_maps_ports() {
        let listed = ContainerSummary {
            ports: Some(vec![PortSummary {
                ip: Some("0.0.0.0".into()),
                private_port: 80,
                public_port: Some(8080),
                typ: Some(PortSummaryTypeEnum::TCP),
            }]),
            ..summary("abc", "/web", ContainerSummaryStateEnum::RUNNING)
        };
        let status = container_status(listed).unwrap();
        assert_eq!(
            status
                .ports
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["0.0.0.0:8080/tcp"]
        );
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
    fn container_event_reads_exit_code_signal_and_time() {
        use EventMessageTypeEnum::CONTAINER;
        let with = |action: &str, attrs: &[(&str, &str)], time: Option<i64>, nanos: Option<i64>| {
            EventMessage {
                actor: Some(EventActor {
                    id: Some("abc".into()),
                    attributes: Some(labels(attrs)),
                }),
                time,
                time_nano: nanos,
                ..message(CONTAINER, action, "abc")
            }
        };
        let cases = [
            (
                with(
                    "die",
                    &[("exitCode", "137")],
                    Some(1_700_000_000),
                    Some(1_700_000_000_123_456_789),
                ),
                Some(137),
                None,
                Some(1_700_000_000_123),
            ),
            (
                with("kill", &[("signal", "15")], Some(1_700_000_000), None),
                None,
                Some(15),
                Some(1_700_000_000_000),
            ),
            (
                with("die", &[("exitCode", "x"), ("signal", "TERM")], None, None),
                None,
                None,
                None,
            ),
        ];
        for (input, exit_code, signal, at) in cases {
            let event = container_event(input.clone()).unwrap();
            assert_eq!(
                (event.exit_code, event.signal, event.at),
                (exit_code, signal, at),
                "{input:?}"
            );
        }
    }

    #[test]
    fn inspected_exit_maps_the_last_run() {
        use bollard::models::ContainerState as State;
        let full = ContainerInspectResponse {
            state: Some(State {
                exit_code: Some(137),
                oom_killed: Some(true),
                started_at: Some("2024-05-01T12:00:00Z".into()),
                finished_at: Some("2024-05-01T12:00:01.5Z".into()),
                ..State::default()
            }),
            restart_count: Some(4),
            ..ContainerInspectResponse::default()
        };
        let never = ContainerInspectResponse {
            state: Some(State {
                exit_code: Some(0),
                started_at: Some("0001-01-01T00:00:00Z".into()),
                finished_at: Some("0001-01-01T00:00:00Z".into()),
                ..State::default()
            }),
            restart_count: Some(-1),
            ..ContainerInspectResponse::default()
        };
        let cases = [
            (
                full,
                Some(InspectedExit {
                    exit_code: Some(137),
                    oom_killed: true,
                    started_at: Some(1_714_564_800_000),
                    finished_at: Some(1_714_564_801_500),
                    restart_count: 4,
                }),
            ),
            (
                never,
                Some(InspectedExit {
                    exit_code: Some(0),
                    ..InspectedExit::default()
                }),
            ),
            (ContainerInspectResponse::default(), None),
        ];
        for (input, want) in cases {
            assert_eq!(inspected_exit(input.clone()), want, "{input:?}");
        }
    }

    fn net(cidr: &str) -> Ipv4Net {
        cidr.parse().unwrap()
    }

    fn ipam(subnets: &[&str]) -> Ipam {
        Ipam {
            config: Some(
                subnets
                    .iter()
                    .map(|s| IpamConfig {
                        subnet: Some((*s).into()),
                        ..IpamConfig::default()
                    })
                    .collect(),
            ),
            ..Ipam::default()
        }
    }

    #[test]
    fn docker_network_maps_networks() {
        let compose = Network {
            id: Some("n1".into()),
            name: Some("blog_default".into()),
            driver: Some("bridge".into()),
            ipam: Some(ipam(&["172.18.0.0/16", "fd00::/64", "not a subnet"])),
            labels: Some(labels(&[
                (PROJECT_LABEL, "blog"),
                ("com.docker.compose.network", "default"),
            ])),
            ..Network::default()
        };
        let host = Network {
            id: Some("n2".into()),
            name: Some("host".into()),
            driver: Some("host".into()),
            ipam: Some(Ipam::default()),
            ..Network::default()
        };
        let cases = [
            (
                compose,
                Some(DockerNetwork {
                    id: "n1".into(),
                    name: "blog_default".into(),
                    driver: Some("bridge".into()),
                    subnets: vec![net("172.18.0.0/16")],
                    labels: BTreeMap::from([(
                        "com.docker.compose.network".to_owned(),
                        "default".to_owned(),
                    )]),
                    project: Some("blog".into()),
                }),
            ),
            (
                host,
                Some(DockerNetwork {
                    id: "n2".into(),
                    name: "host".into(),
                    driver: Some("host".into()),
                    subnets: vec![],
                    labels: BTreeMap::new(),
                    project: None,
                }),
            ),
            (
                Network {
                    name: Some("x".into()),
                    ..Network::default()
                },
                None,
            ),
            (
                Network {
                    id: Some("n3".into()),
                    name: Some(String::new()),
                    ..Network::default()
                },
                None,
            ),
        ];
        for (input, want) in cases {
            assert_eq!(docker_network(input.clone()), want, "{input:?}");
        }
    }

    #[test]
    fn network_request_creates_a_labelled_bridge_on_one_subnet() {
        let request = network_request(NetworkSpec {
            name: "alchemy".into(),
            subnet: net("10.200.0.0/24"),
            labels: BTreeMap::from([("com.alchemy.managed".to_owned(), "true".to_owned())]),
        });
        assert_eq!(
            request,
            NetworkCreateRequest {
                name: "alchemy".into(),
                driver: Some("bridge".into()),
                ipam: Some(Ipam {
                    driver: Some("default".into()),
                    config: Some(vec![IpamConfig {
                        subnet: Some("10.200.0.0/24".into()),
                        ..IpamConfig::default()
                    }]),
                    options: None,
                }),
                labels: Some(labels(&[("com.alchemy.managed", "true")])),
                ..NetworkCreateRequest::default()
            }
        );
    }

    #[test]
    fn address_pools_keep_ipv4_pools() {
        use bollard::models::SystemInfoDefaultAddressPools as Pool;
        let pool = |base: Option<&str>, size: Option<i64>| Pool {
            base: base.map(Into::into),
            size,
        };
        let info = SystemInfo {
            default_address_pools: Some(vec![
                pool(Some("172.17.0.0/16"), Some(16)),
                pool(Some("192.168.0.0/16"), Some(20)),
                pool(Some("fd00::/48"), Some(64)),
                pool(Some("10.0.0.0/8"), Some(33)),
                pool(Some("10.0.0.0/8"), Some(-1)),
                pool(None, Some(24)),
                pool(Some("10.0.0.0/8"), None),
            ]),
            ..SystemInfo::default()
        };
        assert_eq!(
            address_pools(info),
            [
                AddressPool {
                    base: net("172.17.0.0/16"),
                    size: 16
                },
                AddressPool {
                    base: net("192.168.0.0/16"),
                    size: 20
                },
            ]
        );
        assert_eq!(address_pools(SystemInfo::default()), []);
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

    #[test]
    fn log_chunk_maps_log_frames() {
        let bytes = || axum::body::Bytes::from_static(b"hi\n");
        let chunk = |stream| {
            Some(LogChunk {
                stream,
                bytes: b"hi\n".to_vec(),
            })
        };
        let cases = [
            (
                LogOutput::StdOut { message: bytes() },
                chunk(LogStream::Stdout),
            ),
            (
                LogOutput::StdErr { message: bytes() },
                chunk(LogStream::Stderr),
            ),
            (
                LogOutput::Console { message: bytes() },
                chunk(LogStream::Stdout),
            ),
            (LogOutput::StdIn { message: bytes() }, None),
        ];
        for (output, want) in cases {
            assert_eq!(log_chunk(output.clone()), want, "{output:?}");
        }
    }
}
