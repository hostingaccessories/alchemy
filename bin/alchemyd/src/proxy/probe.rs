//! Checking that a proxy listener is really ours.
//!
//! Binding a port doesn't mean its traffic arrives: a `pf` redirect, a
//! program on `127.0.0.1:80` that shadows our `0.0.0.0:80`, or a security
//! tool can take it while the bind succeeds. So the routing task sends
//! each listener a request carrying [`PROBE_HEADER`] with a random
//! [`ProbeMarker`] challenge, and the proxy answers it before routing
//! with the marker's reply. Only a listener that answers with the reply
//! is ours ([`classify`]).
//!
//! The proxy answers a marked request itself and never forwards it, so
//! no app sees a challenge. The marker is fresh for each round of checks
//! and cleared after it, its halves are compared in constant time, and a
//! request with any other value gets the same 404 as an unknown address,
//! so a caller without the challenge learns nothing it couldn't already.
//!
//! Sending the requests is a side effect behind [`ListenerProbe`]:
//! [`TcpProbe`] in the daemon, [`FakeProbe`] in tests.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::auth::{is_secret, secrets_match};
use crate::session::random_secret;

/// The header a probe sends its challenge in, and the proxy its reply.
pub const PROBE_HEADER: &str = "x-alchemy-probe";

/// How long one probe may take, connecting included.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A one-time pair of secrets for a round of probes: the challenge a
/// probe sends and the reply only Alchemy's proxy knows to send back.
///
/// The reply is never on the wire until the proxy has seen the challenge,
/// so a program that echoes requests back can't pass for the proxy.
#[derive(Clone, PartialEq, Eq)]
pub struct ProbeMarker {
    challenge: String,
    reply: String,
}

impl ProbeMarker {
    /// A marker from `challenge` and `reply`.
    pub fn new(challenge: String, reply: String) -> Self {
        Self { challenge, reply }
    }

    /// A fresh marker from the OS's random source.
    pub fn generate() -> io::Result<Self> {
        Ok(Self::new(random_secret()?, random_secret()?))
    }

    /// The challenge a probe sends.
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// The reply the proxy answers the challenge with.
    pub fn reply(&self) -> &str {
        &self.reply
    }

    /// The reply for a request carrying `headers`, when it holds exactly
    /// one [`PROBE_HEADER`] and that is the challenge.
    pub fn answer(&self, headers: &HeaderMap) -> Option<&str> {
        let mut values = headers.get_all(PROBE_HEADER).iter();
        let (Some(given), None) = (values.next(), values.next()) else {
            return None;
        };
        let given = given.to_str().ok()?;
        secrets_match(given, &self.challenge).then_some(self.reply.as_str())
    }
}

impl fmt::Debug for ProbeMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProbeMarker(..)")
    }
}

/// Whether `headers` carry [`PROBE_HEADER`] at all, so the request is a
/// probe the proxy must answer itself.
pub fn is_probe(headers: &HeaderMap) -> bool {
    headers.contains_key(PROBE_HEADER)
}

/// What came back from one probe, before it is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeReply {
    /// Something answered with an HTTP response.
    Answered {
        /// Its status.
        status: u16,
        /// Its [`PROBE_HEADER`], if it sent exactly one that is text.
        proof: Option<String>,
    },
    /// Nothing answered: the connection was refused, timed out, or broke.
    Failed {
        /// What kind of failure.
        kind: io::ErrorKind,
        /// The OS's or HTTP's message.
        message: String,
    },
}

/// What one probe says about a listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// Alchemy's proxy answered.
    Ours,
    /// Something else answered.
    Foreign,
    /// Nothing answered, with why.
    Unreachable(String),
}

/// Judge `reply` to a probe that sent `marker`'s challenge: it is ours
/// only when it is a `204` carrying the marker's reply.
pub fn classify(reply: &ProbeReply, marker: &ProbeMarker) -> ProbeResult {
    match reply {
        ProbeReply::Answered {
            status: 204,
            proof: Some(proof),
        } if is_secret(proof) && secrets_match(proof, marker.reply()) => ProbeResult::Ours,
        ProbeReply::Answered { .. } => ProbeResult::Foreign,
        ProbeReply::Failed { message, .. } => ProbeResult::Unreachable(message.clone()),
    }
}

