//! Sleep and wake: the `woke` event.
//!
//! alchemyd notices that the computer slept when its wall clock moved
//! further than its monotonic clock between two samples a couple of
//! seconds apart. It then reads the LAN address again, announces `.local`
//! names again, and subscribes to Docker's events again, and `/api/events`
//! sends a [`Wake`] as the `woke` event.
//!
//! Unlike the other events, `woke` reports something that happened, not
//! a current value: it is sent once per wake, to the connections open at
//! the time, and never on connect. A Mac's maintenance wakes during a
//! night with the lid closed can each send one, so show it quietly.

use serde::{Deserialize, Serialize};

/// The computer woke from sleep: the `woke` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Wake {
    /// About how long it slept, in whole seconds.
    pub slept_for: u64,
    /// When alchemyd noticed, in Unix seconds.
    pub at: i64,
}
