//! [`SystemPrivileges`]: the facts as this machine reports them.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alchemy_core::privilege::{FileKind, FileMeta, FirewallFacts, HELPER, HelperPath};
use alchemy_core::route::HostOs;
use futures_util::future::BoxFuture;

use super::facts::{FactsError, HelperReading, PrivilegeSystem, SystemFacts};

/// How long a firewall query may take before it counts as no answer.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The real [`PrivilegeSystem`].
///
/// The helper is the `alchemy-helper` next to the running alchemyd's
/// canonical path, itself canonicalized. On Linux the capabilities and
/// `no_new_privs` come from `/proc/self/status`, the mount flags from
/// `statvfs`, and the file capability from the `security.capability`
/// xattr; on macOS none of them apply. The firewall is read without
/// root, as the #110 spike found works: `/etc/ufw/ufw.conf` and
/// `systemctl is-active firewalld` on Linux, `socketfilterfw` on macOS.
/// The service is installed when its unit or plist exists.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPrivileges;

/// The facts that need only the filesystem, read off the async runtime.
#[allow(clippy::struct_excessive_bools)]
struct Local {
    alchemyd: PathBuf,
    uid: u32,
    helper: Option<HelperReading>,
    pkexec: bool,
    capable: bool,
    nosuid: bool,
    no_new_privs: bool,
    file_capable: bool,
    service_installed: bool,
    #[cfg(target_os = "linux")]
    firewall_files: FirewallFiles,
}

/// The firewall files alchemyd can read without root on Linux.
#[cfg(target_os = "linux")]
struct FirewallFiles {
    ufw_conf: Option<String>,
    ufw_profile: bool,
    firewalld_service: bool,
}

impl PrivilegeSystem for SystemPrivileges {
    fn read(&self) -> BoxFuture<'_, Result<SystemFacts, FactsError>> {
        Box::pin(async move {
            let local = tokio::task::spawn_blocking(read_local)
                .await
                .map_err(|e| FactsError::Task(e.to_string()))??;
            let firewall = firewall(&local).await;
            Ok(SystemFacts {
                alchemyd: local.alchemyd,
                uid: local.uid,
                helper: local.helper,
                pkexec: local.pkexec,
                capable: local.capable,
                nosuid: local.nosuid,
                no_new_privs: local.no_new_privs,
                file_capable: local.file_capable,
                firewall,
                service_installed: local.service_installed,
            })
        })
    }
}

fn read_local() -> Result<Local, FactsError> {
    let alchemyd = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| FactsError::CurrentExe(e.to_string()))?;
    let helper = helper(&alchemyd);
    let low = low_ports(&alchemyd);
    let uid = rustix::process::getuid().as_raw();
    let os = if cfg!(target_os = "linux") {
        HostOs::Linux
    } else {
        HostOs::MacOs
    };
    Ok(Local {
        uid,
        service_installed: super::facts::service_file(os, uid).exists(),
        #[cfg(target_os = "linux")]
        firewall_files: FirewallFiles {
            ufw_conf: std::fs::read_to_string("/etc/ufw/ufw.conf").ok(),
            ufw_profile: Path::new("/etc/ufw/applications.d/alchemy").exists(),
            firewalld_service: Path::new("/etc/firewalld/services/alchemy.xml").exists(),
        },
        helper,
        pkexec: cfg!(target_os = "linux") && Path::new(alchemy_core::privilege::PKEXEC).is_file(),
        capable: low.capable,
        nosuid: low.nosuid,
        no_new_privs: low.no_new_privs,
        file_capable: low.file_capable,
        alchemyd,
    })
}

fn helper(alchemyd: &Path) -> Option<HelperReading> {
    let sibling = alchemyd.with_file_name(HELPER);
    let canonical = std::fs::canonicalize(&sibling).ok()?;
    let path = match canonical.to_str().map(HelperPath::new) {
        Some(Ok(path)) => path,
        Some(Err(error)) => {
            tracing::warn!(%error, "alchemy-helper's path can't be used");
            return None;
        }
        None => {
            tracing::warn!(path = %canonical.display(), "alchemy-helper's path isn't UTF-8");
            return None;
        }
    };
    let meta = path
        .trust_paths()
        .into_iter()
        .filter_map(|p| {
            let m = std::fs::symlink_metadata(&p).ok()?;
            let file_type = m.file_type();
            let kind = if file_type.is_symlink() {
                FileKind::Symlink
            } else if file_type.is_dir() {
                FileKind::Dir
            } else if file_type.is_file() {
                FileKind::File
            } else {
                FileKind::Other
            };
            Some((
                p,
                FileMeta {
                    kind,
                    uid: m.uid(),
                    mode: m.mode(),
                },
            ))
        })
        .collect::<BTreeMap<_, _>>();
    Some(HelperReading { path, meta })
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct LowPorts {
    capable: bool,
    nosuid: bool,
    no_new_privs: bool,
    file_capable: bool,
}

#[cfg(target_os = "linux")]
fn low_ports(alchemyd: &Path) -> LowPorts {
    use rustix::fs::StatVfsMountFlags;

    let status = std::fs::read_to_string("/proc/self/status")
        .map(|text| super::facts::parse_proc_status(&text))
        .unwrap_or_default();
    let nosuid = rustix::fs::statvfs(alchemyd)
        .is_ok_and(|vfs| vfs.f_flag.contains(StatVfsMountFlags::NOSUID));
    let mut buf = [0u8; 64];
    let file_capable = rustix::fs::getxattr(alchemyd, "security.capability", &mut buf)
        .ok()
        .and_then(|len| buf.get(..len))
        .is_some_and(super::facts::grants_net_bind_service);
    LowPorts {
        capable: status.capable(),
        nosuid,
        no_new_privs: status.no_new_privs,
        file_capable,
    }
}

#[cfg(target_os = "macos")]
fn low_ports(_alchemyd: &Path) -> LowPorts {
    LowPorts::default()
}

#[cfg(target_os = "linux")]
async fn firewall(local: &Local) -> FirewallFacts {
    let files = &local.firewall_files;
    let firewalld = query("/usr/bin/systemctl", &["is-active", "firewalld"]).await;
    super::facts::linux_firewall(
        files.ufw_conf.as_deref(),
        files.ufw_profile,
        firewalld.as_deref(),
        files.firewalld_service,
    )
}

#[cfg(target_os = "macos")]
async fn firewall(local: &Local) -> FirewallFacts {
    const SOCKETFILTERFW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
    let Some(path) = local.alchemyd.to_str() else {
        return FirewallFacts {
            active: None,
            opened: false,
        };
    };
    let blocked_args = ["--getappblocked", path];
    let (global, apps, blocked) = tokio::join!(
        query(SOCKETFILTERFW, &["--getglobalstate"]),
        query(SOCKETFILTERFW, &["--listapps"]),
        query(SOCKETFILTERFW, &blocked_args),
    );
    super::facts::macos_firewall(global.as_deref(), apps.as_deref(), blocked.as_deref(), path)
}

/// What `program` prints on stdout, whatever its exit status, or `None`
/// when it can't run or takes too long.
async fn query(program: &str, args: &[&str]) -> Option<String> {
    let run = tokio::process::Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(QUERY_TIMEOUT, run).await.ok()?.ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
