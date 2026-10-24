//! Resource usage: what the machine and its containers use, now and over
//! time.
//!
//! The daemon reads raw cumulative counters into a [`Sample`]. Two
//! consecutive samples become one [`MetricsPoint`] of rates with
//! [`usage_from`]. Points are stored at the finest [`Tier`] and rolled up
//! into coarser ones with [`rollup`], following [`plan_rollups`]; each tier
//! is kept for [`Tier::keep`].
//!
//! CPU is a percentage of the whole machine, so containers add up to the
//! machine's figure rather than counting each core as 100%. Rates are bytes
//! per second and memory is bytes. A value that could not be measured is
//! `None`, never zero, and averages skip it.

mod rates;
mod rollup;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use rates::{
    MAX_RATE, clamp_percent, container_memory, counter_delta, cpu_percent, projects_of, rate,
    usage_from,
};
pub use rollup::{bucket_start, min_points, plan_rollups, rollup, rollup_window};

/// A machine's CPU time as its OS reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CpuReading {
    /// Cumulative busy and total ticks since boot, as Linux reports in
    /// `/proc/stat`. Two readings give the busy share in between.
    Ticks {
        /// Ticks spent on anything but idle and I/O wait.
        busy: u64,
        /// Every tick.
        total: u64,
    },
    /// Busy share since the previous reading, already computed by the OS
    /// interface, as on macOS where Alchemy cannot read the raw ticks.
    Percent(f64),
}

/// Bytes received and sent on one network interface, cumulative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Traffic {
    /// Bytes received.
    pub rx: u64,
    /// Bytes sent.
    pub tx: u64,
}

/// Bytes read from and written to one disk, cumulative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskIo {
    /// Bytes read.
    pub read: u64,
    /// Bytes written.
    pub written: u64,
}

/// The machine's raw counters at one moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostSample {
    /// CPU time, if the OS reported it.
    pub cpu: Option<CpuReading>,
    /// Logical CPUs.
    pub cpus: u32,
    /// The 1, 5, and 15 minute load averages, where the OS has them.
    pub load: Option<[f64; 3]>,
    /// Memory in use, in bytes.
    pub memory_used: u64,
    /// Installed memory, in bytes.
    pub memory_total: u64,
    /// Swap in use, in bytes.
    pub swap_used: u64,
    /// Swap available, in bytes.
    pub swap_total: u64,
    /// Space used on the disk Alchemy's data lives on, in bytes.
    pub disk_used: Option<u64>,
    /// Size of that disk, in bytes.
    pub disk_total: Option<u64>,
    /// Cumulative traffic per network interface that counts toward the
    /// machine's total.
    pub net: Vec<(String, Traffic)>,
    /// Cumulative I/O per disk that counts toward the machine's total.
    pub disks: Vec<(String, DiskIo)>,
}

/// One container's raw counters at one moment, from the Engine API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerSample {
    /// Container ID.
    pub id: String,
    /// Container name, without the leading `/`.
    pub name: String,
    /// Compose project, from the `com.docker.compose.project` label.
    pub project: Option<String>,
    /// Compose service, from the `com.docker.compose.service` label.
    pub service: Option<String>,
    /// CPU time the container used, in nanoseconds, cumulative.
    pub cpu_usage: u64,
    /// CPU time the whole engine host used, in nanoseconds, cumulative.
    pub system_cpu: Option<u64>,
    /// Memory in use, without the reclaimable page cache; see
    /// [`container_memory`].
    pub memory: u64,
    /// Traffic summed over the container's networks, or `None` for a
    /// container on the host network, which reports none.
    pub net: Option<Traffic>,
    /// Block I/O, when the engine reports it.
    pub block: Option<DiskIo>,
}

/// Everything read in one collection cycle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// When the cycle ran, in Unix milliseconds.
    pub at_ms: i64,
    /// The machine, or `None` if it could not be read.
    pub host: Option<HostSample>,
    /// Every running container whose stats arrived.
    pub containers: Vec<ContainerSample>,
}

