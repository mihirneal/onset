//! Timing math: converting between seconds and display frames.
//!
//! A display redraws at a fixed rate (the BOLDscreen32 runs at 120 Hz), so a
//! stimulus can only appear at the start of a frame. These helpers turn "show
//! this 2.5 s after the first scanner trigger" into "show this on frame 300",
//! and back.
//!
//! The plain functions trust their input. The `checked_*` versions reject
//! refresh rates and onsets that can't be real.

/// How long one frame lasts, in seconds.
/// At 60 Hz that's 1/60 ≈ 0.01667 s.
pub fn frame_duration(refresh_hz: f64) -> f64 {
    1.0 / refresh_hz
}

/// The frame a stimulus should appear on, given its onset in seconds after
/// the trigger (frame 0 starts at the trigger).
///
/// Onsets that fall between two frames go to the *nearest* frame.
pub fn onset_to_frame(onset_s: f64, refresh_hz: f64) -> u64 {
    (onset_s * refresh_hz).round() as u64
}

/// The time, in seconds after the trigger, at which `frame` starts.
pub fn frame_to_onset(frame: u64, refresh_hz: f64) -> f64 {
    frame as f64 * frame_duration(refresh_hz)
}

/// Why a timing conversion was rejected.
///
/// `derive(Debug, PartialEq)` lets the tests print this value and compare two
/// of them with `assert_eq!`.
#[derive(Debug, PartialEq)]
pub enum TimingError {
    /// The refresh rate was zero, negative, NaN or infinite.
    InvalidRefreshRate,
    /// The onset was negative, NaN or infinite.
    InvalidOnset,
}

/// A refresh rate is real only if it is a finite number greater than zero.
/// `is_finite()` matters: infinity passes a plain `> 0.0` check.
fn check_refresh_rate(refresh_hz: f64) -> Result<(), TimingError> {
    if refresh_hz.is_finite() && refresh_hz > 0.0 {
        Ok(())
    } else {
        Err(TimingError::InvalidRefreshRate)
    }
}

/// Like `frame_duration`, but rejects refresh rates that can't be real:
/// anything that is not a finite number greater than zero.
pub fn checked_frame_duration(refresh_hz: f64) -> Result<f64, TimingError> {
    check_refresh_rate(refresh_hz)?;
    Ok(frame_duration(refresh_hz))
}

/// Like `onset_to_frame`, but rejects bad input instead of guessing.
///
/// A valid onset is a finite number that is zero or greater. If *both* the
/// refresh rate and the onset are invalid, report the refresh rate.
pub fn checked_onset_to_frame(onset_s: f64, refresh_hz: f64) -> Result<u64, TimingError> {
    check_refresh_rate(refresh_hz)?;
    if !(onset_s.is_finite() && onset_s >= 0.0) {
        return Err(TimingError::InvalidOnset);
    }
    Ok(onset_to_frame(onset_s, refresh_hz))
}

/// Like `frame_to_onset`, but rejects an invalid refresh rate.
pub fn checked_frame_to_onset(frame: u64, refresh_hz: f64) -> Result<f64, TimingError> {
    check_refresh_rate(refresh_hz)?;
    Ok(frame_to_onset(frame, refresh_hz))
}

// Everything below only compiles when you run `cargo test`.
#[cfg(test)]
mod tests {
    // Bring the functions above into scope, like `from .. import *` in Python.
    use super::*;

    /// Floats are rarely exactly equal, so compare them with a tolerance.
    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn frame_duration_at_common_refresh_rates() {
        assert!(approx_eq(frame_duration(60.0), 0.016_666_666_667));
        assert!(approx_eq(frame_duration(120.0), 0.008_333_333_333));
    }

    #[test]
    fn onsets_that_land_exactly_on_a_frame() {
        assert_eq!(onset_to_frame(0.0, 60.0), 0);
        assert_eq!(onset_to_frame(2.5, 60.0), 150);
        // 4 volumes into a run with TR = 2 s:
        assert_eq!(onset_to_frame(8.0, 60.0), 480);
    }

