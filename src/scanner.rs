//! Telling from the pulse stream whether the scanner is mid-scan.
//!
//! A run that ends before its scan leaves the last volumes without a
//! stimulus, and a scan that outlasts its run can start the next run with
//! its last pulses. `PulseWatch` answers "has the scanner stopped?", so a
//! run can hold its final screen until the scan is over and ignore the tail
//! of a scan that was already under way when Onset got ready.

/// How long without a pulse counts as "the scanner has stopped" while the TR
/// isn't known yet. Longer than any normal TR.
pub const DEFAULT_QUIET_S: f64 = 5.0;

/// Once the TR is known, the scanner counts as stopped after this many TRs
/// without a pulse. More than 2, so a single dropped pulse (as with a loose
/// trigger fiber) doesn't look like the end of the scan.
pub const QUIET_TRS: f64 = 2.5;

/// A pulse this soon after Onset gets ready is taken as the tail of a scan
/// that was already under way (see `PulseWatch::clear_to_start`).
pub const STARTUP_GRACE_S: f64 = 2.0;

/// The pulse times seen so far, and what they say about the scan.
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
        PulseWatch { quiet_s, times_s: Vec::new() }
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

    /// The TR, from the typical (median) gap between pulses, so the odd
    /// missed pulse doesn't skew it. `None` until there are two pulses.
    pub fn tr_s(&self) -> Option<f64> {
        let mut gaps: Vec<f64> = self.times_s.windows(2).map(|w| w[1] - w[0]).collect();
        if gaps.is_empty() {
            return None;
        }
        gaps.sort_by(f64::total_cmp);
        let mid = gaps.len() / 2;
        Some(if gaps.len() % 2 == 0 { (gaps[mid - 1] + gaps[mid]) / 2.0 } else { gaps[mid] })
    }

    /// How long without a pulse means the scanner has stopped:
    /// max(`quiet_s`, 2.5 TRs).
    pub fn stopped_after_s(&self) -> f64 {
        self.tr_s().map_or(self.quiet_s, |tr| (QUIET_TRS * tr).max(self.quiet_s))
    }

    /// True if no pulse has come for long enough, counting from `since_s` or
    /// from the last pulse, whichever is later. With no pulses at all, this
    /// is just "has `since_s` been long enough ago".
    pub fn quiet_since(&self, since_s: f64, now_s: f64) -> bool {
        let latest = self.last_s().map_or(since_s, |p| p.max(since_s));
        now_s - latest >= self.stopped_after_s()
    }

    /// Before a run: may a pulse at `now_s` start it? Until any stray pulse
    /// has been seen, yes once Onset has been ready for `STARTUP_GRACE_S`.
    /// That's kept short, so a scan started right after launching Onset is
    /// never mistaken for an earlier one. Once stray pulses have come (a scan
    /// was already under way), the scanner must go quiet for the full
    /// `stopped_after_s` first.
    pub fn clear_to_start(&self, ready_s: f64, now_s: f64) -> bool {
        match self.last_s() {
            None => now_s - ready_s >= STARTUP_GRACE_S,
            Some(last) => now_s - last.max(ready_s) >= self.stopped_after_s(),
        }
    }

    /// True if there has been a pulse and none for long enough since.
    pub fn stopped(&self, now_s: f64) -> bool {
        self.last_s().is_some_and(|last| now_s - last >= self.stopped_after_s())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn before_any_pulse_it_waits_the_default_quiet_time() {
        let w = PulseWatch::new(5.0);
        assert_eq!(w.tr_s(), None);
        assert!(!w.quiet_since(10.0, 14.9));
        assert!(w.quiet_since(10.0, 15.0));
        assert!(!w.stopped(100.0), "no pulses: there's no scan to have stopped");
    }

    #[test]
    fn it_uses_two_and_a_half_trs_once_the_tr_is_known() {
        let mut w = PulseWatch::new(1.0);
        for i in 0..10 {
            w.pulse(i as f64 * 2.8);
        }
        assert!(approx_eq(w.tr_s().unwrap(), 2.8));
        assert!(approx_eq(w.stopped_after_s(), 7.0));
        let last = 9.0 * 2.8;
        // One missing pulse (a gap of 2 TRs) is not the end of the scan.
        assert!(!w.quiet_since(0.0, last + 5.6));
        assert!(!w.stopped(last + 5.6));
        assert!(w.quiet_since(0.0, last + 7.0));
        assert!(w.stopped(last + 7.0));
    }

    #[test]
    fn it_never_waits_less_than_the_minimum() {
        let mut w = PulseWatch::new(5.0);
        for i in 0..10 {
            w.pulse(i as f64 * 0.5);
        }
        // 2.5 TRs would be 1.25 s; the 5 s minimum wins.
        assert!(approx_eq(w.stopped_after_s(), 5.0));
    }

    #[test]
    fn a_missed_pulse_doesnt_skew_the_tr() {
        let mut w = PulseWatch::new(5.0);
        for t in [0.0, 2.0, 4.0, 8.0, 10.0] {
            w.pulse(t);
        }
        assert!(approx_eq(w.tr_s().unwrap(), 2.0));
    }

    #[test]
    fn it_counts_from_the_later_of_ready_and_the_last_pulse() {
        // The tail of an earlier scan: pulses every 1.5 s until 30 s.
        let mut w = PulseWatch::new(5.0);
        for i in 0..=20 {
            w.pulse(i as f64 * 1.5);
        }
        // Ready at 31 s: still within 5 s (2.5 TRs = 3.75 s, minimum 5 s).
        assert!(!w.quiet_since(31.0, 35.0));
        assert!(w.quiet_since(31.0, 36.0));
    }
}
