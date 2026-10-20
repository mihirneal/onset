//! Scanner pulses: a fake scanner for testing at a desk, and statistics on
//! the pulses we receive (measured TR, jitter, likely missed pulses).
//!
//! Pulse times here are seconds after the first pulse, so the first one is
//! always 0.0.

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::stats::median;

/// How long without a pulse counts as "the scanner has stopped" while the TR
/// isn't known yet. Longer than any normal TR.
pub const DEFAULT_QUIET_S: f64 = 5.0;

/// Once the TR is known, the scanner counts as stopped after this many TRs
/// without a pulse. More than 2, so a single dropped pulse (as with a loose
/// trigger fiber) doesn't look like the end of the scan.
pub const QUIET_TRS: f64 = 2.5;

/// Watches the pulse stream to tell whether the scanner is mid-scan, so a run
/// never starts on the tail of the previous scan and never ends before the
/// scan does.
///
/// Times are seconds from any fixed starting point, as long as every call
/// uses the same one.
#[derive(Debug, Clone)]
pub struct PulseWatch {
    quiet_s: f64,
    times_s: Vec<f64>,
}

impl PulseWatch {
    /// `quiet_s`: how long without a pulse means "stopped" before the TR is
    /// known, and the least it can mean after.
    pub fn new(quiet_s: f64) -> Self {
        PulseWatch {
            quiet_s,
            times_s: Vec::new(),
        }
    }

    pub fn pulse(&mut self, t_s: f64) {
        self.times_s.push(t_s);
    }

    pub fn count(&self) -> usize {
        self.times_s.len()
    }

    pub fn last_s(&self) -> Option<f64> {
        self.times_s.last().copied()
    }

    /// The TR, from the typical gap between pulses. `None` until there are
    /// two pulses.
    pub fn tr_s(&self) -> Option<f64> {
        let gaps: Vec<f64> = self.times_s.windows(2).map(|w| w[1] - w[0]).collect();
        median(&gaps)
    }

    /// How long without a pulse means the scanner has stopped.
    pub fn stopped_after_s(&self) -> f64 {
        self.tr_s()
            .map_or(self.quiet_s, |tr| (QUIET_TRS * tr).max(self.quiet_s))
    }

    /// True if no pulse has come for long enough, counting from `since_s` or
    /// from the last pulse, whichever is later. With no pulses at all, this
    /// is just "has `since_s` been long enough ago".
    pub fn quiet_since(&self, since_s: f64, now_s: f64) -> bool {
        let latest = self.last_s().map_or(since_s, |p| p.max(since_s));
        now_s - latest >= self.stopped_after_s()
    }
}

/// Start a fake scanner that sends `volumes` pulses, one every `tr`.
///
/// It runs on its own thread and sends the moment of each pulse down a
/// channel; read them by looping over the returned `Receiver`. The loop ends
/// after the last pulse. (Like a Python generator fed by a background
/// thread: `for pulse in fake_scanner(...)`.)
///
/// The first pulse comes one TR after the call, as if the scanner were just
/// starting. Pulses are scheduled from the start time, not from the previous
/// pulse, so small delays in waking up don't add up over a run.
pub fn fake_scanner(tr: Duration, volumes: u32) -> Receiver<Instant> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let start = Instant::now();
        for n in 1..=volumes {
            let due = start + tr * n;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            // `send` only fails if nobody is listening any more; stop then.
            if tx.send(Instant::now()).is_err() {
                break;
            }
        }
    });
    rx
}

/// What a run of pulses says about the scanner's TR.
#[derive(Debug, PartialEq)]
pub struct TrStats {
    pub pulses: usize,
    /// The typical gap between pulses. The median ignores the odd long gap
    /// from a missed pulse, so this is the best guess at the real TR.
    pub median_tr_s: f64,
    pub mean_tr_s: f64,
    /// Standard deviation of the gaps: how much the timing wobbles.
    pub sd_s: f64,
    pub min_tr_s: f64,
    pub max_tr_s: f64,
    /// Gaps of about two TRs or more suggest pulses that never arrived.
    pub likely_missed: usize,
}

