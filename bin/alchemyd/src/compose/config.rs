//! A project's resolved configuration, read with `docker compose config`.
//!
//! Compose itself resolves the project name (from the directory, a `name:`
//! in the compose file, or `COMPOSE_PROJECT_NAME` in `.env`) and merges
//! overrides, so reading its output back is the only way to agree with a
//! hand-run `docker compose` in the same directory.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::{PortRange, Protocol, PublishedPort};
use alchemy_core::route::{ExposedPort, XAlchemy, XAlchemyBlock};
use futures_util::future::BoxFuture;
use rustix::process::Signal;
use serde::Deserialize;
use serde::de::IgnoredAny;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use super::cli::{docker_command, process_group, signal, spawn_failure};
use super::runner::tail;
use super::{ComposeProject, ImageSource, child_env};
use crate::engine::{
    Endpoint, HostEnv, Os, SystemProbe, docker_binary_candidates, find_docker_binary,
};

/// What Alchemy uses from a project's resolved configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposeConfig {
    /// The project name Compose resolved.
    pub name: String,
    /// The services enabled without any profile, by name.
    pub services: Vec<ServiceConfig>,
    /// The networks its services use, by key. Compose leaves out the ones
    /// no service uses.
    pub networks: Vec<NetworkConfig>,
    /// The top-level `x-alchemy` block, or `None` when there is none. A
    /// block of the wrong shape is kept as unreadable, for planning to
    /// report.
    pub x_alchemy: Option<XAlchemyBlock>,
}

/// One service of a [`ComposeConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceConfig {
    /// The service name.
    pub name: String,
    /// Whether it has a `build:` section.
    pub builds: bool,
    /// The ports it publishes on the host, in the order Compose lists
    /// them. One with no `published` port is ephemeral.
    pub ports: Vec<PublishedPort>,
    /// The keys of the [`ComposeConfig::networks`] it joins, sorted. Empty
    /// for a service with a `network_mode`.
    pub networks: Vec<String>,
    /// The container ports it `expose`s without publishing, in the order
    /// Compose lists them.
    pub expose: Vec<ExposedPort>,
    /// Its own `x-alchemy` block, as for [`ComposeConfig::x_alchemy`].
    ///
    /// Compose 5.5 leaves service extensions out of `config --format
    /// json` (its YAML keeps them), so this is `None` in practice; only
    /// the top-level block reaches Alchemy today.
    pub x_alchemy: Option<XAlchemyBlock>,
}

/// One network of a [`ComposeConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkConfig {
    /// The key services refer to it by, such as `default`.
    pub key: String,
    /// The name Docker knows it by, such as `blog_default`.
    pub name: String,
    /// Whether it is created outside the project, so Compose only joins
    /// it.
    pub external: bool,
    /// The IPv4 subnets its `ipam` config asks for. Empty when Docker
    /// picks one from its address pools.
    pub subnets: Vec<Ipv4Net>,
}

#[derive(Deserialize)]
struct RawConfig {
    name: String,
    #[serde(default)]
    services: BTreeMap<String, RawService>,
    #[serde(default)]
    networks: BTreeMap<String, Lenient<Option<RawNetwork>>>,
    #[serde(default, rename = "x-alchemy")]
    x_alchemy: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawService {
    #[serde(default)]
    build: Option<IgnoredAny>,
    #[serde(default)]
    ports: Option<Lenient<Vec<Lenient<RawPort>>>>,
    #[serde(default)]
    networks: Option<Lenient<RawServiceNetworks>>,
    #[serde(default)]
    expose: Option<Lenient<Vec<Lenient<RawPorts>>>>,
    #[serde(default, rename = "x-alchemy")]
    x_alchemy: Option<serde_json::Value>,
}

/// A value Alchemy reads if it can and skips if it can't, so a shape it
/// doesn't expect never makes the whole configuration unreadable.
#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Read(T),
    Skipped(IgnoredAny),
}

impl<T> Lenient<T> {
    fn ok(self) -> Option<T> {
        match self {
            Self::Read(value) => Some(value),
            Self::Skipped(_) => None,
        }
    }
}

/// One entry of a service's `ports`, in the long syntax Compose prints
/// every port in.
#[derive(Deserialize)]
struct RawPort {
    #[serde(default)]
    target: Option<RawPorts>,
    #[serde(default)]
    published: Option<RawPorts>,
    #[serde(default)]
    host_ip: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
}

