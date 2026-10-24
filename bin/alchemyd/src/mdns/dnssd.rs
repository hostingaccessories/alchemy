//! The macOS backend: mDNSResponder through one supervised `dns-sd`
//! child per name.
//!
//! `dns-sd` can't publish an A record alone, so each child runs
//! `dns-sd -i <interface> -P <label> _http._tcp local <port> <label>.local <ip>`,
//! which publishes the A record and an `_http._tcp` service. Its
//! `Got a reply for record <name>: ...` line says whether the name is
//! registered or in use; the service line can say registered while the
//! record conflicts, so only the record line counts ([`parse_line`]).
//!
//! On a conflict, at registration or later, `dns-sd` prints the line and
//! exits with 255. The supervisor reports [`NameState::Conflict`] and
//! doesn't restart it. Any other exit is restarted with backoff
//! ([`restart_delay`]).
//!
//! Killing a child deregisters its name at once, and mDNSResponder sends
//! goodbyes for the service, but not for the A record (#100), so other
//! devices may keep a withdrawn name for up to its TTL of 240 s.
//!
//! The supervision is plain process handling, compiled on every OS, so
//! it is tested with a stand-in script; [`super::system`] picks it on
//! macOS only.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use alchemy_core::mdns::Announcement;
use alchemy_core::route::HostLabel;
use futures_util::future::BoxFuture;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;

use super::{
    AnnounceError, Announcer, Backend, Board, NameState, NameStates, Record, SERVICE_TYPE,
};

/// Where macOS keeps `dns-sd`.
pub const DNS_SD: &str = "/usr/bin/dns-sd";

/// mDNSResponder's `kDNSServiceErr_NameConflict`.
pub const NAME_CONFLICT: i64 = -65548;

/// mDNSResponder's `kDNSServiceErr_PolicyDenied`: no Local Network
/// access.
pub const POLICY_DENIED: i64 = -65570;

/// How long to wait before the first restart of a child that exited.
pub const RESTART_BASE: Duration = Duration::from_secs(1);

/// The longest wait between restarts.
pub const RESTART_MAX: Duration = Duration::from_secs(60);

/// What one line of `dns-sd` output says about the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// The name is registered and answered for.
    Registered,
    /// Another device has the name.
    Conflict,
    /// A call failed with this mDNSResponder error code.
    Error(i64),
}

/// What `line` of a child's output says about the record for `name`,
/// such as `immich.local`, ignoring the timestamp and any other line.
pub fn parse_line(line: &str, name: &str) -> Option<Line> {
    if let Some((_, rest)) = line.split_once("Got a reply for record ") {
        let (named, reply) = rest.split_once(": ")?;
        if !named
            .trim_end_matches('.')
            .eq_ignore_ascii_case(name.trim_end_matches('.'))
        {
            return None;
        }
        let reply = reply.trim();
        return if reply.starts_with("Name now registered and active") {
            Some(Line::Registered)
        } else if reply.starts_with("Name in use") {
            Some(Line::Conflict)
        } else {
            reply
                .strip_prefix("Error ")
                .and_then(leading_code)
                .map(Line::Error)
        };
    }
    let (_, after) = line.split_once(" failed ")?;
    leading_code(after.strip_prefix("with: ").unwrap_or(after)).map(Line::Error)
}

fn leading_code(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let end = text
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && c == '-')))
        .map_or(text.len(), |(i, _)| i);
    text.get(..end)?.parse().ok()
}

/// The state an mDNSResponder error `code` puts a name in.
pub fn error_state(code: i64) -> NameState {
    match code {
        NAME_CONFLICT => NameState::Conflict,
        POLICY_DENIED => NameState::Denied,
        _ => NameState::Failed(format!("mDNSResponder failed with error {code}")),
    }
}

/// The arguments for the child that publishes `record`.
pub fn arguments(record: &Record) -> Vec<String> {
    vec![
        "-i".to_owned(),
        record.interface.clone(),
        "-P".to_owned(),
        record.label.as_str().to_owned(),
        SERVICE_TYPE.to_owned(),
        "local".to_owned(),
        record.port.to_string(),
        record.name.clone(),
        record.addr.to_string(),
    ]
}

/// How long to wait before restarting a child after `failures` exits in
/// a row: `base`, doubling each time, at most `max`.
pub fn restart_delay(failures: u32, base: Duration, max: Duration) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    base.saturating_mul(1 << doublings).min(max)
}

/// Publishes names through `dns-sd` children. See the [module
/// docs](self).
#[derive(Debug)]
pub struct DnsSdAnnouncer {
    binary: PathBuf,
    backoff: (Duration, Duration),
    board: Board,
    children: Mutex<Option<BTreeMap<HostLabel, Supervised>>>,
}