/// Sends one probe to a proxy listener.
///
/// Object safe, so the routing task can hold any implementation as a
/// [`SharedProbe`].
pub trait ListenerProbe: Send + Sync + fmt::Debug {
    /// Send `marker`'s challenge to `addr` and report what came back.
    fn probe(&self, addr: SocketAddr, marker: ProbeMarker) -> BoxFuture<'_, ProbeReply>;
}

/// A [`ListenerProbe`] shared by the routing task.
pub type SharedProbe = Arc<dyn ListenerProbe>;

/// Probes over this host's network with one HTTP/1.1 `GET`, within
/// [`PROBE_TIMEOUT`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpProbe;

impl ListenerProbe for TcpProbe {
    fn probe(&self, addr: SocketAddr, marker: ProbeMarker) -> BoxFuture<'_, ProbeReply> {
        Box::pin(async move {
            match tokio::time::timeout(PROBE_TIMEOUT, send_probe(addr, &marker)).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(error)) => ProbeReply::Failed {
                    kind: error.kind(),
                    message: error.to_string(),
                },
                Err(_) => ProbeReply::Failed {
                    kind: io::ErrorKind::TimedOut,
                    message: "no answer in time".to_owned(),
                },
            }
        })
    }
}

async fn send_probe(addr: SocketAddr, marker: &ProbeMarker) -> io::Result<ProbeReply> {
    let stream = TcpStream::connect(addr).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    let conn = tokio::spawn(conn);
    let host = HeaderValue::from_str(&addr.to_string()).map_err(io::Error::other)?;
    let challenge = HeaderValue::from_str(marker.challenge()).map_err(io::Error::other)?;
    let req = Request::get("/")
        .header(header::HOST, host)
        .header(header::CONNECTION, "close")
        .header(PROBE_HEADER, challenge)
        .body(Empty::<Bytes>::new())
        .map_err(io::Error::other)?;
    let resp = sender.send_request(req).await.map_err(io::Error::other);
    conn.abort();
    let resp = resp?;
    let mut proofs = resp.headers().get_all(PROBE_HEADER).iter();
    let proof = match (proofs.next(), proofs.next()) {
        (Some(one), None) => one.to_str().ok().map(str::to_owned),
        _ => None,
    };
    Ok(ProbeReply::Answered {
        status: resp.status().as_u16(),
        proof,
    })
}

/// How a [`FakeProbe`] answers for a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeAnswer {
    /// As Alchemy's proxy would.
    Ours,
    /// As another program would, with this status.
    Foreign(u16),
    /// Not at all: the connection fails with this kind of error.
    Fail(io::ErrorKind),
}

/// A [`ListenerProbe`] for tests: answers from a table by port, with a
/// default for any other, and records every address it was asked about.
/// It connects to nothing.
#[derive(Debug, Clone)]
pub struct FakeProbe {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug)]
struct FakeInner {
    default: FakeAnswer,
    by_port: HashMap<u16, FakeAnswer>,
    asked: Vec<SocketAddr>,
}

impl Default for FakeProbe {
    fn default() -> Self {
        Self::new(FakeAnswer::Ours)
    }
}

