//! Announcing app names such as `immich.local` to the LAN over multicast
//! DNS.
//!
//! Which names to announce, and where, is decided by
//! [`alchemy_core::mdns`]. Publishing them is a side effect behind
//! [`Announcer`], with one backend per kind of host, chosen at startup by
//! [`system`]:
//!
//! - macOS: mDNSResponder, through one supervised `dns-sd` child per name
//!   ([`dnssd`]).
//! - Linux with avahi: avahi's D-Bus API ([`avahi`]).
//! - Linux without avahi: a responder of our own, `mdns-sd`
//!   ([`builtin`]).
//!
//! Each backend reports what became of every name it publishes, a
//! conflict with another device included, as [`NameStates`] on a watch
//! channel. Tests use [`FakeAnnouncer`].

pub mod avahi;
pub mod builtin;
pub mod dnssd;
mod fake;

use std::collections::BTreeMap;
use std::fmt;
use std::net::Ipv4Addr;
use std::sync::Arc;

use alchemy_core::mdns::Announcement;
use alchemy_core::route::HostLabel;
use futures_util::future::BoxFuture;
use tokio::sync::watch;

pub use fake::{FakeAnnouncer, FakeCall};

/// The DNS-SD service type the `dns-sd` and built-in backends carry with
/// each name, since neither can publish an A record alone.
pub const SERVICE_TYPE: &str = "_http._tcp";

/// Which responder publishes the names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// mDNSResponder on macOS, through `dns-sd`.
    DnsSd,
    /// avahi-daemon on Linux, over D-Bus.
    Avahi,
    /// Alchemy's own responder, `mdns-sd`.
    Builtin,
}

impl Backend {
    /// A short name for logs and reports: `dns-sd`, `avahi`, or
    /// `built-in`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DnsSd => "dns-sd",
            Self::Avahi => "avahi",
            Self::Builtin => "built-in",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What became of one published name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameState {
    /// Handed to the responder, which is probing for it or starting.
    Registering,
    /// Probed and answered for.
    Established,
    /// Another device answers for the name. The backend doesn't retry it
    /// until it is announced again with another address or port, or
    /// withdrawn and announced again.
    Conflict,
    /// The OS denied access to the local network, such as macOS Local
    /// Network privacy. The backend keeps retrying.
    Denied,
    /// The responder failed for this name, with why. The backend keeps
    /// retrying.
    Failed(String),
}

/// The state of every name an [`Announcer`] publishes, by label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameStates(BTreeMap<HostLabel, NameState>);

impl NameStates {
    /// The state of `label`, if it is published.
    pub fn get(&self, label: &HostLabel) -> Option<&NameState> {
        self.0.get(label)
    }

    /// Every published name and its state, in label order.
    pub fn iter(&self) -> impl Iterator<Item = (&HostLabel, &NameState)> {
        self.0.iter()
    }

    /// The labels another device answers for, in label order: what
    /// [`alchemy_core::mdns::conflict_problems`] takes.
    pub fn conflicts(&self) -> impl Iterator<Item = &HostLabel> {
        self.0
            .iter()
            .filter(|(_, state)| **state == NameState::Conflict)
            .map(|(label, _)| label)
    }

    /// How many names are published.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no name is published.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(HostLabel, NameState)> for NameStates {
    fn from_iter<I: IntoIterator<Item = (HostLabel, NameState)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Why an [`Announcer`] call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AnnounceError {
    /// [`Announcer::shutdown`] already ran.
    #[error("the mDNS announcer has shut down")]
    Closed,
    /// The responder could not be reached or refused the request.
    #[error("{backend} failed: {message}")]
    Responder {
        /// The backend.
        backend: Backend,
        /// What went wrong.
        message: String,
    },
}

/// Publishes names on the LAN and withdraws them.
///
/// Calls for one label are meant to come from one task, in order, as
/// [`alchemy_core::mdns::diff`] lists them. What becomes of each name
/// after a call returns, such as a conflict found while probing or a
/// responder restart, arrives on [`states`](Self::states).
///
/// Object safe, so the daemon can hold any implementation as a
/// [`SharedAnnouncer`].
pub trait Announcer: Send + Sync + fmt::Debug {
    /// Which responder this is.
    fn backend(&self) -> Backend;

    /// Publish `announcement`'s name with its address, on its interface
    /// only, and, where the backend carries a service, an
    /// [`SERVICE_TYPE`] service on `port`.
    ///
    /// Announcing a label again with the same interface, address, and
    /// port does nothing, a conflict included. With anything else it
    /// replaces the old record, which also clears a conflict.
    fn announce(
        &self,
        announcement: Announcement,
        port: u16,
    ) -> BoxFuture<'_, Result<(), AnnounceError>>;

    /// Stop publishing `label`, with a goodbye where the responder sends
    /// one. A label that isn't published is not an error.
    fn withdraw(&self, label: HostLabel) -> BoxFuture<'_, Result<(), AnnounceError>>;

