//! The web address proxy against in-process fake apps: routing by port
//! and by `Host`, streaming both ways, upgraded connections, hot route swaps,
//! header hygiene, the self-routing guard, a refused Host listener, and
//! shutdown.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use alchemy_core::route::{
    AppRouteInput, HostSuffix, Reachability, RouteContainer, RouteOptions, RouteTable,
    ServiceRouteInput, XAlchemy, XAlchemyBlock, plan_routes,
};
use alchemyd::proxy::{
    self, FakeConnector, Listen, ListenerStatus, ProxyConfig, ProxyStatus, SharedConnector,
};
use http_body_util::{BodyExt as _, Either, Empty, Full};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(10);

type AppBody = Either<Incoming, Full<Bytes>>;

/// A fake app tagged `tag`, on loopback.
///
/// - `/headers` answers with the request's headers, one `name: value`
///   per line, sorted, and sets a session cookie and an app cookie.
/// - `/count` answers with how many body bytes it read.
/// - `/echo` streams the request body straight back.
/// - `/ws` switches protocols, greets with `hello from <tag>`, then
///   echoes.
/// - anything else answers with the tag.
async fn fake_app(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(move |req| app_handle(tag, req));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    addr
}

async fn app_handle(
    tag: &'static str,
    mut req: Request<Incoming>,
) -> Result<Response<AppBody>, Infallible> {
    let full = |s: String| Either::Right(Full::new(Bytes::from(s)));
    let resp = match req.uri().path() {
        "/headers" => {
            let mut lines: Vec<String> = req
                .headers()
                .iter()
                .map(|(n, v)| format!("{n}: {}", v.to_str().unwrap()))
                .collect();
            lines.sort();
            Response::builder()
                .header("set-cookie", "alchemy_session_7240=planted; Path=/api")
                .header("set-cookie", "app=1")
                .body(full(lines.join("\n")))
                .unwrap()
        }
        "/count" => {
            let mut body = req.into_body();
            let mut count: u64 = 0;
            while let Some(frame) = body.frame().await {
                if let Ok(data) = frame.unwrap().into_data() {
                    count += data.len() as u64;
                }
            }
            Response::new(full(count.to_string()))
        }
        "/echo" => Response::new(Either::Left(req.into_body())),
        "/ws" => {
            let upgrade = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                let mut io = TokioIo::new(upgrade.await.unwrap());
                io.write_all(format!("hello from {tag}\n").as_bytes())
                    .await
                    .unwrap();
                let (mut r, mut w) = tokio::io::split(io);
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-accept", "fake")
                .body(full(String::new()))
                .unwrap()
        }
        _ => Response::new(full(tag.to_owned())),
    };
    Ok(resp)
}

/// A port nothing listens on right now.
async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn container_ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 89, 0, last))
}

/// `name`, routed to its `web` service on port 80 at `ip`, served on
/// proxy port `listen_port`.
fn app(name: &str, ip: IpAddr, listen_port: Option<u16>, running: bool) -> AppRouteInput {
    AppRouteInput {
        app: name.into(),
        x_alchemy: Some(XAlchemyBlock::Read(XAlchemy {
            service: Some("web".into()),
            port: Some(80),
            hostname: None,
        })),
        services: vec![ServiceRouteInput {
            name: "web".into(),
            ..ServiceRouteInput::default()
        }],
        containers: vec![RouteContainer {
            name: format!("{name}-web-1"),
            service: "web".into(),
            running,
            ips: BTreeMap::from([("alchemy".to_owned(), ip)]),
            ports: Vec::new(),
        }],
        listen_port,
    }
}

/// `name` routed straight to `upstream`, as a container IP.
fn app_at(name: &str, upstream: SocketAddr, listen_port: u16) -> AppRouteInput {
    let mut input = app(name, upstream.ip(), Some(listen_port), true);
    if let Some(XAlchemyBlock::Read(x)) = &mut input.x_alchemy {
        x.port = Some(upstream.port());
    }
    input
}

