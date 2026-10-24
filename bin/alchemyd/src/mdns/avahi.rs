//! The Linux backend when avahi-daemon runs: its D-Bus API on the system
//! bus.
//!
//! Each name gets an entry group (`EntryGroupNew`) holding one bare A
//! record, added with `AddAddress(interface, IPv4, NO_REVERSE, name, ip)`
//! on the announcement's interface only, then `Commit`ted. The group's
//! `StateChanged` signal says when it is established or collides
//! ([`group_state`]). `Reset` and a new `AddAddress` re-address it, and
//! `Free` withdraws it with a goodbye.
//!
//! Avahi reports a conflict only while it probes, when we are the
//! newcomer; later, it defends the name and the group stays established
//! (#100). When avahi-daemon restarts, its groups are gone and their
//! signals stop, so the backend watches `NameOwnerChanged` and creates
//! every group again.
//!
//! The decisions are pure and compiled on every OS; the D-Bus client is
//! Linux only.

use super::RecordState;

/// avahi's well-known name on the system bus.
pub const AVAHI_SERVICE: &str = "org.freedesktop.Avahi";

/// `AVAHI_PROTO_INET`: IPv4.
pub const PROTO_INET: i32 = 0;

/// `AVAHI_PUBLISH_NO_REVERSE`. Without it `AddAddress` fails for every
/// name, because the reverse record for the host's address already
/// belongs to its hostname (#100).
pub const PUBLISH_NO_REVERSE: u32 = 16;

/// The D-Bus error avahi answers `AddAddress` with when the name is
/// taken.
pub const COLLISION_ERROR: &str = "org.freedesktop.Avahi.CollisionError";

/// The state an entry group's `state` and `error`, from `GetState` or
/// `StateChanged`, put its name in.
///
/// States: 0 uncommitted, 1 registering, 2 established, 3 collision,
/// 4 failure.
pub fn group_state(state: i32, error: &str) -> RecordState {
    match state {
        0 | 1 => RecordState::Registering,
        2 => RecordState::Established,
        3 => RecordState::Conflict,
        4 => RecordState::Failed(if error.is_empty() {
            "avahi failed to publish the name".to_owned()
        } else {
            format!("avahi failed to publish the name: {error}")
        }),
        other => RecordState::Failed(format!("avahi reported an unknown state {other}")),
    }
}

/// Whether a D-Bus error named `name` means the name is taken.
pub fn is_collision(name: &str) -> bool {
    name == COLLISION_ERROR
}

