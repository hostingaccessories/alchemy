//! The engine facts [`reachability`](alchemy_core::route::reachability)
//! decides from, read from `/version` and `/info`.

use alchemy_core::engine::{EngineFlavor, flavor_from_platform};
use alchemy_core::route::{EngineFacts, HostOs};

use super::{EngineInfo, EngineVersion, Os};

/// What `security_options` in `/info` holds for a rootless engine.
const ROOTLESS: &str = "name=rootless";

/// Docker Desktop's `/info` `OperatingSystem`, on macOS and Linux alike.
const DESKTOP_OS: &str = "Docker Desktop";

/// The kernel Docker Desktop's VM runs, such as `6.10.14-linuxkit`.
const DESKTOP_KERNEL: &str = "linuxkit";

/// The facts about the engine at hand, from what `/version` and `/info`
/// reported and the OS the daemon runs on.
///
/// `version` is `None` when `/version` did not answer, which leaves the
/// flavor unknown. `/info` saying Docker Desktop (its `OperatingSystem`
/// or its `linuxkit` kernel) makes the flavor Docker Desktop whatever the
/// platform name says, since a wrong `ContainerIp` breaks every route.
pub fn engine_facts(version: Option<&EngineVersion>, info: &EngineInfo, os: Os) -> EngineFacts {
    let desktop_vm = info.operating_system.as_deref().map(str::trim) == Some(DESKTOP_OS)
        || info
            .kernel_version
            .as_deref()
            .is_some_and(|kernel| kernel.contains(DESKTOP_KERNEL));
    let flavor = if desktop_vm {
        Some(EngineFlavor::DockerDesktop)
    } else {
        version.map(|v| flavor_from_platform(v.platform.as_deref().unwrap_or("")).0)
    };
    EngineFacts {
        host_os: match os {
            Os::Linux => HostOs::Linux,
            Os::MacOs => HostOs::MacOs,
        },
        flavor,
        rootless: info
            .security_options
            .iter()
            .any(|option| option.split(',').any(|part| part.trim() == ROOTLESS)),
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
    use alchemy_core::route::{Reachability, reachability};

    use super::*;

    fn version(platform: Option<&str>) -> EngineVersion {
        EngineVersion {
            version: Some("28.5.1".into()),
            api_version: Some("1.51".into()),
            platform: platform.map(Into::into),
        }
    }

    fn info(os: Option<&str>, kernel: Option<&str>, security: &[&str]) -> EngineInfo {
        EngineInfo {
            operating_system: os.map(Into::into),
            kernel_version: kernel.map(Into::into),
            security_options: security.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    type Case = (
        &'static str,
        Option<EngineVersion>,
        EngineInfo,
        Option<EngineFlavor>,
        bool,
        Reachability,
    );

    const UBUNTU: Option<&str> = Some("Ubuntu 24.04.3 LTS");
    const KERNEL: Option<&str> = Some("6.8.0-85-generic");
    const LINUXKIT: Option<&str> = Some("6.10.14-linuxkit");
    const DESKTOP_OS: Option<&str> = Some("Docker Desktop");

    #[test]
    fn engine_facts_on_linux() {
        use EngineFlavor::{DockerDesktop, DockerEngine, Other};
        use Reachability::{ContainerIp, PublishedOnly};
        let engine = Some(version(Some("Docker Engine - Community")));
        let desktop = Some(version(Some("Docker Desktop 4.47.0 (206054)")));
        let seccomp = ["name=apparmor", "name=seccomp,profile=builtin"];
        let rootless = ["name=seccomp,profile=builtin", "name=rootless"];
        let cases: [Case; 10] = [
            (
                "Docker Engine",
                engine.clone(),
                info(UBUNTU, KERNEL, &seccomp),
                Some(DockerEngine),
                false,
                ContainerIp,
            ),
            (
                "Podman's Docker API",
                Some(version(Some("Podman Engine"))),
                info(Some("fedora"), Some("6.11.4-301.fc41.x86_64"), &[]),
                Some(Other),
                false,
                ContainerIp,
            ),
            (
                "no /version answer",
                None,
                info(UBUNTU, KERNEL, &[]),
                None,
                false,
                PublishedOnly,
            ),
            (
                "Docker Desktop",
                desktop.clone(),
                info(DESKTOP_OS, LINUXKIT, &[]),
                Some(DockerDesktop),
                false,
                PublishedOnly,
            ),
            (
                "Desktop's OS without a platform",
                Some(version(None)),
                info(DESKTOP_OS, None, &[]),
                Some(DockerDesktop),
                false,
                PublishedOnly,
            ),
            (
                "a linuxkit kernel without /version",
                None,
                info(None, LINUXKIT, &[]),
                Some(DockerDesktop),
                false,
                PublishedOnly,
            ),
            (
                "the platform alone",
                desktop,
                EngineInfo::default(),
                Some(DockerDesktop),
                false,
                PublishedOnly,
            ),
            (
                "rootless Docker",
                engine.clone(),
                info(UBUNTU, KERNEL, &rootless),
                Some(DockerEngine),
                true,
                PublishedOnly,
            ),
            (
                "rootless among other options",
                engine.clone(),
                info(UBUNTU, KERNEL, &["name=seccomp, name=rootless"]),
                Some(DockerEngine),
                true,
                PublishedOnly,
            ),
            (
                "rootless as a substring only",
                engine,
                info(UBUNTU, KERNEL, &["name=rootlessish"]),
                Some(DockerEngine),
                false,
                ContainerIp,
            ),
        ];
        for (name, version, info, flavor, rootless, reach) in cases {
            let facts = engine_facts(version.as_ref(), &info, Os::Linux);
            let want = EngineFacts {
                host_os: HostOs::Linux,
                flavor,
                rootless,
            };
            assert_eq!(facts, want, "{name}");
            assert_eq!(reachability(&facts), reach, "{name}");
        }
    }

    #[test]
    fn engine_facts_on_macos_never_reach_container_ips() {
        let cases = [
            (
                Some(version(Some("Docker Desktop 4.47.0 (206054)"))),
                info(DESKTOP_OS, LINUXKIT, &[]),
            ),
            (
                Some(version(Some("Docker Engine - Community"))),
                info(UBUNTU, KERNEL, &[]),
            ),
            (
                Some(version(Some("OrbStack"))),
                info(Some("OrbStack"), Some("6.15.11-orbstack"), &[]),
            ),
            (None, EngineInfo::default()),
        ];
        for (version, info) in cases {
            let facts = engine_facts(version.as_ref(), &info, Os::MacOs);
            assert_eq!(facts.host_os, HostOs::MacOs, "{version:?}");
            assert_eq!(
                reachability(&facts),
                Reachability::PublishedOnly,
                "{version:?}"
            );
        }
    }
}
