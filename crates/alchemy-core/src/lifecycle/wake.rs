//! Wake detection from a gap between the wall clock and the monotonic
//! clock.
//!
//! Rust's `Instant` stops while the computer sleeps on both platforms
//! (`CLOCK_MONOTONIC` on Linux, `mach_absolute_time` on macOS), and the
//! wall clock doesn't. So between two samples taken a tick apart, the wall
//! clock moving further than the monotonic one means the computer slept
//! for about the difference. Per #12 this is the whole mechanism: no OS
//! power listeners, no FFI.

use std::time::Duration;

/// How much further the wall clock must move than the monotonic clock
/// between two samples to count as a wake.
///
/// It sits well above the noise and well below a real sleep. Scheduling
/// jitter delays when a sample is taken but moves both clocks together,
/// so it adds nothing. NTP slews the wall clock by at most 500 ppm, half
/// a millisecond per second of tick. What remains is a forward step of
/// the wall clock, such as NTP correcting after a wake or the user
/// setting the time; a step this large reads as a wake, which only costs
/// an extra reconcile. #112 measures the real gaps.
pub const WAKE_THRESHOLD: Duration = Duration::from_secs(5);

/// Both clocks, read one right after the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClockSample {
    /// The monotonic clock, from any fixed origin, such as the daemon's
    /// start.
    pub monotonic: Duration,
    /// The wall clock, in Unix milliseconds.
    pub wall_ms: i64,
}

impl ClockSample {
    /// A sample of `monotonic` and `wall_ms`.
    pub const fn new(monotonic: Duration, wall_ms: i64) -> Self {
        Self { monotonic, wall_ms }
    }
}

/// The computer woke from sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Woke {
    /// About how long it slept: how much further the wall clock moved
    /// than the monotonic clock.
    pub slept_for: Duration,
    /// The wall clock at the sample that noticed, in Unix milliseconds.
    pub at_ms: i64,
}

/// Turns a stream of [`ClockSample`]s into [`Woke`] events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WakeDetector {
    threshold: Duration,
    last: Option<ClockSample>,
}

impl Default for WakeDetector {
    fn default() -> Self {
        Self::new(WAKE_THRESHOLD)
    }
}

impl WakeDetector {
    /// A detector that reports gaps larger than `threshold`.
    pub const fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            last: None,
        }
    }

    /// Take the next sample, and say whether the computer slept since the
    /// previous one.
    ///
    /// The first sample only sets the baseline. A wall clock that moved
    /// backwards, or less than the threshold further than the monotonic
    /// clock, is not a wake. A monotonic clock that went backwards, which
    /// it shouldn't, only resets the baseline.
    pub fn observe(&mut self, sample: ClockSample) -> Option<Woke> {
        let last = self.last.replace(sample)?;
        let monotonic = sample.monotonic.checked_sub(last.monotonic)?;
        let wall = u64::try_from(sample.wall_ms.saturating_sub(last.wall_ms)).ok()?;
        let slept_for = Duration::from_millis(wall).checked_sub(monotonic)?;
        (slept_for > self.threshold).then_some(Woke {
            slept_for,
            at_ms: sample.wall_ms,
        })
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

    const T0: i64 = 1_760_000_000_000;

    fn sample(monotonic_ms: u64, wall_ms: i64) -> ClockSample {
        ClockSample::new(Duration::from_millis(monotonic_ms), T0 + wall_ms)
    }

    #[test]
    fn wakes_from_clock_gaps() {
        let cases: [(&str, ClockSample, Option<u64>); 11] = [
            ("steady tick", sample(1_000, 1_000), None),
            ("late tick", sample(30_000, 30_000), None),
            ("jitter", sample(1_000, 1_003), None),
            ("ntp slew ahead", sample(1_000, 1_001), None),
            ("ntp slew behind", sample(1_000, 999), None),
            ("at the threshold", sample(1_000, 6_000), None),
            ("just over", sample(1_000, 6_001), Some(5_001)),
            ("an hour asleep", sample(1_000, 3_601_000), Some(3_600_000)),
            ("clock stepped back", sample(1_000, -3_600_000), None),
            (
                "monotonic stood still",
                sample(0, 3_600_000),
                Some(3_600_000),
            ),
            ("no time at all", sample(0, 0), None),
        ];
        for (name, next, want) in cases {
            let mut detector = WakeDetector::default();
            assert_eq!(detector.observe(sample(0, 0)), None, "{name}: baseline");
            let want = want.map(|ms| Woke {
                slept_for: Duration::from_millis(ms),
                at_ms: next.wall_ms,
            });
            assert_eq!(detector.observe(next), want, "{name}");
        }
    }

    #[test]
    fn monotonic_going_back_only_resets() {
        let mut detector = WakeDetector::default();
        detector.observe(sample(10_000, 0));
        assert_eq!(detector.observe(sample(5_000, 3_600_000)), None);
        assert_eq!(detector.observe(sample(6_000, 3_601_000)), None);
    }

    #[test]
    fn first_sample_is_only_a_baseline() {
        let mut detector = WakeDetector::default();
        assert_eq!(detector.observe(sample(0, 999_999_999)), None);
    }

    #[test]
    fn reports_each_sleep_once() {
        let samples = [
            (sample(0, 0), None),
            (sample(1_000, 1_000), None),
            (sample(2_000, 602_000), Some(600_000)),
            (sample(3_000, 603_000), None),
            (sample(4_000, 0), None),
            (sample(5_000, 1_000), None),
            (sample(6_000, 62_000), Some(60_000)),
        ];
        let mut detector = WakeDetector::default();
        for (i, (next, want)) in samples.into_iter().enumerate() {
            let want = want.map(|ms| Woke {
                slept_for: Duration::from_millis(ms),
                at_ms: next.wall_ms,
            });
            assert_eq!(detector.observe(next), want, "sample {i}");
        }
    }

    #[test]
    fn threshold_is_a_parameter() {
        let cases = [(1_000, 2_500, true), (2_000, 2_500, false)];
        for (threshold_ms, wall_ms, woke) in cases {
            let mut detector = WakeDetector::new(Duration::from_millis(threshold_ms));
            detector.observe(sample(0, 0));
            assert_eq!(
                detector.observe(sample(500, wall_ms)).is_some(),
                woke,
                "{threshold_ms}"
            );
        }
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let mut detector = WakeDetector::default();
        detector.observe(ClockSample::new(Duration::ZERO, i64::MIN));
        let woke = detector.observe(ClockSample::new(Duration::ZERO, i64::MAX));
        assert_eq!(
            woke.map(|w| w.slept_for),
            Some(Duration::from_millis(i64::MAX.unsigned_abs()))
        );
        detector.observe(ClockSample::new(Duration::MAX, i64::MAX));
        assert_eq!(
            detector.observe(ClockSample::new(Duration::MAX, i64::MIN)),
            None
        );
    }
}
