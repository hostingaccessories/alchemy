//! Turning two raw samples into rates.

use std::collections::BTreeMap;

use super::{
    ContainerSample, ContainerUsage, CpuReading, DiskIo, HostSample, MachineUsage, MetricsPoint,
    ProjectUsage, Sample, Traffic,
};

/// The fastest believable transfer rate, 10 GB/s. A faster one comes from
/// a counter glitch and is dropped.
pub const MAX_RATE: f64 = 10e9;

/// `cur - prev` for a cumulative counter. A counter that went backwards
/// was reset, by a restarted container or a rebooted machine, and has
/// counted `cur` since.
pub fn counter_delta(cur: u64, prev: u64) -> u64 {
    cur.checked_sub(prev).unwrap_or(cur)
}

/// A counter's change per second over `secs`, or `None` when `secs` is not
/// positive or the rate is above [`MAX_RATE`].
pub fn rate(cur: u64, prev: u64, secs: f64) -> Option<f64> {
    if secs.is_nan() || secs <= 0.0 {
        return None;
    }
    let rate = to_f64(counter_delta(cur, prev)) / secs;
    (rate <= MAX_RATE).then_some(round2(rate))
}

/// `value` limited to 0 through 100, with NaN as 0.
pub fn clamp_percent(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 100.0)
    }
}

/// The machine's CPU busy percentage between `prev` and `cur`.
///
/// A [`CpuReading::Percent`] stands on its own. Ticks need a previous
/// reading in ticks, and no ticks elapsed means no figure.
pub fn cpu_percent(prev: Option<CpuReading>, cur: CpuReading) -> Option<f64> {
    match (prev, cur) {
        (_, CpuReading::Percent(percent)) => Some(round2(clamp_percent(percent))),
        (
            Some(CpuReading::Ticks {
                busy: prev_busy,
                total: prev_total,
            }),
            CpuReading::Ticks { busy, total },
        ) => {
            let total = counter_delta(total, prev_total);
            if total == 0 {
                return None;
            }
            let busy = counter_delta(busy, prev_busy).min(total);
            Some(round2(clamp_percent(to_f64(busy) / to_f64(total) * 100.0)))
        }
        (_, CpuReading::Ticks { .. }) => None,
    }
}

/// A container's memory in use, as `docker stats` shows it: usage less the
/// reclaimable page cache, which is `total_inactive_file` under cgroup v1
/// and `inactive_file` under cgroup v2.
pub fn container_memory(
    usage: u64,
    total_inactive_file: Option<u64>,
    inactive_file: Option<u64>,
) -> u64 {
    match total_inactive_file.or(inactive_file) {
        Some(cache) if cache < usage => usage - cache,
        _ => usage,
    }
}

/// The point `cur` describes, with rates since `prev`.
///
/// Without a previous sample, or for a container that was not in it, only
/// the levels (memory, disk space) are known and every rate is `None`.
pub fn usage_from(prev: Option<&Sample>, cur: &Sample) -> MetricsPoint {
    let secs = prev.map_or(0.0, |prev| to_f64_i(cur.at_ms - prev.at_ms) / 1000.0);
    let prev_host = prev.and_then(|p| p.host.as_ref());
    let machine = cur
        .host
        .as_ref()
        .map(|host| machine_usage(prev_host, host, secs));
    let mut containers: Vec<ContainerUsage> = cur
        .containers
        .iter()
        .map(|c| {
            let before = prev.and_then(|p| p.containers.iter().find(|b| b.id == c.id));
            container_usage(before, c, secs)
        })
        .collect();
    containers.sort_by(|a, b| a.name.cmp(&b.name));
    MetricsPoint {
        t: cur.at_ms.div_euclid(1000),
        machine,
        projects: projects_of(&containers),
        containers,
    }
}

fn machine_usage(prev: Option<&HostSample>, cur: &HostSample, secs: f64) -> MachineUsage {
    let mut usage = MachineUsage {
        cpus: cur.cpus,
        load: cur.load.map(|load| load.map(round2)),
        memory_used: cur.memory_used,
        memory_total: cur.memory_total,
        swap_used: cur.swap_used,
        swap_total: cur.swap_total,
        disk_used: cur.disk_used,
        disk_total: cur.disk_total,
        cpu: cur
            .cpu
            .and_then(|reading| cpu_percent(prev.and_then(|p| p.cpu), reading)),
        ..MachineUsage::default()
    };
    if let Some(prev) = prev {
        let net = paired(&prev.net, &cur.net);
        usage.net_rx = sum_rates(net.iter().map(|(p, c)| rate(c.rx, p.rx, secs)));
        usage.net_tx = sum_rates(net.iter().map(|(p, c)| rate(c.tx, p.tx, secs)));
        let disks = paired(&prev.disks, &cur.disks);
        usage.disk_read = sum_rates(disks.iter().map(|(p, c)| rate(c.read, p.read, secs)));
        usage.disk_write = sum_rates(disks.iter().map(|(p, c)| rate(c.written, p.written, secs)));
    }
    usage
}