    #[test]
    fn onsets_between_frames_go_to_the_nearest_frame() {
        // 0.02 s * 60 Hz = 1.2 frames -> frame 1
        assert_eq!(onset_to_frame(0.02, 60.0), 1);
        // 0.025 s * 60 Hz = 1.5 frames -> round half up -> frame 2
        assert_eq!(onset_to_frame(0.025, 60.0), 2);
    }

    #[test]
    fn real_displays_are_not_exactly_60_hz() {
        // A "60 Hz" projector often measures 59.94 Hz.
        // 10 s * 59.94 Hz = 599.4 frames -> frame 599
        assert_eq!(onset_to_frame(10.0, 59.94), 599);
    }

    #[test]
    fn frame_to_onset_is_the_inverse() {
        assert!(approx_eq(frame_to_onset(0, 60.0), 0.0));
        assert!(approx_eq(frame_to_onset(150, 60.0), 2.5));
    }

    #[test]
    fn round_trip_frame_to_onset_to_frame() {
        // For each of the first 100,000 frames (~28 min at 60 Hz, ~14 min at
        // the BOLDscreen's 120 Hz), converting to seconds and back must land
        // on the same frame.
        for refresh_hz in [60.0, 59.94, 120.0] {
            for frame in 0..100_000 {
                let onset = frame_to_onset(frame, refresh_hz);
                assert_eq!(
                    onset_to_frame(onset, refresh_hz),
                    frame,
                    "round trip failed for frame {frame} at {refresh_hz} Hz"
                );
            }
        }
    }

    // ---- Checked versions ----

    #[test]
    fn checked_frame_duration_accepts_real_refresh_rates() {
        // `.unwrap()` pulls the value out of an `Ok`, and panics (fails the
        // test) if it is an `Err`. Fine in tests; avoid it in real code.
        assert!(approx_eq(
            checked_frame_duration(60.0).unwrap(),
            0.016_666_666_667
        ));
        assert!(approx_eq(
            checked_frame_duration(59.94).unwrap(),
            1.0 / 59.94
        ));
    }

    #[test]
    fn zero_and_negative_refresh_rates_are_rejected() {
        assert_eq!(
            checked_frame_duration(0.0),
            Err(TimingError::InvalidRefreshRate)
        );
        assert_eq!(
            checked_frame_duration(-60.0),
            Err(TimingError::InvalidRefreshRate)
        );
    }

    #[test]
    fn nan_and_infinite_refresh_rates_are_rejected() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                checked_frame_duration(bad),
                Err(TimingError::InvalidRefreshRate)
            );
        }
    }

    #[test]
    fn valid_onsets_convert_like_the_unchecked_version() {
        assert_eq!(checked_onset_to_frame(2.5, 60.0), Ok(150));
        // Exactly zero is a valid onset: the trigger itself.
        assert_eq!(checked_onset_to_frame(0.0, 60.0), Ok(0));
    }

    #[test]
    fn negative_onsets_are_rejected() {
        assert_eq!(
            checked_onset_to_frame(-1.0, 60.0),
            Err(TimingError::InvalidOnset)
        );
    }

    #[test]
    fn nan_and_infinite_onsets_are_rejected() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                checked_onset_to_frame(bad, 60.0),
                Err(TimingError::InvalidOnset)
            );
        }
    }

    #[test]
    fn a_bad_refresh_rate_is_reported_before_a_bad_onset() {
        assert_eq!(
            checked_onset_to_frame(-1.0, 0.0),
            Err(TimingError::InvalidRefreshRate)
        );
    }

    #[test]
    fn checked_frame_to_onset_works_and_rejects_bad_rates() {
        assert!(approx_eq(checked_frame_to_onset(150, 60.0).unwrap(), 2.5));
        assert_eq!(
            checked_frame_to_onset(150, 0.0),
            Err(TimingError::InvalidRefreshRate)
        );
    }

    #[test]
    fn checked_and_unchecked_agree_on_valid_input() {
        for frame in 0..1_000 {
            let onset = frame_to_onset(frame, 59.94);
            assert_eq!(checked_onset_to_frame(onset, 59.94), Ok(frame));
            assert_eq!(checked_frame_to_onset(frame, 59.94), Ok(onset));
        }
    }
}
