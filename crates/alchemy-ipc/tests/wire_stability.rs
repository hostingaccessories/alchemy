//! Pins the exact JSON bytes of every IPC message. A failure here means
//! the wire contract changed: make it additive or bump
//! `PROTOCOL_VERSION`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

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
fn unknown_request_type_is_a_decode_error() {
    assert!(decode_message::<Request>(br#"{"type":"from_the_future"}"#).is_err());
}
