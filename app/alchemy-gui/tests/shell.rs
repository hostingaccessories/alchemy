//! The shell against a stand-in alchemyd on a real TCP port: attaching,
//! installing when none answers, following the event stream into the
//! tray menu, noticing the daemon go, and starting it again. The installer
//! is a fake that brings the stand-in up, so nothing is installed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alchemy_gui::client::{HttpDaemon, Method, Opened};
use alchemy_gui::launch::{Installed, Installer, Launch, Probe as _, Timing};
use alchemy_gui::shell::{Retry, Shell};
use alchemy_gui::status::MenuModel;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A stand-in alchemyd. While `down`, it answers 503 everywhere, as if
/// nothing were there to talk to.
#[derive(Clone, Default)]
struct Daemon {
    down: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    fn router(&self) -> Router {
        let health = self.clone();
        let events = self.clone();
        let start = self.clone();
        Router::new()
            .route(
                "/api/health",
                get(move || {
                    let d = health.clone();
                    async move {
                        if d.down.load(Ordering::SeqCst) {
                            StatusCode::SERVICE_UNAVAILABLE.into_response()
                        } else {
                            r#"{"status":"ok"}"#.into_response()
                        }
                    }
                }),
            )
            .route(
                "/api/events",
                get(move |headers: HeaderMap| {
                    let d = events.clone();
                    async move {
                        if d.down.load(Ordering::SeqCst) {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        if !signed_in(&headers) {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        sse(&[
                            "event: engine\ndata: {\"state\":\"not_running\",\"flavor\":\"docker_desktop\"}\n\n",
                            "event: apps\ndata: {\"apps\":[],\"unmanaged\":[],\"dirs\":[]}\n\n",
                        ])
                    }
                }),
            )
            .route(
                "/api/engine/start",
                post(move |headers: HeaderMap, body: String| {
                    let d = start.clone();
                    async move {
                        let content_type = headers
                            .get(header::CONTENT_TYPE)
                            .map_or("", |v| v.to_str().unwrap());
                        d.seen.lock().unwrap().push(format!(
                            "{} {content_type} {body}",
                            signed_in(&headers)
                        ));
                        r#"{"result":"unsupported","message":"Not here.","command":null}"#
                    }
                }),
            )
    }
}

fn signed_in(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .is_some_and(|v| v.to_str().unwrap() == format!("Bearer {TOKEN}"))
}

/// An SSE answer with `chunks` and then a stream that stays open, as
/// alchemyd's does between events.
fn sse(chunks: &'static [&'static str]) -> Response {
    let head = futures_util::stream::iter(
        chunks
            .iter()
            .map(|c| Ok::<_, Infallible>(Bytes::from_static(c.as_bytes()))),
    );
    let tail = futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok::<_, Infallible>(Bytes::from_static(b":\n\n"))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(futures_util::StreamExt::chain(
            head, tail,
        )))
        .unwrap()
}

/// Reserve a free loopback port.
fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

fn serve(addr: SocketAddr, daemon: &Daemon) {
    let app = daemon.router();
    let listener = std::net::TcpListener::bind(addr).unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
}

/// "Installs" by bringing the stand-in up: serving it the first time, and
/// clearing `down` after that.
#[derive(Clone)]
struct FakeInstaller {
    addr: SocketAddr,
    daemon: Daemon,
    serving: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl Installer for FakeInstaller {
    fn install(&self, _alchemyd: &Path) -> impl Future<Output = Installed> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.daemon.down.store(false, Ordering::SeqCst);
        if !self.serving.swap(true, Ordering::SeqCst) {
            serve(self.addr, &self.daemon);
        }
        std::future::ready(Installed::Done)
    }
}

fn client(addr: SocketAddr) -> HttpDaemon {
    HttpDaemon::new(addr, Arc::new(|| Ok(TOKEN.to_owned())))
}

fn fast() -> (Timing, Retry) {
    (
        Timing {
            poll: Duration::from_millis(20),
            start: Duration::from_secs(5),
        },
        Retry {
            soon: Duration::from_millis(20),
            idle: Duration::from_millis(50),
            gone_after: Duration::from_millis(300),
        },
    )
}

