//! Rendering resource usage for `machine stats` and `app stats`: units and
//! aligned tables, all pure.

use alchemy_api::{ContainerUsage, ProjectUsage};

/// Shown for a figure alchemyd could not measure, never zero.
pub(crate) const NONE: &str = "-";

/// A percentage with one decimal, such as `12.5%`.
pub(crate) fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| NONE.to_owned(), |v| format!("{v:.1}%"))
}

/// A byte count in binary units, such as `3.9 GiB`.
pub(crate) fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if value < 1024 {
        return format!("{value} B");
    }
    let mut scaled = to_f64(value) / 1024.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if scaled < 1024.0 {
            break;
        }
        scaled /= 1024.0;
        unit = next;
    }
    format!("{scaled:.1} {unit}")
}

/// A byte rate, such as `1.5 KiB/s`.
pub(crate) fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| NONE.to_owned(), |v| format!("{}/s", bytes(to_bytes(v))))
}

/// Rows padded into columns two spaces apart, without trailing spaces.
pub(crate) fn table(rows: &[Vec<String>]) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|i| {
            rows.iter()
                .filter_map(|row| row.get(i))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect();
            cells.join("  ").trim_end().to_owned()
        })
        .collect()
}

/// One row per app: name, containers, CPU, memory, and traffic.
pub(crate) fn project_rows(projects: &[ProjectUsage]) -> Vec<Vec<String>> {
    let header = ["APP", "CONTAINERS", "CPU", "MEMORY", "NET IN", "NET OUT"];
    std::iter::once(header.map(str::to_owned).to_vec())
        .chain(projects.iter().map(|p| {
            vec![
                p.name.clone(),
                p.containers.to_string(),
                percent(p.cpu),
                bytes(p.memory),
                rate(p.net_rx),
                rate(p.net_tx),
            ]
        }))
        .collect()
}

/// One row per container: name, service, CPU, memory, and traffic.
pub(crate) fn container_rows(containers: &[ContainerUsage]) -> Vec<Vec<String>> {
    let header = ["CONTAINER", "SERVICE", "CPU", "MEMORY", "NET IN", "NET OUT"];
    std::iter::once(header.map(str::to_owned).to_vec())
        .chain(containers.iter().map(|c| {
            vec![
                c.name.clone(),
                c.service.clone().unwrap_or_else(|| NONE.to_owned()),
                percent(c.cpu),
                bytes(c.memory),
                rate(c.net_rx),
                rate(c.net_tx),
            ]
        }))
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn to_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_bytes(value: f64) -> u64 {
    value.round().max(0.0) as u64
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
    fn units() {
        let cases = [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (5 * 1024 * 1024, "5.0 MiB"),
            (3_221_225_472, "3.0 GiB"),
            (u64::MAX, "16384.0 PiB"),
        ];
        for (value, want) in cases {
            assert_eq!(bytes(value), want, "{value}");
        }
        assert_eq!(rate(Some(1536.4)), "1.5 KiB/s");
        assert_eq!(rate(Some(0.2)), "0 B/s");
        assert_eq!(rate(None), "-");
        assert_eq!(percent(Some(12.345)), "12.3%");
        assert_eq!(percent(None), "-");
    }

    #[test]
    fn table_aligns_columns_and_trims() {
        let rows = vec![
            vec!["APP".into(), "CPU".into()],
            vec!["blog".into(), "1.0%".into()],
            vec!["a".into(), String::new()],
        ];
        assert_eq!(table(&rows), ["APP   CPU", "blog  1.0%", "a"]);
        assert_eq!(table(&[]), Vec::<String>::new());
    }
}