/// A port or range, which Compose prints as a string and older versions
/// as a number.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawPorts {
    Number(u32),
    Text(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawServiceNetworks {
    Map(BTreeMap<String, serde_json::Value>),
    List(Vec<String>),
}

#[derive(Default, Deserialize)]
struct RawNetwork {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    external: Option<RawExternal>,
    #[serde(default)]
    ipam: Option<Lenient<RawIpam>>,
}

/// `external: true`, or the older `external: {name: ...}`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawExternal {
    Flag(bool),
    Named(IgnoredAny),
}

#[derive(Deserialize)]
struct RawIpam {
    #[serde(default)]
    config: Option<Vec<Lenient<RawIpamPool>>>,
}

#[derive(Deserialize)]
struct RawIpamPool {
    #[serde(default)]
    subnet: Option<String>,
}

impl RawPorts {
    fn range(&self) -> Option<PortRange> {
        match self {
            Self::Number(n) => PortRange::single(u16::try_from(*n).ok()?).ok(),
            Self::Text(text) => text.trim().parse().ok(),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Self::Text(text) if text.trim().is_empty())
    }

    /// An `expose` entry: `80`, `"80"`, `"8000-8010"`, or `"53/udp"`.
    fn exposed(&self) -> Option<ExposedPort> {
        match self {
            Self::Number(_) => Some(ExposedPort {
                ports: self.range()?,
                protocol: Protocol::Tcp,
            }),
            Self::Text(text) => text.parse().ok(),
        }
    }
}

/// An `x-alchemy` value as [`XAlchemyBlock`]: read, or unreadable with
/// the reason. `null` counts as no block.
fn x_alchemy_block(value: Option<serde_json::Value>) -> Option<XAlchemyBlock> {
    let value = value.filter(|value| !value.is_null())?;
    Some(match serde_json::from_value::<XAlchemy>(value) {
        Ok(block) => XAlchemyBlock::Read(block),
        Err(error) => XAlchemyBlock::Unreadable(error.to_string()),
    })
}

impl RawPort {
    /// The port, or `None` if Docker could not publish it as read: an
    /// unknown protocol such as `sctp`, or an unreadable port or address.
    fn published(self) -> Option<PublishedPort> {
        let protocol = match self.protocol.as_deref().map(str::trim) {
            None | Some("") => Protocol::Tcp,
            Some(other) => other.parse().ok()?,
        };
        let host_ip = match self.host_ip.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(ip) => Some(parse_host_ip(ip)?),
        };
        let published = match self.published {
            None => None,
            Some(ports) if ports.is_empty() => None,
            Some(ports) => Some(ports.range()?),
        };
        let target = match self.target {
            None => None,
            Some(ports) => Some(ports.range()?),
        };
        Some(
            PublishedPort::new(protocol, published)
                .with_host_ip(host_ip)
                .with_target(target),
        )
    }
}

/// A host IP as Compose prints it, with or without IPv6 brackets.
fn parse_host_ip(ip: &str) -> Option<IpAddr> {
    let bare = ip
        .strip_prefix('[')
        .and_then(|ip| ip.strip_suffix(']'))
        .unwrap_or(ip);
    bare.parse().ok()
}

impl RawServiceNetworks {
    fn keys(self) -> Vec<String> {
        let mut keys = match self {
            Self::Map(map) => map.into_keys().collect(),
            Self::List(list) => list,
        };
        keys.sort();
        keys.dedup();
        keys
    }
}

impl RawNetwork {
    fn config(self, key: String) -> NetworkConfig {
        NetworkConfig {
            name: self.name.unwrap_or_else(|| key.clone()),
            external: matches!(
                self.external,
                Some(RawExternal::Flag(true) | RawExternal::Named(_))
            ),
            subnets: self
                .ipam
                .and_then(Lenient::ok)
                .and_then(|ipam| ipam.config)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|pool| pool.ok()?.subnet?.trim().parse().ok())
                .collect(),
            key,
        }
    }
}

