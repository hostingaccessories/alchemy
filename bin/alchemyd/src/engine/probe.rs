//! Host checks behind [`HostProbe`], and the discovery steps built on it.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use alchemy_core::engine::SocketProbe;

use super::{
    DockerApi, DockerCli, Endpoint, EndpointError, HostEnv, Os, endpoint_candidates,
    select_endpoint,
};

/// What connecting to a Unix socket found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SocketConnect {
    /// Nothing exists at the path.
    Missing,
    /// Something exists but no one is listening.
    Refused,
    /// The socket exists but this user may not connect to it, such as a
    /// Linux user outside the `docker` group.
    PermissionDenied,
    /// The connection was accepted.
    Connected,
}

impl SocketConnect {
    /// Combine with whether a ping succeeded into the core
    /// [`SocketProbe`]. `ping_ok` only matters for [`Self::Connected`].
    ///
    /// The core model has no permission state yet, so
    /// [`Self::PermissionDenied`] counts as refused.
    pub const fn into_probe(self, ping_ok: bool) -> SocketProbe {
        match self {
            Self::Missing => SocketProbe::Missing,
            Self::Refused | Self::PermissionDenied => SocketProbe::Refused,
            Self::Connected if ping_ok => SocketProbe::Responding,
            Self::Connected => SocketProbe::Unresponsive,
        }
    }
}

/// Filesystem and socket checks on the host.
pub trait HostProbe {
    /// Whether anything exists at `path`, following symlinks.
    fn exists(&self, path: &Path) -> bool;

    /// Whether `path` is a file this process could execute.
    fn is_executable(&self, path: &Path) -> bool;

    /// Try to connect to the Unix socket at `socket`.
    fn connect(&self, socket: &Path) -> impl Future<Output = SocketConnect> + Send;
}

/// [`HostProbe`] backed by the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProbe;

impl HostProbe for SystemProbe {
    fn exists(&self, path: &Path) -> bool {
        path.try_exists().unwrap_or(false)
    }

    fn is_executable(&self, path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    async fn connect(&self, socket: &Path) -> SocketConnect {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(_) => SocketConnect::Connected,
            Err(e) => connect_error(&e),
        }
    }
}

fn connect_error(error: &io::Error) -> SocketConnect {
    match error.kind() {
        io::ErrorKind::NotFound => SocketConnect::Missing,
        io::ErrorKind::PermissionDenied => SocketConnect::PermissionDenied,
        _ => SocketConnect::Refused,
    }
}

/// The first of `candidates` that is executable.
///
/// Build `candidates` with [`docker_binary_candidates`](super::docker_binary_candidates).
pub fn find_docker_binary(probe: &impl HostProbe, candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| probe.is_executable(p)).cloned()
}

/// Whether any of `paths` exists.
///
/// Build `paths` with [`desktop_install_paths`](super::desktop_install_paths).
pub fn desktop_installed(probe: &impl HostProbe, paths: &[PathBuf]) -> bool {
    paths.iter().any(|p| probe.exists(p))
}

/// Resolve the endpoint to use, asking `cli` for the current context only
/// when `DOCKER_HOST` is unset.
///
/// A failing or missing CLI is not an error: discovery falls back to the
/// well-known sockets.
pub async fn discover_endpoint(
    env: &HostEnv,
    cli: Option<&impl DockerCli>,
    probe: &impl HostProbe,
    os: Os,
) -> Result<Endpoint, EndpointError> {
    let docker_host = env.docker_host.as_deref().filter(|h| !h.trim().is_empty());
    let context_host = match (docker_host, cli) {
        (None, Some(cli)) => match cli.context_host().await {
            Ok(host) => Some(host),
            Err(error) => {
                tracing::debug!(error = %error, "docker context lookup failed");
                None
            }
        },
        _ => None,
    };
    let candidates = endpoint_candidates(
        docker_host,
        context_host.as_deref(),
        env.home.as_deref(),
        env.xdg_runtime.as_deref(),
        os,
    );
    select_endpoint(&candidates, |p| probe.exists(p))
}