/// Summarize pulse times (seconds after the first pulse).
///
/// Returns `None` with fewer than two pulses, since there is no gap to
/// measure yet.
pub fn tr_stats(pulse_times_s: &[f64]) -> Option<TrStats> {
    let gaps: Vec<f64> = pulse_times_s.windows(2).map(|w| w[1] - w[0]).collect();
    if gaps.is_empty() {
        return None;
    }

    let n = gaps.len() as f64;
    let mean = gaps.iter().sum::<f64>() / n;
    let sd = if gaps.len() > 1 {
        let var = gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / (n - 1.0);
        var.sqrt()
    } else {
        0.0
    };

    let median = median(&gaps)?;
    let min = gaps.iter().copied().fold(f64::INFINITY, f64::min);
    let max = gaps.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    // A gap of k TRs means k - 1 pulses went missing in between.
    let likely_missed = gaps
        .iter()
        .map(|g| ((g / median).round() as usize).saturating_sub(1))
        .sum();

    Some(TrStats {
        pulses: pulse_times_s.len(),
        median_tr_s: median,
        mean_tr_s: mean,
        sd_s: sd,
        min_tr_s: min,
        max_tr_s: max,
        likely_missed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn fewer_than_two_pulses_has_no_tr() {
        assert_eq!(tr_stats(&[]), None);
        assert_eq!(tr_stats(&[0.0]), None);
    }

    #[test]
    fn perfectly_regular_pulses() {
        let times: Vec<f64> = (0..10).map(|i| i as f64 * 2.0).collect();
        let s = tr_stats(&times).unwrap();
        assert_eq!(s.pulses, 10);
        assert!(approx_eq(s.median_tr_s, 2.0));
        assert!(approx_eq(s.mean_tr_s, 2.0));
        assert!(approx_eq(s.sd_s, 0.0));
        assert_eq!(s.likely_missed, 0);
    }

    #[test]
    fn jitter_shows_up_in_sd_and_range() {
        let s = tr_stats(&[0.0, 2.001, 3.999, 6.002]).unwrap();
        assert!(approx_eq(s.min_tr_s, 1.998));
        assert!(approx_eq(s.max_tr_s, 2.003));
        assert!(s.sd_s > 0.0 && s.sd_s < 0.01);
        assert_eq!(s.likely_missed, 0);
    }

    #[test]
    fn a_missing_pulse_is_counted_and_does_not_skew_the_median() {
        // The pulse at 6.0 never arrived.
        let s = tr_stats(&[0.0, 2.0, 4.0, 8.0, 10.0]).unwrap();
        assert!(approx_eq(s.median_tr_s, 2.0));
        assert_eq!(s.likely_missed, 1);
    }

    #[test]
    fn pulse_watch_before_any_pulse_waits_the_default_quiet_time() {
        let w = PulseWatch::new(5.0);
        assert_eq!(w.tr_s(), None);
        assert!(!w.quiet_since(10.0, 14.9));
        assert!(w.quiet_since(10.0, 15.0));
    }

    #[test]
    fn pulse_watch_uses_two_and_a_half_trs_once_the_tr_is_known() {
        let mut w = PulseWatch::new(1.0);
        for i in 0..10 {
            w.pulse(i as f64 * 2.8);
        }
        assert!(approx_eq(w.tr_s().unwrap(), 2.8));
        assert!(approx_eq(w.stopped_after_s(), 7.0));
        let last = 9.0 * 2.8;
        // One missing pulse (a gap of 2 TRs) is not the end of the scan.
        assert!(!w.quiet_since(0.0, last + 5.6));
        assert!(w.quiet_since(0.0, last + 7.0));
    }

    #[test]
    fn pulse_watch_never_waits_less_than_the_minimum() {
        let mut w = PulseWatch::new(5.0);
        for i in 0..10 {
            w.pulse(i as f64 * 0.5);
        }
        // 2.5 TRs would be 1.25 s; the 5 s minimum wins.
        assert!(approx_eq(w.stopped_after_s(), 5.0));
    }

    #[test]
    fn pulse_watch_counts_from_the_later_of_ready_and_last_pulse() {
        // The tail of an earlier scan: pulses every 1.5 s until 30 s.
        let mut w = PulseWatch::new(5.0);
        for i in 0..=20 {
            w.pulse(i as f64 * 1.5);
        }
        // Ready at 31 s: still within 5 s (2.5 TRs = 3.75 s, minimum 5 s).
        assert!(!w.quiet_since(31.0, 35.0));
        assert!(w.quiet_since(31.0, 36.0));
    }

    #[test]
    fn fake_scanner_sends_the_right_number_of_evenly_spaced_pulses() {
        let tr = Duration::from_millis(20);
        let pulses: Vec<Instant> = fake_scanner(tr, 5).into_iter().collect();
        assert_eq!(pulses.len(), 5);
        let first = pulses[0];
        let times: Vec<f64> = pulses.iter().map(|p| (*p - first).as_secs_f64()).collect();
        let s = tr_stats(&times).unwrap();
        // Sleeping can run late, but the schedule keeps the average on target.
        assert!((s.mean_tr_s - 0.020).abs() < 0.005, "mean TR {}", s.mean_tr_s);
        assert_eq!(s.likely_missed, 0);
    }
}