fn table(apps: &[AppRouteInput]) -> RouteTable {
    plan_routes(
        apps,
        &RouteOptions {
            reachability: Reachability::ContainerIp,
            suffixes: vec![HostSuffix::Localhost],
        },
    )
    .table
}

/// A running proxy.
struct Proxy {
    routes: watch::Sender<RouteTable>,
    status: watch::Receiver<ProxyStatus>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Proxy {
    fn start(config: ProxyConfig, connector: SharedConnector, routes: RouteTable) -> Self {
        let (routes_tx, routes_rx) = watch::channel(routes);
        let (status_tx, status) = watch::channel(ProxyStatus::default());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(proxy::run(
            config,
            connector,
            routes_rx,
            status_tx,
            shutdown_rx,
        ));
        Self {
            routes: routes_tx,
            status,
            shutdown,
            task,
        }
    }

    /// Wait until `listen` reaches a state `pick` accepts.
    async fn wait<T>(&self, listen: Listen, pick: impl Fn(ListenerStatus) -> Option<T>) -> T {
        let mut status = self.status.clone();
        let reached =
            tokio::time::timeout(TIMEOUT, status.wait_for(|s| pick(s.get(listen)).is_some()))
                .await
                .unwrap_or_else(|_| {
                    panic!("{listen:?} never got there: {:?}", self.status.borrow())
                })
                .unwrap()
                .get(listen);
        pick(reached).unwrap()
    }

    async fn listening(&mut self, listen: Listen) -> SocketAddr {
        self.wait(listen, |s| match s {
            ListenerStatus::Listening(addr) => Some(addr),
            _ => None,
        })
        .await
    }

    async fn stop(self) {
        self.shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .expect("proxy did not shut down in time")
            .unwrap();
    }
}

fn loopback_config(own: Vec<SocketAddr>) -> ProxyConfig {
    ProxyConfig {
        host_listen: Some("127.0.0.1:0".parse().unwrap()),
        app_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        own,
    }
}

async fn send<B>(addr: SocketAddr, req: Request<B>) -> Response<Incoming>
where
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    tokio::time::timeout(TIMEOUT, sender.send_request(req))
        .await
        .unwrap()
        .unwrap()
}

fn get(path: &str, host: &str) -> Request<Empty<Bytes>> {
    Request::get(path)
        .header("host", host)
        .body(Empty::new())
        .unwrap()
}

async fn text(resp: Response<Incoming>) -> String {
    let body = tokio::time::timeout(TIMEOUT, resp.into_body().collect())
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(body.to_bytes().to_vec()).unwrap()
}

/// A request body fed chunk by chunk from a channel.
struct ChannelBody(mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|b| Ok(Frame::data(b))))
    }
}

/// A request body of `chunks` 64 KiB chunks, with no length up front.
struct RepeatBody {
    chunks: usize,
}

static CHUNK: [u8; 64 * 1024] = [b'x'; 64 * 1024];

impl Body for RepeatBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.chunks == 0 {
            return Poll::Ready(None);
        }
        self.chunks -= 1;
        Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(&CHUNK)))))
    }
}

/// An open WebSocket-style tunnel through the proxy.
struct Tunnel(BufReader<TcpStream>);

impl Tunnel {
    /// Switch protocols at `addr` and return the tunnel and the app's
    /// greeting.
    async fn open(addr: SocketAddr, host: &str) -> (Self, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!(
                    "GET /ws HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\n\
                     Upgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Version: 13\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            tokio::time::timeout(TIMEOUT, reader.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push(line.trim_end().to_ascii_lowercase());
        }
        assert!(head[0].starts_with("http/1.1 101"), "{head:?}");
        assert!(head.contains(&"upgrade: websocket".to_owned()), "{head:?}");
        assert!(
            head.contains(&"sec-websocket-accept: fake".to_owned()),
            "{head:?}"
        );
        let mut tunnel = Self(reader);
        let greeting = tunnel.line().await;
        (tunnel, greeting)
    }

