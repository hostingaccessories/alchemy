//! Pins the exact JSON bytes of every API type. A failure here means the
//! wire contract changed: make it additive or bump `API_VERSION`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alchemy_api::{
    API_VERSION, ApiEvent, EngineFlavor, EngineState, EngineStatus, ErrorCode, ErrorResponse,
    HealthResponse, HealthStatus, VersionResponse,
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