/// Wait up to 5 s for the menu to satisfy `ok`.
async fn until(menus: &Arc<Mutex<Vec<MenuModel>>>, ok: impl Fn(&MenuModel) -> bool) -> MenuModel {
    for _ in 0..250 {
        if let Some(menu) = menus.lock().unwrap().last().filter(|m| ok(m)) {
            return menu.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("menu never matched: {:?}", menus.lock().unwrap().last());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installs_when_nothing_answers_follows_the_daemon_and_starts_it_again() {
    let addr = free_addr();
    let daemon = Daemon::default();
    let installer = FakeInstaller {
        addr,
        daemon: daemon.clone(),
        serving: Arc::new(AtomicBool::new(false)),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let (timing, retry) = fast();
    let shell = Shell::new(
        client(addr),
        installer.clone(),
        Ok(PathBuf::from("/usr/bin/alchemyd")),
        timing,
        retry,
    );
    let menus = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&menus);
    shell.on_change(move |m| seen.lock().unwrap().push(m.clone()));
    tokio::spawn(Arc::clone(&shell).run());

    let menu = until(&menus, |m| m.status == "alchemyd running · Docker stopped").await;
    assert_eq!(installer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(menu.start_docker, Some(true));
    assert!(!menu.start_daemon);
    assert!(
        menus
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.status == "Starting Alchemy…"),
        "showed the starting state"
    );

    shell.start_docker().await;
    assert_eq!(
        daemon.seen.lock().unwrap().as_slice(),
        ["true application/json {}"]
    );
    assert_eq!(shell.menu().notice.as_deref(), Some("Not here."));

    daemon.down.store(true, Ordering::SeqCst);
    let menu = until(&menus, |m| m.start_daemon).await;
    assert_eq!(shell.launch(), Launch::Stopped);
    assert_eq!(menu.status, "alchemyd isn't running");

    shell.request_start();
    until(&menus, |m| m.status.starts_with("alchemyd running")).await;
    assert_eq!(installer.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_answers_is_attached_without_installing() {
    let addr = free_addr();
    let daemon = Daemon::default();
    serve(addr, &daemon);
    let installer = FakeInstaller {
        addr,
        daemon,
        serving: Arc::new(AtomicBool::new(true)),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let (timing, retry) = fast();
    let shell = Shell::new(
        client(addr),
        installer.clone(),
        Err("never".to_owned()),
        timing,
        retry,
    );
    let menus = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&menus);
    shell.on_change(move |m| seen.lock().unwrap().push(m.clone()));
    tokio::spawn(Arc::clone(&shell).run());
    until(&menus, |m| m.status.starts_with("alchemyd running")).await;
    assert_eq!(installer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_nothing_to_install_the_shell_says_why_and_never_installs() {
    let addr = free_addr();
    let installer = FakeInstaller {
        addr,
        daemon: Daemon::default(),
        serving: Arc::new(AtomicBool::new(false)),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let (timing, retry) = fast();
    let why = "Alchemy runs from an AppImage.".to_owned();
    let shell = Shell::new(
        client(addr),
        installer.clone(),
        Err(why.clone()),
        timing,
        retry,
    );
    let menus = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&menus);
    shell.on_change(move |m| seen.lock().unwrap().push(m.clone()));
    tokio::spawn(Arc::clone(&shell).run());
    let menu = until(&menus, |m| m.notice.is_some()).await;
    assert_eq!(menu.notice.as_deref(), Some(why.as_str()));
    assert!(!menu.start_daemon);
    assert_eq!(shell.launch(), Launch::Unavailable { message: why });
    shell.request_start();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(installer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_signs_in_and_reads_events() {
    let addr = free_addr();
    serve(addr, &Daemon::default());
    let daemon = client(addr);
    assert!(daemon.answers().await);
    let Opened::Stream(mut events) = daemon.events().await.unwrap() else {
        panic!("refused");
    };
    let first = events.next().await.unwrap().unwrap();
    assert_eq!(first.event, "engine");

    let stale = HttpDaemon::new(addr, Arc::new(|| Ok("0".repeat(64))));
    let Opened::Refused(reply) = stale.events().await.unwrap() else {
        panic!("opened");
    };
    assert_eq!(reply.status, 401);

    let unreadable = HttpDaemon::new(addr, Arc::new(|| Err("no token".to_owned())));
    let err = unreadable
        .request(Method::POST, "/api/engine/start", Some(b"{}"))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "no token");

    assert!(!client(free_addr()).answers().await);
}
