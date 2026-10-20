//! A block-design checkerboard localizer, scheduled in whole display frames.
//!
//! The run starts on the first scanner pulse and goes
//! rest, checkerboard, rest, checkerboard, ..., rest.
//!
//! Everything is converted to frames once, up front, so block edges and
//! contrast reversals land on exact frames instead of drifting with
//! floating-point error.

use std::fmt;

use crate::timing::{TimingError, checked_onset_to_frame, frame_to_onset};

/// What the participant sees on one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Gray with a fixation dot.
    Rest,
    /// The checkerboard. `phase` flips at every contrast reversal.
    Checkerboard { phase: bool },
    /// The run is over.
    Done,
}

impl Screen {
    /// A short name for log files.
    pub fn label(&self) -> &'static str {
        match self {
            Screen::Rest => "rest",
            Screen::Checkerboard { phase: false } => "check_a",
            Screen::Checkerboard { phase: true } => "check_b",
            Screen::Done => "done",
        }
    }
}

/// The run's timing in seconds, as the experimenter asks for it.
#[derive(Debug, Clone, Copy)]
pub struct BlockDesign {
    /// Length of every rest and checkerboard block.
    pub block_s: f64,
    /// How many checkerboard blocks. There is one more rest block than this.
    pub checker_blocks: u32,
    /// Contrast reversals per second during checkerboard blocks.
    pub reversals_per_s: f64,
}

/// Why a design can't be shown at a given refresh rate.
#[derive(Debug, PartialEq)]
pub enum DesignError {
    InvalidRefreshRate,
    /// Blocks must be a positive number of seconds, at least one frame long.
    InvalidBlockLength,
    /// Reversals must be positive and no faster than about one per frame.
    InvalidReversalRate,
    NoCheckerboardBlocks,
}

impl fmt::Display for DesignError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let message = match self {
            DesignError::InvalidRefreshRate => "the display's refresh rate isn't usable",
            DesignError::InvalidBlockLength => "blocks must be at least one frame long",
            DesignError::InvalidReversalRate => {
                "the reversal rate must be above zero and no faster than the display"
            }
            DesignError::NoCheckerboardBlocks => "the run needs at least one checkerboard block",
        };
        f.write_str(message)
    }
}

/// A `BlockDesign` converted to whole frames at one refresh rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSchedule {
    pub block_frames: u64,
    pub reversal_frames: u64,
    pub total_blocks: u64,
}

impl FrameSchedule {
    pub fn new(design: &BlockDesign, refresh_hz: f64) -> Result<Self, DesignError> {
        if design.checker_blocks == 0 {
            return Err(DesignError::NoCheckerboardBlocks);
        }

        let block_frames =
            checked_onset_to_frame(design.block_s, refresh_hz).map_err(|e| match e {
                TimingError::InvalidRefreshRate => DesignError::InvalidRefreshRate,
                TimingError::InvalidOnset => DesignError::InvalidBlockLength,
            })?;
        if block_frames == 0 {
            return Err(DesignError::InvalidBlockLength);
        }

        let rate = design.reversals_per_s;
        if !(rate.is_finite() && rate > 0.0) {
            return Err(DesignError::InvalidReversalRate);
        }
        // The refresh rate was already checked above, so only a too-fast
        // reversal rate (rounding to 0 frames) can go wrong here.
        let reversal_frames = checked_onset_to_frame(1.0 / rate, refresh_hz)
            .map_err(|_| DesignError::InvalidReversalRate)?;
        if reversal_frames == 0 {
            return Err(DesignError::InvalidReversalRate);
        }

        Ok(FrameSchedule {
            block_frames,
            reversal_frames,
            total_blocks: 2 * u64::from(design.checker_blocks) + 1,
        })
    }

    /// Frames from the first pulse to the end of the last rest block.
    pub fn total_frames(&self) -> u64 {
        self.block_frames * self.total_blocks
    }

    /// What to show on `frame`, counting the first frame after the first
    /// scanner pulse as frame 0.
    pub fn screen_at(&self, frame: u64) -> Screen {
        let block = frame / self.block_frames;
        if block >= self.total_blocks {
            return Screen::Done;
        }
        if block.is_multiple_of(2) {
            return Screen::Rest;
        }
        let into_block = frame % self.block_frames;
        Screen::Checkerboard {
            phase: (into_block / self.reversal_frames) % 2 == 1,
        }
    }