impl ComposeConfig {
    /// Parse the output of `docker compose config --format json`.
    ///
    /// A port, `expose`, network, or `ipam` entry in a shape Alchemy
    /// can't read is left out rather than failing the whole
    /// configuration. An `x-alchemy` block of the wrong shape is kept as
    /// [`XAlchemyBlock::Unreadable`], so the user hears about it.
    pub fn parse(json: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: RawConfig = serde_json::from_slice(json)?;
        Ok(Self {
            name: raw.name,
            services: raw
                .services
                .into_iter()
                .map(|(name, service)| ServiceConfig {
                    name,
                    builds: service.build.is_some(),
                    ports: service
                        .ports
                        .and_then(Lenient::ok)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|port| port.ok()?.published())
                        .collect(),
                    networks: service
                        .networks
                        .and_then(Lenient::ok)
                        .map(RawServiceNetworks::keys)
                        .unwrap_or_default(),
                    expose: service
                        .expose
                        .and_then(Lenient::ok)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|port| port.ok()?.exposed())
                        .collect(),
                    x_alchemy: x_alchemy_block(service.x_alchemy),
                })
                .collect(),
            networks: raw
                .networks
                .into_iter()
                .map(|(key, network)| network.ok().flatten().unwrap_or_default().config(key))
                .collect(),
            x_alchemy: x_alchemy_block(raw.x_alchemy),
        })
    }

    /// Where the project's images come from: built if any service builds.
    pub fn images(&self) -> ImageSource {
        if self.services.iter().any(|s| s.builds) {
            ImageSource::Build
        } else {
            ImageSource::Registry
        }
    }
}

/// The argv after the `docker` binary that prints `project`'s resolved
/// configuration as JSON.
///
/// The same `--project-directory` form as every [`ComposeStep`], with no
/// `-p` or `-f`, so Compose picks the file and the name itself.
///
/// [`ComposeStep`]: super::ComposeStep
pub fn config_args(project: &ComposeProject) -> Vec<OsString> {
    vec![
        "compose".into(),
        "--project-directory".into(),
        project.dir().as_os_str().to_owned(),
        "config".into(),
        "--format".into(),
        "json".into(),
    ]
}

/// The reason to report from a failed `config`'s standard error.
///
/// Compose logs warnings (such as an unset variable) as
/// `time=... level=warning msg=...` lines before the error itself; those
/// are dropped, and the end of what is left is kept.
pub fn config_error_message(stderr: &str) -> String {
    let kept: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains("level=warning"))
        .collect();
    tail(&kept.join("\n"))
}

/// Why a project's configuration could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// No `docker` binary was found.
    #[error("the docker command wasn't found")]
    NoDocker,
    /// The project's directory is gone, so Compose could not run in it.
    #[error("the project directory {} doesn't exist", dir.display())]
    MissingDir {
        /// The directory Compose should have run in.
        dir: PathBuf,
    },
    /// The binary could not be started.
    #[error("failed to run {}", binary.display())]
    Spawn {
        /// The binary that was run.
        binary: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Compose did not answer in time and was stopped.
    #[error("docker compose config did not finish within {after:?}")]
    Timeout {
        /// How long it was given.
        after: Duration,
    },
    /// Compose rejected the project; `message` is its own reason.
    #[error("{message}")]
    Invalid {
        /// The exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// Compose's message.
        message: String,
    },
    /// Compose printed something that is not a configuration.
    #[error("docker compose config printed output Alchemy can't read")]
    Unreadable(#[source] Option<serde_json::Error>),
}

impl ConfigError {
    /// Whether Compose itself rejected the project's files, as opposed to
    /// Compose not running at all.
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid { .. })
    }
}

/// Reads projects' resolved configuration.
///
/// Object safe, so the daemon can hold any implementation behind an
/// `Arc`. [`CliComposeConfig`] runs `docker compose config`;
/// [`FakeComposeConfig`] answers from a table, for tests.
pub trait ComposeConfigReader: Send + Sync + fmt::Debug {
    /// The resolved configuration of `project`, with Compose pointed at
    /// `endpoint`.
    fn read<'a>(
        &'a self,
        project: &'a ComposeProject,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>>;
}

/// [`ComposeConfigReader`] that runs `docker compose config` as a child.
///
/// It looks for the `docker` binary on every read, so installing Docker
/// later is picked up, and runs it as [`CliComposeRunner`] does: an
/// absolute path, the [`config_args`], no shell, and exactly [`child_env`]
/// of the daemon's environment. A run past the timeout is killed.
///
/// [`CliComposeRunner`]: super::CliComposeRunner
#[derive(Debug, Clone)]
pub struct CliComposeConfig {
    binaries: Vec<PathBuf>,
    parent_env: Vec<(OsString, OsString)>,
    timeout: Duration,
}

/// The largest configuration read, in bytes.
const MAX_CONFIG: usize = 8 * 1024 * 1024;

/// The most standard error kept, in bytes.
const MAX_STDERR: usize = 64 * 1024;

