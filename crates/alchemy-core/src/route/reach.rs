//! How the host reaches an app's containers.

use serde::{Deserialize, Serialize};

use crate::engine::EngineFlavor;

/// How the proxy on the host reaches a routed container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachability {
    /// The host can reach container IPs on the `alchemy` network, so any
    /// container port works, published or not.
    ContainerIp,
    /// The host can't reach container IPs, as with Docker Desktop's VM, so
    /// only ports published on the host work.
    PublishedOnly,
}

/// The operating system the daemon runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostOs {
    /// Linux.
    Linux,
    /// macOS.
    MacOs,
}

/// What the daemon knows about the engine that decides [`Reachability`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EngineFacts {
    /// The host the daemon runs on.
    pub host_os: HostOs,
    /// The engine's distribution, or `None` before it has answered.
    pub flavor: Option<EngineFlavor>,
    /// Whether the engine runs rootless, from `/info`'s `SecurityOptions`
    /// holding `name=rootless`.
    pub rootless: bool,
}

/// How the host reaches containers on the engine `facts` describe.
///
/// Container IPs are reachable only from a Linux host whose engine runs
/// in the host's own network namespace: not Docker Desktop, which runs
/// in a VM on Linux too, and not rootless Docker, whose bridge lives in
/// a user namespace. An unknown flavor is treated as unreachable, since a
/// wrong guess there breaks every route while the safe one only asks for
/// published ports.
pub fn reachability(facts: &EngineFacts) -> Reachability {
    match (facts.host_os, facts.flavor, facts.rootless) {
        (HostOs::Linux, Some(EngineFlavor::DockerEngine | EngineFlavor::Other), false) => {
            Reachability::ContainerIp
        }
        _ => Reachability::PublishedOnly,
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
    fn reachability_from_engine_facts() {
        use EngineFlavor::{DockerDesktop, DockerEngine, Other};
        use HostOs::{Linux, MacOs};
        use Reachability::{ContainerIp, PublishedOnly};
        let cases = [
            (Linux, Some(DockerEngine), false, ContainerIp),
            (Linux, Some(Other), false, ContainerIp),
            (Linux, Some(DockerEngine), true, PublishedOnly),
            (Linux, Some(DockerDesktop), false, PublishedOnly),
            (Linux, None, false, PublishedOnly),
            (MacOs, Some(DockerDesktop), false, PublishedOnly),
            (MacOs, Some(DockerEngine), false, PublishedOnly),
            (MacOs, Some(Other), false, PublishedOnly),
            (MacOs, None, false, PublishedOnly),
        ];
        for (host_os, flavor, rootless, want) in cases {
            let facts = EngineFacts {
                host_os,
                flavor,
                rootless,
            };
            assert_eq!(reachability(&facts), want, "{facts:?}");
        }
    }

    #[test]
    fn reachability_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Reachability::ContainerIp).unwrap(),
            "\"container_ip\""
        );
        assert_eq!(
            serde_json::to_string(&Reachability::PublishedOnly).unwrap(),
            "\"published_only\""
        );
    }
}