/// Connect to the endpoint's socket and, if that works, ping the engine.
pub async fn probe_socket(
    probe: &impl HostProbe,
    endpoint: &Endpoint,
    api: &impl DockerApi,
) -> SocketProbe {
    let connect = probe.connect(endpoint.socket()).await;
    let ping_ok = connect == SocketConnect::Connected && api.ping().await.is_ok();
    connect.into_probe(ping_ok)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::collections::HashMap;
    use std::future::ready;
    use std::sync::Mutex;

    use alchemy_core::engine::{ContainerEvent, ContainerStatus};
    use futures_util::Stream;

    use super::*;
    use crate::engine::{ApiError, CliError, EngineVersion};

    #[derive(Debug, Default)]
    struct FakeProbe {
        existing: Vec<PathBuf>,
        executable: Vec<PathBuf>,
        sockets: HashMap<PathBuf, SocketConnect>,
    }

    impl HostProbe for FakeProbe {
        fn exists(&self, path: &Path) -> bool {
            self.existing.iter().any(|p| p == path)
        }

        fn is_executable(&self, path: &Path) -> bool {
            self.executable.iter().any(|p| p == path)
        }

        fn connect(&self, socket: &Path) -> impl Future<Output = SocketConnect> + Send {
            ready(
                self.sockets
                    .get(socket)
                    .copied()
                    .unwrap_or(SocketConnect::Missing),
            )
        }
    }

    #[derive(Debug, Default)]
    struct FakeCli {
        context: Option<String>,
        calls: Mutex<u32>,
    }

    impl DockerCli for FakeCli {
        fn context_host(&self) -> impl Future<Output = Result<String, CliError>> + Send {
            *self.calls.lock().unwrap() += 1;
            ready(self.context.clone().ok_or(CliError::Failed {
                code: Some(1),
                stderr: "no context".into(),
            }))
        }

        fn compose_version(&self) -> impl Future<Output = Result<String, CliError>> + Send {
            ready(Ok("2.29.7".into()))
        }
    }

    struct FakeApi {
        ping_ok: bool,
    }

    impl DockerApi for FakeApi {
        fn ping(&self) -> impl Future<Output = Result<(), ApiError>> + Send {
            ready(if self.ping_ok {
                Ok(())
            } else {
                Err(ApiError::Timeout)
            })
        }

        fn version(&self) -> impl Future<Output = Result<EngineVersion, ApiError>> + Send {
            ready(Ok(EngineVersion::default()))
        }

        fn list_containers(
            &self,
        ) -> impl Future<Output = Result<Vec<ContainerStatus>, ApiError>> + Send {
            ready(Ok(Vec::new()))
        }

        fn events(
            &self,
        ) -> impl Stream<Item = Result<ContainerEvent, ApiError>> + Send + Unpin + 'static {
            futures_util::stream::empty()
        }
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn into_probe_maps_connect_and_ping() {
        let cases = [
            (SocketConnect::Missing, true, SocketProbe::Missing),
            (SocketConnect::Missing, false, SocketProbe::Missing),
            (SocketConnect::Refused, true, SocketProbe::Refused),
            (SocketConnect::PermissionDenied, true, SocketProbe::Refused),
            (SocketConnect::Connected, true, SocketProbe::Responding),
            (SocketConnect::Connected, false, SocketProbe::Unresponsive),
        ];
        for (connect, ping_ok, want) in cases {
            assert_eq!(connect.into_probe(ping_ok), want, "{connect:?} {ping_ok}");
        }
    }

    #[test]
    fn connect_error_maps_io_kinds() {
        let cases = [
            (io::ErrorKind::NotFound, SocketConnect::Missing),
            (
                io::ErrorKind::PermissionDenied,
                SocketConnect::PermissionDenied,
            ),
            (io::ErrorKind::ConnectionRefused, SocketConnect::Refused),
            (io::ErrorKind::Other, SocketConnect::Refused),
        ];
        for (kind, want) in cases {
            assert_eq!(connect_error(&io::Error::from(kind)), want, "{kind:?}");
        }
    }

    #[test]
    fn find_docker_binary_takes_first_executable() {
        let candidates = paths(&["/a/docker", "/b/docker", "/c/docker"]);
        let cases = [
            (vec![], None),
            (vec!["/c/docker"], Some("/c/docker")),
            (vec!["/c/docker", "/b/docker"], Some("/b/docker")),
        ];
        for (executable, want) in cases {
            let probe = FakeProbe {
                executable: paths(&executable),
                ..FakeProbe::default()
            };
            assert_eq!(
                find_docker_binary(&probe, &candidates),
                want.map(PathBuf::from)
            );
        }
    }

    #[test]
    fn desktop_installed_checks_any_path() {
        let install = paths(&[
            "/Applications/Docker.app",
            "/Users/me/Applications/Docker.app",
        ]);
        let cases = [
            (vec![], false),
            (vec!["/Users/me/Applications/Docker.app"], true),
            (vec!["/opt/docker-desktop"], false),
        ];
        for (existing, want) in cases {
            let probe = FakeProbe {
                existing: paths(&existing),
                ..FakeProbe::default()
            };
            assert_eq!(desktop_installed(&probe, &install), want, "{existing:?}");
        }
    }

    #[tokio::test]
    async fn discover_endpoint_uses_context_unless_docker_host_is_set() {
        let home = PathBuf::from("/Users/me");
        let desktop = "/Users/me/.docker/run/docker.sock";
        let colima = "/Users/me/.colima/default/docker.sock";
        let cases = [
            (
                None,
                Some(format!("unix://{colima}")),
                vec![colima, desktop],
                1,
                Ok(colima),
            ),
            (
                None,
                Some(format!("unix://{colima}")),
                vec![desktop],
                1,
                Ok(desktop),
            ),
            (None, None, vec![desktop], 1, Ok(desktop)),
            (
                Some("tcp://nas:2375"),
                Some(format!("unix://{colima}")),
                vec![colima],
                0,
                Err(()),
            ),
            (Some(""), None, vec![], 1, Ok(desktop)),
        ];
        for (docker_host, context, existing, calls, want) in cases {
            let env = HostEnv {
                docker_host: docker_host.map(str::to_owned),
                home: Some(home.clone()),
                ..HostEnv::default()
            };
            let cli = FakeCli {
                context,
                ..FakeCli::default()
            };
            let probe = FakeProbe {
                existing: paths(&existing),
                ..FakeProbe::default()
            };
            let got = discover_endpoint(&env, Some(&cli), &probe, Os::MacOs).await;
            let want =
                want.map(|p| Endpoint::unix(p).unwrap())
                    .map_err(|()| EndpointError::Unsupported {
                        endpoint: "tcp://nas:2375".into(),
                    });
            assert_eq!(got, want, "{docker_host:?} {existing:?}");
            assert_eq!(*cli.calls.lock().unwrap(), calls, "{docker_host:?}");
        }
    }

    #[tokio::test]
    async fn discover_endpoint_without_cli_uses_well_known_sockets() {
        let env = HostEnv {
            xdg_runtime: Some(PathBuf::from("/run/user/1000")),
            ..HostEnv::default()
        };
        let probe = FakeProbe {
            existing: paths(&["/run/user/1000/docker.sock"]),
            ..FakeProbe::default()
        };
        let got = discover_endpoint(&env, None::<&FakeCli>, &probe, Os::Linux).await;
        assert_eq!(got, Endpoint::unix("/run/user/1000/docker.sock"));
    }

    #[tokio::test]
    async fn probe_socket_pings_only_when_connected() {
        let endpoint = Endpoint::system();
        let cases = [
            (None, true, SocketProbe::Missing),
            (Some(SocketConnect::Refused), true, SocketProbe::Refused),
            (
                Some(SocketConnect::Connected),
                false,
                SocketProbe::Unresponsive,
            ),
            (
                Some(SocketConnect::Connected),
                true,
                SocketProbe::Responding,
            ),
        ];
        for (connect, ping_ok, want) in cases {
            let probe = FakeProbe {
                sockets: connect
                    .map(|c| (endpoint.socket().to_owned(), c))
                    .into_iter()
                    .collect(),
                ..FakeProbe::default()
            };
            let got = probe_socket(&probe, &endpoint, &FakeApi { ping_ok }).await;
            assert_eq!(got, want, "{connect:?} {ping_ok}");
        }
    }
}
