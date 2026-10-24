//! Building the [`NetworkReport`] clients see from what the daemon read,
//! and the sentences in it.
//!
//! Pure: every input is a value, and the same inputs give the same report.

use std::collections::BTreeSet;

use alchemy_api::network::{
    AlchemyNetwork, AlchemyNetworkState, DockerNetwork as WireNetwork, NetworkReport,
    NetworkWarning, NetworkWarningKind, PoolsSource,
};
use alchemy_core::engine::EngineFlavor;
use alchemy_core::network::{
    AddressPool, Collision, Ipv4Net, MANAGED_LABEL, NetworkSubnet, NetworkSurvey, SHARED_NETWORK,
    Subject, default_address_pools, suggest_daemon_json,
};
use serde::Deserialize;

use crate::engine::{self, DockerNetwork};

/// Where Docker Engine on Linux reads its settings.
pub const DAEMON_JSON: &str = "/etc/docker/daemon.json";

/// What the daemon found on the host and in the engine, for [`report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Findings {
    /// The engine's flavor, which decides where its settings are changed.
    pub flavor: Option<EngineFlavor>,
    /// The LAN subnets.
    pub lan: Vec<Ipv4Net>,
    /// Every route's destination, the default route included.
    pub routes: Vec<Ipv4Net>,
    /// Why the host could not be read, if it could not.
    pub host_error: Option<String>,
    /// What the engine said.
    pub engine: EngineSide,
}

/// The engine's part of [`Findings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineSide {
    /// The engine is not ready, so it was not asked.
    NotReady,
    /// Reading its networks failed; the message says why.
    Unreadable(String),
    /// What it reported.
    Read(EngineFindings),
}

/// What a ready engine reported, and what became of Alchemy's network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFindings {
    /// Every network, including Alchemy's if it was just created.
    pub networks: Vec<DockerNetwork>,
    /// The default bridge's subnet.
    pub bridge: Option<Ipv4Net>,
    /// The pools new networks are carved from.
    pub pools: Vec<AddressPool>,
    /// Where `pools` came from.
    pub pools_source: PoolsSource,
    /// What became of Alchemy's network.
    pub alchemy: AlchemyOutcome,
}

/// What became of Alchemy's network on one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlchemyOutcome {
    /// It exists, or was just created, on a subnet that overlaps nothing.
    Ready(Option<Ipv4Net>),
    /// It exists on a subnet that overlaps the host and was left alone.
    Overlapping(Ipv4Net),
    /// No free subnet was left to create it on.
    NoFreeSubnet,
    /// Creating it on `subnet` failed with `error`.
    CreateFailed {
        /// The subnet it was to be created on.
        subnet: Ipv4Net,
        /// Why it failed.
        error: String,
    },
}

/// `daemon.json`'s `default-address-pools`, or `None` when the text is not
/// JSON or sets none. IPv6 and unreadable pools are left out.
pub fn pools_from_daemon_json(text: &str) -> Option<Vec<AddressPool>> {
    #[derive(Deserialize)]
    struct DaemonJson {
        #[serde(rename = "default-address-pools", default)]
        pools: Vec<serde_json::Value>,
    }
    let parsed: DaemonJson = serde_json::from_str(text).ok()?;
    let pools: Vec<AddressPool> = parsed
        .pools
        .into_iter()
        .filter_map(|pool| serde_json::from_value(pool).ok())
        .collect();
    (!pools.is_empty()).then_some(pools)
}

/// The pools Docker carves new networks from: those the engine reported,
/// else those `daemon_json` sets, else Docker's defaults.
pub fn effective_pools(
    engine: &[engine::AddressPool],
    daemon_json: Option<&str>,
) -> (Vec<AddressPool>, PoolsSource) {
    let reported: Vec<AddressPool> = engine
        .iter()
        .filter_map(|pool| AddressPool::new(pool.base, pool.size).ok())
        .collect();
    if !reported.is_empty() {
        return (reported, PoolsSource::Engine);
    }
    match daemon_json.and_then(pools_from_daemon_json) {
        Some(pools) => (pools, PoolsSource::DaemonJson),
        None => (default_address_pools(), PoolsSource::Defaults),
    }
}

