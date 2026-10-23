//! Pure decisions about where Docker lives on this host.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

/// The operating system a decision is made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    /// Linux.
    Linux,
    /// macOS.
    MacOs,
}

impl Os {
    /// The OS this binary was built for.
    pub const fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::Linux
        }
    }
}

const UNIX_SCHEME: &str = "unix://";
const SYSTEM_SOCKET: &str = "/var/run/docker.sock";

/// A Docker engine endpoint the daemon can talk to: always a Unix socket.
///
/// Remote transports (`tcp://`, `ssh://`) are rejected when parsing, because
/// Alchemy never exposes or tunnels the Docker API.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    socket: PathBuf,
}

impl Endpoint {
    /// An endpoint for the socket at `socket`, which must be absolute.
    pub fn unix(socket: impl Into<PathBuf>) -> Result<Self, EndpointError> {
        let socket = socket.into();
        if socket.is_absolute() {
            Ok(Self { socket })
        } else {
            Err(EndpointError::Invalid {
                endpoint: socket.display().to_string(),
            })
        }
    }

    /// Parse a `DOCKER_HOST`-style URL such as `unix:///var/run/docker.sock`.
    pub fn parse(host: &str) -> Result<Self, EndpointError> {
        let host = host.trim();
        if let Some(path) = host.strip_prefix(UNIX_SCHEME) {
            return Self::unix(path).map_err(|_| EndpointError::Invalid {
                endpoint: host.to_owned(),
            });
        }
        match host.split_once("://") {
            Some((scheme, _)) if !scheme.is_empty() => Err(EndpointError::Unsupported {
                endpoint: host.to_owned(),
            }),
            _ => Err(EndpointError::Invalid {
                endpoint: host.to_owned(),
            }),
        }
    }

    /// The `/var/run/docker.sock` endpoint Docker falls back to.
    pub fn system() -> Self {
        Self {
            socket: PathBuf::from(SYSTEM_SOCKET),
        }
    }

    /// Path of the engine socket.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The value for `DOCKER_HOST` that selects this endpoint, such as
    /// `unix:///var/run/docker.sock`.
    ///
    /// An `OsString` so a non-UTF-8 home directory survives the round trip.
    pub fn docker_host(&self) -> OsString {
        let mut host = OsString::from(UNIX_SCHEME);
        host.push(self.socket.as_os_str());
        host
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{UNIX_SCHEME}{}", self.socket.display())
    }
}

/// Why an endpoint cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EndpointError {
    /// The endpoint uses a transport other than a Unix socket.
    #[error("unsupported Docker endpoint {endpoint}: only unix:// sockets are supported")]
    Unsupported {
        /// The endpoint as configured.
        endpoint: String,
    },
    /// The endpoint is not a URL or its socket path is not absolute.
    #[error("invalid Docker endpoint {endpoint:?}")]
    Invalid {
        /// The endpoint as configured.
        endpoint: String,
    },
}

/// Where an endpoint candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointSource {
    /// The `DOCKER_HOST` environment variable.
    DockerHost,
    /// The current `docker context`.
    Context,
    /// A socket path Docker distributions are known to use.
    WellKnown,
}

impl EndpointSource {
    /// Whether the user chose this endpoint, so it is reported even when
    /// unusable rather than skipped.
    pub const fn is_explicit(self) -> bool {
        matches!(self, Self::DockerHost | Self::Context)
    }
}

/// One place the engine might be, in the order [`endpoint_candidates`]
/// prefers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Where the candidate came from.
    pub source: EndpointSource,
    /// The parsed endpoint, or why it cannot be used.
    pub endpoint: Result<Endpoint, EndpointError>,
}

impl Candidate {
    fn well_known(socket: PathBuf) -> Self {
        Self {
            source: EndpointSource::WellKnown,
            endpoint: Endpoint::unix(socket),
        }
    }
}