impl FakeProbe {
    /// A probe that answers `default` for every port.
    pub fn new(default: FakeAnswer) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeInner {
                default,
                by_port: HashMap::new(),
                asked: Vec::new(),
            })),
        }
    }

    /// Answer `answer` for `port` from now on.
    pub fn set(&self, port: u16, answer: FakeAnswer) {
        self.lock().by_port.insert(port, answer);
    }

    /// Answer `answer` for every port without its own from now on.
    pub fn set_default(&self, answer: FakeAnswer) {
        self.lock().default = answer;
    }

    /// Every address asked about so far, in order.
    pub fn asked(&self) -> Vec<SocketAddr> {
        self.lock().asked.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ListenerProbe for FakeProbe {
    fn probe(&self, addr: SocketAddr, marker: ProbeMarker) -> BoxFuture<'_, ProbeReply> {
        let answer = {
            let mut inner = self.lock();
            inner.asked.push(addr);
            inner
                .by_port
                .get(&addr.port())
                .unwrap_or(&inner.default)
                .clone()
        };
        let reply = match answer {
            FakeAnswer::Ours => ProbeReply::Answered {
                status: StatusCode::NO_CONTENT.as_u16(),
                proof: Some(marker.reply().to_owned()),
            },
            FakeAnswer::Foreign(status) => ProbeReply::Answered {
                status,
                proof: None,
            },
            FakeAnswer::Fail(kind) => ProbeReply::Failed {
                kind,
                message: io::Error::from(kind).to_string(),
            },
        };
        Box::pin(async move { reply })
    }
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

    fn marker() -> ProbeMarker {
        ProbeMarker::new("a".repeat(64), "b".repeat(64))
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in values {
            map.append(PROBE_HEADER, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn answer_table() {
        let m = marker();
        let challenge = m.challenge().to_owned();
        let cases: [(&str, Vec<&str>, Option<&str>); 6] = [
            ("no header", vec![], None),
            ("the challenge", vec![&challenge], Some(m.reply())),
            ("the reply", vec![m.reply()], None),
            ("a prefix", vec![&challenge[..10]], None),
            ("twice", vec![&challenge, &challenge], None),
            ("empty", vec![""], None),
        ];
        for (name, values, want) in cases {
            assert_eq!(m.answer(&headers(&values)), want, "{name}");
            assert_eq!(is_probe(&headers(&values)), !values.is_empty(), "{name}");
        }
    }

    #[test]
    fn classify_table() {
        let m = marker();
        let answered = |status, proof: Option<&str>| ProbeReply::Answered {
            status,
            proof: proof.map(str::to_owned),
        };
        let cases = [
            ("ours", answered(204, Some(m.reply())), ProbeResult::Ours),
            (
                "right proof, wrong status",
                answered(200, Some(m.reply())),
                ProbeResult::Foreign,
            ),
            (
                "echoed challenge",
                answered(204, Some(m.challenge())),
                ProbeResult::Foreign,
            ),
            ("no proof", answered(204, None), ProbeResult::Foreign),
            (
                "not a secret",
                answered(204, Some("ok")),
                ProbeResult::Foreign,
            ),
            ("a 404 page", answered(404, None), ProbeResult::Foreign),
            (
                "refused",
                ProbeReply::Failed {
                    kind: io::ErrorKind::ConnectionRefused,
                    message: "refused".into(),
                },
                ProbeResult::Unreachable("refused".into()),
            ),
        ];
        for (name, reply, want) in cases {
            assert_eq!(classify(&reply, &m), want, "{name}");
        }
    }

    #[test]
    fn markers_are_fresh_and_hide_their_secrets() {
        let a = ProbeMarker::generate().unwrap();
        let b = ProbeMarker::generate().unwrap();
        assert!(is_secret(a.challenge()) && is_secret(a.reply()));
        assert_ne!(a.challenge(), a.reply());
        assert_ne!(a, b);
        assert_eq!(format!("{a:?}"), "ProbeMarker(..)");
    }

    #[tokio::test]
    async fn fake_probe_answers_by_port() {
        let probe = FakeProbe::new(FakeAnswer::Foreign(404));
        probe.set(8100, FakeAnswer::Ours);
        probe.set(8101, FakeAnswer::Fail(io::ErrorKind::ConnectionRefused));
        let m = marker();
        let at = |port| SocketAddr::from(([127, 0, 0, 1], port));
        let got = [
            classify(&probe.probe(at(8100), m.clone()).await, &m),
            classify(&probe.probe(at(80), m.clone()).await, &m),
        ];
        assert_eq!(got, [ProbeResult::Ours, ProbeResult::Foreign]);
        assert!(matches!(
            probe.probe(at(8101), m.clone()).await,
            ProbeReply::Failed {
                kind: io::ErrorKind::ConnectionRefused,
                ..
            }
        ));
        assert_eq!(probe.asked(), [at(8100), at(80), at(8101)]);
    }
}