    async fn line(&mut self) -> String {
        let mut line = String::new();
        tokio::time::timeout(TIMEOUT, self.0.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        line.trim_end().to_owned()
    }

    async fn echo(&mut self, message: &str) -> String {
        self.0
            .get_mut()
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        self.line().await
    }
}

#[tokio::test]
async fn routes_by_port_and_host_and_names_alchemy_for_unknown_hosts() {
    let wiki = fake_app("wiki").await;
    let blog = fake_app("blog").await;
    let connector = FakeConnector::new();
    connector.redirect(SocketAddr::new(container_ip(2), 80), wiki);
    connector.redirect(SocketAddr::new(container_ip(3), 80), blog);
    let (wiki_port, blog_port) = (free_port().await, free_port().await);
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(connector),
        table(&[
            app("wiki", container_ip(2), Some(wiki_port), true),
            app("blog", container_ip(3), Some(blog_port), true),
        ]),
    );
    let wiki_addr = proxy.listening(Listen::App(wiki_port)).await;
    let blog_addr = proxy.listening(Listen::App(blog_port)).await;
    let host_addr = proxy.listening(Listen::Host).await;

    assert_eq!(
        text(send(wiki_addr, get("/", "anything")).await).await,
        "wiki"
    );
    assert_eq!(text(send(blog_addr, get("/", "x")).await).await, "blog");
    for (host, want) in [
        ("wiki.localhost", "wiki"),
        ("BLOG.localhost:80", "blog"),
        ("blog.localhost.", "blog"),
    ] {
        let resp = send(host_addr, get("/", host)).await;
        assert_eq!(resp.status(), StatusCode::OK, "{host}");
        assert_eq!(text(resp).await, want, "{host}");
    }

    for host in ["nope.localhost", "wiki.example.com", "127.0.0.1"] {
        let resp = send(host_addr, get("/", host)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{host}");
        assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
        let body = text(resp).await;
        assert!(body.contains("Alchemy"), "{body}");
        assert!(!body.contains(host), "{body}");
    }
    proxy.stop().await;
}

#[tokio::test]
async fn a_stopped_app_gets_503_and_an_unreachable_one_502() {
    let connector = FakeConnector::new();
    let (stopped_port, gone_port) = (free_port().await, free_port().await);
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(connector),
        table(&[
            app("stopped", container_ip(2), Some(stopped_port), false),
            app("gone", container_ip(9), Some(gone_port), true),
        ]),
    );
    let stopped = proxy.listening(Listen::App(stopped_port)).await;
    let gone = proxy.listening(Listen::App(gone_port)).await;
    let resp = send(stopped, get("/", "x")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(text(resp).await.contains("isn't running"));
    let resp = send(gone, get("/", "x")).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    proxy.stop().await;
}

#[tokio::test]
async fn strips_session_cookies_and_untrusted_forwarding_headers() {
    let wiki = fake_app("wiki").await;
    let connector = FakeConnector::new();
    connector.redirect(SocketAddr::new(container_ip(2), 80), wiki);
    let port = free_port().await;
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(connector),
        table(&[app("wiki", container_ip(2), Some(port), true)]),
    );
    let addr = proxy.listening(Listen::App(port)).await;

    let req = Request::get("/headers")
        .header("host", format!("localhost:{port}"))
        .header(
            "cookie",
            "theme=dark; alchemy_session_7240=secret; alchemy_session_7241=other",
        )
        .header("cookie", "lang=en")
        .header("x-forwarded-for", "6.6.6.6")
        .header("x-forwarded-proto", "https")
        .header("forwarded", "for=6.6.6.6")
        .header("proxy-authorization", "Basic c2VjcmV0")
        .header("connection", "keep-alive, x-drop-me")
        .header("x-drop-me", "1")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = send(addr, req).await;
    let set_cookies: Vec<&str> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(set_cookies, ["app=1"]);
    let seen = text(resp).await;
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(
        lines,
        [
            "cookie: theme=dark; lang=en",
            &format!("host: localhost:{port}"),
            "x-forwarded-for: 127.0.0.1",
            &format!("x-forwarded-host: localhost:{port}"),
            "x-forwarded-proto: http",
        ]
    );
    assert!(!seen.contains("secret"));
    proxy.stop().await;
}

