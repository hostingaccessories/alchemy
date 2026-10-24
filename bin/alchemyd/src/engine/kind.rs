//! Whether the host can reach the engine's containers at their own IPs.

use alchemy_core::engine::{EngineFlavor, flavor_from_platform};

use super::{EngineInfo, EngineVersion, Os};

/// Where the engine runs, as far as reaching its containers goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EngineKind {
    /// Docker Engine on this Linux host's own kernel and network stack:
    /// a container's IP on a bridge network is reachable from the host,
    /// so its unpublished ports are too.
    Native,
    /// Docker Desktop, or any engine whose containers sit outside the
    /// host's network stack: every engine on macOS (each runs in a VM),
    /// Docker Desktop on Linux, and rootless Docker. Only published ports
    /// are reachable from the host.
    Desktop,
}

/// What `security_options` in `/info` holds for a rootless engine.
const ROOTLESS: &str = "name=rootless";

/// Docker Desktop's `/info` `OperatingSystem`, on macOS and Linux alike.
const DESKTOP_OS: &str = "Docker Desktop";

/// The kernel Docker Desktop's VM runs, such as `6.10.14-linuxkit`.
const DESKTOP_KERNEL: &str = "linuxkit";

/// Decide the engine's kind from what `/version` and `/info` report and
/// the OS the daemon runs on.
///
/// Anything that points away from a native engine wins: the facts are
/// read independently and any one can be missing, and treating a native
/// engine as Desktop only costs routes to unpublished ports, while the
/// reverse sends the proxy to IPs it cannot reach.
pub fn engine_kind(version: &EngineVersion, info: &EngineInfo, os: Os) -> EngineKind {
    let (flavor, _) = flavor_from_platform(version.platform.as_deref().unwrap_or(""));
    let desktop = os == Os::MacOs
        || flavor == EngineFlavor::DockerDesktop
        || info.operating_system.as_deref().map(str::trim) == Some(DESKTOP_OS)
        || info
            .kernel_version
            .as_deref()
            .is_some_and(|kernel| kernel.contains(DESKTOP_KERNEL))
        || info
            .security_options
            .iter()
            .any(|option| option.split(',').any(|part| part.trim() == ROOTLESS));
    if desktop {
        EngineKind::Desktop
    } else {
        EngineKind::Native
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

    type Case = (&'static str, Option<&'static str>, EngineInfo, EngineKind);

    const ENGINE: Option<&str> = Some("Docker Engine - Community");
    const DESKTOP: Option<&str> = Some("Docker Desktop 4.47.0 (206054)");
    const UBUNTU: Option<&str> = Some("Ubuntu 24.04.3 LTS");
    const KERNEL: Option<&str> = Some("6.8.0-85-generic");
    const LINUXKIT: Option<&str> = Some("6.10.14-linuxkit");
    const DESKTOP_OS: Option<&str> = Some("Docker Desktop");

    #[test]
    fn engine_kind_on_linux() {
        use EngineKind::{Desktop, Native};
        let seccomp = ["name=apparmor", "name=seccomp,profile=builtin"];
        let rootless = ["name=seccomp,profile=builtin", "name=rootless"];
        let cases: [Case; 10] = [
            (
                "Docker Engine",
                ENGINE,
                info(UBUNTU, KERNEL, &seccomp),
                Native,
            ),
            ("nothing known", None, EngineInfo::default(), Native),
            (
                "Podman's Docker API",
                Some("Podman Engine"),
                info(Some("fedora"), Some("6.11.4-301.fc41.x86_64"), &[]),
                Native,
            ),
            (
                "Docker Desktop",
                DESKTOP,
                info(DESKTOP_OS, LINUXKIT, &[]),
                Desktop,
            ),
            (
                "Desktop's OS alone",
                None,
                info(DESKTOP_OS, None, &[]),
                Desktop,
            ),
            (
                "a linuxkit kernel alone",
                None,
                info(None, LINUXKIT, &[]),
                Desktop,
            ),
            (
                "the platform alone",
                DESKTOP,
                EngineInfo::default(),
                Desktop,
            ),
            (
                "rootless Docker",
                ENGINE,
                info(UBUNTU, KERNEL, &rootless),
                Desktop,
            ),
            (
                "rootless among other options",
                ENGINE,
                info(UBUNTU, KERNEL, &["name=seccomp, name=rootless"]),
                Desktop,
            ),
            (
                "rootless as a substring only",
                ENGINE,
                info(UBUNTU, KERNEL, &["name=rootlessish"]),
                Native,
            ),
        ];
        for (name, platform, info, want) in cases {
            assert_eq!(
                engine_kind(&version(platform), &info, Os::Linux),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn engine_kind_on_macos_is_always_desktop() {
        let cases: [Case; 4] = [
            (
                "Docker Desktop",
                DESKTOP,
                info(DESKTOP_OS, LINUXKIT, &[]),
                EngineKind::Desktop,
            ),
            (
                "Colima",
                ENGINE,
                info(UBUNTU, KERNEL, &[]),
                EngineKind::Desktop,
            ),
            (
                "OrbStack",
                Some("OrbStack"),
                info(Some("OrbStack"), Some("6.15.11-orbstack"), &[]),
                EngineKind::Desktop,
            ),
            (
                "nothing known",
                None,
                EngineInfo::default(),
                EngineKind::Desktop,
            ),
        ];
        for (name, platform, info, want) in cases {
            assert_eq!(
                engine_kind(&version(platform), &info, Os::MacOs),
                want,
                "{name}"
            );
        }
    }
}