/// The machine's usage at one point.
///
/// On macOS this is the Mac itself, not the virtual machine Docker Desktop
/// runs containers in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineUsage {
    /// CPU busy, as a percentage of the whole machine.
    #[serde(default)]
    pub cpu: Option<f64>,
    /// The highest [`cpu`](Self::cpu) within a rolled-up point.
    #[serde(default)]
    pub cpu_max: Option<f64>,
    /// Logical CPUs.
    pub cpus: u32,
    /// The 1, 5, and 15 minute load averages, where the OS has them.
    #[serde(default)]
    pub load: Option<[f64; 3]>,
    /// Memory in use, in bytes.
    pub memory_used: u64,
    /// Installed memory, in bytes.
    pub memory_total: u64,
    /// Swap in use, in bytes.
    pub swap_used: u64,
    /// Swap available, in bytes.
    pub swap_total: u64,
    /// Space used on the disk Alchemy's data lives on, in bytes.
    #[serde(default)]
    pub disk_used: Option<u64>,
    /// Size of that disk, in bytes.
    #[serde(default)]
    pub disk_total: Option<u64>,
    /// Disk reads, in bytes per second.
    #[serde(default)]
    pub disk_read: Option<f64>,
    /// Disk writes, in bytes per second.
    #[serde(default)]
    pub disk_write: Option<f64>,
    /// Network received, in bytes per second.
    #[serde(default)]
    pub net_rx: Option<f64>,
    /// Network sent, in bytes per second.
    #[serde(default)]
    pub net_tx: Option<f64>,
}

/// One container's usage at one point.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContainerUsage {
    /// Container ID; the latest one in a rolled-up point.
    pub id: String,
    /// Container name.
    pub name: String,
    /// Compose project, or `None` for a container outside Compose.
    #[serde(default)]
    pub project: Option<String>,
    /// Compose service.
    #[serde(default)]
    pub service: Option<String>,
    /// CPU busy, as a percentage of the whole engine host.
    #[serde(default)]
    pub cpu: Option<f64>,
    /// The highest [`cpu`](Self::cpu) within a rolled-up point.
    #[serde(default)]
    pub cpu_max: Option<f64>,
    /// Memory in use, in bytes.
    pub memory: u64,
    /// The highest [`memory`](Self::memory) within a rolled-up point.
    #[serde(default)]
    pub memory_max: Option<u64>,
    /// Network received, in bytes per second.
    #[serde(default)]
    pub net_rx: Option<f64>,
    /// Network sent, in bytes per second.
    #[serde(default)]
    pub net_tx: Option<f64>,
    /// Block reads, in bytes per second.
    #[serde(default)]
    pub block_read: Option<f64>,
    /// Block writes, in bytes per second.
    #[serde(default)]
    pub block_write: Option<f64>,
}

/// One Compose project's containers added up.
///
/// A project is what the UI and CLI call an app.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectUsage {
    /// Compose project name.
    pub name: String,
    /// Containers counted.
    pub containers: u32,
    /// CPU busy, as a percentage of the whole engine host, or `None` if no
    /// container had a figure.
    #[serde(default)]
    pub cpu: Option<f64>,
    /// Memory in use, in bytes.
    pub memory: u64,
    /// Network received, in bytes per second.
    #[serde(default)]
    pub net_rx: Option<f64>,
    /// Network sent, in bytes per second.
    #[serde(default)]
    pub net_tx: Option<f64>,
}

/// Usage at one time: the machine, each Compose project, and each
/// container.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricsPoint {
    /// Unix seconds: when it was sampled, or the start of the bucket for a
    /// rolled-up point.
    pub t: i64,
    /// The machine, or `None` if it could not be read.
    #[serde(default)]
    pub machine: Option<MachineUsage>,
    /// Each Compose project, by name.
    #[serde(default)]
    pub projects: Vec<ProjectUsage>,
    /// Each container, by name.
    #[serde(default)]
    pub containers: Vec<ContainerUsage>,
}

impl MetricsPoint {
    /// The point with only `project`'s containers and totals, and no
    /// machine figures.
    #[must_use]
    pub fn for_project(mut self, project: &str) -> Self {
        self.machine = None;
        self.projects.retain(|p| p.name == project);
        self.containers
            .retain(|c| c.project.as_deref() == Some(project));
        self
    }
}

/// How far back a history request looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MetricsRange {
    /// The last hour, a point a minute.
    #[serde(rename = "1h")]
    Hour,
    /// The last 12 hours, a point every 10 minutes.
    #[serde(rename = "12h")]
    HalfDay,
    /// The last 24 hours, a point every 20 minutes.
    #[serde(rename = "24h")]
    Day,
    /// The last 7 days, a point every 2 hours.
    #[serde(rename = "7d")]
    Week,
    /// The last 30 days, a point every 8 hours.
    #[serde(rename = "30d")]
    Month,
}

impl MetricsRange {
    /// Every range, shortest first.
    pub const ALL: [Self; 5] = [
        Self::Hour,
        Self::HalfDay,
        Self::Day,
        Self::Week,
        Self::Month,
    ];

