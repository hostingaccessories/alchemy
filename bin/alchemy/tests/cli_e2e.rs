//! End-to-end: a real alchemyd IPC server on a tempdir socket, driven
//! through the CLI's public API.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alchemy::{Cli, ClientError, Command};
use alchemy_ipc::{PROTOCOL_VERSION, Request, Response};
use tokio::sync::watch;

#[tokio::test(flavor = "multi_thread")]
async fn status_round_trips_through_a_real_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("alchemyd.sock");
    let listener = alchemyd::ipc::bind(&socket).unwrap();
    let (stop, stop_rx) = watch::channel(false);
    let server = tokio::spawn(alchemyd::ipc::serve(listener, stop_rx));

    assert_eq!(
        alchemy::exchange_at(&socket, &Request::Ping).await.unwrap(),
        Response::Pong
    );

    let out = alchemy::run(Cli {
        json: true,
        socket: Some(socket.clone()),
        command: Command::Status,
    })
    .await
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["daemon"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["protocol"], PROTOCOL_VERSION);

    stop.send_replace(true);
    server.await.unwrap();
}

#[tokio::test]
async fn missing_daemon_is_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let err = alchemy::run(Cli {
        json: false,
        socket: Some(tmp.path().join("nope.sock")),
        command: Command::Status,
    })
    .await
    .unwrap_err();
    assert!(matches!(err, ClientError::DaemonUnreachable(_)), "{err:?}");
    assert_eq!(err.exit_code(), 69);
}