#[cfg(target_os = "linux")]
pub use linux::AvahiAnnouncer;

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Weak};

    use alchemy_core::mdns::Announcement;
    use alchemy_core::route::HostLabel;
    use futures_util::StreamExt as _;
    use futures_util::future::BoxFuture;
    use tokio::sync::{Mutex, watch};
    use tokio::task::JoinHandle;
    use zbus::Connection;
    use zbus::fdo::DBusProxy;
    use zbus::names::BusName;

    use super::super::{
        AnnounceError, Announcer, Backend, Board, Record, RecordState, RecordStates,
    };
    use super::{AVAHI_SERVICE, PROTO_INET, PUBLISH_NO_REVERSE, group_state, is_collision};

    mod server {
        use zbus::proxy;
        use zbus::zvariant::OwnedObjectPath;

        #[proxy(
            interface = "org.freedesktop.Avahi.Server",
            default_service = "org.freedesktop.Avahi",
            default_path = "/"
        )]
        pub(super) trait Server {
            fn get_network_interface_index_by_name(&self, name: &str) -> zbus::Result<i32>;
            fn entry_group_new(&self) -> zbus::Result<OwnedObjectPath>;
        }
    }

    mod group {
        use zbus::proxy;

        #[proxy(
            interface = "org.freedesktop.Avahi.EntryGroup",
            default_service = "org.freedesktop.Avahi"
        )]
        pub(super) trait EntryGroup {
            fn add_address(
                &self,
                interface: i32,
                protocol: i32,
                flags: u32,
                name: &str,
                address: &str,
            ) -> zbus::Result<()>;
            fn commit(&self) -> zbus::Result<()>;
            fn reset(&self) -> zbus::Result<()>;
            fn free(&self) -> zbus::Result<()>;
            #[zbus(signal)]
            fn state_changed(&self, state: i32, error: String) -> zbus::Result<()>;
        }
    }

    use group::EntryGroupProxy;
    use server::ServerProxy;

    /// Publishes names through avahi-daemon. See the [module
    /// docs](super).
    #[derive(Debug)]
    pub struct AvahiAnnouncer {
        inner: Arc<Inner>,
        owner_watch: JoinHandle<()>,
    }

    #[derive(Debug)]
    struct Inner {
        conn: Connection,
        board: Board,
        groups: Mutex<Option<BTreeMap<HostLabel, Group>>>,
    }

    #[derive(Debug)]
    struct Group {
        record: Record,
        proxy: Option<EntryGroupProxy<'static>>,
        signals: Option<JoinHandle<()>>,
        committed: bool,
    }

    impl Group {
        fn stop_signals(&mut self) {
            if let Some(signals) = self.signals.take() {
                signals.abort();
            }
        }
    }

    fn failed(error: &zbus::Error) -> AnnounceError {
        AnnounceError::Responder {
            backend: Backend::Avahi,
            message: error.to_string(),
        }
    }

    impl AvahiAnnouncer {
        /// Connect to avahi on the system bus. `None` when the bus is up
        /// but avahi-daemon isn't on it; an error when there is no
        /// system bus.
        pub async fn connect() -> Result<Option<Self>, AnnounceError> {
            let conn = Connection::system().await.map_err(|e| failed(&e))?;
            let dbus = DBusProxy::new(&conn).await.map_err(|e| failed(&e))?;
            let name = BusName::try_from(AVAHI_SERVICE).map_err(|e| failed(&e.into()))?;
            if !dbus
                .name_has_owner(name)
                .await
                .map_err(|e| failed(&e.into()))?
            {
                return Ok(None);
            }
            let owners = dbus
                .receive_name_owner_changed_with_args(&[(0, AVAHI_SERVICE)])
                .await
                .map_err(|e| failed(&e))?;
            let inner = Arc::new(Inner {
                conn,
                board: Board::new(),
                groups: Mutex::new(Some(BTreeMap::new())),
            });
            let owner_watch = tokio::spawn(watch_owner(Arc::downgrade(&inner), owners));
            Ok(Some(Self { inner, owner_watch }))
        }
    }

    impl Drop for AvahiAnnouncer {
        fn drop(&mut self) {
            self.owner_watch.abort();
        }
    }

    async fn watch_owner(inner: Weak<Inner>, mut owners: zbus::fdo::NameOwnerChangedStream) {
        while let Some(change) = owners.next().await {
            let Some(inner) = inner.upgrade() else { return };
            let back = change
                .args()
                .is_ok_and(|args| args.new_owner().as_ref().is_some());
            inner.avahi_restarted(back).await;
        }
    }

    impl Inner {
        /// Publish every name again on a new avahi-daemon, or mark them
        /// failed while there is none.
        async fn avahi_restarted(&self, back: bool) {
            let mut groups = self.groups.lock().await;
            let Some(groups) = groups.as_mut() else {
                return;
            };
            tracing::info!(back, "avahi-daemon changed owner, publishing names again");
            for group in groups.values_mut() {
                group.stop_signals();
                group.proxy = None;
                group.committed = false;
                if back {
                    if let Err(error) = self.publish(group).await {
                        tracing::warn!(%error, label = %group.record.label, "cannot publish with avahi");
                        self.board
                            .set(&group.record.label, RecordState::Failed(error.to_string()));
                    }
                } else {
                    self.board.set(
                        &group.record.label,
                        RecordState::Failed("avahi-daemon stopped".to_owned()),
                    );
                }
            }
        }

        /// Create `group`'s entry group if it has none, else reset it,
        /// then add its address and commit.
        async fn publish(&self, group: &mut Group) -> zbus::Result<()> {
            let published = self.try_publish(group).await;
            group.committed = published.is_ok();
            published
        }

        async fn try_publish(&self, group: &mut Group) -> zbus::Result<()> {
            let record = &group.record;
            let label = record.label.clone();
            let server = ServerProxy::new(&self.conn).await?;
            let interface = server
                .get_network_interface_index_by_name(&record.interface)
                .await?;
            let proxy = if let Some(proxy) = group.proxy.clone() {
                proxy.reset().await?;
                proxy
            } else {
                let path = server.entry_group_new().await?;
                let proxy = EntryGroupProxy::builder(&self.conn)
                    .path(path)?
                    .build()
                    .await?;
                let mut changes = proxy.receive_state_changed().await?;
                let board = self.board.clone();
                let signal_label = label.clone();
                group.signals = Some(tokio::spawn(async move {
                    while let Some(change) = changes.next().await {
                        if let Ok(args) = change.args() {
                            tracing::debug!(label = %signal_label, state = args.state, error = %args.error, "avahi entry group");
                            board.set(&signal_label, group_state(args.state, &args.error));
                        }
                    }
                }));
                group.proxy = Some(proxy.clone());
                proxy
            };
            self.board.set(&label, RecordState::Registering);
            let added = proxy
                .add_address(
                    interface,
                    PROTO_INET,
                    PUBLISH_NO_REVERSE,
                    &record.name,
                    &record.addr.to_string(),
                )
                .await;
            match added {
                Ok(()) => proxy.commit().await,
                Err(zbus::Error::MethodError(name, _, _)) if is_collision(name.as_str()) => {
                    self.board.set(&label, RecordState::Conflict);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }

        async fn free(&self, mut group: Group) {
            group.stop_signals();
            if let Some(proxy) = group.proxy.take()
                && let Err(error) = proxy.free().await
            {
                tracing::debug!(%error, label = %group.record.label, "cannot free avahi entry group");
            }
        }
    }

    impl Announcer for AvahiAnnouncer {
        fn backend(&self) -> Backend {
            Backend::Avahi
        }

        fn announce(
            &self,
            announcement: Announcement,
            port: u16,
        ) -> BoxFuture<'_, Result<(), AnnounceError>> {
            Box::pin(async move {
                let record = Record::new(&announcement, port);
                let inner = &self.inner;
                let mut groups = inner.groups.lock().await;
                let groups = groups.as_mut().ok_or(AnnounceError::Closed)?;
                let same_address = |old: &Record| {
                    old.name == record.name
                        && old.interface == record.interface
                        && old.addr == record.addr
                };
                let group = match groups.get_mut(&record.label) {
                    Some(group) if group.committed && same_address(&group.record) => {
                        group.record = record;
                        return Ok(());
                    }
                    Some(group) => {
                        group.record = record;
                        group
                    }
                    None => groups.entry(record.label.clone()).or_insert(Group {
                        record,
                        proxy: None,
                        signals: None,
                        committed: false,
                    }),
                };
                inner.publish(group).await.map_err(|e| failed(&e))
            })
        }

        fn withdraw(&self, label: HostLabel) -> BoxFuture<'_, Result<(), AnnounceError>> {
            Box::pin(async move {
                let inner = &self.inner;
                let mut groups = inner.groups.lock().await;
                let groups = groups.as_mut().ok_or(AnnounceError::Closed)?;
                if let Some(group) = groups.remove(&label) {
                    inner.free(group).await;
                }
                inner.board.remove(&label);
                Ok(())
            })
        }

        fn shutdown(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                self.owner_watch.abort();
                let inner = &self.inner;
                let groups = inner.groups.lock().await.take().unwrap_or_default();
                for (_, group) in groups {
                    inner.free(group).await;
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

    #[test]
    fn group_states_map_to_name_states() {
        let cases = [
            (0, "", RecordState::Registering),
            (1, "org.freedesktop.Avahi.Success", RecordState::Registering),
            (2, "org.freedesktop.Avahi.Success", RecordState::Established),
            (
                3,
                "org.freedesktop.Avahi.CollisionError",
                RecordState::Conflict,
            ),
            (
                4,
                "org.freedesktop.Avahi.FailureError",
                RecordState::Failed(
                    "avahi failed to publish the name: org.freedesktop.Avahi.FailureError"
                        .to_owned(),
                ),
            ),
            (
                4,
                "",
                RecordState::Failed("avahi failed to publish the name".to_owned()),
            ),
            (
                9,
                "",
                RecordState::Failed("avahi reported an unknown state 9".to_owned()),
            ),
        ];
        for (state, error, want) in cases {
            assert_eq!(group_state(state, error), want, "{state} {error}");
        }
    }

    #[test]
    fn only_avahi_collision_error_is_a_collision() {
        assert!(is_collision("org.freedesktop.Avahi.CollisionError"));
        assert!(!is_collision("org.freedesktop.Avahi.FailureError"));
        assert!(!is_collision("org.freedesktop.DBus.Error.ServiceUnknown"));
    }
}