    /// The name used on the wire, such as `24h`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::HalfDay => "12h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
        }
    }

    /// The tier that serves this range.
    pub fn tier(self) -> Tier {
        match self {
            Self::Hour => TIERS[0],
            Self::HalfDay => TIERS[1],
            Self::Day => TIERS[2],
            Self::Week => TIERS[3],
            Self::Month => TIERS[4],
        }
    }
}

impl fmt::Display for MetricsRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A range name that is not one of [`MetricsRange::ALL`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown range {0:?}; use 1h, 12h, 24h, 7d, or 30d")]
pub struct UnknownRange(pub String);

impl FromStr for MetricsRange {
    type Err = UnknownRange;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|range| range.as_str() == s)
            .ok_or_else(|| UnknownRange(s.to_owned()))
    }
}

/// One resolution of stored history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// The range this tier serves.
    pub range: MetricsRange,
    /// Seconds per point.
    pub step: i64,
    /// Seconds the range covers.
    pub span: i64,
    /// Seconds points are kept: a little over `span`, so the oldest
    /// bucket of the range is complete.
    pub keep: i64,
}

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// Every tier, finest first. Each tier after the first is rolled up from
/// the one before it, and every range has 60 to 90 points.
pub const TIERS: [Tier; 5] = [
    Tier {
        range: MetricsRange::Hour,
        step: MINUTE,
        span: HOUR,
        keep: 2 * HOUR,
    },
    Tier {
        range: MetricsRange::HalfDay,
        step: 10 * MINUTE,
        span: 12 * HOUR,
        keep: 13 * HOUR,
    },
    Tier {
        range: MetricsRange::Day,
        step: 20 * MINUTE,
        span: DAY,
        keep: 25 * HOUR,
    },
    Tier {
        range: MetricsRange::Week,
        step: 2 * HOUR,
        span: 7 * DAY,
        keep: 8 * DAY,
    },
    Tier {
        range: MetricsRange::Month,
        step: 8 * HOUR,
        span: 30 * DAY,
        keep: 31 * DAY,
    },
];

/// Whether containers run in a virtual machine rather than on the machine
/// Alchemy measures: always on macOS, and with Docker Desktop anywhere.
/// Container CPU is then a share of that VM, not of the machine.
pub fn containers_in_vm(macos: bool, docker_desktop: bool) -> bool {
    macos || docker_desktop
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
    fn ranges_parse_and_print() {
        for range in MetricsRange::ALL {
            assert_eq!(range.as_str().parse::<MetricsRange>().unwrap(), range);
            assert_eq!(range.to_string(), range.as_str());
            assert_eq!(
                serde_json::to_string(&range).unwrap(),
                format!("\"{range}\"")
            );
            assert_eq!(range.tier().range, range);
        }
        for bad in ["", "1H", "2h", "1d"] {
            assert_eq!(
                bad.parse::<MetricsRange>(),
                Err(UnknownRange(bad.to_owned()))
            );
        }
    }

    #[test]
    fn tiers_nest_and_hold_60_to_90_points() {
        for (index, tier) in TIERS.iter().enumerate() {
            let points = tier.span / tier.step;
            assert!((60..=90).contains(&points), "{tier:?}: {points}");
            assert!(tier.keep > tier.span, "{tier:?}");
            if let Some(finer) = index.checked_sub(1).map(|i| TIERS[i]) {
                assert_eq!(tier.step % finer.step, 0, "{tier:?}");
                assert!(tier.keep > finer.keep, "{tier:?}");
            }
        }
    }

    #[test]
    fn for_project_keeps_only_that_project() {
        let container = |name: &str, project: Option<&str>| ContainerUsage {
            id: name.into(),
            name: name.into(),
            project: project.map(Into::into),
            ..ContainerUsage::default()
        };
        let project = |name: &str| ProjectUsage {
            name: name.into(),
            containers: 1,
            ..ProjectUsage::default()
        };
        let point = MetricsPoint {
            t: 60,
            machine: Some(MachineUsage::default()),
            projects: vec![project("web"), project("db")],
            containers: vec![
                container("web-1", Some("web")),
                container("db-1", Some("db")),
                container("loose", None),
            ],
        };
        let got = point.for_project("web");
        assert_eq!(got.machine, None);
        assert_eq!(got.projects, [project("web")]);
        assert_eq!(got.containers, [container("web-1", Some("web"))]);
    }

    #[test]
    fn containers_in_vm_on_macos_or_docker_desktop() {
        let cases = [
            (false, false, false),
            (false, true, true),
            (true, false, true),
            (true, true, true),
        ];
        for (macos, desktop, want) in cases {
            assert_eq!(containers_in_vm(macos, desktop), want, "{macos} {desktop}");
        }
    }
}
