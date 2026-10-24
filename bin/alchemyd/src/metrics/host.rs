//! Pure decisions behind reading the machine's counters: parsing what the
//! OS reports and choosing which interfaces and disk count.

use std::path::Path;

use alchemy_core::metrics::CpuReading;

/// The aggregate `cpu` line of Linux's `/proc/stat` as busy and total
/// ticks.
///
/// The fields are user, nice, system, idle, iowait, irq, softirq, steal,
/// then guest and guest nice, which Linux already counts in user and nice
/// and so are left out of the total. Idle and iowait are not busy.
pub fn parse_proc_stat(text: &str) -> Option<CpuReading> {
    let line = text.lines().find(|line| {
        line.strip_prefix("cpu")
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    })?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let idle = fields.get(3)? + fields.get(4).copied().unwrap_or(0);
    let total: u64 = fields.iter().sum();
    Some(CpuReading::Ticks {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// Interface name prefixes that are loopback, container, VM, or tunnel
/// devices. Their traffic is either local or already counted on a
/// physical interface.
const VIRTUAL_INTERFACES: &[&str] = &[
    "lo",
    "docker",
    "veth",
    "br-",
    "virbr",
    "vnet",
    "cni",
    "flannel",
    "cali",
    "tun",
    "tap",
    "utun",
    "tailscale",
    "wg",
    "zt",
    "awdl",
    "llw",
    "bridge",
    "gif",
    "stf",
    "anpi",
    "ap",
];

/// Whether traffic on interface `name` counts toward the machine's total.
pub fn counts_toward_total(name: &str) -> bool {
    !VIRTUAL_INTERFACES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// The index of the mount point in `mounts` that holds `path`: the longest
/// one `path` starts with.
pub fn disk_for<'a>(mounts: impl IntoIterator<Item = &'a Path>, path: &Path) -> Option<usize> {
    mounts
        .into_iter()
        .enumerate()
        .filter(|(_, mount)| path.starts_with(mount))
        .max_by_key(|(_, mount)| mount.components().count())
        .map(|(index, _)| index)
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
    fn parse_proc_stat_reads_the_aggregate_line() {
        let ticks = |busy, total| Some(CpuReading::Ticks { busy, total });
        let cases = [
            (
                "kernel 6",
                "cpu  100 5 50 800 20 3 2 10 7 0\ncpu0 50 2 25 400 10 1 1 5 3 0\nintr 1",
                ticks(170, 990),
            ),
            (
                "old kernel, four fields",
                "cpu 10 0 10 80\n",
                ticks(20, 100),
            ),
            ("per-CPU lines only", "cpu0 1 2 3 4\n", None),
            ("garbage", "cpu  a b c d\n", None),
            ("too short", "cpu  1 2 3\n", None),
            ("empty", "", None),
        ];
        for (name, text, want) in cases {
            assert_eq!(parse_proc_stat(text), want, "{name}");
        }
    }

    #[test]
    fn only_physical_interfaces_count() {
        let cases = [
            ("eth0", true),
            ("enp3s0", true),
            ("wlan0", true),
            ("en0", true),
            ("eno1", true),
            ("lo", false),
            ("lo0", false),
            ("docker0", false),
            ("veth1a2b3c", false),
            ("br-0123abcd", false),
            ("virbr0", false),
            ("tailscale0", false),
            ("wg0", false),
            ("utun3", false),
            ("awdl0", false),
            ("bridge100", false),
        ];
        for (name, want) in cases {
            assert_eq!(counts_toward_total(name), want, "{name}");
        }
    }

    #[test]
    fn disk_for_picks_the_deepest_mount_holding_the_path() {
        let mounts = [
            Path::new("/"),
            Path::new("/System/Volumes/Data"),
            Path::new("/home"),
            Path::new("/home2"),
        ];
        let cases = [
            ("/Users/me", Some(0)),
            ("/System/Volumes/Data/Users/me", Some(1)),
            ("/home/me", Some(2)),
            ("/home2/me", Some(3)),
            ("relative", None),
        ];
        for (path, want) in cases {
            assert_eq!(disk_for(mounts, Path::new(path)), want, "{path}");
        }
        assert_eq!(disk_for([], Path::new("/x")), None);
    }
}
