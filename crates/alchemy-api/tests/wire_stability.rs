//! Pins the exact JSON bytes of every API type. A failure here means the
//! wire contract changed: make it additive or bump `API_VERSION`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alchemy_api::{
    API_VERSION, ApiEvent, ContainerUsage, EngineFlavor, EngineState, EngineStatus, ErrorCode,
    ErrorResponse, HealthResponse, HealthStatus, MachineUsage, MetricsHistory, MetricsPoint,
    MetricsRange, MetricsResponse, ProjectUsage, VersionResponse,
};

fn assert_wire<T>(value: &T, json: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_string(value).unwrap(), json);
    assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value);
}

#[test]
fn api_version() {
    assert_eq!(API_VERSION, 1);
}

#[test]
fn health() {
    assert_wire(
        &HealthResponse {
            status: HealthStatus::Ok,
        },
        r#"{"status":"ok"}"#,
    );
}

#[test]
fn version() {
    assert_wire(
        &VersionResponse {
            version: "0.1.0".into(),
            api: 1,
        },
        r#"{"version":"0.1.0","api":1}"#,
    );
}

#[test]
fn error_response() {
    assert_wire(
        &ErrorResponse::new(ErrorCode::NotFound, "not found"),
        r#"{"error":"not found","code":"not_found"}"#,
    );
}

