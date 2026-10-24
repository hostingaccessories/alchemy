//! Downsampling points from one tier into the next.

use std::collections::BTreeMap;

use super::rates::{projects_of, round2, to_f64};
use super::{ContainerUsage, MachineUsage, MetricsPoint, Tier};

/// The start of the `step`-second bucket holding `ts`, aligned to the Unix
/// epoch.
pub fn bucket_start(ts: i64, step: i64) -> i64 {
    if step <= 0 {
        return ts;
    }
    ts.div_euclid(step) * step
}

/// The fewest `source_step` points a `step` bucket needs to be built: half
/// of those it could hold, and at least one. A window the daemon was down
/// for most of stays a gap rather than a point from a few samples.
pub fn min_points(step: i64, source_step: i64) -> usize {
    let expected = if source_step > 0 {
        step / source_step
    } else {
        0
    };
    usize::try_from(expected / 2).unwrap_or(0).max(1)
}

/// The `[from, to)` window of `tier` buckets that [`plan_rollups`]
/// considers at `now`: every closed bucket whose source points are still
/// kept.
pub fn rollup_window(now: i64, tier: Tier, source: Tier) -> (i64, i64) {
    let to = bucket_start(now, tier.step);
    let oldest = now - source.keep;
    let mut from = bucket_start(oldest, tier.step);
    if from < oldest {
        from += tier.step;
    }
    (from.min(to), to)
}

