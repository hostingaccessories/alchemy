//! [`SystemMetrics`]: this machine's counters and its engine's container
//! stats.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use alchemy_core::engine::ContainerStatus;
use alchemy_core::metrics::{ContainerSample, CpuReading, DiskIo, HostSample, Traffic};
use sysinfo::{Disks, Networks, System};

use super::host::{counts_toward_total, disk_for};
use super::{MetricsSource, STATS_TIMEOUT, SourceError};
use crate::engine::{BollardApi, Endpoint};

/// The real [`MetricsSource`]: the machine through `sysinfo` and, on
/// Linux, `/proc/stat`; containers through the Engine API.
///
/// On macOS these are the Mac's own figures, not those of the VM Docker
/// Desktop runs containers in. Disk space and disk I/O are for the disk
/// holding the user's home directory, where Alchemy keeps its data. Cheap
/// to clone: clones share the readers.
#[derive(Clone)]
pub struct SystemMetrics {
    host: Arc<Mutex<Option<HostReader>>>,
    engine: Arc<Mutex<Option<(String, BollardApi)>>>,
    data_dir: PathBuf,
}

impl SystemMetrics {
    /// A source measuring the disk that holds the user's home directory.
    /// Nothing is read until the first sample.
    pub fn new() -> Self {
        Self::for_data_dir(std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")))
    }

    /// A source measuring the disk that holds `data_dir`.
    pub fn for_data_dir(data_dir: PathBuf) -> Self {
        Self {
            host: Arc::new(Mutex::new(None)),
            engine: Arc::new(Mutex::new(None)),
            data_dir,
        }
    }

    fn engine_for(&self, endpoint: &str) -> Result<BollardApi, SourceError> {
        let mut cached = self.engine.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_endpoint, api)) = cached.as_ref()
            && cached_endpoint == endpoint
        {
            return Ok(api.clone());
        }
        let api = BollardApi::connect(&Endpoint::parse(endpoint)?, STATS_TIMEOUT)?;
        *cached = Some((endpoint.to_owned(), api.clone()));
        Ok(api)
    }
}

impl Default for SystemMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SystemMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemMetrics")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl MetricsSource for SystemMetrics {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
            .unwrap_or(0)
    }

    async fn host(&self) -> Result<HostSample, SourceError> {
        let host = Arc::clone(&self.host);
        let data_dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let mut reader = host.lock().unwrap_or_else(PoisonError::into_inner);
            reader.get_or_insert_with(HostReader::new).sample(&data_dir)
        })
        .await
        .map_err(|error| SourceError::Host(error.to_string()))
    }

    async fn container(
        &self,
        endpoint: &str,
        container: &ContainerStatus,
    ) -> Result<ContainerSample, SourceError> {
        let api = self.engine_for(endpoint)?;
        Ok(api.container_stats(container).await?)
    }
}

/// The `sysinfo` state kept between samples.
struct HostReader {
    system: System,
    networks: Networks,
    disks: Disks,
    /// Whether a sample has refreshed the CPU figure since [`Self::new`].
    /// On macOS it is a percentage since the previous refresh, which means
    /// nothing until there was one a sampling interval ago.
    #[cfg(target_os = "macos")]
    cpu_primed: bool,
}

impl HostReader {
    fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        Self {
            system,
            networks: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            #[cfg(target_os = "macos")]
            cpu_primed: false,
        }
    }

    fn sample(&mut self, data_dir: &Path) -> HostSample {
        self.system.refresh_memory();
        self.system.refresh_cpu_usage();
        self.networks.refresh(true);
        self.disks.refresh(true);

        let cpu = self.cpu();
        let load = System::load_average();
        let disk = disk_for(
            self.disks.list().iter().map(sysinfo::Disk::mount_point),
            data_dir,
        )
        .and_then(|index| self.disks.list().get(index));
        HostSample {
            cpu,
            cpus: u32::try_from(self.system.cpus().len()).unwrap_or(u32::MAX),
            load: load_average(load.one, load.five, load.fifteen),
            memory_used: self.system.used_memory(),
            memory_total: self.system.total_memory(),
            swap_used: self.system.used_swap(),
            swap_total: self.system.total_swap(),
            disk_used: disk.map(|d| d.total_space().saturating_sub(d.available_space())),
            disk_total: disk.map(sysinfo::Disk::total_space),
            net: self
                .networks
                .list()
                .iter()
                .filter(|(name, _)| counts_toward_total(name))
                .map(|(name, data)| {
                    (
                        name.clone(),
                        Traffic {
                            rx: data.total_received(),
                            tx: data.total_transmitted(),
                        },
                    )
                })
                .collect(),
            disks: disk
                .map(|d| {
                    let usage = d.usage();
                    (
                        d.name().to_string_lossy().into_owned(),
                        DiskIo {
                            read: usage.total_read_bytes,
                            written: usage.total_written_bytes,
                        },
                    )
                })
                .into_iter()
                .collect(),
        }
    }

    /// Raw ticks from `/proc/stat`, so rates and counter resets are handled
    /// in one place with everything else.
    #[cfg(target_os = "linux")]
    #[expect(clippy::unused_self, reason = "the macOS reader needs self")]
    fn cpu(&mut self) -> Option<CpuReading> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        super::host::parse_proc_stat(&text)
    }

    /// The busy share since the previous refresh: macOS has no file of raw
    /// ticks, and reading them through Mach needs `unsafe`, which this
    /// workspace forbids.
    #[cfg(target_os = "macos")]
    fn cpu(&mut self) -> Option<CpuReading> {
        let primed = std::mem::replace(&mut self.cpu_primed, true);
        primed.then(|| CpuReading::Percent(f64::from(self.system.global_cpu_usage())))
    }
}

/// Load averages, or `None` where the OS reports none (all zeros and not
/// a number).
fn load_average(one: f64, five: f64, fifteen: f64) -> Option<[f64; 3]> {
    let load = [one, five, fifteen];
    load.iter().all(|v| v.is_finite()).then_some(load)
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
    fn load_average_drops_non_numbers() {
        assert_eq!(load_average(1.0, 0.5, 0.0), Some([1.0, 0.5, 0.0]));
        assert_eq!(load_average(f64::NAN, 0.5, 0.0), None);
    }

    /// Reads this machine: every figure is plausible, and a second sample
    /// gives the CPU a figure.
    #[tokio::test]
    async fn samples_this_machine() {
        let source = SystemMetrics::new();
        let first = source.host().await.unwrap();
        assert!(first.cpus >= 1);
        assert!(first.memory_total > 0);
        assert!(first.memory_used <= first.memory_total);
        assert!(source.now_ms() > 1_600_000_000_000);
        if let (Some(used), Some(total)) = (first.disk_used, first.disk_total) {
            assert!(used <= total);
        }
        assert!(first.net.iter().all(|(name, _)| counts_toward_total(name)));
        let second = source.host().await.unwrap();
        assert!(second.cpu.is_some());
        if cfg!(target_os = "macos") {
            assert_eq!(first.cpu, None, "a percentage needs a previous refresh");
        }
    }

    #[tokio::test]
    async fn container_stats_need_a_unix_endpoint() {
        let source = SystemMetrics::new();
        let container =
            ContainerStatus::new("abc", "web", alchemy_core::engine::ContainerState::Running);
        for endpoint in ["tcp://10.0.0.1:2375", "unix:///nonexistent/docker.sock"] {
            assert!(
                source.container(endpoint, &container).await.is_err(),
                "{endpoint}"
            );
        }
    }
}