/// The survey of `lan` and `routes` against Docker's bridge, pools, and
/// networks.
pub fn survey(
    lan: &[Ipv4Net],
    routes: &[Ipv4Net],
    bridge: Option<Ipv4Net>,
    pools: &[AddressPool],
    networks: &[DockerNetwork],
) -> NetworkSurvey {
    NetworkSurvey {
        lan: lan.to_vec(),
        routes: routes.to_vec(),
        bridge,
        pools: pools.to_vec(),
        networks: networks
            .iter()
            .flat_map(|network| {
                network.subnets.iter().map(|subnet| NetworkSubnet {
                    name: network.name.clone(),
                    subnet: *subnet,
                })
            })
            .collect(),
    }
}

/// The report for `findings`.
pub fn report(findings: &Findings) -> NetworkReport {
    let mut report = NetworkReport {
        lan: sorted(findings.lan.iter().copied()),
        ..NetworkReport::default()
    };
    let mut warnings = Vec::new();
    let mut docker: Vec<Ipv4Net> = Vec::new();
    match &findings.engine {
        EngineSide::NotReady => {}
        EngineSide::Unreadable(error) => warnings.push(warning(
            NetworkWarningKind::EngineUnreadable,
            format!("Alchemy couldn't read Docker's networks, so it can't check them against your network: {error}"),
            None,
        )),
        EngineSide::Read(engine) => {
            report.engine_ready = true;
            report.bridge = engine.bridge;
            report.pools.clone_from(&engine.pools);
            report.pools_source = engine.pools_source;
            report.networks = wire_networks(&engine.networks);
            report.alchemy = alchemy(engine);
            docker = engine
                .bridge
                .into_iter()
                .chain(engine.networks.iter().flat_map(|n| n.subnets.iter().copied()))
                .collect();
            let survey = survey(
                &findings.lan,
                &findings.routes,
                engine.bridge,
                &engine.pools,
                &engine.networks,
            );
            warnings.extend(alchemy_warnings(&engine.alchemy));
            warnings.extend(collision_warnings(&survey, &engine.networks, findings.flavor));
        }
    }
    report.routes = sorted(findings.routes.iter().copied().filter(|route| {
        !route.is_default_route() && !findings.lan.contains(route) && !docker.contains(route)
    }));
    if let Some(error) = &findings.host_error {
        warnings.push(warning(
            NetworkWarningKind::HostUnreadable,
            format!("Alchemy couldn't read this machine's networks, so it can't check Docker's against them: {error}"),
            None,
        ));
    }
    report.warnings = warnings;
    report
}

fn sorted(nets: impl Iterator<Item = Ipv4Net>) -> Vec<Ipv4Net> {
    nets.collect::<BTreeSet<_>>().into_iter().collect()
}

fn wire_networks(networks: &[DockerNetwork]) -> Vec<WireNetwork> {
    let mut wire: Vec<WireNetwork> = networks
        .iter()
        .map(|network| WireNetwork {
            name: network.name.clone(),
            driver: network.driver.clone(),
            subnets: network.subnets.clone(),
            project: network.project.clone(),
            managed: is_managed(network),
        })
        .collect();
    wire.sort_by(|a, b| a.name.cmp(&b.name));
    wire
}

/// Whether Alchemy created `network`.
pub fn is_managed(network: &DockerNetwork) -> bool {
    network.labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
}

fn alchemy(engine: &EngineFindings) -> AlchemyNetwork {
    let (state, subnet) = match &engine.alchemy {
        AlchemyOutcome::Ready(subnet) => (AlchemyNetworkState::Ready, *subnet),
        AlchemyOutcome::Overlapping(subnet) => (AlchemyNetworkState::Overlapping, Some(*subnet)),
        AlchemyOutcome::NoFreeSubnet => (AlchemyNetworkState::Unavailable, None),
        AlchemyOutcome::CreateFailed { subnet, .. } => {
            (AlchemyNetworkState::Unavailable, Some(*subnet))
        }
    };
    AlchemyNetwork {
        name: SHARED_NETWORK.to_owned(),
        state,
        subnet,
    }
}

