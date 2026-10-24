//! [`FakeAnnouncer`]: an [`Announcer`] that publishes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use alchemy_core::mdns::Announcement;
use alchemy_core::route::HostLabel;
use futures_util::future::BoxFuture;
use tokio::sync::watch;

use super::{AnnounceError, Announcer, Backend, Board, Record, RecordState, RecordStates};

/// One call a [`FakeAnnouncer`] received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    /// [`Announcer::announce`].
    Announce(Record),
    /// [`Announcer::withdraw`].
    Withdraw(HostLabel),
    /// [`Announcer::shutdown`].
    Shutdown,
}

#[derive(Debug, Default)]
struct Inner {
    published: BTreeMap<HostLabel, Record>,
    calls: Vec<FakeCall>,
    taken: BTreeSet<HostLabel>,
    failing: Option<String>,
    closed: bool,
}

/// An [`Announcer`] for tests.
///
/// It records every call, and a name it publishes is
/// [`RecordState::Established`] at once, or [`RecordState::Conflict`] when
/// [`take`](Self::take) says another device has it. Clones share their
/// state, so a test can steer it while the daemon holds a clone.
#[derive(Debug, Clone)]
pub struct FakeAnnouncer {
    backend: Backend,
    inner: Arc<Mutex<Inner>>,
    board: Board,
}

impl Default for FakeAnnouncer {
    fn default() -> Self {
        Self::new(Backend::Builtin)
    }
}

impl FakeAnnouncer {
    /// A fake that says it is `backend`.
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            inner: Arc::default(),
            board: Board::new(),
        }
    }

    /// Act as if another device answers for `label`: a published name
    /// goes to [`RecordState::Conflict`] now, as a late conflict would,
    /// and an announced one does from then on.
    pub fn take(&self, label: &HostLabel) {
        let mut inner = self.lock();
        inner.taken.insert(label.clone());
        if inner.published.contains_key(label) {
            self.board.set(label, RecordState::Conflict);
        }
    }

    /// Let `label` be announced without a conflict again. A name already
    /// in conflict stays so until it is announced anew.
    pub fn release(&self, label: &HostLabel) {
        self.lock().taken.remove(label);
    }

    /// Make every later call fail with `message`, or succeed again with
    /// `None`.
    pub fn set_failing(&self, message: Option<&str>) {
        self.lock().failing = message.map(str::to_owned);
    }

    /// Put `label` in `state`, as a backend reporting on its own would.
    pub fn set_state(&self, label: &HostLabel, state: RecordState) {
        self.board.set(label, state);
    }

    /// What is published now, by label.
    pub fn published(&self) -> Vec<Record> {
        self.lock().published.values().cloned().collect()
    }

    /// Every call so far, oldest first.
    pub fn calls(&self) -> Vec<FakeCall> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check(&self, inner: &Inner) -> Result<(), AnnounceError> {
        if inner.closed {
            return Err(AnnounceError::Closed);
        }
        match &inner.failing {
            Some(message) => Err(AnnounceError::Responder {
                backend: self.backend,
                message: message.clone(),
            }),
            None => Ok(()),
        }
    }
}

impl Announcer for FakeAnnouncer {
    fn backend(&self) -> Backend {
        self.backend
    }

    fn announce(
        &self,
        announcement: Announcement,
        port: u16,
    ) -> BoxFuture<'_, Result<(), AnnounceError>> {
        let record = Record::new(&announcement, port);
        let result = (|| {
            let mut inner = self.lock();
            inner.calls.push(FakeCall::Announce(record.clone()));
            self.check(&inner)?;
            if inner.published.get(&record.label) == Some(&record) {
                return Ok(());
            }
            let state = if inner.taken.contains(&record.label) {
                RecordState::Conflict
            } else {
                RecordState::Established
            };
            self.board.set(&record.label, state);
            inner.published.insert(record.label.clone(), record);
            Ok(())
        })();
        Box::pin(async move { result })
    }

    fn withdraw(&self, label: HostLabel) -> BoxFuture<'_, Result<(), AnnounceError>> {
        let result = (|| {
            let mut inner = self.lock();
            inner.calls.push(FakeCall::Withdraw(label.clone()));
            self.check(&inner)?;
            inner.published.remove(&label);
            self.board.remove(&label);
            Ok(())
        })();
        Box::pin(async move { result })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        {
            let mut inner = self.lock();
            inner.calls.push(FakeCall::Shutdown);
            inner.published.clear();
            inner.closed = true;
        }
        self.board.clear();
        Box::pin(async {})
    }

    fn states(&self) -> watch::Receiver<RecordStates> {
        self.board.subscribe()
    }
}