/// The counters of every name present in both `prev` and `cur`. Rates are
/// per interface or disk, then summed, so one that disappears cannot make
/// the total go backwards.
fn paired<T: Copy>(prev: &[(String, T)], cur: &[(String, T)]) -> Vec<(T, T)> {
    cur.iter()
        .filter_map(|(name, c)| prev.iter().find(|(p, _)| p == name).map(|(_, p)| (*p, *c)))
        .collect()
}

fn sum_rates(rates: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    rates.flatten().reduce(|a, b| a + b).map(round2)
}

fn container_usage(
    prev: Option<&ContainerSample>,
    cur: &ContainerSample,
    secs: f64,
) -> ContainerUsage {
    let mut usage = ContainerUsage {
        id: cur.id.clone(),
        name: cur.name.clone(),
        project: cur.project.clone(),
        service: cur.service.clone(),
        memory: cur.memory,
        ..ContainerUsage::default()
    };
    let Some(prev) = prev else {
        return usage;
    };
    usage.cpu = match (prev.system_cpu, cur.system_cpu) {
        (Some(before), Some(now)) => {
            let system = counter_delta(now, before);
            (system > 0).then(|| {
                let used = counter_delta(cur.cpu_usage, prev.cpu_usage);
                round2(clamp_percent(to_f64(used) / to_f64(system) * 100.0))
            })
        }
        _ => None,
    };
    if let (Some(Traffic { rx: prx, tx: ptx }), Some(Traffic { rx, tx })) = (prev.net, cur.net) {
        usage.net_rx = rate(rx, prx, secs);
        usage.net_tx = rate(tx, ptx, secs);
    }
    if let (
        Some(DiskIo {
            read: pread,
            written: pwritten,
        }),
        Some(DiskIo { read, written }),
    ) = (prev.block, cur.block)
    {
        usage.block_read = rate(read, pread, secs);
        usage.block_write = rate(written, pwritten, secs);
    }
    usage
}

/// Each Compose project's containers added up, by name. Containers outside
/// Compose are left out.
pub fn projects_of(containers: &[ContainerUsage]) -> Vec<ProjectUsage> {
    let mut projects: BTreeMap<&str, ProjectUsage> = BTreeMap::new();
    for container in containers {
        let Some(name) = container.project.as_deref() else {
            continue;
        };
        let project = projects.entry(name).or_insert_with(|| ProjectUsage {
            name: name.to_owned(),
            ..ProjectUsage::default()
        });
        project.containers += 1;
        project.cpu = add(project.cpu, container.cpu);
        project.memory = project.memory.saturating_add(container.memory);
        project.net_rx = add(project.net_rx, container.net_rx);
        project.net_tx = add(project.net_tx, container.net_tx);
    }
    projects.into_values().collect()
}

fn add(total: Option<f64>, value: Option<f64>) -> Option<f64> {
    match (total, value) {
        (Some(a), Some(b)) => Some(round2(a + b)),
        (a, b) => a.or(b),
    }
}