impl CliComposeConfig {
    /// How long one read may take by default.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Look for `docker` where the engine monitor does, given this
    /// process's environment.
    pub fn system() -> Self {
        let env = HostEnv::current();
        Self::with_candidates(docker_binary_candidates(
            env.path.as_deref(),
            env.home.as_deref(),
            Os::current(),
        ))
    }

    /// Always run the binary at `binary`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self::with_candidates(vec![binary.into()])
    }

    fn with_candidates(binaries: Vec<PathBuf>) -> Self {
        Self {
            binaries: binaries.into_iter().filter(|b| b.is_absolute()).collect(),
            parent_env: std::env::vars_os().collect(),
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Build the child's environment from `parent` instead of the daemon's.
    #[must_use]
    pub fn with_parent_env<K, V>(mut self, parent: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.parent_env = parent
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    /// Use `timeout` for each read instead of the default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run `docker` in `dir` with `args`, which end in
    /// `config --format json`, and parse what it prints.
    pub(super) async fn run(
        &self,
        dir: &Path,
        args: &[OsString],
        endpoint: &Endpoint,
    ) -> Result<ComposeConfig, ConfigError> {
        let binary =
            find_docker_binary(&SystemProbe, &self.binaries).ok_or(ConfigError::NoDocker)?;
        let env = child_env(self.parent_env.iter().cloned(), endpoint.docker_host());
        let mut child = docker_command(&binary, dir, args, &env)
            .spawn()
            .map_err(|source| match spawn_failure(dir, source) {
                Ok(dir) => ConfigError::MissingDir { dir },
                Err(source) => ConfigError::Spawn {
                    binary: binary.clone(),
                    source,
                },
            })?;
        let group = process_group(&child);
        let stdout = read_capped(child.stdout.take(), MAX_CONFIG + 1);
        let stderr = read_capped(child.stderr.take(), MAX_STDERR);
        let finished = async { tokio::join!(stdout, stderr, child.wait()) };
        let Ok((stdout, stderr, status)) = tokio::time::timeout(self.timeout, finished).await
        else {
            signal(group, Signal::KILL);
            return Err(ConfigError::Timeout {
                after: self.timeout,
            });
        };
        let status = status.map_err(|source| ConfigError::Spawn {
            binary: binary.clone(),
            source,
        })?;
        if !status.success() {
            return Err(ConfigError::Invalid {
                code: status.code(),
                message: config_error_message(&String::from_utf8_lossy(&stderr)),
            });
        }
        if stdout.len() > MAX_CONFIG {
            return Err(ConfigError::Unreadable(None));
        }
        ComposeConfig::parse(&stdout).map_err(|e| ConfigError::Unreadable(Some(e)))
    }
}

impl ComposeConfigReader for CliComposeConfig {
    fn read<'a>(
        &'a self,
        project: &'a ComposeProject,
        endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>> {
        Box::pin(async move {
            self.run(project.dir(), &config_args(project), endpoint)
                .await
        })
    }
}

/// Read `reader` to its end, keeping at most `max` bytes. A read error ends
/// it like end of file: the exit status decides the outcome.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, max: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let Some(mut reader) = reader else {
        return kept;
    };
    let mut buffer = vec![0; 8 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                let room = max.saturating_sub(kept.len());
                kept.extend_from_slice(buffer.get(..read.min(room)).unwrap_or_default());
            }
        }
    }
}

/// What [`FakeComposeConfig`] answers for one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeConfig {
    /// This configuration.
    Config(ComposeConfig),
    /// [`ConfigError::Invalid`] with this message.
    Invalid(String),
    /// [`ConfigError::NoDocker`].
    NoDocker,
}

/// [`ComposeConfigReader`] that answers from a table of project
/// directories and counts reads, for tests of what sits above it.
///
/// A directory not in the table reads as `Invalid("no configuration file
/// provided: not found")`, as Compose says for a directory without one.
#[derive(Debug, Clone, Default)]
pub struct FakeComposeConfig {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug, Default)]
struct FakeInner {
    answers: HashMap<PathBuf, FakeConfig>,
    reads: HashMap<PathBuf, usize>,
}

impl FakeComposeConfig {
    /// A reader with no projects.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `answer` for the project in `dir` from now on. Clones share
    /// the table, so a test can change answers while the daemon runs.
    pub fn set(&self, dir: impl Into<PathBuf>, answer: FakeConfig) {
        self.lock().answers.insert(dir.into(), answer);
    }