/// The process environment discovery reads, captured once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEnv {
    /// `DOCKER_HOST`.
    pub docker_host: Option<String>,
    /// `HOME`.
    pub home: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR`.
    pub xdg_runtime: Option<PathBuf>,
    /// `PATH`.
    pub path: Option<OsString>,
}

impl HostEnv {
    /// Read the variables from this process's environment.
    pub fn current() -> Self {
        Self {
            docker_host: std::env::var_os("DOCKER_HOST").map(|v| v.to_string_lossy().into_owned()),
            home: std::env::var_os("HOME").map(PathBuf::from),
            xdg_runtime: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            path: std::env::var_os("PATH"),
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

fn absolute(dir: Option<&Path>) -> Option<&Path> {
    dir.filter(|d| d.is_absolute())
}

/// Every place to look for the engine, most preferred first.
///
/// A non-empty `DOCKER_HOST` is the only candidate, matching the Docker
/// CLI. Otherwise the current context's host (from [`DockerCli`]) comes
/// first, followed by the sockets Docker Desktop, rootless Docker, and the
/// system engine use on `os`. Relative `home` and `xdg_runtime` values are
/// ignored.
///
/// [`DockerCli`]: super::DockerCli
pub fn endpoint_candidates(
    docker_host: Option<&str>,
    context_host: Option<&str>,
    home: Option<&Path>,
    xdg_runtime: Option<&Path>,
    os: Os,
) -> Vec<Candidate> {
    if let Some(host) = non_empty(docker_host) {
        return vec![Candidate {
            source: EndpointSource::DockerHost,
            endpoint: Endpoint::parse(host),
        }];
    }
    let mut candidates = Vec::new();
    if let Some(host) = non_empty(context_host) {
        candidates.push(Candidate {
            source: EndpointSource::Context,
            endpoint: Endpoint::parse(host),
        });
    }
    let home = absolute(home);
    match os {
        Os::MacOs => {
            if let Some(home) = home {
                candidates.push(Candidate::well_known(home.join(".docker/run/docker.sock")));
            }
        }
        Os::Linux => {
            if let Some(home) = home {
                candidates.push(Candidate::well_known(
                    home.join(".docker/desktop/docker.sock"),
                ));
            }
            if let Some(xdg) = absolute(xdg_runtime) {
                candidates.push(Candidate::well_known(xdg.join("docker.sock")));
            }
        }
    }
    candidates.push(Candidate::well_known(PathBuf::from(SYSTEM_SOCKET)));
    dedup_by_key(candidates, |c| c.endpoint.clone())
}

/// Pick the endpoint to use from `candidates`, given which socket paths
/// exist.
///
/// An explicit candidate that cannot be parsed is returned as the error,
/// so a `tcp://` `DOCKER_HOST` or context is reported rather than silently
/// replaced by a local socket. Otherwise the first endpoint whose socket
/// exists wins. When none exists, the first usable endpoint is returned so
/// the daemon reports Docker as not running there.
pub fn select_endpoint(
    candidates: &[Candidate],
    mut exists: impl FnMut(&Path) -> bool,
) -> Result<Endpoint, EndpointError> {
    let mut fallback = None;
    for candidate in candidates {
        match &candidate.endpoint {
            Err(e) if candidate.source.is_explicit() => return Err(e.clone()),
            Err(_) => {}
            Ok(endpoint) if exists(endpoint.socket()) => return Ok(endpoint.clone()),
            Ok(endpoint) => {
                fallback.get_or_insert(endpoint);
            }
        }
    }
    Ok(fallback.cloned().unwrap_or_else(Endpoint::system))
}