    /// `(first frame, length in frames)` of every checkerboard block.
    pub fn checker_blocks(&self) -> impl Iterator<Item = (u64, u64)> + use<> {
        let block_frames = self.block_frames;
        (0..self.total_blocks)
            .filter(|b| b % 2 == 1)
            .map(move |b| (b * block_frames, block_frames))
    }

    /// The events.tsv rows: one per checkerboard block that was shown.
    ///
    /// `drawn` is every frame drawn during the run, in order (so `index`
    /// never goes down). `refresh_hz` is the rate this schedule was built
    /// for. See `BlockEvent` for how onsets and durations are chosen.
    pub fn block_events(&self, drawn: &[DrawnFrame], refresh_hz: f64) -> Vec<BlockEvent> {
        let Some(last) = drawn.last() else {
            return Vec::new();
        };
        // The drawn frames showing stimulus frames `from..to`. A skipped
        // frame means the exact first frame of a block may never have been
        // drawn; then the next one is where the block began.
        let showing = |from: u64, to: u64| {
            let a = drawn.partition_point(|f| f.index < from);
            let b = drawn.partition_point(|f| f.index < to);
            &drawn[a..b.max(a)]
        };

        let mut events = Vec::new();
        for (first, len) in self.checker_blocks() {
            let end = first + len;
            let block = showing(first, end);
            if block.is_empty() {
                continue; // never reached (the run was stopped before it)
            }
            let planned_onset_s = frame_to_onset(first, refresh_hz);
            // Shorter than planned if the run was stopped during the block.
            let shown = len.min(last.index + 1 - first);
            let planned_duration_s = frame_to_onset(shown, refresh_hz);
            let (onset_s, onset_source) = first_on_screen(block, refresh_hz)
                .unwrap_or((planned_onset_s, OnsetSource::Planned));
            // The block ends when the rest after it first reached the screen.
            let off = first_on_screen(showing(end, end + len), refresh_hz);
            let duration_s = match (onset_source, off) {
                (OnsetSource::Planned, _) | (_, None) => planned_duration_s,
                (_, Some((off_s, _))) => off_s - onset_s,
            };
            events.push(BlockEvent {
                onset_s,
                duration_s,
                onset_source,
                planned_onset_s,
                planned_duration_s,
            });
        }
        events
    }
}

/// When `frames` (the drawn frames of one block, in order) first reached
/// the screen, from the first of them with a time on screen.
///
/// If frames before that one have no time, it depends on why. Frames Metal
/// said were never shown don't count: the block appeared with the timed
/// frame. Frames with no report (or a report that made no sense) may well
/// have been on screen, so the onset is estimated from the timed frame,
/// one refresh earlier per stimulus frame back to the first of those.
/// `None` if no frame of the block has a time.
fn first_on_screen(frames: &[DrawnFrame], refresh_hz: f64) -> Option<(f64, OnsetSource)> {
    let i = frames.iter().position(|f| f.presented_s.is_some())?;
    let timed = frames[i];
    let on = timed.presented_s?;
    Some(match frames[..i].iter().find(|f| !f.not_shown) {
        None => (on, OnsetSource::Presented),
        Some(unknown) => (
            on - frame_to_onset(timed.index - unknown.index, refresh_hz),
            OnsetSource::Estimated,
        ),
    })
}

/// A frame drawn during the run: the stimulus frame it showed, and when it
/// reached the screen (seconds from the first pulse), if Metal reported that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawnFrame {
    pub index: u64,
    pub presented_s: Option<f64>,
    /// Metal reported that this frame was never shown (presentedTime 0).
    /// False when it was shown, or nothing is known.
    pub not_shown: bool,
}

/// Where a `BlockEvent`'s onset comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnsetSource {
    /// The time the block's first frame on screen reached it.
    Presented,
    /// Worked back from a later frame of the block, because the frames
    /// before it have no time on screen but may have been shown.
    Estimated,
    /// No frame of the block has a time on screen: the schedule's onset,
    /// which leaves out the 1-3 frames from the pulse to frame 0.
    Planned,
}

impl OnsetSource {
    /// A short name for log files.
    pub fn label(&self) -> &'static str {
        match self {
            OnsetSource::Presented => "presented",
            OnsetSource::Estimated => "estimated",
            OnsetSource::Planned => "planned",
        }
    }
}

