//! [`OperationHooks`]: what else happens when an operation starts and
//! ends.

use std::fmt;

use alchemy_core::operation::Operation;
use futures_util::future::BoxFuture;

/// Called by the operations manager around every operation, for work
/// that belongs to other parts of the daemon.
///
/// This is where telling a deliberate stop from a crash (#64) records that
/// Alchemy itself stopped or started an app's containers: `started` runs
/// before the first step, so the intent is stored before any container
/// changes, and `finished` runs once the operation has ended, after it is
/// stored. The manager waits for each call, so keep them short.
pub trait OperationHooks: Send + Sync + fmt::Debug {
    /// `operation` is about to run its first step.
    fn started<'a>(&'a self, operation: &'a Operation) -> BoxFuture<'a, ()>;

    /// `operation` has ended, one way or another.
    fn finished<'a>(&'a self, operation: &'a Operation) -> BoxFuture<'a, ()>;
}

/// [`OperationHooks`] that do nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl OperationHooks for NoHooks {
    fn started<'a>(&'a self, _operation: &'a Operation) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }

    fn finished<'a>(&'a self, _operation: &'a Operation) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }
}
