//! The Linux backend when no avahi-daemon runs: a responder of our own,
//! `mdns-sd`.
//!
//! `mdns-sd` publishes only services, so each name is an `_http._tcp`
//! service whose host is the name: A, SRV, TXT, and PTR records, on the
//! announcement's interface only. Withdrawing unregisters it, which
//! sends goodbyes for all four.
//!
//! On a conflict while probing, `mdns-sd` renames the host silently
//! (`immich.local.` to `immich-2.local.`) and reports a `NameChange` for
//! the A record. Names never change silently in Alchemy, so the backend
//! unregisters the service at once and reports [`RecordState::Conflict`](super::RecordState::Conflict).
//! A later conflict goes unreported, as with avahi (#100).
//!
//! Not used on macOS, where raw multicast from a launchd agent may need
//! Local Network permission that mDNSResponder's clients don't.
//!
//! The decisions are pure and compiled on every OS; the responder is
//! Linux only.

use alchemy_core::route::HostLabel;

/// The label of `host`, such as `immich` for `immich.local.`, when it is
/// a `.local` name with a valid label.
pub fn host_label(host: &str) -> Option<HostLabel> {
    let host = host.strip_suffix('.').unwrap_or(host);
    let (label, domain) = host.rsplit_once('.')?;
    if !domain.eq_ignore_ascii_case("local") {
        return None;
    }
    HostLabel::new(label).ok()
}

/// The label a `NameChange` for record type A of `original` is a
/// conflict on. Changes to other records, such as a renamed service
/// instance, aren't.
pub fn conflict_label(original: &str, is_a_record: bool) -> Option<HostLabel> {
    if !is_a_record {
        return None;
    }
    host_label(original)
}

/// The label an `Announce` event's `detail`, `<host>:<interface>`, says
/// is now answered for. A renamed host such as `immich-2.local.` gives
/// another label, which the backend doesn't publish.
pub fn announced_label(detail: &str) -> Option<HostLabel> {
    let (host, _interface) = detail.rsplit_once(':')?;
    host_label(host)
}