/// One checkerboard block, as written to events.tsv. Seconds from the first
/// scanner pulse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockEvent {
    /// When the block first reached the screen. That's 1-3 frames after the
    /// planned onset, because frame 0 is the first frame drawn after the
    /// pulse. See `onset_source` for how it was found.
    pub onset_s: f64,
    /// From `onset_s` to when the rest after the block first reached the
    /// screen (found the same way). The planned duration if that isn't
    /// known, if the onset is only the planned one, or if the run was
    /// stopped during the block.
    pub duration_s: f64,
    pub onset_source: OnsetSource,
    /// Where the schedule put the block: frame number / refresh rate.
    pub planned_onset_s: f64,
    pub planned_duration_s: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn design(block_s: f64, checker_blocks: u32, reversals_per_s: f64) -> BlockDesign {
        BlockDesign {
            block_s,
            checker_blocks,
            reversals_per_s,
        }
    }

    #[test]
    fn default_design_at_the_boldscreens_120_hz() {
        let s = FrameSchedule::new(&design(20.0, 4, 8.0), 120.0).unwrap();
        assert_eq!(s.block_frames, 2400);
        assert_eq!(s.reversal_frames, 15);
        assert_eq!(s.total_blocks, 9);
        assert_eq!(s.total_frames(), 21_600); // 180 s
    }

    #[test]
    fn blocks_alternate_starting_and_ending_with_rest() {
        let s = FrameSchedule::new(&design(20.0, 4, 8.0), 120.0).unwrap();
        assert_eq!(s.screen_at(0), Screen::Rest);
        assert_eq!(s.screen_at(2399), Screen::Rest);
        assert_eq!(s.screen_at(2400), Screen::Checkerboard { phase: false });
        assert_eq!(s.screen_at(4800), Screen::Rest);
        assert_eq!(s.screen_at(7200), Screen::Checkerboard { phase: false });
        assert_eq!(s.screen_at(21_599), Screen::Rest);
        assert_eq!(s.screen_at(21_600), Screen::Done);
    }

    #[test]
    fn the_checkerboard_reverses_every_reversal_period() {
        let s = FrameSchedule::new(&design(20.0, 4, 8.0), 120.0).unwrap();
        assert_eq!(s.screen_at(2414), Screen::Checkerboard { phase: false });
        assert_eq!(s.screen_at(2415), Screen::Checkerboard { phase: true });
        assert_eq!(s.screen_at(2429), Screen::Checkerboard { phase: true });
        assert_eq!(s.screen_at(2430), Screen::Checkerboard { phase: false });
    }

    #[test]
    fn reversal_period_is_rounded_to_whole_frames() {
        // 8 reversals/s at 60 Hz would be 7.5 frames each; it rounds to 8.
        let s = FrameSchedule::new(&design(20.0, 4, 8.0), 60.0).unwrap();
        assert_eq!(s.reversal_frames, 8);
    }

    #[test]
    fn checker_block_list_matches_the_schedule() {
        let s = FrameSchedule::new(&design(20.0, 3, 8.0), 120.0).unwrap();
        let blocks: Vec<(u64, u64)> = s.checker_blocks().collect();
        assert_eq!(blocks, vec![(2400, 2400), (7200, 2400), (12_000, 2400)]);
    }

    /// 10 Hz with 1 s blocks: checkerboard at frames 10..20 and 30..40.
    fn small_schedule() -> FrameSchedule {
        FrameSchedule::new(&design(1.0, 2, 4.0), 10.0).unwrap()
    }

    /// Frames 0..=last, each on screen 0.15 s + index / 10 Hz after the pulse.
    fn all_shown(last: u64) -> Vec<DrawnFrame> {
        (0..=last)
            .map(|i| DrawnFrame {
                index: i,
                presented_s: Some(0.15 + i as f64 * 0.1),
                not_shown: false,
            })
            .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn block_onsets_are_when_the_first_frame_reached_the_screen() {
        let s = small_schedule();
        let events = s.block_events(&all_shown(49), 10.0);
        assert_eq!(events.len(), 2);
        assert!(close(events[0].onset_s, 1.15), "{:?}", events[0]);
        assert!(close(events[0].duration_s, 1.0), "{:?}", events[0]);
        assert_eq!(events[0].onset_source, OnsetSource::Presented);
        assert!(close(events[0].planned_onset_s, 1.0));
        assert!(close(events[0].planned_duration_s, 1.0));
        assert!(close(events[1].onset_s, 3.15), "{:?}", events[1]);
        assert!(close(events[1].planned_onset_s, 3.0));
    }

    #[test]
    fn a_skipped_first_frame_starts_the_block_on_the_next_one() {
        let s = small_schedule();
        let drawn: Vec<DrawnFrame> = all_shown(49)
            .into_iter()
            .filter(|f| f.index != 10)
            .collect();
        let events = s.block_events(&drawn, 10.0);
        assert!(close(events[0].onset_s, 1.25), "{:?}", events[0]);
        assert!(close(events[0].duration_s, 0.9), "{:?}", events[0]);
        assert_eq!(events[0].onset_source, OnsetSource::Presented);
    }

    #[test]
    fn a_first_frame_metal_never_showed_starts_the_block_on_the_next_one() {
        let s = small_schedule();
        let mut drawn = all_shown(49);
        drawn[10].presented_s = None;
        drawn[10].not_shown = true;
        let events = s.block_events(&drawn, 10.0);
        // The checkerboard first appeared with frame 11, not at the plan's
        // 1.0 s, and the duration is measured from there.
        assert!(close(events[0].onset_s, 1.25), "{:?}", events[0]);
        assert!(close(events[0].duration_s, 0.9), "{:?}", events[0]);
        assert_eq!(events[0].onset_source, OnsetSource::Presented);
    }

    #[test]
    fn missing_times_are_worked_back_from_the_next_timed_frame() {
        let s = small_schedule();
        let mut drawn = all_shown(49);
        drawn[30].presented_s = None; // first frame of block 2
        drawn[31].presented_s = None;
        drawn[20].presented_s = None; // first frame after block 1
        let events = s.block_events(&drawn, 10.0);
        // Block 1: its end is worked back from frame 21.
        assert!(close(events[0].onset_s, 1.15));
        assert!(close(events[0].duration_s, 1.0), "{:?}", events[0]);
        assert_eq!(events[0].onset_source, OnsetSource::Presented);
        // Block 2: frames 30 and 31 have no report but were probably on
        // screen, so the onset is frame 32's time less two refreshes.
        assert!(close(events[1].onset_s, 3.15), "{:?}", events[1]);
        assert!(close(events[1].duration_s, 1.0), "{:?}", events[1]);
        assert_eq!(events[1].onset_source, OnsetSource::Estimated);
    }

    #[test]
    fn with_no_times_at_all_the_plan_is_used() {
        let s = small_schedule();
        let mut drawn = all_shown(49);
        for f in &mut drawn[10..20] {
            f.presented_s = None;
        }
        let events = s.block_events(&drawn, 10.0);
        assert!(close(events[0].onset_s, 1.0), "{:?}", events[0]);
        assert!(close(events[0].duration_s, 1.0), "{:?}", events[0]);
        assert_eq!(events[0].onset_source, OnsetSource::Planned);
        // The next block is unaffected.
        assert_eq!(events[1].onset_source, OnsetSource::Presented);
    }

    #[test]
    fn stopping_early_shortens_or_drops_blocks() {
        let s = small_schedule();
        // Stopped on frame 34, halfway through block 2.
        let events = s.block_events(&all_shown(34), 10.0);
        assert_eq!(events.len(), 2);
        assert!(close(events[1].onset_s, 3.15));
        assert!(close(events[1].planned_duration_s, 0.5), "{:?}", events[1]);
        assert!(close(events[1].duration_s, 0.5));
        // Stopped before any checkerboard.
        assert!(s.block_events(&all_shown(5), 10.0).is_empty());
        assert!(s.block_events(&[], 10.0).is_empty());
    }

    #[test]
    fn impossible_designs_are_rejected() {
        let bad = |d: BlockDesign, hz: f64| FrameSchedule::new(&d, hz).unwrap_err();
        assert_eq!(
            bad(design(20.0, 0, 8.0), 120.0),
            DesignError::NoCheckerboardBlocks
        );
        assert_eq!(
            bad(design(0.0, 4, 8.0), 120.0),
            DesignError::InvalidBlockLength
        );
        assert_eq!(
            bad(design(-5.0, 4, 8.0), 120.0),
            DesignError::InvalidBlockLength
        );
        assert_eq!(
            bad(design(f64::NAN, 4, 8.0), 120.0),
            DesignError::InvalidBlockLength
        );
        assert_eq!(
            bad(design(20.0, 4, 0.0), 120.0),
            DesignError::InvalidReversalRate
        );
        assert_eq!(
            bad(design(20.0, 4, 1000.0), 120.0),
            DesignError::InvalidReversalRate
        );
        assert_eq!(
            bad(design(20.0, 4, 8.0), 0.0),
            DesignError::InvalidRefreshRate
        );
    }
}