    /// Answer a project named `name` with `services`, none of which build.
    pub fn project(&self, dir: impl Into<PathBuf>, name: &str, services: &[&str]) {
        self.set(
            dir,
            FakeConfig::Config(ComposeConfig {
                name: name.to_owned(),
                services: services
                    .iter()
                    .map(|s| ServiceConfig {
                        name: (*s).to_owned(),
                        ..ServiceConfig::default()
                    })
                    .collect(),
                ..ComposeConfig::default()
            }),
        );
    }

    /// How many times the project in `dir` has been read.
    pub fn reads(&self, dir: &Path) -> usize {
        self.lock().reads.get(dir).copied().unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ComposeConfigReader for FakeComposeConfig {
    fn read<'a>(
        &'a self,
        project: &'a ComposeProject,
        _endpoint: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ComposeConfig, ConfigError>> {
        let answer = {
            let mut inner = self.lock();
            *inner.reads.entry(project.dir().to_owned()).or_default() += 1;
            inner.answers.get(project.dir()).cloned()
        };
        Box::pin(async move {
            match answer {
                Some(FakeConfig::Config(config)) => Ok(config),
                Some(FakeConfig::Invalid(message)) => Err(ConfigError::Invalid {
                    code: Some(1),
                    message,
                }),
                Some(FakeConfig::NoDocker) => Err(ConfigError::NoDocker),
                None => Err(ConfigError::Invalid {
                    code: Some(1),
                    message: "no configuration file provided: not found".to_owned(),
                }),
            }
        })
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
    use super::*;

    #[test]
    fn parses_the_name_services_and_builds() {
        let json = br#"{
            "name": "blog",
            "networks": {"default": {"name": "blog_default"}},
            "services": {
                "web": {"image": "nginx", "command": null},
                "api": {"build": {"context": "/srv/blog", "dockerfile": "Dockerfile"}},
                "worker": {"image": "x", "build": null}
            }
        }"#;
        let config = ComposeConfig::parse(json).unwrap();
        assert_eq!(config.name, "blog");
        let services: Vec<_> = config
            .services
            .iter()
            .map(|s| (s.name.as_str(), s.builds))
            .collect();
        assert_eq!(services, [("api", true), ("web", false), ("worker", false)]);
        assert_eq!(config.images(), ImageSource::Build);
    }

    #[test]
    fn decides_where_images_come_from() {
        let cases: [(&str, ImageSource); 3] = [
            (r#"{"name":"a"}"#, ImageSource::Registry),
            (
                r#"{"name":"a","services":{"x":{"image":"redis"}}}"#,
                ImageSource::Registry,
            ),
            (
                r#"{"name":"a","services":{"x":{"image":"redis"},"y":{"build":"."}}}"#,
                ImageSource::Build,
            ),
        ];
        for (json, want) in cases {
            assert_eq!(
                ComposeConfig::parse(json.as_bytes()).unwrap().images(),
                want,
                "{json}"
            );
        }
    }

    /// What Compose 5.5 prints for a project using every port syntax,
    /// trimmed to the fields read here. The compose file was:
    ///
    /// ```yaml
    /// services:
    ///   web:
    ///     ports:
    ///       - "8080:80"
    ///       - "127.0.0.1:8443:443/tcp"
    ///       - "53:53/udp"
    ///       - "9000-9001:9000-9001"
    ///       - "3000"
    ///       - "[::1]:6000:6000"
    ///       - "8000-8005:80"
    ///       - "127.0.0.1::81"
    ///       - {target: 5000, published: "5001", host_ip: 0.0.0.0, protocol: udp, mode: host}
    ///     networks: [front, back, outside]
    ///   db:
    ///     networks: {back: {ipv4_address: 10.99.0.10}}
    ///   plain: {}
    /// networks:
    ///   front: {}
    ///   back:
    ///     ipam:
    ///       config: [{subnet: 10.99.0.0/24, gateway: 10.99.0.1}, {subnet: "fd00:99::/64"}]
    ///   outside: {external: true, name: shared_net}
    /// ```
    const PORTS_AND_NETWORKS: &str = r#"{
      "name": "fixture",
      "networks": {
        "back": {"name": "fixture_back", "ipam": {"driver": "default", "config": [
          {"subnet": "10.99.0.0/24", "gateway": "10.99.0.1"}, {"subnet": "fd00:99::/64"}]}},
        "default": {"name": "fixture_default", "ipam": {}},
        "front": {"name": "fixture_front", "ipam": {}},
        "outside": {"name": "shared_net", "ipam": {}, "external": true}
      },
      "services": {
        "db": {"command": null, "image": "postgres",
          "networks": {"back": {"aliases": ["database"], "ipv4_address": "10.99.0.10"}}},
        "plain": {"command": null, "expose": ["6379"], "image": "redis", "networks": {"default": null}},
        "web": {"command": null, "image": "nginx",
          "networks": {"back": null, "front": null, "outside": {}},
          "ports": [
            {"mode": "ingress", "target": 80, "published": "8080", "protocol": "tcp"},
            {"mode": "ingress", "host_ip": "127.0.0.1", "target": 443, "published": "8443", "protocol": "tcp"},
            {"mode": "ingress", "target": 53, "published": "53", "protocol": "udp"},
            {"mode": "ingress", "target": 9000, "published": "9000", "protocol": "tcp"},
            {"mode": "ingress", "target": 9001, "published": "9001", "protocol": "tcp"},
            {"mode": "ingress", "target": 3000, "protocol": "tcp"},
            {"mode": "ingress", "host_ip": "::1", "target": 6000, "published": "6000", "protocol": "tcp"},
            {"mode": "ingress", "target": 80, "published": "8000-8005", "protocol": "tcp"},
            {"mode": "ingress", "host_ip": "127.0.0.1", "target": 81, "protocol": "tcp"},
            {"mode": "host", "host_ip": "0.0.0.0", "target": 5000, "published": "5001", "protocol": "udp"}
          ]}
      }
    }"#;

    fn port(spec: &str) -> PublishedPort {
        let (rest, protocol) = spec.rsplit_once('/').unwrap();
        let (host, target) = rest.rsplit_once("->").unwrap();
        let (ip, published) = match host.rsplit_once('|') {
            Some((ip, published)) => (Some(ip.parse().unwrap()), published),
            None => (None, host),
        };
        PublishedPort::new(
            protocol.parse().unwrap(),
            (!published.is_empty()).then(|| published.parse().unwrap()),
        )
        .with_host_ip(ip)
        .with_target(Some(target.parse().unwrap()))
    }

    fn service<'a>(config: &'a ComposeConfig, name: &str) -> &'a ServiceConfig {
        config.services.iter().find(|s| s.name == name).unwrap()
    }