#[cfg(target_os = "linux")]
pub use linux::BuiltinAnnouncer;

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::fmt;
    use std::sync::{Arc, Weak};
    use std::time::Duration;

    use alchemy_core::mdns::Announcement;
    use alchemy_core::route::HostLabel;
    use futures_util::future::BoxFuture;
    use mdns_sd::{DaemonEvent, IfKind, RRType, ServiceDaemon, ServiceInfo};
    use tokio::sync::{Mutex, watch};
    use tokio::task::JoinHandle;

    use super::super::{
        AnnounceError, Announcer, Backend, Board, Record, RecordState, RecordStates, SERVICE_TYPE,
    };
    use super::{announced_label, conflict_label};

    /// How long to wait for `mdns-sd` to confirm an unregister or a
    /// shutdown, goodbyes sent.
    const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);

    /// Publishes names with `mdns-sd`. See the [module docs](super).
    #[derive(Debug)]
    pub struct BuiltinAnnouncer {
        inner: Arc<Inner>,
        monitor: JoinHandle<()>,
    }

    struct Inner {
        daemon: ServiceDaemon,
        board: Board,
        names: Mutex<Option<Names>>,
    }

    impl fmt::Debug for Inner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Inner")
                .field("board", &self.board)
                .field("names", &self.names)
                .finish_non_exhaustive()
        }
    }

    #[derive(Debug, Default)]
    struct Names {
        by_label: BTreeMap<HostLabel, Registered>,
        interfaces: BTreeSet<String>,
    }

    #[derive(Debug)]
    struct Registered {
        record: Record,
        fullname: String,
        conflicted: bool,
    }

    fn failed(error: &mdns_sd::Error) -> AnnounceError {
        AnnounceError::Responder {
            backend: Backend::Builtin,
            message: error.to_string(),
        }
    }

    impl BuiltinAnnouncer {
        /// Start the responder's thread, on no interface until a name is
        /// announced. Needs a Tokio runtime.
        pub fn new() -> Result<Self, AnnounceError> {
            let daemon = ServiceDaemon::new().map_err(|e| failed(&e))?;
            daemon
                .disable_interface(IfKind::All)
                .map_err(|e| failed(&e))?;
            let events = daemon.monitor().map_err(|e| failed(&e))?;
            let inner = Arc::new(Inner {
                daemon,
                board: Board::new(),
                names: Mutex::new(Some(Names::default())),
            });
            let monitor = tokio::spawn(monitor(Arc::downgrade(&inner), events));
            Ok(Self { inner, monitor })
        }
    }

    impl Drop for BuiltinAnnouncer {
        fn drop(&mut self) {
            self.monitor.abort();
            drop(self.inner.daemon.shutdown());
        }
    }

    async fn monitor(inner: Weak<Inner>, events: mdns_sd::Receiver<DaemonEvent>) {
        while let Ok(event) = events.recv_async().await {
            let Some(inner) = inner.upgrade() else { return };
            inner.saw(event).await;
        }
    }

    impl Inner {
        async fn saw(&self, event: DaemonEvent) {
            let mut names = self.names.lock().await;
            let Some(names) = names.as_mut() else { return };
            match event {
                DaemonEvent::NameChange(change) => {
                    let Some(label) = conflict_label(&change.original, change.rr_type == RRType::A)
                    else {
                        return;
                    };
                    let Some(registered) = names.by_label.get_mut(&label) else {
                        return;
                    };
                    if registered.conflicted {
                        return;
                    }
                    tracing::info!(%label, renamed = %change.new_name, "mDNS name in use by another device");
                    registered.conflicted = true;
                    self.unregister(&registered.fullname).await;
                    self.board.set(&label, RecordState::Conflict);
                }
                DaemonEvent::Announce(_, detail) => {
                    let Some(label) = announced_label(&detail) else {
                        return;
                    };
                    if names.by_label.get(&label).is_some_and(|r| !r.conflicted) {
                        self.board.set(&label, RecordState::Established);
                    }
                }
                DaemonEvent::Error(error) => {
                    tracing::debug!(%error, "mdns-sd");
                }
                _ => {}
            }
        }

        async fn unregister(&self, fullname: &str) {
            match self.daemon.unregister(fullname) {
                Ok(done) => {
                    if tokio::time::timeout(CONFIRM_TIMEOUT, done.recv_async())
                        .await
                        .is_err()
                    {
                        tracing::debug!(fullname, "mdns-sd did not confirm the unregister");
                    }
                }
                Err(error) => tracing::debug!(%error, fullname, "cannot unregister"),
            }
        }

        async fn withdraw(&self, registered: Registered) {
            if !registered.conflicted {
                self.unregister(&registered.fullname).await;
            }
        }

        fn register(&self, names: &mut Names, record: &Record) -> Result<String, AnnounceError> {
            if names.interfaces.insert(record.interface.clone()) {
                self.daemon
                    .enable_interface(IfKind::Name(record.interface.clone()))
                    .map_err(|e| failed(&e))?;
            }
            let mut info = ServiceInfo::new(
                &format!("{SERVICE_TYPE}.local."),
                record.label.as_str(),
                &format!("{}.", record.name),
                record.addr.to_string().as_str(),
                record.port,
                None::<HashMap<String, String>>,
            )
            .map_err(|e| failed(&e))?;
            info.set_interfaces(vec![IfKind::Name(record.interface.clone())]);
            let fullname = info.get_fullname().to_owned();
            self.daemon.register(info).map_err(|e| failed(&e))?;
            Ok(fullname)
        }
    }

    impl Announcer for BuiltinAnnouncer {
        fn backend(&self) -> Backend {
            Backend::Builtin
        }

        fn announce(
            &self,
            announcement: Announcement,
            port: u16,
        ) -> BoxFuture<'_, Result<(), AnnounceError>> {
            Box::pin(async move {
                let record = Record::new(&announcement, port);
                let inner = &self.inner;
                let mut names = inner.names.lock().await;
                let names = names.as_mut().ok_or(AnnounceError::Closed)?;
                if names
                    .by_label
                    .get(&record.label)
                    .is_some_and(|r| r.record == record)
                {
                    return Ok(());
                }
                if let Some(old) = names.by_label.remove(&record.label) {
                    inner.withdraw(old).await;
                }
                let fullname = inner.register(names, &record)?;
                inner.board.set(&record.label, RecordState::Registering);
                names.by_label.insert(
                    record.label.clone(),
                    Registered {
                        record,
                        fullname,
                        conflicted: false,
                    },
                );
                Ok(())
            })
        }

        fn withdraw(&self, label: HostLabel) -> BoxFuture<'_, Result<(), AnnounceError>> {
            Box::pin(async move {
                let inner = &self.inner;
                let mut names = inner.names.lock().await;
                let names = names.as_mut().ok_or(AnnounceError::Closed)?;
                if let Some(old) = names.by_label.remove(&label) {
                    inner.withdraw(old).await;
                }
                inner.board.remove(&label);
                Ok(())
            })
        }

        fn shutdown(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                let inner = &self.inner;
                let names = inner.names.lock().await.take().unwrap_or_default();
                for (_, registered) in names.by_label {
                    inner.withdraw(registered).await;
                }
                self.monitor.abort();
                match inner.daemon.shutdown() {
                    Ok(done) => {
                        drop(tokio::time::timeout(CONFIRM_TIMEOUT, done.recv_async()).await);
                    }
                    Err(error) => tracing::debug!(%error, "cannot shut mdns-sd down"),
                }
                inner.board.clear();
            })
        }

        fn states(&self) -> watch::Receiver<RecordStates> {
            self.inner.board.subscribe()
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

    fn label(text: &str) -> Option<HostLabel> {
        HostLabel::new(text).ok()
    }

    #[test]
    fn host_labels() {
        let cases = [
            ("immich.local.", label("immich")),
            ("immich.local", label("immich")),
            ("Immich.LOCAL.", label("immich")),
            ("immich-2.local.", label("immich-2")),
            ("immich.example.", None),
            ("local.", None),
            ("a.b.local.", None),
            ("", None),
        ];
        for (host, want) in cases {
            assert_eq!(host_label(host), want, "{host}");
        }
    }

    #[test]
    fn only_a_record_name_changes_are_conflicts() {
        assert_eq!(conflict_label("immich.local.", true), label("immich"));
        assert_eq!(conflict_label("immich._http._tcp.local.", true), None);
        assert_eq!(conflict_label("immich.local.", false), None);
    }

    #[test]
    fn announced_labels_come_from_the_host() {
        let cases = [
            ("immich.local.:eth0", label("immich")),
            ("immich-2.local.:eth0", label("immich-2")),
            ("immich.local.", None),
            ("garbage", None),
        ];
        for (detail, want) in cases {
            assert_eq!(announced_label(detail), want, "{detail}");
        }
    }
}