fn alchemy_warnings(outcome: &AlchemyOutcome) -> Option<NetworkWarning> {
    match outcome {
        AlchemyOutcome::Ready(_) | AlchemyOutcome::Overlapping(_) => None,
        AlchemyOutcome::NoFreeSubnet => Some(warning(
            NetworkWarningKind::NoFreeSubnet,
            format!(
                "Alchemy couldn't create its network {SHARED_NETWORK}: every subnet it tries overlaps your network, a route, or another Docker network."
            ),
            Some(
                "Remove Docker networks you no longer use with `docker network prune`.".to_owned(),
            ),
        )),
        AlchemyOutcome::CreateFailed { subnet, error } => {
            let mut warning = warning(
                NetworkWarningKind::CreateFailed,
                format!(
                    "Alchemy couldn't create its network {SHARED_NETWORK} on {subnet}: {error}"
                ),
                None,
            );
            warning.subnet = Some(*subnet);
            warning.network = Some(SHARED_NETWORK.to_owned());
            Some(warning)
        }
    }
}

fn collision_warnings(
    survey: &NetworkSurvey,
    networks: &[DockerNetwork],
    flavor: Option<EngineFlavor>,
) -> Vec<NetworkWarning> {
    let pools_fix = pools_fix(survey, flavor);
    let collisions = survey.collisions();
    let alchemy_first = collisions
        .iter()
        .filter(|c| is_alchemy(c))
        .chain(collisions.iter().filter(|c| !is_alchemy(c)));
    alchemy_first
        .map(|collision| {
            let (kind, message, fix, network) = match &collision.subject {
                Subject::Bridge => (
                    NetworkWarningKind::BridgeOverlap,
                    collision.to_string(),
                    Some(pools_fix.clone()),
                    None,
                ),
                Subject::Pool { .. } => (
                    NetworkWarningKind::PoolOverlap,
                    collision.to_string(),
                    Some(pools_fix.clone()),
                    None,
                ),
                Subject::Network { name } if name == SHARED_NETWORK => (
                    NetworkWarningKind::AlchemyNetworkOverlap,
                    format!(
                        "{collision} Alchemy left it as it is, since containers may be attached to it."
                    ),
                    Some(format!(
                        "Once no container uses it, remove it with `docker network rm {SHARED_NETWORK}`. Alchemy creates it again on a free subnet."
                    )),
                    Some(name.clone()),
                ),
                Subject::Network { name } => {
                    let project = networks
                        .iter()
                        .find(|n| &n.name == name)
                        .and_then(|n| n.project.as_deref());
                    (
                        NetworkWarningKind::NetworkOverlap,
                        collision.to_string(),
                        Some(network_fix(name, project)),
                        Some(name.clone()),
                    )
                }
            };
            NetworkWarning {
                kind,
                message,
                fix,
                subnet: Some(collision.subnet),
                host: Some(collision.host),
                host_subnet: Some(collision.host_subnet),
                network,
            }
        })
        .collect()
}

fn is_alchemy(collision: &Collision) -> bool {
    matches!(&collision.subject, Subject::Network { name } if name == SHARED_NETWORK)
}

/// How to move the bridge and pools off the host, for `flavor`'s engine.
pub fn pools_fix(survey: &NetworkSurvey, flavor: Option<EngineFlavor>) -> String {
    let after =
        "Networks that already exist keep their subnets until they are removed and created again.";
    let Some(fix) = suggest_daemon_json(survey) else {
        return format!(
            "Set `default-address-pools` (and `bip` for the default bridge) in Docker's daemon.json to ranges your network and routes don't use, then restart Docker. {after}"
        );
    };
    let json = fix.to_json();
    match flavor {
        Some(EngineFlavor::DockerDesktop) => format!(
            "In Docker Desktop, open Settings, then Docker Engine, add these settings to the JSON there, and select Apply & restart:\n{json}\n{after}"
        ),
        Some(EngineFlavor::DockerEngine) => format!(
            "Add these settings to {DAEMON_JSON} (create it if it doesn't exist), then restart Docker with `sudo systemctl restart docker`:\n{json}\n{after}"
        ),
        _ => format!(
            "Add these settings to your Docker engine's daemon.json, then restart it:\n{json}\n{after}"
        ),
    }
}

