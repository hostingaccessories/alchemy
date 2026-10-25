//! When the engine follower checks a Docker connection that went quiet.
//!
//! Docker's `/events` stream sends nothing while nothing happens, and a
//! stream whose other end vanished during sleep can hang without an error
//! or an end. So a follower that has heard nothing for a while pings the
//! engine, on its own connection, and a ping that fails or doesn't answer
//! in time means the connection is gone.

use std::time::Duration;

/// How long a follower waits without hearing from the engine before it
/// pings. Long enough that an idle engine costs one small request every
/// so often; short enough that a hung stream is caught well within a
/// minute even without a wake to prompt it.
pub const IDLE_PING: Duration = Duration::from_secs(30);

/// How long a ping may take before the connection counts as gone. A
/// healthy local engine answers `/_ping` in milliseconds; Docker Desktop
/// still waking its VM can take a few seconds.
pub const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// What the follower does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LivenessCheck {
    /// Keep following; check again after this long if still quiet.
    Wait(Duration),
    /// Ping the engine, giving up after [`Liveness::ping_timeout`].
    Ping,
}

/// How a ping went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PingOutcome {
    /// The engine answered, after this long.
    Answered(Duration),
    /// The request failed, such as a refused or reset connection.
    Failed,
    /// No answer within the timeout.
    TimedOut,
}

/// The idle ping rule, with its timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Liveness {
    /// How long without an event before pinging.
    pub idle_ping: Duration,
    /// How long a ping may take.
    pub ping_timeout: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            idle_ping: IDLE_PING,
            ping_timeout: PING_TIMEOUT,
        }
    }
}

impl Liveness {
    /// What to do when the follower has heard nothing for `quiet_for`,
    /// counted from the last event or answered ping. A wake pings at
    /// once, since sleep is when streams hang.
    pub fn check(&self, quiet_for: Duration, woke: bool) -> LivenessCheck {
        match self.idle_ping.checked_sub(quiet_for) {
            Some(left) if !woke && !left.is_zero() => LivenessCheck::Wait(left),
            _ => LivenessCheck::Ping,
        }
    }

    /// Whether the connection is still alive after `ping`. When it isn't,
    /// the follower ends as `Disconnected` and reconnects with the usual
    /// backoff.
    pub fn alive(&self, ping: PingOutcome) -> bool {
        match ping {
            PingOutcome::Answered(after) => after <= self.ping_timeout,
            PingOutcome::Failed | PingOutcome::TimedOut => false,
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

    const fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn pings_when_quiet_or_after_a_wake() {
        let cases = [
            (secs(0), false, LivenessCheck::Wait(secs(30))),
            (secs(10), false, LivenessCheck::Wait(secs(20))),
            (
                Duration::from_millis(29_999),
                false,
                LivenessCheck::Wait(Duration::from_millis(1)),
            ),
            (secs(30), false, LivenessCheck::Ping),
            (secs(31), false, LivenessCheck::Ping),
            (Duration::MAX, false, LivenessCheck::Ping),
            (secs(0), true, LivenessCheck::Ping),
            (secs(10), true, LivenessCheck::Ping),
            (secs(31), true, LivenessCheck::Ping),
        ];
        let rule = Liveness::default();
        for (quiet, woke, want) in cases {
            assert_eq!(rule.check(quiet, woke), want, "{quiet:?} {woke}");
        }
    }

    #[test]
    fn a_failed_or_slow_ping_disconnects() {
        let cases = [
            (PingOutcome::Answered(Duration::from_millis(2)), true),
            (PingOutcome::Answered(secs(5)), true),
            (PingOutcome::Answered(Duration::from_millis(5_001)), false),
            (PingOutcome::Failed, false),
            (PingOutcome::TimedOut, false),
        ];
        let rule = Liveness::default();
        for (ping, want) in cases {
            assert_eq!(rule.alive(ping), want, "{ping:?}");
        }
    }

    #[test]
    fn timings_are_parameters() {
        let rule = Liveness {
            idle_ping: secs(5),
            ping_timeout: secs(1),
        };
        assert_eq!(rule.check(secs(2), false), LivenessCheck::Wait(secs(3)));
        assert_eq!(rule.check(secs(5), false), LivenessCheck::Ping);
        assert!(!rule.alive(PingOutcome::Answered(secs(2))));
    }
}
