//! Forwarding one request: find its route in the latest table, connect
//! to the app, and stream both bodies through, bridging a protocol
//! switch such as a WebSocket.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use alchemy_core::route::{Route, RouteTable};
use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::upgrade::OnUpgrade;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use tokio::sync::{mpsc, watch};

use super::connect::{SharedConnector, is_own_address};
use super::headers::{self, Forwarded};
use super::{Listen, ProxyStatus, stopped};

/// The body of a proxied response: the app's, streamed, or one of
/// Alchemy's own short pages.
pub type ProxyBody = Either<Incoming, Full<Bytes>>;

/// How long connecting to an app may take before the request fails.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What every request on every proxy listener shares.
#[derive(Debug)]
pub(super) struct Shared {
    pub routes: watch::Receiver<RouteTable>,
    pub status: watch::Receiver<ProxyStatus>,
    pub connector: SharedConnector,
    pub own: Vec<SocketAddr>,
    pub shutdown: watch::Receiver<bool>,
}

impl Shared {
    /// The daemon's own addresses and every address the proxy listens on.
    fn own_addresses(&self) -> Vec<SocketAddr> {
        let status = self.status.borrow();
        self.own.iter().copied().chain(status.listening()).collect()
    }
}

/// The route for a request that arrived on `listen` with `host`.
fn find_route(table: &RouteTable, listen: Listen, host: Option<&str>) -> Option<Route> {
    match listen {
        Listen::Host => host.and_then(|h| table.lookup_host(h)),
        Listen::App(port) => table.lookup_port(port),
    }
    .cloned()
}

/// The `Host` a request names: its header, or the authority of an
/// absolute-form target.
fn request_host(req: &Request<Incoming>) -> Option<HeaderValue> {
    req.headers().get(header::HOST).cloned().or_else(|| {
        req.uri()
            .authority()
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    })
}

/// Answer `req`, which arrived on `listen` from `peer`.
///
/// `drain` is held by any WebSocket or other upgraded connection this
/// starts, so shutdown can wait for it.
pub(super) async fn handle(
    shared: Arc<Shared>,
    listen: Listen,
    peer: SocketAddr,
    drain: mpsc::Sender<()>,
    mut req: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    if req.method() == Method::CONNECT {
        return Ok(page(Page::NotAllowed));
    }
    let host = request_host(&req);
    let route = {
        let table = shared.routes.borrow();
        find_route(&table, listen, host.as_ref().and_then(|h| h.to_str().ok()))
    };
    let Some(route) = route else {
        return Ok(page(Page::NotFound));
    };
    let Some(upstream) = route.upstream else {
        return Ok(page(Page::NotRunning));
    };
    if is_own_address(&upstream, &shared.own_addresses()) {
        tracing::warn!(
            app = %route.app,
            upstream = %upstream.addr(),
            "refusing to proxy an app to Alchemy itself"
        );
        return Ok(page(Page::BadGateway));
    }

    let upgrade = headers::upgrade_protocol(req.headers());
    let client_upgrade = upgrade.is_some().then(|| hyper::upgrade::on(&mut req));
    headers::forward_request(
        req.headers_mut(),
        &Forwarded {
            client: peer.ip(),
            host,
        },
        upgrade.as_ref(),
    );
    *req.uri_mut() = origin_form(req.uri());

    let mut resp = match send(&shared.connector, upstream.addr(), req).await {
        Ok(resp) => resp,
        Err(error) => {
            tracing::debug!(app = %route.app, %error, "proxy request failed");
            return Ok(page(Page::BadGateway));
        }
    };
    let switching = resp.status() == StatusCode::SWITCHING_PROTOCOLS;
    if switching {
        let Some(client_upgrade) = client_upgrade else {
            return Ok(page(Page::BadGateway));
        };
        let app_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(tunnel(
            client_upgrade,
            app_upgrade,
            shared.shutdown.clone(),
            drain,
        ));
    }
    headers::forward_response(resp.headers_mut(), switching);
    Ok(resp.map(Either::Left))
}

