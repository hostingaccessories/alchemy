//! Lifecycle operations: actions run on apps in the background, with
//! progress clients can follow.
//!
//! [`Operations`] owns them. It allows one running operation per app,
//! keeps each one's [`Operation`](alchemy_core::operation::Operation) and
//! newest output lines in memory for an hour after it ends, stores every
//! operation in the [`Store`](crate::store::Store) when it starts and
//! again when it ends, and publishes the ones in memory as
//! [`State::operations`](crate::State::operations). A [`Follower`] reads one
//! operation's events from its first kept line on, so a client that
//! connects late still sees what happened.
//!
//! The plan comes from [`ComposePlan`](crate::compose::ComposePlan) and
//! runs through an [`OperationRunner`]: [`SystemRunner`] in the daemon,
//! [`FakeComposeRunner`](crate::compose::FakeComposeRunner) in tests. The
//! pure mappings between the runner's events and the operation model are
//! in [`convert`].

pub mod convert;
mod manager;
mod runner;

pub use manager::{
    CancelError, Follower, HISTORY_KEPT, MAX_FINISHED, Operations, RETENTION, StartError,
};
pub use runner::{OperationRunner, SystemRunner};