/// Every path a `docker` binary might be at, most preferred first.
///
/// `PATH` entries come first, then the locations Docker Desktop and
/// Homebrew install to. The extras matter because apps launched from
/// Finder or a desktop session get a minimal `PATH`. Relative `PATH`
/// entries and a relative `home` are ignored.
pub fn docker_binary_candidates(path: Option<&OsStr>, home: Option<&Path>, os: Os) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let home = absolute(home);
    match os {
        Os::MacOs => {
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            if let Some(home) = home {
                dirs.push(home.join(".docker/bin"));
            }
            dirs.push(PathBuf::from(
                "/Applications/Docker.app/Contents/Resources/bin",
            ));
        }
        Os::Linux => {
            dirs.push(PathBuf::from("/usr/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
            dirs.push(PathBuf::from("/snap/bin"));
            if let Some(home) = home {
                dirs.push(home.join("bin"));
            }
        }
    }
    dedup_by_key(
        dirs.into_iter().map(|d| d.join("docker")).collect(),
        Clone::clone,
    )
}

/// Paths whose presence means Docker Desktop is installed.
pub fn desktop_install_paths(home: Option<&Path>, os: Os) -> Vec<PathBuf> {
    match os {
        Os::MacOs => {
            let mut paths = vec![PathBuf::from("/Applications/Docker.app")];
            if let Some(home) = absolute(home) {
                paths.push(home.join("Applications/Docker.app"));
            }
            paths
        }
        Os::Linux => vec![PathBuf::from("/opt/docker-desktop")],
    }
}

fn dedup_by_key<T, K: PartialEq>(items: Vec<T>, key: impl Fn(&T) -> K) -> Vec<T> {
    let mut seen: Vec<K> = Vec::with_capacity(items.len());
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let k = key(&item);
        if !seen.contains(&k) {
            seen.push(k);
            out.push(item);
        }
    }
    out
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

    fn unix(path: &str) -> Result<Endpoint, EndpointError> {
        Endpoint::unix(path)
    }

    fn unsupported(endpoint: &str) -> Result<Endpoint, EndpointError> {
        Err(EndpointError::Unsupported {
            endpoint: endpoint.to_owned(),
        })
    }

    fn invalid(endpoint: &str) -> Result<Endpoint, EndpointError> {
        Err(EndpointError::Invalid {
            endpoint: endpoint.to_owned(),
        })
    }

    #[test]
    fn parse_accepts_only_absolute_unix_sockets() {
        let cases = [
            ("unix:///var/run/docker.sock", unix("/var/run/docker.sock")),
            (
                "  unix:///Users/me/.docker/run/docker.sock\n",
                unix("/Users/me/.docker/run/docker.sock"),
            ),
            ("tcp://127.0.0.1:2375", unsupported("tcp://127.0.0.1:2375")),
            ("ssh://me@nas", unsupported("ssh://me@nas")),
            (
                "npipe:////./pipe/docker_engine",
                unsupported("npipe:////./pipe/docker_engine"),
            ),
            (
                "http://localhost:2375",
                unsupported("http://localhost:2375"),
            ),
            ("fd://", unsupported("fd://")),
            ("unix://", invalid("unix://")),
            ("unix://relative.sock", invalid("unix://relative.sock")),
            ("/var/run/docker.sock", invalid("/var/run/docker.sock")),
            ("://nothing", invalid("://nothing")),
            ("", invalid("")),
        ];
        for (host, want) in cases {
            assert_eq!(Endpoint::parse(host), want, "{host:?}");
        }
    }

    #[test]
    fn endpoint_round_trips_through_docker_host() {
        let endpoint = Endpoint::unix("/run/user/1000/docker.sock").unwrap();
        assert_eq!(endpoint.docker_host(), "unix:///run/user/1000/docker.sock");
        assert_eq!(endpoint.to_string(), "unix:///run/user/1000/docker.sock");
        let host = endpoint.docker_host();
        assert_eq!(Endpoint::parse(host.to_str().unwrap()).unwrap(), endpoint);
        assert_eq!(
            Endpoint::system().socket(),
            Path::new("/var/run/docker.sock")
        );
    }

    fn well_known(path: &str) -> Candidate {
        Candidate::well_known(PathBuf::from(path))
    }

    fn explicit(source: EndpointSource, host: &str) -> Candidate {
        Candidate {
            source,
            endpoint: Endpoint::parse(host),
        }
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn endpoint_candidates_per_os() {
        use EndpointSource::{Context, DockerHost};
        let home = Some(Path::new("/home/me"));
        let xdg = Some(Path::new("/run/user/1000"));
        let cases: Vec<(
            Option<&str>,
            Option<&str>,
            Option<&Path>,
            Option<&Path>,
            Os,
            Vec<Candidate>,
        )> = vec![
            (
                Some("tcp://nas:2375"),
                Some("unix:///ctx.sock"),
                home,
                xdg,
                Os::Linux,
                vec![explicit(DockerHost, "tcp://nas:2375")],
            ),
            (
                Some("unix:///custom.sock"),
                None,
                home,
                xdg,
                Os::MacOs,
                vec![explicit(DockerHost, "unix:///custom.sock")],
            ),
            (
                Some("  "),
                None,
                home,
                xdg,
                Os::Linux,
                vec![
                    well_known("/home/me/.docker/desktop/docker.sock"),
                    well_known("/run/user/1000/docker.sock"),
                    well_known("/var/run/docker.sock"),
                ],
            ),
            (
                None,
                Some("unix:///home/me/.colima/default/docker.sock"),
                home,
                None,
                Os::MacOs,
                vec![
                    explicit(Context, "unix:///home/me/.colima/default/docker.sock"),
                    well_known("/home/me/.docker/run/docker.sock"),
                    well_known("/var/run/docker.sock"),
                ],
            ),
            (
                None,
                None,
                home,
                xdg,
                Os::MacOs,
                vec![
                    well_known("/home/me/.docker/run/docker.sock"),
                    well_known("/var/run/docker.sock"),
                ],
            ),
            (
                None,
                Some("unix:///var/run/docker.sock"),
                None,
                None,
                Os::Linux,
                vec![explicit(Context, "unix:///var/run/docker.sock")],
            ),
            (
                None,
                Some("ssh://me@nas"),
                Some(Path::new("relative")),
                Some(Path::new("relative")),
                Os::Linux,
                vec![
                    explicit(Context, "ssh://me@nas"),
                    well_known("/var/run/docker.sock"),
                ],
            ),
        ];
        for (docker_host, context, home, xdg, os, want) in cases {
            assert_eq!(
                endpoint_candidates(docker_host, context, home, xdg, os),
                want,
                "{docker_host:?} {context:?} {home:?} {xdg:?} {os:?}"
            );
        }
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn select_endpoint_prefers_existing_and_reports_explicit_errors() {
        use EndpointSource::{Context, DockerHost};
        let desktop = "/home/me/.docker/run/docker.sock";
        let system = "/var/run/docker.sock";
        let cases: Vec<(Vec<Candidate>, Vec<&str>, Result<Endpoint, EndpointError>)> = vec![
            (
                vec![explicit(DockerHost, "tcp://nas:2375")],
                vec![system],
                unsupported("tcp://nas:2375"),
            ),
            (
                vec![explicit(Context, "ssh://nas"), well_known(system)],
                vec![system],
                unsupported("ssh://nas"),
            ),
            (
                vec![explicit(DockerHost, "unix:///missing.sock")],
                vec![],
                unix("/missing.sock"),
            ),
            (
                vec![
                    explicit(Context, "unix:///ctx.sock"),
                    well_known(desktop),
                    well_known(system),
                ],
                vec![system],
                unix(system),
            ),
            (
                vec![
                    explicit(Context, "unix:///ctx.sock"),
                    well_known(desktop),
                    well_known(system),
                ],
                vec!["/ctx.sock", system],
                unix("/ctx.sock"),
            ),
            (
                vec![
                    explicit(Context, "unix:///ctx.sock"),
                    well_known(desktop),
                    well_known(system),
                ],
                vec![],
                unix("/ctx.sock"),
            ),
            (
                vec![well_known("relative.sock"), well_known(system)],
                vec![],
                unix(system),
            ),
            (vec![], vec![], Ok(Endpoint::system())),
        ];
        for (candidates, existing, want) in cases {
            let got = select_endpoint(&candidates, |p| existing.iter().any(|e| Path::new(e) == p));
            assert_eq!(got, want, "{candidates:?} {existing:?}");
        }
    }

    #[test]
    fn docker_binary_candidates_per_os() {
        let path = std::env::join_paths(["/opt/bin", "relative", "/usr/local/bin"]).unwrap();
        let cases = [
            (
                Some(path.as_os_str()),
                Some(Path::new("/Users/me")),
                Os::MacOs,
                vec![
                    "/opt/bin/docker",
                    "/usr/local/bin/docker",
                    "/opt/homebrew/bin/docker",
                    "/Users/me/.docker/bin/docker",
                    "/Applications/Docker.app/Contents/Resources/bin/docker",
                ],
            ),
            (
                None,
                None,
                Os::MacOs,
                vec![
                    "/usr/local/bin/docker",
                    "/opt/homebrew/bin/docker",
                    "/Applications/Docker.app/Contents/Resources/bin/docker",
                ],
            ),
            (
                Some(path.as_os_str()),
                Some(Path::new("/home/me")),
                Os::Linux,
                vec![
                    "/opt/bin/docker",
                    "/usr/local/bin/docker",
                    "/usr/bin/docker",
                    "/home/linuxbrew/.linuxbrew/bin/docker",
                    "/snap/bin/docker",
                    "/home/me/bin/docker",
                ],
            ),
            (
                Some(OsStr::new("")),
                Some(Path::new("relative")),
                Os::Linux,
                vec![
                    "/usr/bin/docker",
                    "/usr/local/bin/docker",
                    "/home/linuxbrew/.linuxbrew/bin/docker",
                    "/snap/bin/docker",
                ],
            ),
        ];
        for (path, home, os, want) in cases {
            let want: Vec<PathBuf> = want.into_iter().map(PathBuf::from).collect();
            assert_eq!(
                docker_binary_candidates(path, home, os),
                want,
                "{path:?} {home:?} {os:?}"
            );
        }
    }

    #[test]
    fn desktop_install_paths_per_os() {
        let cases = [
            (
                Some(Path::new("/Users/me")),
                Os::MacOs,
                vec![
                    "/Applications/Docker.app",
                    "/Users/me/Applications/Docker.app",
                ],
            ),
            (None, Os::MacOs, vec!["/Applications/Docker.app"]),
            (
                Some(Path::new("relative")),
                Os::MacOs,
                vec!["/Applications/Docker.app"],
            ),
            (
                Some(Path::new("/home/me")),
                Os::Linux,
                vec!["/opt/docker-desktop"],
            ),
            (None, Os::Linux, vec!["/opt/docker-desktop"]),
        ];
        for (home, os, want) in cases {
            let want: Vec<PathBuf> = want.into_iter().map(PathBuf::from).collect();
            assert_eq!(desktop_install_paths(home, os), want, "{home:?} {os:?}");
        }
    }

    #[test]
    fn current_os_matches_target() {
        assert_eq!(Os::current() == Os::MacOs, cfg!(target_os = "macos"));
    }
}
