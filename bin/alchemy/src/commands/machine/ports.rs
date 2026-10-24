//! `alchemy machine ports`: every host port in use, reserved, or declared,
//! and who holds it.

use std::io::Write;

use alchemy_api::PublishedPort;
use alchemy_api::ports::{PortList, PortSource};

use crate::commands::usage::table;
use crate::context::Context;
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "machine ports";

/// The route the ports come from.
pub(crate) const PATH: &str = "/api/ports";

/// Arguments to `alchemy machine ports`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {}

/// Run `alchemy machine ports`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    _args: Args,
) -> Result<(), ClientError> {
    let ports = ctx.get::<PortList>(PATH).await?;
    for line in ports_lines(&ports, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}

/// What `alchemy machine ports` prints: a table of ports, then anything
/// alchemyd could not find out, or the response as one JSON object.
pub(crate) fn ports_lines(list: &PortList, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::to_string(list).unwrap_or_default()];
    }
    let mut lines = if list.ports.is_empty() {
        vec!["No ports are in use, reserved, or declared".to_owned()]
    } else {
        let header = ["PORT", "ADDRESS", "STATUS", "HELD BY"].map(str::to_owned);
        let rows: Vec<Vec<String>> = std::iter::once(header.to_vec())
            .chain(list.ports.iter().map(|claim| {
                vec![
                    port(&claim.port),
                    address(&claim.port),
                    status(claim.source).to_owned(),
                    claim.holder.to_string(),
                ]
            }))
            .collect();
        table(&rows)
    };
    if !list.warnings.is_empty() {
        lines.push(String::new());
        lines.extend(list.warnings.iter().cloned());
    }
    lines
}

/// `8080/tcp`, or `8000-8010/udp` for a range.
fn port(port: &PublishedPort) -> String {
    let number = port
        .published
        .map_or_else(|| "-".to_owned(), |range| range.to_string());
    format!("{number}/{}", port.protocol)
}

/// The address it is bound on, `*` for every address.
fn address(port: &PublishedPort) -> String {
    port.host_ip
        .map_or_else(|| "*".to_owned(), |ip| ip.to_string())
}

fn status(source: PortSource) -> &'static str {
    match source {
        PortSource::Live => "in use",
        PortSource::Reserved => "reserved",
        PortSource::Declared => "declared",
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
    use alchemy_api::Protocol;
    use alchemy_api::ports::{PortClaim, PortHolder};

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    fn claim(port: &str, ip: Option<&str>, holder: PortHolder, source: PortSource) -> PortClaim {
        let (number, protocol) = port.split_once('/').unwrap();
        PortClaim {
            port: PublishedPort::new(protocol.parse::<Protocol>().unwrap(), number.parse().ok())
                .with_host_ip(ip.map(|ip| ip.parse().unwrap())),
            holder,
            source,
        }
    }

    fn sample() -> PortList {
        PortList {
            ports: vec![
                claim(
                    "3000/tcp",
                    None,
                    PortHolder::App {
                        app: "wiki".into(),
                        service: "app".into(),
                    },
                    PortSource::Declared,
                ),
                claim(
                    "5353/udp",
                    None,
                    PortHolder::Container {
                        name: "pihole".into(),
                    },
                    PortSource::Live,
                ),
                claim(
                    "8000-8010/tcp",
                    None,
                    PortHolder::App {
                        app: "blog".into(),
                        service: "web".into(),
                    },
                    PortSource::Live,
                ),
                claim(
                    "8443/tcp",
                    Some("127.0.0.1"),
                    PortHolder::Reservation {
                        owner: "proxy".into(),
                    },
                    PortSource::Reserved,
                ),
                claim(
                    "9000/tcp",
                    Some("0.0.0.0"),
                    PortHolder::Program {
                        process: Some("nc".into()),
                        pid: Some(42),
                    },
                    PortSource::Live,
                ),
            ],
            warnings: vec![],
        }
    }

    #[test]
    fn text_is_a_table_of_ports() {
        assert_eq!(
            ports_lines(&sample(), false),
            [
                "PORT           ADDRESS    STATUS    HELD BY",
                "3000/tcp       *          declared  service app of app wiki",
                "5353/udp       *          in use    container pihole",
                "8000-8010/tcp  *          in use    service web of app blog",
                "8443/tcp       127.0.0.1  reserved  a reservation for proxy",
                "9000/tcp       0.0.0.0    in use    another program (nc, pid 42)",
            ]
        );
    }

    #[test]
    fn text_covers_no_ports_and_warnings() {
        let empty = PortList::default();
        assert_eq!(
            ports_lines(&empty, false),
            ["No ports are in use, reserved, or declared"]
        );
        let partial = PortList {
            warnings: vec!["alchemyd couldn't read which programs are listening.".into()],
            ..sample()
        };
        let lines = ports_lines(&partial, false);
        assert_eq!(
            lines[lines.len() - 2..],
            ["", "alchemyd couldn't read which programs are listening."]
        );
    }

    #[test]
    fn json_is_the_response() {
        let lines = ports_lines(&sample(), true);
        assert_eq!(lines.len(), 1);
        let back: PortList = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(back, sample());
    }

    #[tokio::test]
    async fn run_asks_for_the_ports() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &sample());
        let mut ctx = Context::buffered(fake, false, NAME);
        run(&mut ctx, Args::default()).await.unwrap();
        assert!(ctx.written().starts_with("PORT "), "{}", ctx.written());
        assert_eq!(
            ctx.daemon().requests(),
            ["GET /api/version", "GET /api/ports"]
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
        assert!(err.to_string().contains("machine ports"), "{err}");
    }
}
