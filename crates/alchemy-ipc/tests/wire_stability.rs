//! Pins the exact JSON bytes of every IPC message. A failure here means
//! the wire contract changed: make it additive or bump
//! `PROTOCOL_VERSION`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alchemy_core::engine::{EngineFlavor, EngineState, EngineStatus};
use alchemy_ipc::{ErrorCode, PROTOCOL_VERSION, Request, Response, decode_message, encode_message};

fn assert_wire<T>(value: &T, json: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(
        String::from_utf8(encode_message(value).unwrap()).unwrap(),
        json
    );
    assert_eq!(&decode_message::<T>(json.as_bytes()).unwrap(), value);
}

#[test]
fn protocol_version() {
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn requests() {
    assert_wire(&Request::Ping, r#"{"type":"ping"}"#);
    assert_wire(&Request::Version, r#"{"type":"version"}"#);
    assert_wire(&Request::Engine, r#"{"type":"engine"}"#);
    assert_wire(&Request::WatchEngine, r#"{"type":"watch_engine"}"#);
}

#[test]
fn responses() {
    assert_wire(&Response::Pong, r#"{"type":"pong"}"#);
    assert_wire(
        &Response::Version {
            daemon: "1.2.3".into(),
            protocol: 1,
        },
        r#"{"type":"version","daemon":"1.2.3","protocol":1}"#,
    );
    assert_wire(
        &Response::Error {
            code: ErrorCode::BadRequest,
            message: "nope".into(),
        },
        r#"{"type":"error","code":"bad_request","message":"nope"}"#,
    );
    assert_wire(
        &Response::Error {
            code: ErrorCode::Internal,
            message: String::new(),
        },
        r#"{"type":"error","code":"internal","message":""}"#,
    );
}

#[test]
fn engine_responses() {
    assert_wire(
        &Response::Engine(EngineStatus::new(EngineState::NotRunning)),
        r#"{"type":"engine","state":"not_running","flavor":null,"engine_version":null,"api_version":null,"compose_version":null,"desktop_version":null,"endpoint":null}"#,
    );
    let mut ready = EngineStatus::new(EngineState::Ready);
    ready.flavor = Some(EngineFlavor::DockerDesktop);
    ready.engine_version = Some("27.3.1".into());
    ready.api_version = Some("1.47".into());
    ready.compose_version = Some("2.29.7".into());
    ready.desktop_version = Some("4.34.2".into());
    ready.endpoint = Some("unix:///var/run/docker.sock".into());
    assert_wire(
        &Response::Engine(ready),
        r#"{"type":"engine","state":"ready","flavor":"docker_desktop","engine_version":"27.3.1","api_version":"1.47","compose_version":"2.29.7","desktop_version":"4.34.2","endpoint":"unix:///var/run/docker.sock"}"#,
    );
    let mut unsupported = EngineStatus::new(EngineState::Unsupported);
    unsupported.endpoint = Some("tcp://10.0.0.5:2375".into());
    assert_wire(
        &Response::Engine(unsupported),
        r#"{"type":"engine","state":"unsupported","flavor":null,"engine_version":null,"api_version":null,"compose_version":null,"desktop_version":null,"endpoint":"tcp://10.0.0.5:2375"}"#,
    );
}

#[test]
fn engine_response_tolerates_missing_optional_fields() {
    assert_eq!(
        decode_message::<Response>(br#"{"type":"engine","state":"starting"}"#).unwrap(),
        Response::Engine(EngineStatus::new(EngineState::Starting))
    );
}

#[test]
fn unknown_request_type_is_a_decode_error() {
    assert!(decode_message::<Request>(br#"{"type":"from_the_future"}"#).is_err());
}