    #[test]
    fn parses_published_ports() {
        let config = ComposeConfig::parse(PORTS_AND_NETWORKS.as_bytes()).unwrap();
        assert_eq!(
            service(&config, "web").ports,
            [
                port("8080->80/tcp"),
                port("127.0.0.1|8443->443/tcp"),
                port("53->53/udp"),
                port("9000->9000/tcp"),
                port("9001->9001/tcp"),
                port("->3000/tcp"),
                port("::1|6000->6000/tcp"),
                port("8000-8005->80/tcp"),
                port("127.0.0.1|->81/tcp"),
                port("0.0.0.0|5001->5000/udp"),
            ]
        );
        assert!(service(&config, "web").ports[5].is_ephemeral());
        assert_eq!(service(&config, "db").ports, []);
        assert_eq!(
            service(&config, "plain").ports,
            [],
            "expose publishes nothing"
        );
    }

    #[test]
    fn parses_networks_and_their_subnets() {
        let config = ComposeConfig::parse(PORTS_AND_NETWORKS.as_bytes()).unwrap();
        let joined: Vec<_> = config
            .services
            .iter()
            .map(|s| (s.name.as_str(), s.networks.clone()))
            .collect();
        assert_eq!(
            joined,
            [
                ("db", vec!["back".to_owned()]),
                ("plain", vec!["default".to_owned()]),
                (
                    "web",
                    vec!["back".to_owned(), "front".to_owned(), "outside".to_owned()]
                ),
            ]
        );
        let network = |key: &str, name: &str, external: bool, subnets: &[&str]| NetworkConfig {
            key: key.into(),
            name: name.into(),
            external,
            subnets: subnets.iter().map(|s| s.parse().unwrap()).collect(),
        };
        assert_eq!(
            config.networks,
            [
                network("back", "fixture_back", false, &["10.99.0.0/24"]),
                network("default", "fixture_default", false, &[]),
                network("front", "fixture_front", false, &[]),
                network("outside", "shared_net", true, &[]),
            ]
        );
    }

