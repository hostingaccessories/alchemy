//! When the engine follower checks a Docker connection that went quiet.
//!
//! Docker's `/events` stream sends nothing while nothing happens, and a
//! stream whose other end vanished during sleep can hang without an error
//! or an end. So a follower that has heard nothing for a while pings the
//! engine, on its own connection, and a ping that fails or doesn't answer
//! in time means the connection is gone.
//!
//! A ping that answers doesn't prove the old stream is still alive, and
//! sleep is when streams hang, so after a wake the follower subscribes
//! again whatever a ping would say. The idle ping catches the hang that
//! no wake explains.

use std::time::Duration;

/// How long a follower waits without hearing from the engine before it
/// pings, while a container is running. #112 recommends a minute: an
/// idle engine costs one small request a minute, and a hung stream is
/// caught within about a minute even without a wake to prompt it.
pub const IDLE_PING: Duration = Duration::from_secs(60);

/// The same while no container is running.
///
/// Docker Desktop's Resource Saver pauses its VM after 5 minutes with no
/// running containers, and #112 couldn't rule out that a ping wakes it.
/// Pinging less often than that lets the VM pause between pings either
/// way, and with nothing running a hung stream has little to miss.
pub const IDLE_PING_STOPPED: Duration = Duration::from_secs(10 * 60);

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
    /// How long without an event before pinging, while a container runs.
    pub idle_ping: Duration,
    /// How long without an event before pinging, while none runs.
    pub idle_ping_stopped: Duration,
    /// How long a ping may take.
    pub ping_timeout: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            idle_ping: IDLE_PING,
            idle_ping_stopped: IDLE_PING_STOPPED,
            ping_timeout: PING_TIMEOUT,
        }
    }
}

impl Liveness {
    /// What to do when the follower has heard nothing for `quiet_for`,
    /// counted from the last event or answered ping, with `running`
    /// saying whether any container is running.
    pub fn check(&self, quiet_for: Duration, running: bool) -> LivenessCheck {
        let idle = if running {
            self.idle_ping
        } else {
            self.idle_ping_stopped
        };
        match idle.checked_sub(quiet_for) {
            Some(left) if !left.is_zero() => LivenessCheck::Wait(left),
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
    fn pings_when_quiet() {
        let cases = [
            (secs(0), true, LivenessCheck::Wait(secs(60))),
            (secs(10), true, LivenessCheck::Wait(secs(50))),
            (
                Duration::from_millis(59_999),
                true,
                LivenessCheck::Wait(Duration::from_millis(1)),
            ),
            (secs(60), true, LivenessCheck::Ping),
            (secs(61), true, LivenessCheck::Ping),
            (Duration::MAX, true, LivenessCheck::Ping),
            (secs(0), false, LivenessCheck::Wait(secs(600))),
            (secs(60), false, LivenessCheck::Wait(secs(540))),
            (secs(599), false, LivenessCheck::Wait(secs(1))),
            (secs(600), false, LivenessCheck::Ping),
            (Duration::MAX, false, LivenessCheck::Ping),
        ];
        let rule = Liveness::default();
        for (quiet, running, want) in cases {
            assert_eq!(rule.check(quiet, running), want, "{quiet:?} {running}");
        }
    }

    #[test]
    fn pings_less_often_than_resource_saver_pauses_while_nothing_runs() {
        assert!(Liveness::default().idle_ping_stopped > secs(5 * 60));
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
            idle_ping_stopped: secs(7),
            ping_timeout: secs(1),
        };
        assert_eq!(rule.check(secs(2), true), LivenessCheck::Wait(secs(3)));
        assert_eq!(rule.check(secs(5), true), LivenessCheck::Ping);
        assert_eq!(rule.check(secs(5), false), LivenessCheck::Wait(secs(2)));
        assert!(!rule.alive(PingOutcome::Answered(secs(2))));
    }
}