/// Two decimals: plenty for percentages and byte rates, and it keeps
/// stored points short.
pub(super) fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Byte counts lose precision above 2^53, far past any real counter.
#[allow(clippy::cast_precision_loss)]
pub(super) fn to_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::cast_precision_loss)]
fn to_f64_i(value: i64) -> f64 {
    value as f64
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
mod tests {
    use super::*;

    #[test]
    fn counter_delta_treats_a_backwards_counter_as_reset() {
        let cases = [(10, 4, 6), (4, 4, 0), (3, 10, 3), (0, 10, 0)];
        for (cur, prev, want) in cases {
            assert_eq!(counter_delta(cur, prev), want, "{cur} {prev}");
        }
    }

    #[test]
    fn rate_is_per_second_and_drops_nonsense() {
        let cases = [
            ("steady", 1600, 1000, 60.0, Some(10.0)),
            ("rounded", 100, 0, 3.0, Some(33.33)),
            ("reset", 50, 1000, 10.0, Some(5.0)),
            ("no time", 1600, 1000, 0.0, None),
            ("time went back", 1600, 1000, -1.0, None),
            ("NaN", 1600, 1000, f64::NAN, None),
            ("too fast", 20_000_000_000, 0, 1.0, None),
            ("at the limit", 10_000_000_000, 0, 1.0, Some(MAX_RATE)),
        ];
        for (name, cur, prev, secs, want) in cases {
            assert_eq!(rate(cur, prev, secs), want, "{name}");
        }
    }

    #[test]
    fn clamp_percent_limits_to_0_through_100() {
        let cases = [
            (-5.0, 0.0),
            (0.0, 0.0),
            (42.5, 42.5),
            (100.0, 100.0),
            (180.0, 100.0),
            (f64::NAN, 0.0),
            (f64::INFINITY, 100.0),
        ];
        for (value, want) in cases {
            assert_eq!(clamp_percent(value), want, "{value}");
        }
    }

    #[test]
    fn cpu_percent_from_ticks_or_a_percentage() {
        let ticks = |busy, total| CpuReading::Ticks { busy, total };
        let cases = [
            ("first ticks", None, ticks(10, 100), None),
            (
                "quarter busy",
                Some(ticks(10, 100)),
                ticks(35, 200),
                Some(25.0),
            ),
            ("idle", Some(ticks(10, 100)), ticks(10, 200), Some(0.0)),
            ("no ticks", Some(ticks(10, 100)), ticks(10, 100), None),
            (
                "busy past total",
                Some(ticks(0, 100)),
                ticks(500, 200),
                Some(100.0),
            ),
            ("reboot", Some(ticks(900, 1000)), ticks(30, 60), Some(50.0)),
            ("percent", None, CpuReading::Percent(12.345), Some(12.35)),
            (
                "percent clamped",
                None,
                CpuReading::Percent(130.0),
                Some(100.0),
            ),
            (
                "percent after ticks",
                Some(ticks(1, 2)),
                CpuReading::Percent(-3.0),
                Some(0.0),
            ),
            (
                "ticks after percent",
                Some(CpuReading::Percent(5.0)),
                ticks(1, 2),
                None,
            ),
        ];
        for (name, prev, cur, want) in cases {
            assert_eq!(cpu_percent(prev, cur), want, "{name}");
        }
    }

    #[test]
    fn container_memory_drops_the_page_cache() {
        let cases = [
            ("v2", 1000, None, Some(300), 700),
            ("v1", 1000, Some(400), Some(300), 600),
            ("no stats", 1000, None, None, 1000),
            ("cache above usage", 1000, None, Some(2000), 1000),
            ("cache equal to usage", 1000, Some(1000), None, 1000),
        ];
        for (name, usage, v1, v2, want) in cases {
            assert_eq!(container_memory(usage, v1, v2), want, "{name}");
        }
    }

    fn host(cpu: Option<CpuReading>, rx: u64, tx: u64) -> HostSample {
        HostSample {
            cpu,
            cpus: 4,
            load: Some([0.5, 0.25, 0.125]),
            memory_used: 4_000,
            memory_total: 16_000,
            swap_used: 10,
            swap_total: 100,
            disk_used: Some(50),
            disk_total: Some(500),
            net: vec![("eth0".into(), Traffic { rx, tx })],
            disks: vec![(
                "sda".into(),
                DiskIo {
                    read: rx * 2,
                    written: tx * 2,
                },
            )],
        }
    }

    fn container(
        id: &str,
        project: Option<&str>,
        cpu: u64,
        system: u64,
        rx: u64,
    ) -> ContainerSample {
        ContainerSample {
            id: id.into(),
            name: format!("{id}-name"),
            project: project.map(Into::into),
            service: Some("svc".into()),
            cpu_usage: cpu,
            system_cpu: Some(system),
            memory: 1_000,
            net: Some(Traffic { rx, tx: rx / 2 }),
            block: Some(DiskIo {
                read: rx,
                written: 0,
            }),
        }
    }

    #[test]
    fn first_sample_has_levels_but_no_rates() {
        let cur = Sample {
            at_ms: 61_500,
            host: Some(host(Some(CpuReading::Ticks { busy: 1, total: 2 }), 100, 50)),
            containers: vec![container("a", Some("web"), 10, 100, 0)],
        };
        let point = usage_from(None, &cur);
        assert_eq!(point.t, 61);
        let machine = point.machine.unwrap();
        assert_eq!(machine.cpu, None);
        assert_eq!(machine.net_rx, None);
        assert_eq!(machine.disk_read, None);
        assert_eq!(machine.memory_used, 4_000);
        assert_eq!(machine.disk_total, Some(500));
        assert_eq!(machine.load, Some([0.5, 0.25, 0.13]));
        let c = &point.containers[0];
        assert_eq!((c.cpu, c.net_rx, c.memory), (None, None, 1_000));
        assert_eq!(
            point.projects,
            [ProjectUsage {
                name: "web".into(),
                containers: 1,
                cpu: None,
                memory: 1_000,
                net_rx: None,
                net_tx: None,
            }]
        );
    }

    #[test]
    fn two_samples_give_rates() {
        let prev = Sample {
            at_ms: 0,
            host: Some(host(Some(CpuReading::Ticks { busy: 0, total: 0 }), 0, 0)),
            containers: vec![
                container("a", Some("web"), 0, 0, 0),
                container("b", Some("web"), 0, 0, 0),
                container("gone", None, 0, 0, 0),
            ],
        };
        let cur = Sample {
            at_ms: 10_000,
            host: Some(host(
                Some(CpuReading::Ticks {
                    busy: 30,
                    total: 120,
                }),
                1_000,
                500,
            )),
            containers: vec![
                container("b", Some("web"), 300, 1_000, 2_000),
                container("a", Some("web"), 100, 1_000, 1_000),
                container("c", Some("db"), 50, 1_000, 10),
                container("loose", None, 500, 1_000, 0),
            ],
        };
        let point = usage_from(Some(&prev), &cur);
        assert_eq!(point.t, 10);

        let machine = point.machine.unwrap();
        assert_eq!(machine.cpu, Some(25.0));
        assert_eq!((machine.net_rx, machine.net_tx), (Some(100.0), Some(50.0)));
        assert_eq!(
            (machine.disk_read, machine.disk_write),
            (Some(200.0), Some(100.0))
        );

        let names: Vec<_> = point.containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a-name", "b-name", "c-name", "loose-name"]);
        let a = &point.containers[0];
        assert_eq!(a.cpu, Some(10.0));
        assert_eq!((a.net_rx, a.net_tx), (Some(100.0), Some(50.0)));
        assert_eq!((a.block_read, a.block_write), (Some(100.0), Some(0.0)));
        let c = &point.containers[2];
        assert_eq!((c.cpu, c.net_rx), (None, None), "new container");

        assert_eq!(
            point.projects,
            [
                ProjectUsage {
                    name: "db".into(),
                    containers: 1,
                    cpu: None,
                    memory: 1_000,
                    net_rx: None,
                    net_tx: None,
                },
                ProjectUsage {
                    name: "web".into(),
                    containers: 2,
                    cpu: Some(40.0),
                    memory: 2_000,
                    net_rx: Some(300.0),
                    net_tx: Some(150.0),
                },
            ]
        );
    }

    #[test]
    fn interfaces_that_come_and_go_do_not_count() {
        let mut prev = host(None, 1_000, 1_000);
        prev.net.push((
            "wlan0".into(),
            Traffic {
                rx: 9_999,
                tx: 9_999,
            },
        ));
        let mut cur = host(None, 1_100, 1_000);
        cur.net.push(("usb0".into(), Traffic { rx: 50, tx: 50 }));
        let usage = machine_usage(Some(&prev), &cur, 1.0);
        assert_eq!((usage.net_rx, usage.net_tx), (Some(100.0), Some(0.0)));

        cur.net.retain(|(name, _)| name == "usb0");
        let usage = machine_usage(Some(&prev), &cur, 1.0);
        assert_eq!((usage.net_rx, usage.net_tx), (None, None));
    }

    #[test]
    fn container_cpu_needs_system_time_on_both_sides() {
        type Case = (&'static str, Option<u64>, Option<u64>, Option<f64>);
        let mut prev = container("a", None, 0, 0, 0);
        let mut cur = container("a", None, 500, 1_000, 0);
        let cases: [Case; 4] = [
            ("both", Some(0), Some(1_000), Some(50.0)),
            ("no previous", None, Some(1_000), None),
            ("no current", Some(0), None, None),
            ("no system time passed", Some(1_000), Some(1_000), None),
        ];
        for (name, before, now, want) in cases {
            prev.system_cpu = before;
            cur.system_cpu = now;
            assert_eq!(container_usage(Some(&prev), &cur, 1.0).cpu, want, "{name}");
        }
    }

    #[test]
    fn host_network_containers_have_no_traffic() {
        let mut prev = container("a", None, 0, 0, 0);
        let mut cur = container("a", None, 0, 0, 100);
        prev.net = None;
        cur.net = None;
        let usage = container_usage(Some(&prev), &cur, 1.0);
        assert_eq!((usage.net_rx, usage.net_tx), (None, None));
    }
}