#[derive(Debug)]
struct Supervised {
    record: Record,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Supervised {
    async fn stop(self) {
        let _ = self.stop.send(());
        if let Err(error) = self.task.await {
            tracing::warn!(%error, label = %self.record.label, "dns-sd supervisor failed");
        }
    }
}

impl DnsSdAnnouncer {
    /// An announcer that runs `binary` as `dns-sd`. Nothing runs until a
    /// name is announced.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            backoff: (RESTART_BASE, RESTART_MAX),
            board: Board::new(),
            children: Mutex::new(Some(BTreeMap::new())),
        }
    }

    /// Restart exited children after `base`, doubling up to `max`,
    /// instead of [`RESTART_BASE`] and [`RESTART_MAX`].
    #[must_use]
    pub fn with_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.backoff = (base, max);
        self
    }

    fn supervise(&self, record: Record) -> Supervised {
        let (stop, stopped) = oneshot::channel();
        let child = Child {
            binary: self.binary.clone(),
            record: record.clone(),
            board: self.board.clone(),
            backoff: self.backoff,
        };
        Supervised {
            record,
            stop,
            task: tokio::spawn(child.supervise(stopped)),
        }
    }
}

impl Announcer for DnsSdAnnouncer {
    fn backend(&self) -> Backend {
        Backend::DnsSd
    }

    fn announce(
        &self,
        announcement: Announcement,
        port: u16,
    ) -> BoxFuture<'_, Result<(), AnnounceError>> {
        Box::pin(async move {
            let record = Record::new(&announcement, port);
            let mut children = self.children.lock().await;
            let children = children.as_mut().ok_or(AnnounceError::Closed)?;
            if children
                .get(&record.label)
                .is_some_and(|s| s.record == record)
            {
                return Ok(());
            }
            if let Some(old) = children.remove(&record.label) {
                old.stop().await;
            }
            self.board.set(&record.label, NameState::Registering);
            children.insert(record.label.clone(), self.supervise(record));
            Ok(())
        })
    }

    fn withdraw(&self, label: HostLabel) -> BoxFuture<'_, Result<(), AnnounceError>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let children = children.as_mut().ok_or(AnnounceError::Closed)?;
            if let Some(old) = children.remove(&label) {
                old.stop().await;
            }
            self.board.remove(&label);
            Ok(())
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let children = self.children.lock().await.take().unwrap_or_default();
            for (_, child) in children {
                child.stop().await;
            }
            self.board.clear();
        })
    }

    fn states(&self) -> watch::Receiver<NameStates> {
        self.board.subscribe()
    }
}

/// How one run of a child ended.
enum Ended {
    /// It was told to stop and was killed.
    Stopped,
    /// It reported a conflict.
    Conflict,
    /// It exited or could not start, with the state to report.
    Exited(NameState),
}

struct Child {
    binary: PathBuf,
    record: Record,
    board: Board,
    backoff: (Duration, Duration),
}

