//! Converting between `Instant` and the display's "host time".
//!
//! Metal reports when each frame reached the screen as host time: an `f64`
//! number of seconds (`MTLDrawable.presentedTime`). Onset times scanner
//! pulses and key presses with `Instant`. To put both on one timeline we need
//! to convert between the two.
//!
//! On macOS that's simple, because they read the same clock:
//! - `Instant::now()` is `clock_gettime(CLOCK_UPTIME_RAW)` (Rust's std
//!   source), which the man page says equals `mach_absolute_time()`.
//! - `CACurrentMediaTime()` is `mach_absolute_time()` in seconds (QuartzCore
//!   `CABase.h`), and Metal's host time uses that same base.
//!
//! So the offset between an `Instant` and host seconds never changes and
//! there's no rate to fit: one pair of readings taken at the same moment
//! converts between them for as long as the program runs. (Measured on the
//! lab's M3: within one 42 ns clock tick, no drift over 10 s.)
//!
//! Don't use `SystemTime`, chrono or `CLOCK_MONOTONIC` for frame timing: on
//! macOS those count time asleep or get nudged by network time, and were off
//! by more than a day from this clock on the lab Mac.
//!
//! `Instant` has no public raw value (and its `Debug` output isn't a stable
//! format), so the pair is sampled as host time, `Instant::now()`, host time,
//! and the tightest of many tries is kept.

use std::time::{Duration, Instant};

/// Current host time in seconds: the clock `presentedTime` uses.
#[cfg(target_os = "macos")]
pub fn host_now() -> f64 {
    objc2_quartz_core::CACurrentMediaTime()
}

/// Elsewhere there's no presentation feedback to match, so host time only
/// has to be consistent with itself: seconds since the first call.
#[cfg(not(target_os = "macos"))]
pub fn host_now() -> f64 {
    use std::sync::OnceLock;
    static ZERO: OnceLock<Instant> = OnceLock::new();
    ZERO.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// One (`Instant`, host seconds) pair taken at the same moment.
#[derive(Debug, Clone, Copy)]
pub struct HostClock {
    instant: Instant,
    host_s: f64,
    uncertainty_s: f64,
}

impl HostClock {
    /// From a pair you sampled yourself. `uncertainty_s` is how far apart
    /// the two readings could have been.
    pub fn from_pair(instant: Instant, host_s: f64, uncertainty_s: f64) -> Self {
        HostClock {
            instant,
            host_s,
            uncertainty_s,
        }
    }

    /// The tightest of `tries` (host time, `Instant`, host time) samples.
    /// 1000 tries take well under a millisecond.
    pub fn calibrate(tries: usize) -> Self {
        let mut best: Option<HostClock> = None;
        for _ in 0..tries.max(1) {
            let before = host_now();
            let instant = Instant::now();
            let after = host_now();
            let pair =
                HostClock::from_pair(instant, 0.5 * (before + after), 0.5 * (after - before));
            if best.is_none_or(|b| pair.uncertainty_s < b.uncertainty_s) {
                best = Some(pair);
            }
        }
        best.expect("at least one try")
    }

    pub fn uncertainty_s(&self) -> f64 {
        self.uncertainty_s
    }

    /// Host seconds for an `Instant`, e.g. a scanner pulse.
    pub fn instant_to_host(&self, at: Instant) -> f64 {
        self.host_s + signed_secs(at, self.instant)
    }

    /// The `Instant` for a host time. `None` for 0 (Metal's "never shown"),
    /// negative numbers, NaN, infinity, or a time `Instant` can't hold.
    ///
    /// Goes through `Duration::try_from_secs_f64` and `checked_add/sub`
    /// because the plain versions panic on bad input.
    pub fn host_to_instant(&self, host_s: f64) -> Option<Instant> {
        if !(host_s > 0.0 && host_s.is_finite()) {
            return None;
        }
        let d = host_s - self.host_s;
        let magnitude = Duration::try_from_secs_f64(d.abs()).ok()?;
        if d >= 0.0 {
            self.instant.checked_add(magnitude)
        } else {
            self.instant.checked_sub(magnitude)
        }
    }

    /// Seconds from `zero` (an `Instant`, e.g. the first pulse) to the host
    /// time `host_s`, done in plain `f64` arithmetic. `None` for 0, NaN or
    /// infinity.
    pub fn secs_since(&self, zero: Instant, host_s: f64) -> Option<f64> {
        if !(host_s > 0.0 && host_s.is_finite()) {
            return None;
        }
        Some(host_s - self.instant_to_host(zero))
    }
}

/// Seconds from `zero` to `at`, negative if `at` came first.
fn signed_secs(at: Instant, zero: Instant) -> f64 {
    if at >= zero {
        (at - zero).as_secs_f64()
    } else {
        -(zero - at).as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_a_made_up_pair() {
        let base = Instant::now();
        let clock = HostClock::from_pair(base, 500_000.0, 0.0);
        let later = base + Duration::from_millis(1500);
        let h = clock.instant_to_host(later);
        assert!((h - 500_001.5).abs() < 1e-9, "{h}");
        let back = clock.host_to_instant(h).unwrap();
        let err = signed_secs(back, later).abs();
        assert!(err < 2e-9, "{err}");
        // Earlier than the anchor works too.
        let early = clock.host_to_instant(499_999.25).unwrap();
        assert!((signed_secs(early, base) + 0.75).abs() < 2e-9);
    }

    #[test]
    fn never_shown_and_garbage_are_refused() {
        let clock = HostClock::from_pair(Instant::now(), 500_000.0, 0.0);
        assert_eq!(clock.host_to_instant(0.0), None);
        assert_eq!(clock.host_to_instant(-1.0), None);
        assert_eq!(clock.host_to_instant(f64::NAN), None);
        assert_eq!(clock.host_to_instant(f64::INFINITY), None);
        assert_eq!(clock.secs_since(Instant::now(), 0.0), None);
    }

    #[test]
    fn seconds_since_a_pulse() {
        let pulse = Instant::now();
        let clock = HostClock::from_pair(pulse + Duration::from_millis(10), 1000.010, 0.0);
        // A frame presented at host 1000.5 is 0.5 s after the pulse.
        let t = clock.secs_since(pulse, 1000.5).unwrap();
        assert!((t - 0.5).abs() < 1e-9, "{t}");
    }

    /// Compares the real clocks on this Mac (no window involved).
    #[cfg(target_os = "macos")]
    #[test]
    fn calibrated_clock_matches_instant() {
        let clock = HostClock::calibrate(1000);
        assert!(clock.uncertainty_s() < 5e-6, "{}", clock.uncertainty_s());
        for _ in 0..1000 {
            let before = Instant::now();
            let h = host_now();
            let after = Instant::now();
            let i = clock.host_to_instant(h).unwrap();
            // Within the sampling window, plus anchor uncertainty and a tick.
            let slack = Duration::from_nanos(100) + Duration::from_secs_f64(clock.uncertainty_s());
            assert!(
                i + slack >= before && i <= after + slack,
                "{i:?} not in [{before:?}, {after:?}]"
            );
        }
    }
}