/// Why a request to an app failed before it got a response.
#[derive(Debug, thiserror::Error)]
enum SendError {
    #[error("connecting timed out")]
    Timeout,
    #[error("cannot connect")]
    Connect(#[source] std::io::Error),
    #[error("HTTP error")]
    Http(#[source] hyper::Error),
}

/// Send `req` to `addr` over a new connection, returning the response
/// once its head arrives. The body streams after.
async fn send(
    connector: &SharedConnector,
    addr: SocketAddr,
    req: Request<Incoming>,
) -> Result<Response<Incoming>, SendError> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(addr))
        .await
        .map_err(|_| SendError::Timeout)?
        .map_err(SendError::Connect)?;
    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .handshake(TokioIo::new(stream))
        .await
        .map_err(SendError::Http)?;
    tokio::spawn(async move {
        if let Err(error) = conn.with_upgrades().await {
            tracing::debug!(%error, "proxy upstream connection ended");
        }
    });
    sender.send_request(req).await.map_err(SendError::Http)
}

/// Copy bytes both ways between a client and an app once both have
/// switched protocols, until either side closes or the daemon stops.
async fn tunnel(
    client: OnUpgrade,
    app: OnUpgrade,
    mut shutdown: watch::Receiver<bool>,
    _drain: mpsc::Sender<()>,
) {
    let (client, app) = match tokio::try_join!(client, app) {
        Ok(pair) => pair,
        Err(error) => {
            tracing::debug!(%error, "proxy upgrade failed");
            return;
        }
    };
    let mut client = TokioIo::new(client);
    let mut app = TokioIo::new(app);
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut client, &mut app) => {
            if let Err(error) = result {
                tracing::debug!(%error, "proxy tunnel ended");
            }
        }
        () = stopped(&mut shutdown) => {}
    }
}

/// `uri` as a path and query, the form an origin server expects.
fn origin_form(uri: &Uri) -> Uri {
    uri.path_and_query()
        .map_or_else(|| Uri::from_static("/"), |pq| Uri::from(pq.clone()))
}

/// One of Alchemy's own answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    NotFound,
    NotRunning,
    BadGateway,
    NotAllowed,
}

impl Page {
    fn status(self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::NotRunning => StatusCode::SERVICE_UNAVAILABLE,
            Self::BadGateway => StatusCode::BAD_GATEWAY,
            Self::NotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::NotFound => "Alchemy has no app at this address.",
            Self::NotRunning => "This app isn't running. Start it in Alchemy to reach it here.",
            Self::BadGateway => "Alchemy couldn't reach this app.",
            Self::NotAllowed => "Alchemy's proxy doesn't accept CONNECT requests.",
        }
    }
}

/// A short HTML page for `page`. It never echoes the request, so it
/// can't reflect anything a client sent.
fn page(page: Page) -> Response<ProxyBody> {
    let status = page.status();
    let title = status.canonical_reason().unwrap_or("Error");
    let html = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body><h1>{title}</h1><p>{}</p></body></html>\n",
        page.text()
    );
    let mut resp = Response::new(Either::Right(Full::new(Bytes::from(html))));
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;

    #[test]
    fn origin_form_table() {
        let cases = [
            ("/", "/"),
            ("/a/b?c=d", "/a/b?c=d"),
            ("http://wiki.localhost/x?y", "/x?y"),
            ("http://wiki.localhost", "/"),
            ("*", "*"),
        ];
        for (input, want) in cases {
            let uri: Uri = input.parse().unwrap();
            assert_eq!(origin_form(&uri).to_string(), want, "{input}");
        }
    }

    #[test]
    fn pages_name_alchemy_and_have_the_right_status() {
        for (p, status) in [
            (Page::NotFound, 404),
            (Page::NotRunning, 503),
            (Page::BadGateway, 502),
            (Page::NotAllowed, 405),
        ] {
            assert_eq!(p.status().as_u16(), status);
            assert!(p.text().contains("Alchemy"), "{p:?}");
            let resp = page(p);
            assert_eq!(resp.status().as_u16(), status);
            assert_eq!(
                resp.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
        }
    }
}
