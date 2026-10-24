//! [`SystemNetwork`]: this host's network through `sysinfo`, and
//! `/proc/net` on Linux or `netstat` on macOS.

use std::future::Future;
use std::time::Duration;

use alchemy_core::network::Ipv4Net;
use alchemy_core::ports::Listener;
use futures_util::future::BoxFuture;
use sysinfo::Networks;

use super::{HostNetwork, NetworkError, Route, lan_subnets_from, unique};

/// The real [`HostNetwork`].
///
/// LAN subnets come from `sysinfo`. On Linux, routes and sockets are read
/// from `/proc/net`, and owners are found by matching socket inodes in
/// `/proc/<pid>/fd`, which shows only this user's processes unless the
/// daemon runs as root. On macOS they come from `/usr/sbin/netstat`,
/// which names the owner of every socket, or from `/usr/sbin/lsof`, which
/// sees only this user's, when macOS hides them from `netstat` (see
/// [`lsof`](super::lsof)). Each read is given
/// [`Self::DEFAULT_TIMEOUT`]. Cheap to clone.
#[derive(Debug, Clone)]
pub struct SystemNetwork {
    timeout: Duration,
}

impl SystemNetwork {
    /// How long each read may take by default.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    /// A reader with [`Self::DEFAULT_TIMEOUT`]. Nothing is read until
    /// asked.
    pub fn new() -> Self {
        Self {
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Use `timeout` for each read instead of the default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn bounded<T>(
        &self,
        what: &str,
        read: impl Future<Output = Result<T, NetworkError>>,
    ) -> Result<T, NetworkError> {
        tokio::time::timeout(self.timeout, read)
            .await
            .map_err(|_| NetworkError::Timeout {
                what: what.to_owned(),
                after: self.timeout,
            })?
    }
}

impl Default for SystemNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl HostNetwork for SystemNetwork {
    fn lan_subnets(&self) -> BoxFuture<'_, Result<Vec<Ipv4Net>, NetworkError>> {
        Box::pin(self.bounded("reading interface addresses", async {
            tokio::task::spawn_blocking(|| {
                let networks = Networks::new_with_refreshed_list();
                lan_subnets_from(networks.iter().flat_map(|(name, data)| {
                    data.ip_networks()
                        .iter()
                        .map(move |ip| (name.as_str(), ip.addr, ip.prefix))
                }))
            })
            .await
            .map_err(|error| NetworkError::Task(error.to_string()))
        }))
    }

    fn routes(&self) -> BoxFuture<'_, Result<Vec<Route>, NetworkError>> {
        Box::pin(self.bounded("reading routes", os::routes()))
    }

    fn listeners(&self) -> BoxFuture<'_, Result<Vec<Listener>, NetworkError>> {
        Box::pin(self.bounded("reading listening sockets", async {
            os::listeners().await.map(unique)
        }))
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::path::{Path, PathBuf};

    use alchemy_core::ports::{Listener, Protocol};

    use super::super::proc::{
        Owner, ProcSocket, parse_route, parse_sockets, socket_inode, with_owners,
    };
    use super::super::{NetworkError, Route};

    const ROUTE: &str = "/proc/net/route";
    const SOCKETS: [(&str, Protocol); 4] = [
        ("/proc/net/tcp", Protocol::Tcp),
        ("/proc/net/tcp6", Protocol::Tcp),
        ("/proc/net/udp", Protocol::Udp),
        ("/proc/net/udp6", Protocol::Udp),
    ];

    pub(super) async fn routes() -> Result<Vec<Route>, NetworkError> {
        blocking(|| {
            let text =
                std::fs::read_to_string(ROUTE).map_err(|source| read_error(ROUTE, source))?;
            Ok(parse_route(&text))
        })
        .await
    }