#[tokio::test]
async fn streams_an_upload_larger_than_any_buffer() {
    let wiki = fake_app("wiki").await;
    let connector = FakeConnector::new();
    connector.redirect(SocketAddr::new(container_ip(2), 80), wiki);
    let port = free_port().await;
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(connector),
        table(&[app("wiki", container_ip(2), Some(port), true)]),
    );
    let addr = proxy.listening(Listen::App(port)).await;

    let chunks = 1024;
    let req = Request::post("/count")
        .header("host", "x")
        .body(RepeatBody { chunks })
        .unwrap();
    let resp = send(addr, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(text(resp).await, (chunks * CHUNK.len()).to_string());
    proxy.stop().await;
}

#[tokio::test]
async fn streams_both_ways_without_waiting_for_the_whole_body() {
    let wiki = fake_app("wiki").await;
    let connector = FakeConnector::new();
    connector.redirect(SocketAddr::new(container_ip(2), 80), wiki);
    let port = free_port().await;
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(connector),
        table(&[app("wiki", container_ip(2), Some(port), true)]),
    );
    let addr = proxy.listening(Listen::App(port)).await;

    let (chunks, rx) = mpsc::channel(1);
    let req = Request::post("/echo")
        .header("host", "x")
        .body(ChannelBody(rx))
        .unwrap();
    chunks.send(Bytes::from_static(b"first")).await.unwrap();
    let mut body = send(addr, req).await.into_body();
    for message in ["first", "second", "third"] {
        if message != "first" {
            chunks.send(Bytes::from(message)).await.unwrap();
        }
        let mut got = Vec::new();
        while got.len() < message.len() {
            let frame = tokio::time::timeout(TIMEOUT, body.frame())
                .await
                .expect("echo did not arrive before the body ended")
                .unwrap()
                .unwrap();
            got.extend_from_slice(&frame.into_data().unwrap());
        }
        assert_eq!(got, message.as_bytes());
    }
    drop(chunks);
    assert!(
        tokio::time::timeout(TIMEOUT, body.frame())
            .await
            .unwrap()
            .is_none()
    );
    proxy.stop().await;
}

#[tokio::test]
async fn bridges_websockets_and_keeps_them_across_a_route_swap() {
    let first = fake_app("first").await;
    let second = fake_app("second").await;
    let connector = FakeConnector::new();
    connector.redirect(SocketAddr::new(container_ip(2), 80), first);
    connector.redirect(SocketAddr::new(container_ip(3), 80), second);
    let port = free_port().await;
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(connector),
        table(&[app("wiki", container_ip(2), Some(port), true)]),
    );
    let addr = proxy.listening(Listen::App(port)).await;
    let host_addr = proxy.listening(Listen::Host).await;

    let (mut by_port, greeting) = Tunnel::open(addr, "x").await;
    assert_eq!(greeting, "hello from first");
    let (mut by_host, greeting) = Tunnel::open(host_addr, "wiki.localhost").await;
    assert_eq!(greeting, "hello from first");
    assert_eq!(by_port.echo("ping").await, "ping");

    proxy
        .routes
        .send_replace(table(&[app("wiki", container_ip(3), Some(port), true)]));
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if text(send(addr, get("/", "x")).await).await == "second" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "swap never applied");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_, greeting) = Tunnel::open(addr, "x").await;
    assert_eq!(greeting, "hello from second");
    assert_eq!(by_port.echo("still first").await, "still first");
    assert_eq!(by_host.echo("me too").await, "me too");

    proxy.routes.send_replace(RouteTable::default());
    proxy
        .wait(Listen::App(port), |s| {
            (s == ListenerStatus::Off).then_some(())
        })
        .await;
    proxy
        .wait(Listen::Host, |s| (s == ListenerStatus::Off).then_some(()))
        .await;
    assert!(TcpStream::connect(addr).await.is_err());
    assert!(TcpStream::connect(host_addr).await.is_err());
    assert_eq!(by_port.echo("after removal").await, "after removal");
    assert_eq!(by_host.echo("still here").await, "still here");

    proxy.stop().await;
}

