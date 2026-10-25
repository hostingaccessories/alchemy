//! Noticing that the computer slept, from a gap between its clocks.
//!
//! [`run`] samples the monotonic and wall clocks through a [`Clock`] every
//! [`WAKE_TICK`] and hands each sample to
//! [`WakeDetector`](alchemy_core::lifecycle::WakeDetector), which decides.
//! A wake goes to [`State::record_wake`], where the tasks that recover after
//! sleep subscribe with [`State::watch_wakes`]:
//!
//! - the [`reconcile`](crate::reconcile) task brings back the apps that
//!   were running,
//! - routing reads the LAN address again at once and announces `.local`
//!   names again,
//! - the engine monitor subscribes to Docker's events again,
//! - and `/api/events` sends the `woke` event.
//!
//! Per #12 there are no OS power listeners: this is the whole mechanism.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alchemy_api::Wake;
use alchemy_core::lifecycle::{ClockSample, WAKE_TICK, WakeDetector, Woke};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::State;

/// Reads both clocks, one right after the other.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The monotonic clock, which stops while the computer sleeps, and
    /// the wall clock, which doesn't.
    fn sample(&self) -> ClockSample;
}

/// A [`Clock`] shared by the daemon's tasks.
pub type SharedClock = Arc<dyn Clock>;

/// This machine's clocks: [`Instant`] since the clock was made, and
/// [`SystemTime`].
///
/// `Instant` is `CLOCK_MONOTONIC` on Linux and `CLOCK_UPTIME_RAW` on
/// macOS, and #112 confirmed both stop during suspend. macOS's own
/// `CLOCK_MONOTONIC` counts sleep, so it must never be used here.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock whose monotonic time starts now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn sample(&self) -> ClockSample {
        let monotonic = self.origin.elapsed();
        let wall_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_millis()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
        };
        ClockSample::new(monotonic, wall_ms)
    }
}

/// A [`Clock`] for tests that moves only when told to. Clones share it.
#[derive(Debug, Clone)]
pub struct FakeClock {
    now: Arc<Mutex<ClockSample>>,
}

impl FakeClock {
    /// A clock at monotonic zero and the wall clock at `wall_ms`.
    pub fn new(wall_ms: i64) -> Self {
        Self {
            now: Arc::new(Mutex::new(ClockSample::new(Duration::ZERO, wall_ms))),
        }
    }

    /// Move both clocks on by `by`, as time passing while awake does.
    pub fn advance(&self, by: Duration) {
        self.update(|now| {
            now.monotonic = now.monotonic.saturating_add(by);
            now.wall_ms = now.wall_ms.saturating_add(millis(by));
        });
    }

    /// Move only the wall clock on by `by`, as sleeping that long does.
    pub fn sleep(&self, by: Duration) {
        self.update(|now| now.wall_ms = now.wall_ms.saturating_add(millis(by)));
    }

    fn update(&self, change: impl FnOnce(&mut ClockSample)) {
        change(&mut self.now.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

impl Clock for FakeClock {
    fn sample(&self) -> ClockSample {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// What [`run`] samples, and how often.
#[derive(Debug, Clone)]
pub struct WakeDeps {
    /// Where the clocks are read.
    pub clock: SharedClock,
    /// How long between samples.
    pub tick: Duration,
}

impl WakeDeps {
    /// Sample `clock` every `tick`.
    pub fn new(clock: SharedClock, tick: Duration) -> Self {
        Self { clock, tick }
    }

    /// This machine's clocks, every [`WAKE_TICK`].
    pub fn system() -> Self {
        Self::new(Arc::new(SystemClock::new()), WAKE_TICK)
    }
}

/// Sample the clocks every `deps.tick` and record each wake in
/// [`State::record_wake`], until `shutdown` flips to `true` or its sender is
/// dropped. The first sample is only a baseline.
pub async fn run(deps: WakeDeps, state: State, mut shutdown: watch::Receiver<bool>) {
    let mut detector = WakeDetector::default();
    let mut tick = tokio::time::interval(deps.tick);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => return,
            _ = tick.tick() => {}
        }
        if let Some(woke) = detector.observe(deps.clock.sample()) {
            tracing::info!(
                slept_for = ?woke.slept_for,
                "the computer woke from sleep; checking the LAN and Docker again"
            );
            state.record_wake(woke);
        }
    }
}

/// `woke` as the `woke` event sends it: whole seconds.
pub fn api_wake(woke: Woke) -> Wake {
    Wake {
        slept_for: woke.slept_for.as_secs(),
        at: woke.at_ms.div_euclid(1000),
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
    fn api_wake_rounds_down_to_seconds() {
        let cases = [
            (
                Duration::from_millis(10_999),
                1_700_000_000_999,
                (10, 1_700_000_000),
            ),
            (
                Duration::from_secs(3_600),
                1_700_000_001_000,
                (3_600, 1_700_000_001),
            ),
            (Duration::ZERO, -1, (0, -1)),
        ];
        for (slept_for, at_ms, (secs, at)) in cases {
            let wake = api_wake(Woke { slept_for, at_ms });
            assert_eq!(
                wake,
                Wake {
                    slept_for: secs,
                    at
                },
                "{slept_for:?} {at_ms}"
            );
        }
    }

    #[test]
    fn fake_clock_sleeps_on_the_wall_clock_only() {
        let clock = FakeClock::new(1_000);
        clock.advance(Duration::from_secs(2));
        assert_eq!(
            clock.sample(),
            ClockSample::new(Duration::from_secs(2), 3_000)
        );
        clock.sleep(Duration::from_secs(60));
        assert_eq!(
            clock.sample(),
            ClockSample::new(Duration::from_secs(2), 63_000)
        );
    }

    #[test]
    fn system_clock_moves_both_clocks_together() {
        let clock = SystemClock::new();
        let first = clock.sample();
        std::thread::sleep(Duration::from_millis(20));
        let second = clock.sample();
        let mut detector = WakeDetector::default();
        detector.observe(first);
        assert_eq!(detector.observe(second), None);
        assert!(second.monotonic > first.monotonic);
        assert!(second.wall_ms > first.wall_ms);
    }

    #[tokio::test(start_paused = true)]
    async fn reports_each_sleep_once() {
        let clock = FakeClock::new(1_700_000_000_000);
        let state = State::new();
        let mut woke = state.watch_last_wake();
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            WakeDeps::new(Arc::new(clock.clone()), WAKE_TICK),
            state.clone(),
            stop_rx,
        ));
        for _ in 0..3 {
            tokio::time::sleep(WAKE_TICK).await;
            clock.advance(WAKE_TICK);
        }
        assert!(!woke.has_changed().unwrap(), "no wake while awake");

        clock.sleep(Duration::from_secs(600));
        woke.changed().await.unwrap();
        let got = woke.borrow_and_update().unwrap();
        assert_eq!(got.slept_for, Duration::from_secs(600));
        for _ in 0..3 {
            tokio::time::sleep(WAKE_TICK).await;
            clock.advance(WAKE_TICK);
        }
        assert!(!woke.has_changed().unwrap(), "once per sleep");

        stop.send_replace(true);
        task.await.unwrap();
    }
}