    /// A missing `tcp6` or `udp6` means IPv6 is off, not an error.
    pub(super) async fn listeners() -> Result<Vec<Listener>, NetworkError> {
        blocking(|| {
            let mut sockets: Vec<ProcSocket> = Vec::new();
            for (path, protocol) in SOCKETS {
                match std::fs::read_to_string(path) {
                    Ok(text) => sockets.extend(parse_sockets(&text, protocol)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => return Err(read_error(path, source)),
                }
            }
            let inodes: HashSet<u64> = sockets
                .iter()
                .map(|socket| socket.inode)
                .filter(|inode| *inode != 0)
                .collect();
            Ok(with_owners(sockets, &socket_owners(&inodes)))
        })
        .await
    }

    async fn blocking<T: Send + 'static>(
        read: impl FnOnce() -> Result<T, NetworkError> + Send + 'static,
    ) -> Result<T, NetworkError> {
        tokio::task::spawn_blocking(read)
            .await
            .map_err(|error| NetworkError::Task(error.to_string()))?
    }

    fn read_error(path: &str, source: io::Error) -> NetworkError {
        NetworkError::Read {
            path: PathBuf::from(path),
            source,
        }
    }

    /// The owners of `inodes` among the processes whose descriptors this
    /// user can read. Processes that exit mid-scan are skipped.
    fn socket_owners(inodes: &HashSet<u64>) -> HashMap<u64, Owner> {
        let mut owners = HashMap::new();
        if inodes.is_empty() {
            return owners;
        }
        let Ok(processes) = std::fs::read_dir("/proc") else {
            return owners;
        };
        for process in processes.flatten() {
            let Some(pid) = process.file_name().to_str().and_then(|p| p.parse().ok()) else {
                continue;
            };
            let dir = process.path();
            let Ok(fds) = std::fs::read_dir(dir.join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Some(inode) = std::fs::read_link(fd.path())
                    .ok()
                    .and_then(|link| socket_inode(&link.to_string_lossy()))
                    .filter(|inode| inodes.contains(inode))
                else {
                    continue;
                };
                owners
                    .entry(inode)
                    .or_insert_with(|| (pid, process_name(&dir)));
            }
        }
        owners
    }

    fn process_name(dir: &Path) -> Option<String> {
        let comm = std::fs::read_to_string(dir.join("comm")).ok()?;
        Some(comm.trim_end().to_owned()).filter(|name| !name.is_empty())
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::process::Stdio;

    use alchemy_core::ports::{Listener, Protocol};
    use tokio::process::Command;

    use super::super::netstat::{
        ROUTE_ARGS, lists_sockets, parse_routes, parse_sockets, socket_args,
    };
    use super::super::{NetworkError, Route, lsof};

    /// Absolute paths, so `PATH` cannot swap in another binary.
    const NETSTAT: &str = "/usr/sbin/netstat";
    const LSOF: &str = "/usr/sbin/lsof";
    const MAX_CAPTURED: usize = 512;

    pub(super) async fn routes() -> Result<Vec<Route>, NetworkError> {
        Ok(parse_routes(&run(NETSTAT, &ROUTE_ARGS).await?))
    }

    pub(super) async fn listeners() -> Result<Vec<Listener>, NetworkError> {
        let (tcp_args, udp_args) = (socket_args(Protocol::Tcp), socket_args(Protocol::Udp));
        let (tcp, udp) = tokio::try_join!(run(NETSTAT, &tcp_args), run(NETSTAT, &udp_args))?;
        if !lists_sockets(&tcp) && !lists_sockets(&udp) {
            return Ok(lsof::parse(&run(LSOF, &lsof::ARGS).await?));
        }
        let mut listeners = parse_sockets(&tcp, Protocol::Tcp);
        listeners.extend(parse_sockets(&udp, Protocol::Udp));
        Ok(listeners)
    }

    /// Run `binary` with `args`, no shell, killing it if the caller gives
    /// up. `lsof` exits with 1 when it finds nothing, which is not a
    /// failure when it printed nothing else.
    async fn run(binary: &str, args: &[&str]) -> Result<String, NetworkError> {
        let output = Command::new(binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|source| NetworkError::Spawn {
                binary: binary.into(),
                source,
            })?;
        let found_nothing =
            binary == LSOF && output.status.code() == Some(1) && output.stderr.is_empty();
        if !output.status.success() && !found_nothing {
            return Err(NetworkError::Failed {
                binary: binary.into(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(MAX_CAPTURED)
                    .collect(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
