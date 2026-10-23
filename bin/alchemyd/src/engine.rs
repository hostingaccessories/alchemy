//! The Docker engine at the daemon's edge: finding it and talking to it.
//!
//! Which endpoint, which `docker` binary, and whether Docker Desktop is
//! installed are pure decisions over plain values, taking the [`Os`] as a
//! parameter so both platforms are tested everywhere. The effects behind
//! them sit behind three traits, each with a real implementation here and
//! a fake in tests:
//!
//! - [`HostProbe`]: filesystem checks and socket connects ([`SystemProbe`]),
//! - [`DockerCli`]: the `docker` binary ([`ProcessDockerCli`]),
//! - [`DockerApi`]: the Engine API ([`BollardApi`]).
//!
//! `bollard` types are mapped to [`alchemy_core::engine`] types inside
//! [`BollardApi`] and nowhere else.

mod api;
mod cli;
mod discovery;
mod probe;

pub use api::{ApiError, BollardApi, DockerApi, EngineVersion};
pub use cli::{CliError, DockerCli, ProcessDockerCli, parse_compose_version, parse_context_host};
pub use discovery::{
    Candidate, Endpoint, EndpointError, EndpointSource, HostEnv, Os, desktop_install_paths,
    docker_binary_candidates, endpoint_candidates, select_endpoint,
};
pub use probe::{
    HostProbe, SocketConnect, SystemProbe, desktop_installed, discover_endpoint,
    find_docker_binary, probe_socket,
};
