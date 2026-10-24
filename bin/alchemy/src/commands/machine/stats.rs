//! `alchemy machine stats`: this machine's usage and each app's share.

use std::io::Write;

use alchemy_api::{MachineUsage, MetricsResponse};

use crate::commands::usage::{self, NONE, bytes, percent, rate, table};
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "machine stats";

/// The route the figures come from.
pub(crate) const PATH: &str = "/api/metrics";

/// Arguments to `alchemy machine stats`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {}

/// Run `alchemy machine stats`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    _args: Args,
) -> Result<(), ClientError> {
    let metrics = ctx.get::<MetricsResponse>(PATH).await?;
    for line in stats_lines(&metrics, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// What `alchemy machine stats` prints: the machine, then a table of apps,
/// or the response as one JSON object.
pub(crate) fn stats_lines(metrics: &MetricsResponse, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::to_string(metrics).unwrap_or_default()];
    }
    if metrics.at.is_none() {
        return vec!["No usage yet: alchemyd samples it shortly after it starts".to_owned()];
    }
    let mut lines = match &metrics.machine {
        Some(machine) => machine_lines(machine),
        None => vec!["alchemyd couldn't read this machine's usage".to_owned()],
    };
    if metrics.containers_in_vm {
        lines.push(
            "These are this machine's figures, not the VM Docker runs containers in; app CPU is a share of that VM"
                .to_owned(),
        );
    }
    lines.push(String::new());
    if metrics.projects.is_empty() {
        lines.push("No apps are running".to_owned());
    } else {
        lines.extend(table(&usage::project_rows(&metrics.projects)));
    }
    lines
}

fn machine_lines(m: &MachineUsage) -> Vec<String> {
    let load = m.load.map_or_else(String::new, |[one, five, fifteen]| {
        format!("  load {one:.2} {five:.2} {fifteen:.2}")
    });
    let cpus = if m.cpus == 1 { "CPU" } else { "CPUs" };
    let disk = match (m.disk_used, m.disk_total) {
        (Some(used), Some(total)) => format!("{} of {}", bytes(used), bytes(total)),
        _ => NONE.to_owned(),
    };
    let rows = vec![
        vec![
            "CPU".to_owned(),
            format!("{} of {} {cpus}{load}", percent(m.cpu), m.cpus),
        ],
        vec![
            "Memory".to_owned(),
            format!("{} of {}", bytes(m.memory_used), bytes(m.memory_total)),
        ],
        vec![
            "Swap".to_owned(),
            format!("{} of {}", bytes(m.swap_used), bytes(m.swap_total)),
        ],
        vec![
            "Disk".to_owned(),
            format!(
                "{disk}  read {}  write {}",
                rate(m.disk_read),
                rate(m.disk_write)
            ),
        ],
        vec![
            "Network".to_owned(),
            format!("in {}  out {}", rate(m.net_rx), rate(m.net_tx)),
        ],
    ];
    table(&rows)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_api::ProjectUsage;

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    fn sample() -> MetricsResponse {
        MetricsResponse {
            at: Some(1_700_000_000),
            interval: 60,
            containers_in_vm: false,
            machine: Some(MachineUsage {
                cpu: Some(12.5),
                cpus: 8,
                load: Some([0.5, 0.25, 0.75]),
                memory_used: 4 * 1024 * 1024 * 1024,
                memory_total: 16 * 1024 * 1024 * 1024,
                swap_used: 0,
                swap_total: 2 * 1024 * 1024 * 1024,
                disk_used: Some(100 * 1024 * 1024 * 1024),
                disk_total: Some(500 * 1024 * 1024 * 1024),
                disk_read: Some(2048.0),
                disk_write: None,
                net_rx: Some(1536.0),
                net_tx: Some(512.0),
                ..MachineUsage::default()
            }),
            projects: vec![ProjectUsage {
                name: "blog".into(),
                containers: 2,
                cpu: Some(3.5),
                memory: 300 * 1024 * 1024,
                net_rx: Some(100.0),
                net_tx: None,
            }],
            containers: vec![],
        }
    }

    #[test]
    fn text_shows_the_machine_then_apps() {
        assert_eq!(
            stats_lines(&sample(), false),
            [
                "CPU      12.5% of 8 CPUs  load 0.50 0.25 0.75",
                "Memory   4.0 GiB of 16.0 GiB",
                "Swap     0 B of 2.0 GiB",
                "Disk     100.0 GiB of 500.0 GiB  read 2.0 KiB/s  write -",
                "Network  in 1.5 KiB/s  out 512 B/s",
                "",
                "APP   CONTAINERS  CPU   MEMORY     NET IN   NET OUT",
                "blog  2           3.5%  300.0 MiB  100 B/s  -",
            ]
        );
    }

    #[test]
    fn text_covers_missing_figures() {
        let mut vm = sample();
        vm.containers_in_vm = true;
        vm.projects.clear();
        let lines = stats_lines(&vm, false);
        assert!(
            lines[5].contains("not the VM Docker runs containers in"),
            "{lines:?}"
        );
        assert_eq!(lines.last().unwrap(), "No apps are running");

        let mut bare = sample();
        bare.machine = Some(MachineUsage {
            cpus: 1,
            ..MachineUsage::default()
        });
        let lines = stats_lines(&bare, false);
        assert_eq!(lines[0], "CPU      - of 1 CPU");
        assert_eq!(lines[3], "Disk     -  read -  write -");

        let mut unread = sample();
        unread.machine = None;
        assert_eq!(
            stats_lines(&unread, false)[0],
            "alchemyd couldn't read this machine's usage"
        );

        let empty = MetricsResponse {
            interval: 60,
            ..MetricsResponse::default()
        };
        assert_eq!(
            stats_lines(&empty, false),
            ["No usage yet: alchemyd samples it shortly after it starts"]
        );
    }

    #[test]
    fn json_is_the_response() {
        let lines = stats_lines(&sample(), true);
        assert_eq!(lines.len(), 1);
        let back: MetricsResponse = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(back, sample());
    }

    #[tokio::test]
    async fn run_asks_for_the_metrics() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &sample());
        let mut ctx = Context::buffered(fake, false, NAME);
        run(&mut ctx, Args::default()).await.unwrap();
        assert!(
            ctx.written().starts_with("CPU      12.5%"),
            "{}",
            ctx.written()
        );
        assert_eq!(
            ctx.daemon().requests(),
            ["GET /api/version", "GET /api/metrics"]
        );
    }

    #[tokio::test]
    async fn an_old_daemon_is_too_old() {
        let fake = FakeDaemon::new().version("0.0.1", 1).error(
            Method::GET,
            PATH,
            404,
            "not_found",
            "not found",
        );
        let mut ctx = Context::buffered(fake, true, NAME);
        let err = run(&mut ctx, Args::default()).await.unwrap_err();
        assert_eq!(err.exit_code(), 76, "{err}");
        assert!(err.to_string().contains("machine stats"), "{err}");
    }
}