impl Child {
    async fn supervise(self, mut stopped: oneshot::Receiver<()>) {
        let mut failures = 0;
        loop {
            match self.run(&mut stopped, &mut failures).await {
                Ended::Stopped => return,
                Ended::Conflict => {
                    self.board.set(&self.record.label, NameState::Conflict);
                    drop(stopped.await);
                    return;
                }
                Ended::Exited(state) => {
                    failures += 1;
                    let delay = restart_delay(failures, self.backoff.0, self.backoff.1);
                    tracing::debug!(label = %self.record.label, ?state, ?delay, "dns-sd exited, restarting");
                    self.board.set(&self.record.label, state);
                    tokio::select! {
                        _ = &mut stopped => return,
                        () = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    async fn run(&self, stopped: &mut oneshot::Receiver<()>, failures: &mut u32) -> Ended {
        let spawned = Command::new(&self.binary)
            .args(arguments(&self.record))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return Ended::Exited(NameState::Failed(format!(
                    "cannot run {}: {error}",
                    self.binary.display()
                )));
            }
        };
        let mut out = child.stdout.take().map(|o| BufReader::new(o).lines());
        let mut err = child.stderr.take().map(|e| BufReader::new(e).lines());
        let mut last = None;
        loop {
            tokio::select! {
                _ = &mut *stopped => {
                    drop(child.kill().await);
                    return Ended::Stopped;
                }
                line = next_line(&mut out) => self.saw(line, &mut out, &mut last, failures),
                line = next_line(&mut err) => self.saw(line, &mut err, &mut last, failures),
                status = child.wait(), if out.is_none() && err.is_none() => {
                    return match (last, status) {
                        (Some(NameState::Conflict), _) => Ended::Conflict,
                        (Some(state), _) => Ended::Exited(state),
                        (None, Ok(status)) => Ended::Exited(NameState::Failed(format!("dns-sd exited with {status}"))),
                        (None, Err(error)) => Ended::Exited(NameState::Failed(format!("dns-sd failed: {error}"))),
                    };
                }
            }
        }
    }

    fn saw<R>(
        &self,
        line: Option<String>,
        stream: &mut Option<R>,
        last: &mut Option<NameState>,
        failures: &mut u32,
    ) {
        let Some(line) = line else {
            *stream = None;
            return;
        };
        let state = match parse_line(&line, &self.record.name) {
            Some(Line::Registered) => {
                *failures = 0;
                *last = None;
                NameState::Established
            }
            Some(Line::Conflict) => NameState::Conflict,
            Some(Line::Error(code)) => error_state(code),
            None => return,
        };
        tracing::debug!(label = %self.record.label, ?state, "dns-sd");
        if state != NameState::Established {
            *last = Some(state.clone());
        }
        if state != NameState::Conflict {
            self.board.set(&self.record.label, state);
        }
    }
}

/// The next line of `lines`, or `None` at its end or on a read error.
/// Pending forever once `lines` is gone, so a `select!` skips it.
async fn next_line<R>(lines: &mut Option<tokio::io::Lines<R>>) -> Option<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    match lines {
        Some(lines) => lines.next_line().await.ok().flatten(),
        None => std::future::pending().await,
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
    use super::*;

    #[test]
    fn parses_record_lines_for_our_name_only() {
        let cases: &[(&str, Option<Line>)] = &[
            (
                "13:22:37.110  Got a reply for record immich.local: Name now registered and active",
                Some(Line::Registered),
            ),
            (
                "13:23:02.856  Got a reply for record immich.local: Name in use, please choose another",
                Some(Line::Conflict),
            ),
            (
                "13:23:02.856  Got a reply for record IMMICH.local.: Name now registered and active",
                Some(Line::Registered),
            ),
            (
                "13:23:02.856  Got a reply for record immich.local: Error -65570",
                Some(Line::Error(-65570)),
            ),
            (
                "13:22:37.110  Got a reply for record other.local: Name in use, please choose another",
                None,
            ),
            (
                "13:22:37.110  Got a reply for service immich._http._tcp.local.: Name in use, please choose another",
                None,
            ),
            (
                "13:22:37.110  Got a reply for service immich._http._tcp.local.: Name now registered and active",
                None,
            ),
            (
                "DNSService call failed -65563 (Service Not Running)",
                Some(Line::Error(-65563)),
            ),
            (
                "DNSServiceRegister() failed with: -65570",
                Some(Line::Error(-65570)),
            ),
            (
                "SharedConnection DNSServiceRegister failed -65540",
                Some(Line::Error(-65540)),
            ),
            ("Using interface 12", None),
            (
                "Registering Service immich._http._tcp.local host immich.local port 80",
                None,
            ),
            ("DATE: ---Sat 24 Oct 2026---", None),
            ("13:22:36.358  ...STARTING...", None),
            ("", None),
            ("something failed badly", None),
        ];
        for (line, want) in cases {
            assert_eq!(parse_line(line, "immich.local"), *want, "{line}");
        }
    }

    #[test]
    fn error_codes_map_to_states() {
        assert_eq!(error_state(NAME_CONFLICT), NameState::Conflict);
        assert_eq!(error_state(POLICY_DENIED), NameState::Denied);
        assert_eq!(
            error_state(-65563),
            NameState::Failed("mDNSResponder failed with error -65563".to_owned())
        );
    }

    #[test]
    fn builds_the_child_arguments() {
        let record = Record {
            label: HostLabel::new("immich").unwrap(),
            name: "immich.local".to_owned(),
            interface: "en0".to_owned(),
            addr: "10.0.1.50".parse().unwrap(),
            port: 8101,
        };
        assert_eq!(
            arguments(&record),
            [
                "-i",
                "en0",
                "-P",
                "immich",
                "_http._tcp",
                "local",
                "8101",
                "immich.local",
                "10.0.1.50"
            ]
        );
    }

    #[test]
    fn restart_delay_doubles_up_to_the_cap() {
        let s = Duration::from_secs;
        let cases = [
            (0, s(1)),
            (1, s(1)),
            (2, s(2)),
            (3, s(4)),
            (6, s(32)),
            (7, s(60)),
            (u32::MAX, s(60)),
        ];
        for (failures, want) in cases {
            assert_eq!(restart_delay(failures, s(1), s(60)), want, "{failures}");
        }
    }
}