fn network_fix(name: &str, project: Option<&str>) -> String {
    match project {
        Some(project) => format!(
            "If the compose file of project {project} sets this subnet, change it there. Otherwise, after fixing Docker's address pools if they overlap too, run `docker compose down` and then `docker compose up -d` for {project}, so Compose creates the network again on a free subnet."
        ),
        None => format!(
            "Once no container uses it, remove it with `docker network rm {name}` and create it again on a subnet your network doesn't use."
        ),
    }
}

fn warning(kind: NetworkWarningKind, message: String, fix: Option<String>) -> NetworkWarning {
    NetworkWarning {
        kind,
        message,
        fix,
        subnet: None,
        host: None,
        host_subnet: None,
        network: None,
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
    use std::collections::BTreeMap;

    use alchemy_api::network::HostSide;
    use alchemy_core::network::default_bridge;

    use super::*;

    fn n(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn network(name: &str, subnet: &str, project: Option<&str>) -> DockerNetwork {
        DockerNetwork {
            id: format!("{name}-id"),
            name: name.into(),
            driver: Some("bridge".into()),
            subnets: vec![n(subnet)],
            labels: BTreeMap::new(),
            project: project.map(str::to_owned),
        }
    }

    fn managed(name: &str, subnet: &str) -> DockerNetwork {
        let mut network = network(name, subnet, None);
        network
            .labels
            .insert(MANAGED_LABEL.to_owned(), "true".to_owned());
        network
    }

    fn findings(lan: &str, networks: Vec<DockerNetwork>, alchemy: AlchemyOutcome) -> Findings {
        Findings {
            flavor: Some(EngineFlavor::DockerEngine),
            lan: vec![n(lan)],
            routes: vec![n("0.0.0.0/0"), n(lan), default_bridge()],
            host_error: None,
            engine: EngineSide::Read(EngineFindings {
                networks,
                bridge: Some(default_bridge()),
                pools: default_address_pools(),
                pools_source: PoolsSource::Defaults,
                alchemy,
            }),
        }
    }

    fn kinds(report: &NetworkReport) -> Vec<(NetworkWarningKind, Option<String>)> {
        report
            .warnings
            .iter()
            .map(|w| (w.kind, w.subnet.map(|s| s.to_string())))
            .collect()
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn daemon_json_pools() {
        let cases: [(&str, &str, Option<Vec<(&str, u8)>>); 7] = [
            (
                "pools",
                r#"{"default-address-pools":[{"base":"10.10.0.0/16","size":24}]}"#,
                Some(vec![("10.10.0.0/16", 24)]),
            ),
            (
                "IPv6 and bad pools skipped",
                r#"{"default-address-pools":[{"base":"fd00::/48","size":64},{"base":"10.10.0.0/16","size":8},{"base":"172.80.0.0/12","size":24}]}"#,
                Some(vec![("172.80.0.0/12", 24)]),
            ),
            ("no pools", r#"{"log-driver":"json-file"}"#, None),
            ("empty list", r#"{"default-address-pools":[]}"#, None),
            ("empty file", "", None),
            ("not JSON", "default-address-pools = 1", None),
            ("not an object", "[]", None),
        ];
        for (name, text, want) in cases {
            let want = want.map(|pools| {
                pools
                    .into_iter()
                    .map(|(base, size)| AddressPool::new(n(base), size).unwrap())
                    .collect::<Vec<_>>()
            });
            assert_eq!(pools_from_daemon_json(text), want, "{name}");
        }
    }

    #[test]
    fn pools_come_from_the_engine_then_daemon_json_then_defaults() {
        let engine_pool = engine::AddressPool {
            base: n("10.50.0.0/16"),
            size: 24,
        };
        let file = r#"{"default-address-pools":[{"base":"10.60.0.0/16","size":24}]}"#;
        let pool = |base: &str| vec![AddressPool::new(n(base), 24).unwrap()];
        assert_eq!(
            effective_pools(&[engine_pool], Some(file)),
            (pool("10.50.0.0/16"), PoolsSource::Engine)
        );
        assert_eq!(
            effective_pools(&[], Some(file)),
            (pool("10.60.0.0/16"), PoolsSource::DaemonJson)
        );
        assert_eq!(
            effective_pools(&[], Some("{}")),
            (default_address_pools(), PoolsSource::Defaults)
        );
        assert_eq!(
            effective_pools(&[], None),
            (default_address_pools(), PoolsSource::Defaults)
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn home_networks_warn_with_the_fix() {
        let ready = || AlchemyOutcome::Ready(Some(n("172.18.0.0/24")));
        let alchemy = || managed("alchemy", "172.18.0.0/24");
        let cases: [(&str, Findings, Vec<(NetworkWarningKind, Option<&str>)>); 4] = [
            (
                "10.0.1.0/24 is clear",
                findings("10.0.1.0/24", vec![alchemy()], ready()),
                vec![],
            ),
            (
                "172.17.0.0/16 hits the bridge and its pool",
                findings("172.17.0.0/16", vec![alchemy()], ready()),
                vec![
                    (NetworkWarningKind::BridgeOverlap, Some("172.17.0.0/16")),
                    (NetworkWarningKind::PoolOverlap, Some("172.17.0.0/16")),
                ],
            ),
            (
                "192.168.0.0/16 hits a pool and a Compose network",
                findings(
                    "192.168.0.0/16",
                    vec![
                        alchemy(),
                        network("blog_default", "192.168.16.0/20", Some("blog")),
                    ],
                    ready(),
                ),
                vec![
                    (NetworkWarningKind::PoolOverlap, Some("192.168.0.0/16")),
                    (NetworkWarningKind::NetworkOverlap, Some("192.168.16.0/20")),
                ],
            ),
            (
                "Alchemy's network on the LAN comes first",
                findings(
                    "172.18.0.0/16",
                    vec![managed("alchemy", "172.18.0.0/24")],
                    AlchemyOutcome::Overlapping(n("172.18.0.0/24")),
                ),
                vec![
                    (
                        NetworkWarningKind::AlchemyNetworkOverlap,
                        Some("172.18.0.0/24"),
                    ),
                    (NetworkWarningKind::PoolOverlap, Some("172.18.0.0/16")),
                ],
            ),
        ];
        for (name, findings, want) in cases {
            let want: Vec<_> = want
                .into_iter()
                .map(|(kind, subnet)| (kind, subnet.map(str::to_owned)))
                .collect();
            assert_eq!(kinds(&report(&findings)), want, "{name}");
        }
    }

    #[test]
    fn pool_warning_spells_out_daemon_json() {
        let report = report(&findings(
            "192.168.0.0/16",
            vec![],
            AlchemyOutcome::Ready(None),
        ));
        let warning = &report.warnings[0];
        assert_eq!(warning.kind, NetworkWarningKind::PoolOverlap);
        assert_eq!(warning.host, Some(HostSide::Lan));
        assert_eq!(warning.host_subnet, Some(n("192.168.0.0/16")));
        assert_eq!(
            warning.message,
            "Docker's address pool 192.168.0.0/16 overlaps your local network 192.168.0.0/16, so a new network may be given addresses that hide devices on it."
        );
        assert_eq!(
            warning.fix.as_deref(),
            Some(concat!(
                "Add these settings to /etc/docker/daemon.json (create it if it doesn't exist), then restart Docker with `sudo systemctl restart docker`:\n",
                "{\n  \"default-address-pools\": [\n    { \"base\": \"10.200.0.0/16\", \"size\": 24 }\n  ]\n}\n",
                "Networks that already exist keep their subnets until they are removed and created again."
            ))
        );
    }

    #[test]
    fn fix_names_where_each_engine_keeps_its_settings() {
        let s = survey(
            &[n("172.17.0.0/16")],
            &[],
            Some(default_bridge()),
            &default_address_pools(),
            &[],
        );
        let json = "{\n  \"bip\": \"172.18.0.1/24\",\n  \"default-address-pools\": [\n    { \"base\": \"10.200.0.0/16\", \"size\": 24 }\n  ]\n}";
        let cases = [
            (
                Some(EngineFlavor::DockerDesktop),
                "In Docker Desktop, open Settings, then Docker Engine, add these settings to the JSON there, and select Apply & restart:\n",
            ),
            (
                Some(EngineFlavor::DockerEngine),
                "Add these settings to /etc/docker/daemon.json (create it if it doesn't exist), then restart Docker with `sudo systemctl restart docker`:\n",
            ),
            (
                Some(EngineFlavor::Other),
                "Add these settings to your Docker engine's daemon.json, then restart it:\n",
            ),
            (
                None,
                "Add these settings to your Docker engine's daemon.json, then restart it:\n",
            ),
        ];
        for (flavor, lead) in cases {
            let fix = pools_fix(&s, flavor);
            assert!(
                fix.starts_with(&format!("{lead}{json}\n")),
                "{flavor:?}: {fix}"
            );
        }
        let all_taken = survey(
            &[n("172.16.0.0/12")],
            &[n("192.168.0.0/16"), n("10.0.0.0/8")],
            Some(default_bridge()),
            &default_address_pools(),
            &[],
        );
        assert!(pools_fix(&all_taken, None).starts_with("Set `default-address-pools`"));
    }

    #[test]
    fn alchemy_network_state_and_failures() {
        let cases = [
            (
                AlchemyOutcome::Ready(Some(n("172.18.0.0/24"))),
                AlchemyNetworkState::Ready,
                Some("172.18.0.0/24"),
                vec![],
            ),
            (
                AlchemyOutcome::NoFreeSubnet,
                AlchemyNetworkState::Unavailable,
                None,
                vec![NetworkWarningKind::NoFreeSubnet],
            ),
            (
                AlchemyOutcome::CreateFailed {
                    subnet: n("172.18.0.0/24"),
                    error: "Docker engine returned HTTP 500: boom".into(),
                },
                AlchemyNetworkState::Unavailable,
                Some("172.18.0.0/24"),
                vec![NetworkWarningKind::CreateFailed],
            ),
        ];
        for (outcome, state, subnet, want) in cases {
            let report = report(&findings("10.0.1.0/24", vec![], outcome.clone()));
            assert_eq!(report.alchemy.state, state, "{outcome:?}");
            assert_eq!(report.alchemy.subnet, subnet.map(n), "{outcome:?}");
            let got: Vec<_> = report.warnings.iter().map(|w| w.kind).collect();
            assert_eq!(got, want, "{outcome:?}");
        }
    }

    #[test]
    fn report_lists_host_routes_and_networks() {
        let mut f = findings(
            "10.0.1.0/24",
            vec![
                network("blog_default", "172.19.0.0/16", Some("blog")),
                managed("alchemy", "172.18.0.0/24"),
            ],
            AlchemyOutcome::Ready(Some(n("172.18.0.0/24"))),
        );
        f.routes
            .extend([n("100.64.0.0/10"), n("172.19.0.0/16"), n("100.64.0.0/10")]);
        let report = report(&f);
        assert!(report.engine_ready);
        assert_eq!(report.lan, [n("10.0.1.0/24")]);
        assert_eq!(report.routes, [n("100.64.0.0/10")]);
        assert_eq!(report.bridge, Some(default_bridge()));
        assert_eq!(report.pools_source, PoolsSource::Defaults);
        let names: Vec<(&str, bool)> = report
            .networks
            .iter()
            .map(|n| (n.name.as_str(), n.managed))
            .collect();
        assert_eq!(names, [("alchemy", true), ("blog_default", false)]);
        assert_eq!(report.warnings, []);
    }

    #[test]
    fn unreadable_sides_become_warnings() {
        let report = report(&Findings {
            flavor: None,
            lan: vec![],
            routes: vec![],
            host_error: Some("netstat timed out".into()),
            engine: EngineSide::Unreadable("Docker engine request timed out".into()),
        });
        assert!(!report.engine_ready);
        assert_eq!(report.alchemy.state, AlchemyNetworkState::Pending);
        let got: Vec<_> = report.warnings.iter().map(|w| w.kind).collect();
        assert_eq!(
            got,
            [
                NetworkWarningKind::EngineUnreadable,
                NetworkWarningKind::HostUnreadable
            ]
        );
        let not_ready = super::report(&Findings {
            engine: EngineSide::NotReady,
            host_error: None,
            ..findings("10.0.1.0/24", vec![], AlchemyOutcome::NoFreeSubnet)
        });
        assert_eq!(
            not_ready,
            NetworkReport {
                lan: vec![n("10.0.1.0/24")],
                routes: vec![default_bridge()],
                ..NetworkReport::default()
            }
        );
    }
}