/// The `tier` buckets to build at `now` from `source`, oldest first.
///
/// `built` holds the bucket starts `tier` already has and `source_times`
/// the timestamps of `source` points, both within [`rollup_window`]. A
/// bucket is built once it has closed, if it is missing and its source has
/// at least [`min_points`]. Building only what is missing makes this
/// idempotent, and looking back over everything the source still keeps
/// catches up after the daemon was stopped.
pub fn plan_rollups(
    now: i64,
    tier: Tier,
    source: Tier,
    built: &[i64],
    source_times: &[i64],
) -> Vec<i64> {
    let (from, to) = rollup_window(now, tier, source);
    let need = min_points(tier.step, source.step);
    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
    for &ts in source_times {
        if (from..to).contains(&ts) {
            *counts.entry(bucket_start(ts, tier.step)).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter(|&(bucket, count)| count >= need && !built.contains(&bucket))
        .map(|(bucket, _)| bucket)
        .collect()
}

/// One point at bucket start `t` from `points`, oldest first, or `None`
/// when there are none: an empty window is a gap, not zeros.
///
/// Averages are over the points where a value is present, so a container
/// that ran for part of the window is not diluted toward zero, and a
/// figure that was never measured stays `None`. The peaks (`cpu_max`,
/// `memory_max`) carry through every tier. Sizes such as installed memory
/// take the latest value.
pub fn rollup(t: i64, points: &[MetricsPoint]) -> Option<MetricsPoint> {
    if points.is_empty() {
        return None;
    }
    let machines: Vec<&MachineUsage> = points.iter().filter_map(|p| p.machine.as_ref()).collect();
    let mut by_name: BTreeMap<&str, Vec<&ContainerUsage>> = BTreeMap::new();
    for container in points.iter().flat_map(|p| &p.containers) {
        by_name.entry(&container.name).or_default().push(container);
    }
    let containers: Vec<ContainerUsage> = by_name
        .into_values()
        .filter_map(|points| roll_container(&points))
        .collect();
    Some(MetricsPoint {
        t,
        machine: roll_machine(&machines),
        projects: projects_of(&containers),
        containers,
    })
}

fn roll_machine(machines: &[&MachineUsage]) -> Option<MachineUsage> {
    let latest = machines.last()?;
    let loads: Vec<[f64; 3]> = machines.iter().filter_map(|m| m.load).collect();
    let load = (!loads.is_empty()).then(|| {
        [0, 1, 2].map(|i| mean(loads.iter().map(|l| l.get(i).copied())).unwrap_or_default())
    });
    Some(MachineUsage {
        cpu: mean(machines.iter().map(|m| m.cpu)),
        cpu_max: peak(machines.iter().flat_map(|m| [m.cpu_max, m.cpu])),
        cpus: latest.cpus,
        load,
        memory_used: mean_bytes(machines.iter().map(|m| m.memory_used)),
        memory_total: latest.memory_total,
        swap_used: mean_bytes(machines.iter().map(|m| m.swap_used)),
        swap_total: latest.swap_total,
        disk_used: mean(machines.iter().map(|m| m.disk_used.map(to_f64))).map(to_bytes),
        disk_total: machines.iter().rev().find_map(|m| m.disk_total),
        disk_read: mean(machines.iter().map(|m| m.disk_read)),
        disk_write: mean(machines.iter().map(|m| m.disk_write)),
        net_rx: mean(machines.iter().map(|m| m.net_rx)),
        net_tx: mean(machines.iter().map(|m| m.net_tx)),
    })
}

fn roll_container(points: &[&ContainerUsage]) -> Option<ContainerUsage> {
    let latest = points.last()?;
    let memory_max = points
        .iter()
        .flat_map(|c| [c.memory_max, Some(c.memory)])
        .flatten()
        .max();
    Some(ContainerUsage {
        id: latest.id.clone(),
        name: latest.name.clone(),
        project: latest.project.clone(),
        service: latest.service.clone(),
        cpu: mean(points.iter().map(|c| c.cpu)),
        cpu_max: peak(points.iter().flat_map(|c| [c.cpu_max, c.cpu])),
        memory: mean_bytes(points.iter().map(|c| c.memory)),
        memory_max,
        net_rx: mean(points.iter().map(|c| c.net_rx)),
        net_tx: mean(points.iter().map(|c| c.net_tx)),
        block_read: mean(points.iter().map(|c| c.block_read)),
        block_write: mean(points.iter().map(|c| c.block_write)),
    })
}

/// The mean of the present values, or `None` if none is.
fn mean(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let (sum, count) = values
        .flatten()
        .fold((0.0, 0u32), |(sum, count), v| (sum + v, count + 1));
    (count > 0).then(|| round2(sum / f64::from(count)))
}

fn peak(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.flatten().reduce(f64::max)
}

fn mean_bytes(values: impl Iterator<Item = u64>) -> u64 {
    mean(values.map(|v| Some(to_f64(v)))).map_or(0, to_bytes)
}

/// A non-negative byte count from an average; `as` saturates, and the
/// fraction of a byte is dropped.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_bytes(value: f64) -> u64 {
    value.round().max(0.0) as u64
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
    use super::super::{ProjectUsage, TIERS};
    use super::*;

    #[test]
    fn bucket_start_aligns_to_the_step() {
        let cases = [
            (0, 600, 0),
            (599, 600, 0),
            (600, 600, 600),
            (1_234, 600, 1_200),
            (-1, 600, -600),
            (1_234, 0, 1_234),
        ];
        for (ts, step, want) in cases {
            assert_eq!(bucket_start(ts, step), want, "{ts} {step}");
        }
    }

    #[test]
    fn min_points_is_half_the_bucket() {
        let cases = [
            (600, 60, 5),
            (1_200, 600, 1),
            (7_200, 1_200, 3),
            (28_800, 7_200, 2),
            (60, 60, 1),
            (60, 0, 1),
        ];
        for (step, source, want) in cases {
            assert_eq!(min_points(step, source), want, "{step} {source}");
        }
    }

    #[test]
    fn rollup_window_covers_closed_buckets_the_source_still_keeps() {
        let (ten, minute) = (TIERS[1], TIERS[0]);
        let now = 100_000;
        let (from, to) = rollup_window(now, ten, minute);
        assert_eq!(to, 99_600);
        assert_eq!(from, 93_000);
        assert!(from >= now - minute.keep);
        assert_eq!((to - from) / ten.step, 11);
    }

    /// Timestamps a minute apart from `from`, `count` of them.
    fn minutes(from: i64, count: i64) -> Vec<i64> {
        (0..count).map(|i| from + i * 60).collect()
    }

    #[test]
    fn plan_rollups_builds_closed_missing_buckets_with_enough_points() {
        type Case = (&'static str, Vec<i64>, Vec<i64>, Vec<i64>);
        let (ten, minute) = (TIERS[1], TIERS[0]);
        let now = 36_000 + 30;
        let cases: [Case; 7] = [
            ("nothing yet", vec![], vec![], vec![]),
            ("one full bucket", vec![], minutes(34_800, 10), vec![34_800]),
            ("already built", vec![34_800], minutes(34_800, 10), vec![]),
            ("too few points", vec![], minutes(34_800, 4), vec![]),
            ("exactly half", vec![], minutes(34_800, 5), vec![34_800]),
            ("still open", vec![], minutes(36_000, 1), vec![]),
            (
                "catch up after a restart",
                vec![33_600],
                [minutes(33_000, 50), minutes(36_000, 1)].concat(),
                vec![33_000, 34_200, 34_800, 35_400],
            ),
        ];
        for (name, built, source, want) in cases {
            assert_eq!(
                plan_rollups(now, ten, minute, &built, &source),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn plan_rollups_ignores_points_outside_the_window() {
        let (ten, minute) = (TIERS[1], TIERS[0]);
        let now = 36_000;
        let old = minutes(now - minute.keep - 600, 10);
        assert_eq!(plan_rollups(now, ten, minute, &[], &old), Vec::<i64>::new());
    }

    fn machine(cpu: Option<f64>, memory: u64, rx: Option<f64>) -> MachineUsage {
        MachineUsage {
            cpu,
            cpus: 4,
            load: Some([1.0, 1.0, 1.0]),
            memory_used: memory,
            memory_total: 16_000,
            disk_used: Some(100),
            disk_total: Some(1_000),
            net_rx: rx,
            ..MachineUsage::default()
        }
    }

    fn container(name: &str, cpu: Option<f64>, memory: u64) -> ContainerUsage {
        ContainerUsage {
            id: format!("{name}-id"),
            name: name.into(),
            project: Some("web".into()),
            cpu,
            memory,
            ..ContainerUsage::default()
        }
    }

    fn point(
        t: i64,
        machine: Option<MachineUsage>,
        containers: Vec<ContainerUsage>,
    ) -> MetricsPoint {
        MetricsPoint {
            t,
            machine,
            projects: projects_of(&containers),
            containers,
        }
    }

    #[test]
    fn rollup_of_nothing_is_a_gap() {
        assert_eq!(rollup(600, &[]), None);
    }

    #[test]
    fn rollup_averages_present_values_and_carries_peaks() {
        let points = [
            point(
                600,
                Some(machine(None, 1_000, None)),
                vec![container("a", None, 100)],
            ),
            point(
                660,
                Some(machine(Some(20.0), 2_000, Some(10.0))),
                vec![
                    container("a", Some(10.0), 200),
                    container("b", Some(5.0), 50),
                ],
            ),
            point(
                720,
                Some(machine(Some(60.0), 3_000, None)),
                vec![container("a", Some(30.0), 300)],
            ),
            point(780, None, vec![]),
        ];
        let got = rollup(600, &points).unwrap();
        assert_eq!(got.t, 600);

        let m = got.machine.unwrap();
        assert_eq!(m.cpu, Some(40.0), "skips the missing first figure");
        assert_eq!(m.cpu_max, Some(60.0));
        assert_eq!(m.memory_used, 2_000);
        assert_eq!(m.net_rx, Some(10.0));
        assert_eq!(m.net_tx, None, "never measured stays None");
        assert_eq!(m.disk_read, None);
        assert_eq!(
            (m.memory_total, m.disk_total, m.cpus),
            (16_000, Some(1_000), 4)
        );
        assert_eq!(m.load, Some([1.0, 1.0, 1.0]));

        let a = &got.containers[0];
        assert_eq!(a.name, "a");
        assert_eq!(a.cpu, Some(20.0));
        assert_eq!(a.cpu_max, Some(30.0));
        assert_eq!((a.memory, a.memory_max), (200, Some(300)));
        let b = &got.containers[1];
        assert_eq!((b.cpu, b.memory), (Some(5.0), 50), "not diluted by absence");

        assert_eq!(
            got.projects,
            [ProjectUsage {
                name: "web".into(),
                containers: 2,
                cpu: Some(25.0),
                memory: 250,
                net_rx: None,
                net_tx: None,
            }]
        );
    }

    #[test]
    fn rollup_of_rollups_keeps_the_peak() {
        let mut first = container("a", Some(10.0), 100);
        first.cpu_max = Some(90.0);
        first.memory_max = Some(5_000);
        let points = [
            point(0, None, vec![first]),
            point(600, None, vec![container("a", Some(20.0), 200)]),
        ];
        let got = rollup(0, &points).unwrap();
        let a = &got.containers[0];
        assert_eq!((a.cpu, a.cpu_max), (Some(15.0), Some(90.0)));
        assert_eq!((a.memory, a.memory_max), (150, Some(5_000)));
        assert_eq!(got.machine, None, "no machine figures, no machine");
    }

    #[test]
    fn rollup_takes_the_latest_identity_and_sizes() {
        let mut old = container("a", None, 0);
        old.id = "old".into();
        let mut machine_old = machine(None, 0, None);
        machine_old.memory_total = 8_000;
        machine_old.disk_total = Some(2_000);
        let mut machine_new = machine(None, 0, None);
        machine_new.disk_total = None;
        let points = [
            point(0, Some(machine_old), vec![old]),
            point(60, Some(machine_new), vec![container("a", None, 0)]),
        ];
        let got = rollup(0, &points).unwrap();
        assert_eq!(got.containers[0].id, "a-id");
        let m = got.machine.unwrap();
        assert_eq!(m.memory_total, 16_000);
        assert_eq!(m.disk_total, Some(2_000), "latest known");
    }
}
