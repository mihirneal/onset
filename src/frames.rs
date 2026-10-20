//! Frame timing checks: measuring the real refresh rate from the gaps
//! between frames, and deciding which refresh rate to trust.
//!
//! Which stimulus frame to draw, and which refreshes were missed, is the
//! planner's job (`planner.rs`), from Metal's reports of when each frame was
//! on screen.

use crate::planner::PeriodEstimate;
use crate::stats::median;

/// A measured refresh rate further than this fraction from the nominal one
/// is warned about, and not used for the schedule.
pub const REFRESH_MISMATCH: f64 = 0.005;

/// New frames reaching the screen at a rate this far (as a fraction) from
/// the refresh rate means something's off: e.g. only every second refresh
/// shows a new frame, or the display runs at another rate than it reports.
/// Looser than `REFRESH_MISMATCH`, because the gaps between single frames
/// are noisier than the fitted refresh period.
pub const FRAME_RATE_MISMATCH: f64 = 0.1;

/// The measured refresh rate is used for the schedule only once its
/// standard error is below this fraction: 20 parts per million, about 4 ms
/// over a 3-minute run. Until then the nominal rate is more trustworthy
/// than the measurement (a display's real rate is usually within a few
/// hundred ppm of the one it reports).
pub const MAX_RATE_UNCERTAINTY: f64 = 20e-6;

/// How many gaps between frames on screen are needed before they're used to
/// check the nominal refresh rate (about a quarter of a second at 120 Hz).
pub const MIN_GAPS_FOR_RATE_CHECK: usize = 30;

/// The rate implied by a run of frame-to-frame gaps. Uses the median, so a
/// few dropped frames don't skew it.
///
/// Fed with the gaps between frames that reached the screen one after the
/// other, this is how many new frames per second the participant sees: the
/// refresh rate when every frame is shown, half of it if the display only
/// takes every second frame.
pub fn measured_refresh_hz(intervals_s: &[f64]) -> Option<f64> {
    median(intervals_s).filter(|m| *m > 0.0).map(|m| 1.0 / m)
}

/// Whether new frames reaching the screen at `seen_hz` show that
/// `nominal_hz`, the rate the planner counts refreshes with, is wrong.
///
/// With vsync on, new frames can't reach the screen faster than the display
/// refreshes, so seeing clearly more frames per second than `nominal_hz`
/// means it's too low, even if the display reported it. Seeing fewer is
/// normal when only every second refresh shows a new frame, so that only
/// counts against a rate that was a guess (`reported` false).
pub fn nominal_rate_is_wrong(nominal_hz: f64, seen_hz: f64, reported: bool) -> bool {
    let ratio = seen_hz / nominal_hz;
    if reported {
        ratio > 1.0 + FRAME_RATE_MISMATCH
    } else {
        (ratio - 1.0).abs() > FRAME_RATE_MISMATCH
    }
}

/// Why the measured refresh rate isn't used for the schedule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateProblem {
    /// Not enough reports from the display yet.
    NotMeasured,
    /// Measured, but not precisely enough yet. `uncertainty` is the
    /// standard error as a fraction of the period.
    Imprecise { measured_hz: f64, uncertainty: f64 },
    /// Measured precisely, but too far from the nominal rate to believe.
    Mismatch { measured_hz: f64 },
}

/// The refresh rate to build the run's schedule from: the measured one, if
/// it's precise and agrees with `nominal_hz` to within `REFRESH_MISMATCH`.
/// That corrects small differences like a display that reports 120 Hz but
/// runs at 119.95 (0.2 s over 10 minutes). Otherwise the reason not to,
/// and the caller schedules at `nominal_hz`.
pub fn schedule_rate(
    nominal_hz: f64,
    measured: Option<PeriodEstimate>,
) -> Result<f64, RateProblem> {
    let Some(m) = measured else {
        return Err(RateProblem::NotMeasured);
    };
    let measured_hz = 1.0 / m.period_s;
    let uncertainty = m.std_err_s / m.period_s;
    if !(measured_hz.is_finite() && measured_hz > 0.0) {
        return Err(RateProblem::NotMeasured);
    }
    // NaN (no scatter estimate) counts as imprecise.
    if uncertainty.is_nan() || uncertainty > MAX_RATE_UNCERTAINTY {
        return Err(RateProblem::Imprecise {
            measured_hz,
            uncertainty,
        });
    }
    let off = (measured_hz / nominal_hz - 1.0).abs();
    if off.is_nan() || off > REFRESH_MISMATCH {
        return Err(RateProblem::Mismatch { measured_hz });
    }
    Ok(measured_hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_120: f64 = 1.0 / 120.0;

    #[test]
    fn refresh_rate_from_gaps_ignores_a_dropped_frame() {
        let mut gaps = vec![FRAME_120; 100];
        gaps.push(2.0 * FRAME_120);
        let hz = measured_refresh_hz(&gaps).unwrap();
        assert!((hz - 120.0).abs() < 1e-6, "{hz}");
        assert_eq!(measured_refresh_hz(&[]), None);
    }

    #[test]
    fn a_wrong_nominal_rate_shows_in_the_frames_on_screen() {
        // Reported 60 Hz, but 120 new frames a second: the report is wrong.
        assert!(nominal_rate_is_wrong(60.0, 120.0, true));
        // Reported 120 Hz, 60 new frames a second: every other frame is
        // dropped (warned about elsewhere), but 120 may well be right.
        assert!(!nominal_rate_is_wrong(120.0, 60.0, true));
        // Guessed 60 Hz: wrong either way.
        assert!(nominal_rate_is_wrong(60.0, 120.0, false));
        assert!(nominal_rate_is_wrong(60.0, 50.0, false));
        // Small differences are noise in the frame gaps.
        assert!(!nominal_rate_is_wrong(120.0, 119.0, true));
        assert!(!nominal_rate_is_wrong(60.0, 59.94, false));
    }

    fn estimate(hz: f64, uncertainty: f64) -> Option<PeriodEstimate> {
        Some(PeriodEstimate {
            period_s: 1.0 / hz,
            std_err_s: uncertainty / hz,
        })
    }

    #[test]
    fn a_precise_measurement_close_to_the_nominal_rate_is_used() {
        let hz = schedule_rate(120.0, estimate(119.95, 2e-6)).unwrap();
        assert!((hz - 119.95).abs() < 1e-9, "{hz}");
    }

    #[test]
    fn an_unusable_measurement_falls_back_to_the_nominal_rate() {
        assert_eq!(schedule_rate(120.0, None), Err(RateProblem::NotMeasured));
        // Half a second of noisy reports: not better than the nominal rate.
        assert!(matches!(
            schedule_rate(120.0, estimate(119.94, 400e-6)),
            Err(RateProblem::Imprecise { .. })
        ));
        // Precise, but 10 % off: the vsync grid it's built on is wrong
        // (e.g. a guessed 60 Hz on a 120 Hz display fits as 66 Hz).
        assert!(matches!(
            schedule_rate(60.0, estimate(66.06, 1e-6)),
            Err(RateProblem::Mismatch { .. })
        ));
        assert!(matches!(
            schedule_rate(
                120.0,
                Some(PeriodEstimate {
                    period_s: 1.0 / 120.0,
                    std_err_s: f64::NAN
                })
            ),
            Err(RateProblem::Imprecise { .. })
        ));
    }
}