    #[test]
    fn reads_each_port_field_leniently() {
        let cases: [(&str, Option<&str>); 14] = [
            (r#"{"target":80,"published":"8080"}"#, Some("8080->80/tcp")),
            (r#"{"target":80,"published":8080}"#, Some("8080->80/tcp")),
            (
                r#"{"target":"80","published":"8080","protocol":"UDP"}"#,
                Some("8080->80/udp"),
            ),
            (r#"{"target":80,"published":""}"#, Some("->80/tcp")),
            (r#"{"target":80,"published":null}"#, Some("->80/tcp")),
            (
                r#"{"target":80,"published":"8080","host_ip":""}"#,
                Some("8080->80/tcp"),
            ),
            (
                r#"{"target":80,"published":"8080","host_ip":"[::]"}"#,
                Some("::|8080->80/tcp"),
            ),
            (
                r#"{"target":80,"published":"8080","protocol":""}"#,
                Some("8080->80/tcp"),
            ),
            (
                r#"{"target":80,"published":"8080","protocol":"sctp"}"#,
                None,
            ),
            (r#"{"target":80,"published":"0"}"#, None),
            (r#"{"target":80,"published":"9-1"}"#, None),
            (r#"{"target":80,"published":70000}"#, None),
            (
                r#"{"target":80,"published":"8080","host_ip":"localhost"}"#,
                None,
            ),
            (r#""8080:80""#, None),
        ];
        for (raw, want) in cases {
            let json = format!(r#"{{"name":"a","services":{{"web":{{"ports":[{raw}]}}}}}}"#);
            let config = ComposeConfig::parse(json.as_bytes()).unwrap();
            assert_eq!(
                config.services[0].ports,
                want.map(port).into_iter().collect::<Vec<_>>(),
                "{raw}"
            );
        }
    }

    #[test]
    fn parses_exposed_ports() {
        let config = ComposeConfig::parse(PORTS_AND_NETWORKS.as_bytes()).unwrap();
        assert_eq!(
            service(&config, "plain").expose,
            [ExposedPort {
                protocol: Protocol::Tcp,
                ports: PortRange::single(6379).unwrap(),
            }]
        );
        assert_eq!(service(&config, "web").expose, []);
    }

    #[test]
    fn reads_each_expose_entry_leniently() {
        let exposed = |protocol, ports: &str| ExposedPort {
            protocol,
            ports: ports.parse().unwrap(),
        };
        let cases: [(&str, Option<ExposedPort>); 11] = [
            (r#""80""#, Some(exposed(Protocol::Tcp, "80"))),
            ("80", Some(exposed(Protocol::Tcp, "80"))),
            (
                r#"" 8000-8010 ""#,
                Some(exposed(Protocol::Tcp, "8000-8010")),
            ),
            (r#""53/udp""#, Some(exposed(Protocol::Udp, "53"))),
            (r#""53/UDP""#, Some(exposed(Protocol::Udp, "53"))),
            (r#""9000/tcp""#, Some(exposed(Protocol::Tcp, "9000"))),
            (r#""9000/sctp""#, None),
            (r#""0""#, None),
            ("70000", None),
            (r#""web""#, None),
            (r#"{"target":80}"#, None),
        ];
        for (raw, want) in cases {
            let json = format!(r#"{{"name":"a","services":{{"web":{{"expose":[{raw}]}}}}}}"#);
            let config = ComposeConfig::parse(json.as_bytes()).unwrap();
            assert_eq!(
                config.services[0].expose,
                want.into_iter().collect::<Vec<_>>(),
                "{raw}"
            );
        }
        let odd = ComposeConfig::parse(br#"{"name":"a","services":{"web":{"expose":"80"}}}"#);
        assert_eq!(odd.unwrap().services[0].expose, []);
    }

    /// Extensions pass through as written. Compose 5.5 prints the
    /// top-level block and drops per-service ones from its JSON, but a
    /// version that keeps them is read the same way.
    #[test]
    fn keeps_x_alchemy_blocks_as_written() {
        let json = br#"{
            "name": "blog",
            "x-alchemy": {"service": "web", "port": 8080, "hostname": "blog"},
            "x-other": {"ignored": true},
            "services": {
                "web": {"image": "nginx", "x-alchemy": {"port": "80"}},
                "db": {"image": "postgres", "x-alchemy": null},
                "odd": {"image": "redis", "x-alchemy": ["not", "a", "map"]},
                "bad": {"image": "redis", "x-alchemy": {"port": "http"}},
                "plain": {"image": "redis"}
            }
        }"#;
        let config = ComposeConfig::parse(json).unwrap();
        assert_eq!(
            config.x_alchemy,
            Some(XAlchemyBlock::Read(XAlchemy {
                service: Some("web".into()),
                port: Some(8080),
                hostname: Some("blog".into()),
            }))
        );
        assert_eq!(
            service(&config, "web").x_alchemy,
            Some(XAlchemyBlock::Read(XAlchemy {
                port: Some(80),
                ..XAlchemy::default()
            }))
        );
        assert_eq!(service(&config, "db").x_alchemy, None);
        for name in ["odd", "bad"] {
            assert!(
                matches!(
                    &service(&config, name).x_alchemy,
                    Some(XAlchemyBlock::Unreadable(reason)) if !reason.is_empty()
                ),
                "{name}: {:?}",
                service(&config, name).x_alchemy
            );
        }
        assert_eq!(service(&config, "plain").x_alchemy, None);
        assert_eq!(
            ComposeConfig::parse(br#"{"name":"a"}"#).unwrap().x_alchemy,
            None
        );
    }

    #[test]
    fn reads_networks_leniently() {
        let json = br#"{
            "name": "a",
            "networks": {
                "old": {"external": {"name": "legacy"}},
                "bare": null,
                "odd": {"ipam": {"config": [{"subnet": "nope"}, {"subnet": "10.1.0.0/16"}, 7]}},
                "weird": {"ipam": "x"},
                "broken": 5
            },
            "services": {
                "list": {"networks": ["b", "a", "b"]},
                "bad": {"networks": 3, "ports": "x"}
            }
        }"#;
        let config = ComposeConfig::parse(json).unwrap();
        let summary: Vec<_> = config
            .networks
            .iter()
            .map(|n| {
                (
                    n.key.as_str(),
                    n.name.as_str(),
                    n.external,
                    n.subnets
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("bare", "bare", false, vec![]),
                ("broken", "broken", false, vec![]),
                ("odd", "odd", false, vec!["10.1.0.0/16".to_owned()]),
                ("old", "old", true, vec![]),
                ("weird", "weird", false, vec![]),
            ]
        );
        assert_eq!(service(&config, "list").networks, ["a", "b"]);
        assert_eq!(service(&config, "bad").networks, [] as [String; 0]);
    }

    #[test]
    fn rejects_output_without_a_name() {
        for json in [r#"{"services":{}}"#, "not json", "", r#"{"name":1}"#] {
            assert!(ComposeConfig::parse(json.as_bytes()).is_err(), "{json}");
        }
    }

    #[test]
    fn config_args_let_compose_pick_the_file_and_name() {
        let project = ComposeProject::new("/srv/blog").unwrap();
        let args: Vec<_> = config_args(&project)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "compose",
                "--project-directory",
                "/srv/blog",
                "config",
                "--format",
                "json"
            ]
        );
    }

    #[test]
    fn error_message_drops_warnings() {
        let cases = [
            ("", ""),
            (
                "no configuration file provided: not found\n",
                "no configuration file provided: not found",
            ),
            (
                concat!(
                    "time=\"2026-10-23T20:54:55-04:00\" level=warning msg=\"The \\\"X\\\" variable is not set.\"\n",
                    "\n",
                    "yaml: line 4: did not find expected node content\n",
                ),
                "yaml: line 4: did not find expected node content",
            ),
            ("a\nb\n", "a\nb"),
        ];
        for (stderr, want) in cases {
            assert_eq!(config_error_message(stderr), want, "{stderr:?}");
        }
    }

    #[tokio::test]
    async fn fake_answers_from_its_table_and_counts_reads() {
        let fake = FakeComposeConfig::new();
        let blog = ComposeProject::new("/srv/blog").unwrap();
        let other = ComposeProject::new("/srv/other").unwrap();
        fake.project("/srv/blog", "blog", &["web"]);
        let endpoint = Endpoint::system();

        let config = fake.read(&blog, &endpoint).await.unwrap();
        assert_eq!(config.name, "blog");
        assert_eq!(config.images(), ImageSource::Registry);
        let err = fake.read(&other, &endpoint).await.unwrap_err();
        assert!(err.is_invalid(), "{err:?}");
        fake.set("/srv/blog", FakeConfig::NoDocker);
        let err = fake.read(&blog, &endpoint).await.unwrap_err();
        assert!(matches!(err, ConfigError::NoDocker), "{err:?}");
        assert!(!err.is_invalid());

        assert_eq!(fake.reads(Path::new("/srv/blog")), 2);
        assert_eq!(fake.reads(Path::new("/srv/other")), 1);
        assert_eq!(fake.reads(Path::new("/srv/none")), 0);
    }
}