    /// Withdraw every name and stop. Later calls fail with
    /// [`AnnounceError::Closed`].
    fn shutdown(&self) -> BoxFuture<'_, ()>;

    /// The state of every published name, updated as the responder
    /// reports.
    fn states(&self) -> watch::Receiver<NameStates>;
}

/// An [`Announcer`] shared by the daemon's tasks.
pub type SharedAnnouncer = Arc<dyn Announcer>;

/// The announcer for this host: `dns-sd` on macOS; on Linux avahi when
/// `org.freedesktop.Avahi` is on the system bus, else the built-in
/// responder.
#[cfg(target_os = "macos")]
#[allow(clippy::unused_async)]
pub async fn system() -> Result<SharedAnnouncer, AnnounceError> {
    Ok(Arc::new(dnssd::DnsSdAnnouncer::new(dnssd::DNS_SD)))
}

/// The announcer for this host: `dns-sd` on macOS; on Linux avahi when
/// `org.freedesktop.Avahi` is on the system bus, else the built-in
/// responder.
#[cfg(target_os = "linux")]
pub async fn system() -> Result<SharedAnnouncer, AnnounceError> {
    match avahi::AvahiAnnouncer::connect().await {
        Ok(Some(avahi)) => return Ok(Arc::new(avahi)),
        Ok(None) => tracing::debug!("avahi is not running, using the built-in mDNS responder"),
        Err(error) => {
            tracing::debug!(%error, "cannot reach avahi, using the built-in mDNS responder");
        }
    }
    Ok(Arc::new(builtin::BuiltinAnnouncer::new()?))
}

/// The published names and their states, shared by a backend's tasks.
#[derive(Debug, Clone)]
pub(crate) struct Board {
    tx: Arc<watch::Sender<NameStates>>,
}

impl Board {
    pub(crate) fn new() -> Self {
        Self {
            tx: Arc::new(watch::Sender::new(NameStates::default())),
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<NameStates> {
        self.tx.subscribe()
    }

    pub(crate) fn set(&self, label: &HostLabel, state: NameState) {
        self.tx.send_if_modified(|states| {
            if states.0.get(label) == Some(&state) {
                return false;
            }
            states.0.insert(label.clone(), state);
            true
        });
    }

    pub(crate) fn remove(&self, label: &HostLabel) {
        self.tx
            .send_if_modified(|states| states.0.remove(label).is_some());
    }

    pub(crate) fn clear(&self) {
        self.tx.send_if_modified(|states| {
            let had = !states.0.is_empty();
            states.0.clear();
            had
        });
    }
}

/// What a backend publishes for one label, compared to tell a repeat
/// from a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The label, such as `immich`.
    pub label: HostLabel,
    /// The name, such as `immich.local`.
    pub name: String,
    /// The interface to publish on, such as `en0`.
    pub interface: String,
    /// The address the name answers with.
    pub addr: Ipv4Addr,
    /// The port of the service the `dns-sd` and built-in backends carry.
    pub port: u16,
}

impl Record {
    /// The record for `announcement` with a service on `port`.
    pub fn new(announcement: &Announcement, port: u16) -> Self {
        Self {
            label: announcement.label.clone(),
            name: announcement.name(),
            interface: announcement.lan.interface().to_owned(),
            addr: announcement.lan.addr(),
            port,
        }
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

    fn label(text: &str) -> HostLabel {
        HostLabel::new(text).unwrap()
    }

    #[test]
    fn conflicts_lists_only_conflicting_labels_in_order() {
        let states: NameStates = [
            (label("zed"), NameState::Conflict),
            (label("immich"), NameState::Established),
            (label("alpha"), NameState::Conflict),
            (label("beta"), NameState::Failed("boom".to_owned())),
        ]
        .into_iter()
        .collect();
        let found: Vec<&str> = states.conflicts().map(HostLabel::as_str).collect();
        assert_eq!(found, ["alpha", "zed"]);
    }

    #[test]
    fn board_notifies_only_on_change() {
        let board = Board::new();
        let mut rx = board.subscribe();
        board.set(&label("immich"), NameState::Registering);
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        board.set(&label("immich"), NameState::Registering);
        assert!(!rx.has_changed().unwrap());
        board.remove(&label("other"));
        assert!(!rx.has_changed().unwrap());
        board.set(&label("immich"), NameState::Established);
        assert!(rx.has_changed().unwrap());
        assert_eq!(
            rx.borrow_and_update().get(&label("immich")),
            Some(&NameState::Established)
        );
        board.clear();
        assert!(rx.borrow().is_empty());
    }

    #[test]
    fn backend_names() {
        for (backend, name) in [
            (Backend::DnsSd, "dns-sd"),
            (Backend::Avahi, "avahi"),
            (Backend::Builtin, "built-in"),
        ] {
            assert_eq!(backend.to_string(), name);
        }
    }
}