#[tokio::test]
async fn never_connects_to_the_daemon_or_itself() {
    let daemon = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daemon_addr = daemon.local_addr().unwrap();
    let connector = FakeConnector::new();
    let (to_daemon, to_self) = (free_port().await, free_port().await);
    let self_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), to_self);
    let mut proxy = Proxy::start(
        loopback_config(vec![daemon_addr]),
        Arc::new(connector.clone()),
        table(&[
            app_at("daemon", daemon_addr, to_daemon),
            app_at("loop", self_addr, to_self),
        ]),
    );
    let daemon_route = proxy.listening(Listen::App(to_daemon)).await;
    let loop_route = proxy.listening(Listen::App(to_self)).await;
    let host_addr = proxy.listening(Listen::Host).await;

    for addr in [daemon_route, loop_route] {
        let resp = send(addr, get("/", "x")).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
    proxy
        .routes
        .send_replace(table(&[app_at("host", host_addr, to_daemon)]));
    proxy
        .wait(Listen::App(to_self), |s| {
            (s == ListenerStatus::Off).then_some(())
        })
        .await;
    let resp = send(daemon_route, get("/", "x")).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(connector.attempts(), []);
    proxy.stop().await;
}

#[tokio::test]
async fn a_refused_host_listener_is_reported_and_app_ports_still_serve() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wiki = fake_app("wiki").await;
    let connector = FakeConnector::new();
    connector.redirect(SocketAddr::new(container_ip(2), 80), wiki);
    let port = free_port().await;
    let config = ProxyConfig {
        host_listen: Some(taken.local_addr().unwrap()),
        ..loopback_config(vec![])
    };
    let mut proxy = Proxy::start(
        config,
        Arc::new(connector),
        table(&[app("wiki", container_ip(2), Some(port), true)]),
    );
    let failure = proxy
        .wait(Listen::Host, |s| match s {
            ListenerStatus::Unavailable(e) => Some(e),
            _ => None,
        })
        .await;
    assert_eq!(failure.kind, std::io::ErrorKind::AddrInUse);
    assert_eq!(failure.addr, taken.local_addr().unwrap());
    let addr = proxy.listening(Listen::App(port)).await;
    assert_eq!(text(send(addr, get("/", "x")).await).await, "wiki");
    proxy.stop().await;
}

#[tokio::test]
async fn an_empty_table_binds_nothing() {
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(FakeConnector::new()),
        RouteTable::default(),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*proxy.status.borrow_and_update(), ProxyStatus::default());
    proxy.stop().await;
}

#[tokio::test]
async fn shutdown_closes_listeners_and_open_tunnels() {
    let wiki = fake_app("wiki").await;
    let connector = FakeConnector::new();
    connector.redirect(SocketAddr::new(container_ip(2), 80), wiki);
    let port = free_port().await;
    let mut proxy = Proxy::start(
        loopback_config(vec![]),
        Arc::new(connector),
        table(&[app("wiki", container_ip(2), Some(port), true)]),
    );
    let addr = proxy.listening(Listen::App(port)).await;
    let (mut tunnel, _) = Tunnel::open(addr, "x").await;
    let idle = TcpStream::connect(addr).await.unwrap();
    let status = proxy.status.clone();

    proxy.stop().await;
    assert_eq!(*status.borrow(), ProxyStatus::default());
    assert!(TcpStream::connect(addr).await.is_err());
    let mut rest = Vec::new();
    let read = tokio::time::timeout(TIMEOUT, tunnel.0.read_to_end(&mut rest)).await;
    assert!(read.is_ok(), "tunnel stayed open after shutdown");
    drop(idle);
}