#[test]
fn error_codes() {
    let codes = [
        (ErrorCode::NotFound, r#""not_found""#),
        (ErrorCode::MethodNotAllowed, r#""method_not_allowed""#),
        (ErrorCode::BadRequest, r#""bad_request""#),
        (ErrorCode::Forbidden, r#""forbidden""#),
        (
            ErrorCode::UnsupportedMediaType,
            r#""unsupported_media_type""#,
        ),
        (ErrorCode::Conflict, r#""conflict""#),
        (ErrorCode::Invalid, r#""invalid""#),
        (ErrorCode::Internal, r#""internal""#),
        (ErrorCode::Unknown, r#""unknown""#),
    ];
    for (code, want) in codes {
        assert_wire(&code, want);
    }
}

#[test]
fn unknown_or_missing_error_code_decodes_as_unknown() {
    let cases = [
        r#"{"error":"x","code":"from_the_future"}"#,
        r#"{"error":"x"}"#,
    ];
    for json in cases {
        let got: ErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(got, ErrorResponse::new(ErrorCode::Unknown, "x"), "{json}");
    }
}

#[test]
fn engine_status() {
    let mut status = EngineStatus::new(EngineState::Ready);
    status.flavor = Some(EngineFlavor::DockerDesktop);
    status.engine_version = Some("29.8.1".into());
    status.api_version = Some("1.52".into());
    status.compose_version = Some("2.40.0".into());
    status.desktop_version = Some("4.93.0".into());
    status.endpoint = Some("unix:///var/run/docker.sock".into());
    assert_wire(
        &status,
        concat!(
            r#"{"state":"ready","flavor":"docker_desktop","engine_version":"29.8.1","#,
            r#""api_version":"1.52","compose_version":"2.40.0","desktop_version":"4.93.0","#,
            r#""endpoint":"unix:///var/run/docker.sock"}"#,
        ),
    );

    assert_wire(
        &EngineStatus::new(EngineState::NotRunning),
        concat!(
            r#"{"state":"not_running","flavor":null,"engine_version":null,"#,
            r#""api_version":null,"compose_version":null,"desktop_version":null,"#,
            r#""endpoint":null}"#,
        ),
    );
}

#[test]
fn engine_states() {
    let states = [
        (EngineState::NotInstalled, r#""not_installed""#),
        (EngineState::NotRunning, r#""not_running""#),
        (EngineState::Starting, r#""starting""#),
        (EngineState::Ready, r#""ready""#),
        (EngineState::Unsupported, r#""unsupported""#),
    ];
    for (state, want) in states {
        assert_wire(&state, want);
    }
}

#[test]
fn engine_flavors() {
    let flavors = [
        (EngineFlavor::DockerDesktop, r#""docker_desktop""#),
        (EngineFlavor::DockerEngine, r#""docker_engine""#),
        (EngineFlavor::Other, r#""other""#),
    ];
    for (flavor, want) in flavors {
        assert_wire(&flavor, want);
    }
}

#[test]
fn api_event() {
    let event = ApiEvent::Engine(EngineStatus::new(EngineState::Starting));
    assert_eq!(event.name(), "engine");
    assert_wire(
        &event,
        concat!(
            r#"{"event":"engine","data":{"state":"starting","flavor":null,"#,
            r#""engine_version":null,"api_version":null,"compose_version":null,"#,
            r#""desktop_version":null,"endpoint":null}}"#,
        ),
    );
}

fn machine() -> MachineUsage {
    MachineUsage {
        cpu: Some(12.5),
        cpu_max: None,
        cpus: 8,
        load: Some([0.5, 0.25, 0.75]),
        memory_used: 4_000,
        memory_total: 16_000,
        swap_used: 0,
        swap_total: 2_000,
        disk_used: Some(100),
        disk_total: Some(1_000),
        disk_read: Some(1.5),
        disk_write: None,
        net_rx: Some(10.5),
        net_tx: Some(2.5),
    }
}

fn container() -> ContainerUsage {
    ContainerUsage {
        id: "abc".into(),
        name: "web-app-1".into(),
        project: Some("web".into()),
        service: Some("app".into()),
        cpu: Some(3.5),
        cpu_max: Some(9.5),
        memory: 500,
        memory_max: Some(600),
        net_rx: Some(1.5),
        net_tx: None,
        block_read: None,
        block_write: Some(0.5),
    }
}

fn project() -> ProjectUsage {
    ProjectUsage {
        name: "web".into(),
        containers: 1,
        cpu: Some(3.5),
        memory: 500,
        net_rx: Some(1.5),
        net_tx: None,
    }
}

const MACHINE_JSON: &str = concat!(
    r#"{"cpu":12.5,"cpu_max":null,"cpus":8,"load":[0.5,0.25,0.75],"#,
    r#""memory_used":4000,"memory_total":16000,"swap_used":0,"swap_total":2000,"#,
    r#""disk_used":100,"disk_total":1000,"disk_read":1.5,"disk_write":null,"#,
    r#""net_rx":10.5,"net_tx":2.5}"#,
);

const CONTAINER_JSON: &str = concat!(
    r#"{"id":"abc","name":"web-app-1","project":"web","service":"app","#,
    r#""cpu":3.5,"cpu_max":9.5,"memory":500,"memory_max":600,"#,
    r#""net_rx":1.5,"net_tx":null,"block_read":null,"block_write":0.5}"#,
);

const PROJECT_JSON: &str =
    r#"{"name":"web","containers":1,"cpu":3.5,"memory":500,"net_rx":1.5,"net_tx":null}"#;

#[test]
fn metrics_usage() {
    assert_wire(&machine(), MACHINE_JSON);
    assert_wire(&container(), CONTAINER_JSON);
    assert_wire(&project(), PROJECT_JSON);
}

#[test]
fn metrics_response() {
    assert_wire(
        &MetricsResponse {
            at: None,
            interval: 60,
            containers_in_vm: false,
            machine: None,
            projects: vec![],
            containers: vec![],
        },
        r#"{"at":null,"interval":60,"containers_in_vm":false,"machine":null,"projects":[],"containers":[]}"#,
    );
    assert_wire(
        &MetricsResponse {
            at: Some(1_700_000_000),
            interval: 60,
            containers_in_vm: true,
            machine: Some(machine()),
            projects: vec![project()],
            containers: vec![container()],
        },
        &format!(
            r#"{{"at":1700000000,"interval":60,"containers_in_vm":true,"machine":{MACHINE_JSON},"projects":[{PROJECT_JSON}],"containers":[{CONTAINER_JSON}]}}"#
        ),
    );
}

#[test]
fn metrics_history() {
    assert_wire(
        &MetricsHistory {
            range: MetricsRange::Day,
            step: 1200,
            project: Some("web".into()),
            points: vec![MetricsPoint {
                t: 1_200,
                machine: None,
                projects: vec![project()],
                containers: vec![container()],
            }],
        },
        &format!(
            r#"{{"range":"24h","step":1200,"project":"web","points":[{{"t":1200,"machine":null,"projects":[{PROJECT_JSON}],"containers":[{CONTAINER_JSON}]}}]}}"#
        ),
    );
}

#[test]
fn metrics_ranges() {
    let ranges = [
        (MetricsRange::Hour, r#""1h""#),
        (MetricsRange::HalfDay, r#""12h""#),
        (MetricsRange::Day, r#""24h""#),
        (MetricsRange::Week, r#""7d""#),
        (MetricsRange::Month, r#""30d""#),
    ];
    for (range, want) in ranges {
        assert_wire(&range, want);
    }
}

#[test]
fn metrics_event() {
    let event = ApiEvent::Metrics(MetricsResponse {
        at: Some(60),
        interval: 60,
        containers_in_vm: false,
        machine: None,
        projects: vec![],
        containers: vec![],
    });
    assert_eq!(event.name(), "metrics");
    assert_wire(
        &event,
        concat!(
            r#"{"event":"metrics","data":{"at":60,"interval":60,"containers_in_vm":false,"#,
            r#""machine":null,"projects":[],"containers":[]}}"#,
        ),
    );
    assert_eq!(
        serde_json::to_string(&event.data()).unwrap(),
        r#"{"at":60,"interval":60,"containers_in_vm":false,"machine":null,"projects":[],"containers":[]}"#
    );
}
