//! Timing math: converting between seconds and display frames.
//!
//! A display redraws at a fixed rate (e.g. 60 Hz), so a stimulus can only
//! appear at the start of a frame. These helpers turn "show this 2.5 s after
//! the first scanner trigger" into "show this on frame 150", and back.
//!
//! EXERCISE 1: replace each `todo!()` with a real implementation until
//! `cargo test` passes. Don't change the function signatures or the tests.

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
        // For every frame in a ~28-minute run, converting to seconds and back
        // must land on the same frame, at both refresh rates.
        for refresh_hz in [60.0, 59.94] {
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
}
