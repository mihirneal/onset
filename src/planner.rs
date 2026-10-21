//! Which stimulus frame to draw next, worked out from when frames actually
//! reached the screen.
//!
//! The problem this solves: the frame loop is paced by the display (each new
//! frame buffer is handed over about once per refresh), but *when* the CPU
//! gets it is jittery. On a 120 Hz external display the macOS compositor can hand
//! buffers over 4.5 ms, 12 ms, 4.5 ms, ... apart, pairs that add up to two
//! refreshes. Rounding each CPU-measured gap to a whole number of refreshes
//! (the old rule) read every 12 ms gap as a dropped frame and jumped the
//! stimulus ahead, so frames were really skipped.
//!
//! Instead, Metal tells us when each frame was on screen (`presentedTime`,
//! see `present.rs`). From that the planner knows the display's vsync grid
//! (when each refresh happens, and the exact refresh period), and predicts
//! which refresh each new frame will land on:
//!
//! ```text
//! V(k) = max(V(k-1) + 1,  first vsync at or after (now_k - guard))
//! ```
//!
//! - `V(k-1) + 1`: frames are shown in order, at most one new frame per
//!   refresh. This is the normal case and involves no CPU timing at all.
//! - `first vsync at or after now_k - guard`: a frame can't appear on a
//!   refresh that had already passed when we started drawing it. This is the
//!   only place CPU time comes in, so it only matters when we're genuinely
//!   late (by more than `guard`, a quarter of a refresh). CPU jitter smaller
//!   than that can't make a frame look late.
//! - Whenever new feedback arrives, the chain `V(...)` is restarted from the
//!   newest frame whose real vsync we know, so predictions can't drift.
//! - With no usable feedback at all there's no vsync grid, so the second
//!   term can't be computed. Then only a gap of more than
//!   `BLIND_STALL_PERIODS` refreshes between two frames moves the index on
//!   by more than one, so a real stall is still caught up but jitter isn't.
//!
//! The stimulus index is then `V(k) - V(frame 0)`: one step per refresh,
//! counted from the frame drawn right after the first scanner pulse. After a
//! real hiccup (a frame that missed its refresh) the next prediction jumps
//! ahead, so the run catches up instead of running late; the index never
//! goes backwards (if we turn out to be ahead, the index is held for a frame).
//!
//! All times here are host seconds (see `hostclock.rs`). This module is
//! plain arithmetic with no Metal in it, so it's tested with a simulated
//! display in `tests/planner_sim.rs`.
//!
//! How the frame loop uses it, each frame:
//!   1. get a frame buffer, then read `now` (host seconds) straight away;
//!   2. pass every `(frame id, presentedTime)` that came in to
//!      `on_feedback` (any order), then call `expire(now)`;
//!   3. `begin_frame(now)` gives the frame id and the vsync it should land
//!      on; `vsync_time` turns that vsync into the host time it will be on
//!      screen.
//!
//! `onset run` schedules by time: each frame shows whatever is planned for
//! the moment `vsync_time(target_vsync)` predicts (see `run.rs`). A schedule
//! counted in frames can instead call `start_run(pulse time)` on the first
//! scanner pulse and draw `planned_index`, the stimulus frame for each
//! refresh since.

use std::collections::VecDeque;

/// The causal guard, as a fraction of a refresh period. A frame counts as
/// late only if we started drawing it more than this long after the refresh
/// it was meant for. It must be larger than any amount by which
/// `presentedTime` can come before the real latch deadline (not documented
/// by Apple; to be checked with the photodiode). Too small and CPU jitter
/// causes false catch-ups; too big and real lateness is noticed later.
pub const GUARD_FRAC: f64 = 0.25;

/// How many recent (vsync, presentedTime) points the refresh-period fit
/// uses: about 17 s at 120 Hz. Long enough to measure the period to a few
/// parts per million, short enough to follow slow drift.
pub const FIT_WINDOW: usize = 2048;

/// A frame still without feedback this long after we started drawing it is
/// logged as having none. Feedback that turns up later is still accepted.
/// The frame loop also waits about this long after the run before writing
/// logs, so the last frames get their feedback.
pub const FEEDBACK_TIMEOUT_S: f64 = 0.5;

/// Time-locked pacing only: extra error, in frames, beyond half a frame
/// before an index is skipped or held. Stops `presentedTime` noise from
/// making it flip-flop.
pub const HYSTERESIS_FRAMES: f64 = 0.1;

/// `presentedTime` this much earlier than when we started drawing is still
/// accepted (rounding between clocks). Any earlier is impossible.
pub const CAUSALITY_TOLERANCE_S: f64 = 1e-4;

/// `presentedTime` more than this after we started drawing is taken to be
/// on the wrong clock (e.g. one that counts sleep) and ignored. A frame that
/// really took over 2 s to reach the screen would be ignored too; that's a
/// deliberate trade-off.
pub const MAX_PRESENT_LATENCY_S: f64 = 2.0;

/// Feedback points needed before the vsync grid is trusted (about half a
/// second at 120 Hz). Start the run after this if you can.
pub const WARM_UP_POINTS: usize = 64;

/// A fitted refresh period further than this fraction from the nominal one
/// is rejected: the display isn't running at the rate it claims, and the
/// vsync numbering built on the nominal period can't be trusted either.
pub const MAX_PERIOD_ERROR: f64 = 0.1;

/// With no feedback at all (no vsync grid), a gap longer than this many
/// refreshes between two frames counts as a real stall, and the stimulus
/// index catches up by the refreshes that went by (minus one, to be safe).
/// Shorter gaps never move it, so CPU jitter (under 2 refreshes even with
/// the compositor's uneven hand-overs) can't skip frames.
pub const BLIND_STALL_PERIODS: f64 = 3.0;

/// The fit needs a few points before it means anything...
const MIN_FIT_POINTS: usize = 8;
/// ...then refits on every new point until there are this many...
const FIT_EVERY_POINT_UNTIL: usize = 64;
/// ...and after that only every this many points, to keep the cost down.
const REFIT_EVERY: usize = 32;

/// How the stimulus index follows the display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pacing {
    /// Stimulus frame i is due on the i-th refresh after frame 0's refresh.
    /// Build the schedule from the measured refresh rate so this matches
    /// the scanner's clock.
    VsyncCount,
    /// Stimulus frame i is due at `origin + i * schedule_period_s` on the
    /// host clock, even if the display runs slightly faster or slower than
    /// that: the planner skips or holds an index now and then to keep up.
    TimeLocked { schedule_period_s: f64 },
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Starting guess for the refresh period (from the rate the display
    /// reports). Must be within `MAX_PERIOD_ERROR` of the truth; the planner
    /// then measures the real period from feedback.
    pub nominal_period_s: f64,
    pub guard_frac: f64,
    pub fit_window: usize,
    pub feedback_timeout_s: f64,
    pub pacing: Pacing,
    pub hysteresis_frames: f64,
    pub causality_tolerance_s: f64,
    pub max_present_latency_s: f64,
}

impl Config {
    /// The defaults above, counting refreshes (`Pacing::VsyncCount`).
    pub fn new(nominal_period_s: f64) -> Self {
        Config {
            nominal_period_s,
            guard_frac: GUARD_FRAC,
            fit_window: FIT_WINDOW,
            feedback_timeout_s: FEEDBACK_TIMEOUT_S,
            pacing: Pacing::VsyncCount,
            hysteresis_frames: HYSTERESIS_FRAMES,
            causality_tolerance_s: CAUSALITY_TOLERANCE_S,
            max_present_latency_s: MAX_PRESENT_LATENCY_S,
        }
    }
}

/// What we know about whether and when a frame reached the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Feedback {
    /// Nothing heard yet.
    Pending,
    Shown {
        presented_s: f64,
        vsync: i64,
    },
    /// `presentedTime` was 0: Metal says the frame was never shown, or was
    /// skipped (`MTLDrawable.h`). Not a time.
    NotShown,
    /// Nothing heard within `feedback_timeout_s`.
    NoFeedback,
    /// Feedback that breaks causality or display order; ignored.
    Anomalous {
        presented_s: f64,
    },
}

/// The planner's record of one frame.
#[derive(Debug, Clone, Copy)]
struct Rec {
    render_start: f64,
    predicted: i64,
    planned: Option<u64>,
    fb: Feedback,
}

/// What `begin_frame` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePlan {
    /// Our frame counter; Metal's feedback for this frame comes back with it.
    pub id: u64,
    /// The vsync (on the planner's grid) this frame should land on.
    pub target_vsync: i64,
    /// Stimulus frame to draw; `None` before the run starts.
    pub planned_index: Option<u64>,
    /// Stimulus frames jumped over to catch up (0 normally).
    pub skipped: u64,
    /// This frame repeats the previous index (we were ahead).
    pub held: bool,
}

/// `Feedback` without the numbers, for logs and counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Shown,
    NotShown,
    NoFeedback,
    Pending,
    Anomalous,
}

impl Status {
    /// A short name for log files.
    pub fn label(&self) -> &'static str {
        match self {
            Status::Shown => "shown",
            Status::NotShown => "not_shown",
            Status::NoFeedback => "no_feedback",
            Status::Pending => "pending",
            Status::Anomalous => "anomalous",
        }
    }
}

/// One row of the per-frame log, for frames from the run's frame 0 on.
/// Times are seconds from the first pulse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRow {
    pub id: u64,
    /// The stimulus frame that was drawn.
    pub planned_index: u64,
    /// The vsync the planner predicted, counted from frame 0's.
    pub planned_vsync: i64,
    /// When we started drawing it (CPU time, right after getting the frame
    /// buffer).
    pub render_start_s: f64,
    /// When it reached the screen, if known.
    pub presented_s: Option<f64>,
    /// The vsync it really landed on, counted from frame 0's planned vsync.
    pub vsync_index: Option<i64>,
    /// Refreshes just before this frame that showed the previous image
    /// again (dropped frames), if known.
    pub missed_vsyncs: Option<u64>,
    /// How late the planned content appeared, in vsyncs (rounded for
    /// time-locked pacing).
    pub late_vsyncs: Option<i64>,
    /// The same in seconds.
    pub late_s: Option<f64>,
    pub skipped: u64,
    pub held: bool,
    pub status: Status,
}

/// Counts over a set of `FrameRow`s.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub frames: u64,
    pub shown: u64,
    pub not_shown: u64,
    pub no_feedback: u64,
    pub pending: u64,
    pub anomalous: u64,
    /// Refreshes that showed a stale image: the real dropped frames.
    pub missed_vsyncs: u64,
    pub skipped_indices: u64,
    pub holds: u64,
    pub period_s: f64,
}

impl Summary {
    pub fn of(rows: &[FrameRow], period_s: f64) -> Summary {
        let mut s = Summary { frames: rows.len() as u64, period_s, ..Summary::default() };
        for r in rows {
            match r.status {
                Status::Shown => s.shown += 1,
                Status::NotShown => s.not_shown += 1,
                Status::NoFeedback => s.no_feedback += 1,
                Status::Pending => s.pending += 1,
                Status::Anomalous => s.anomalous += 1,
            }
            s.missed_vsyncs += r.missed_vsyncs.unwrap_or(0);
            s.skipped_indices += r.skipped;
            s.holds += u64::from(r.held);
        }
        s
    }
}

/// The newest frame whose real vsync we know. Everything is counted from it.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    id: u64,
    vsync: i64,
    presented: f64,
}

#[derive(Debug, Clone, Copy)]
struct Run {
    pulse_s: f64,
    first_id: u64,
    /// Frame 0's predicted vsync.
    n0: i64,
    /// Time-locked pacing: when frame 0 was due on screen.
    t_origin: Option<f64>,
    last_planned: u64,
    drift_corr: i64,
}

/// The measured refresh period and how precise it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeriodEstimate {
    pub period_s: f64,
    /// Standard error of `period_s`, from the scatter of the fitted points.
    pub std_err_s: f64,
}

/// Least-squares fit of presentedTime against vsync number: the slope is
/// the refresh period. Uses many frames, so one noisy timestamp barely
/// moves it, and it's exact enough to count refreshes over a whole run.
struct PeriodFit {
    pts: VecDeque<(i64, f64)>,
    cap: usize,
    nominal: f64,
    period: f64,
    /// Standard error of `period` (meaningful once `fitted`).
    std_err: f64,
    since_fit: usize,
    /// At least one fit came out close enough to `nominal` to be used.
    fitted: bool,
}

impl PeriodFit {
    fn new(nominal: f64, cap: usize) -> Self {
        PeriodFit {
            pts: VecDeque::with_capacity(cap),
            cap: cap.max(MIN_FIT_POINTS),
            nominal,
            period: nominal,
            std_err: f64::INFINITY,
            since_fit: 0,
            fitted: false,
        }
    }

    /// Adds a point; returns true if the period estimate changed.
    fn push(&mut self, vsync: i64, t: f64) -> bool {
        if self.pts.len() == self.cap {
            self.pts.pop_front();
        }
        self.pts.push_back((vsync, t));
        self.since_fit += 1;
        let n = self.pts.len();
        if n < MIN_FIT_POINTS || (n > FIT_EVERY_POINT_UNTIL && self.since_fit < REFIT_EVERY) {
            return false;
        }
        self.since_fit = 0;
        match least_squares_slope(&self.pts) {
            Some((p, std_err)) if (p / self.nominal - 1.0).abs() < MAX_PERIOD_ERROR => {
                self.fitted = true;
                self.std_err = std_err;
                let changed = p != self.period;
                self.period = p;
                changed
            }
            _ => false,
        }
    }
}

/// Slope of t against vsync, and its standard error. Values are measured
/// from the first point before squaring, so host times around 500,000 s
/// don't lose precision. The residuals are summed directly (not as
/// Σy² − slope·Σxy), because with microsecond scatter that difference
/// would cancel down to rounding noise.
fn least_squares_slope(pts: &VecDeque<(i64, f64)>) -> Option<(f64, f64)> {
    let (v0, t0) = *pts.front()?;
    if pts.len() < 3 {
        return None;
    }
    let n = pts.len() as f64;
    let (mut sx, mut sy) = (0.0, 0.0);
    for &(v, t) in pts {
        sx += (v - v0) as f64;
        sy += t - t0;
    }
    let (mx, my) = (sx / n, sy / n);
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for &(v, t) in pts {
        let dx = (v - v0) as f64 - mx;
        sxx += dx * dx;
        sxy += dx * (t - t0 - my);
    }
    if !sxx.is_finite() || sxx <= 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    let rss: f64 = pts
        .iter()
        .map(|&(v, t)| {
            let r = (t - t0 - my) - slope * ((v - v0) as f64 - mx);
            r * r
        })
        .sum();
    let std_err = (rss / (n - 2.0) / sxx).sqrt();
    Some((slope, std_err))
}

pub struct Planner {
    cfg: Config,
    /// One record per frame, indexed by frame id.
    recs: Vec<Rec>,
    anchor: Option<Anchor>,
    /// The anchor or the period changed, so the prediction chain must be
    /// rebuilt from the anchor before the next frame.
    chain_dirty: bool,
    chain_last: Option<i64>,
    fit: PeriodFit,
    /// Frames before this one have all been through `expire`.
    first_pending: usize,
    /// Feedback for frames before this one is ignored (they were drawn
    /// before `restart_measuring`).
    measure_from: usize,
    pulse_pending: Option<f64>,
    run: Option<Run>,
    anomalies: u64,
}

impl Planner {
    pub fn new(cfg: Config) -> Self {
        Planner {
            fit: PeriodFit::new(cfg.nominal_period_s, cfg.fit_window),
            cfg,
            recs: Vec::new(),
            anchor: None,
            chain_dirty: false,
            chain_last: None,
            first_pending: 0,
            measure_from: 0,
            pulse_pending: None,
            run: None,
            anomalies: 0,
        }
    }

    /// The measured refresh period (the nominal one until there's enough
    /// feedback).
    pub fn period_s(&self) -> f64 {
        self.fit.period
    }

    /// The measured refresh period with its standard error, once a fit
    /// has been accepted. `None` means nothing has been measured, and
    /// `period_s()` is still just the nominal period.
    pub fn measured_period(&self) -> Option<PeriodEstimate> {
        self.fit.fitted.then_some(PeriodEstimate { period_s: self.fit.period, std_err_s: self.fit.std_err })
    }

    /// Before the run: start measuring again from a different nominal
    /// period, e.g. because the display reported no refresh rate (or a
    /// wrong one) and frames clearly reach the screen at another rate.
    /// The vsync grid, the period fit and the anomaly count start over.
    /// Frames already begun keep their ids, but reports still to come for
    /// them are ignored, since they were planned on the old grid.
    ///
    /// Returns false, and changes nothing, once `start_run` has been called
    /// or if the period isn't a positive number.
    pub fn restart_measuring(&mut self, nominal_period_s: f64) -> bool {
        let usable = nominal_period_s.is_finite() && nominal_period_s > 0.0;
        if !usable || self.run.is_some() || self.pulse_pending.is_some() {
            return false;
        }
        self.cfg.nominal_period_s = nominal_period_s;
        self.fit = PeriodFit::new(nominal_period_s, self.cfg.fit_window);
        self.anchor = None;
        self.chain_dirty = false;
        self.measure_from = self.recs.len();
        self.anomalies = 0;
        true
    }

    /// Enough feedback to trust the vsync grid and the measured period:
    /// start the run after this if you can.
    pub fn is_warmed_up(&self) -> bool {
        self.anchor.is_some() && self.fit.fitted && self.fit.pts.len() >= WARM_UP_POINTS
    }

    /// How many feedback reports have been used so far.
    pub fn feedback_points(&self) -> usize {
        self.fit.pts.len()
    }

    /// The first scanner pulse (host seconds). The next `begin_frame` is
    /// frame 0 of the run. Later calls are ignored.
    pub fn start_run(&mut self, pulse_s: f64) {
        if self.run.is_none() && self.pulse_pending.is_none() {
            self.pulse_pending = Some(pulse_s);
        }
    }

    /// Plan the frame we're about to draw. `now_s` is when we got its frame
    /// buffer.
    pub fn begin_frame(&mut self, now_s: f64) -> FramePlan {
        let id = self.recs.len() as u64;
        let mut target = self.last_prediction().map_or(0, |v| v + 1);
        match self.earliest_vsync(now_s) {
            Some(earliest) => target = target.max(earliest),
            // No vsync grid (no usable feedback yet, or ever): only a stall
            // far longer than CPU jitter moves the index on by more than one.
            None => {
                if let Some(prev) = self.recs.last() {
                    let gap = (now_s - prev.render_start) / self.fit.period;
                    if gap > BLIND_STALL_PERIODS {
                        let passed = gap.round() as i64;
                        target = target.max(prev.predicted.saturating_add(passed - 1));
                    }
                }
            }
        }
        let (planned, skipped, held) = self.plan_index(id, target);
        self.recs.push(Rec { render_start: now_s, predicted: target, planned, fb: Feedback::Pending });
        self.chain_last = Some(target);
        FramePlan { id, target_vsync: target, planned_index: planned, skipped, held }
    }

    /// One report from Metal: frame `id` was on screen at `presented_s`
    /// (0 = never shown). Reports may come in any order and late; repeats
    /// are ignored.
    pub fn on_feedback(&mut self, id: u64, presented_s: f64) {
        let Some(rec) = self.recs.get(id as usize).copied() else {
            self.anomalies += 1;
            return;
        };
        if (id as usize) < self.measure_from || !matches!(rec.fb, Feedback::Pending | Feedback::NoFeedback) {
            return;
        }
        let fb = if !(presented_s > 0.0 && presented_s.is_finite()) {
            Feedback::NotShown
        } else if presented_s + self.cfg.causality_tolerance_s < rec.render_start
            || presented_s - rec.render_start > self.cfg.max_present_latency_s
        {
            self.anomalies += 1;
            Feedback::Anomalous { presented_s }
        } else {
            // Which vsync was that? Count whole periods from the anchor.
            let vsync = match self.anchor {
                None => rec.predicted,
                Some(a) => a.vsync + ((presented_s - a.presented) / self.fit.period).round() as i64,
            };
            // Frames reach the screen in order, one per vsync at most: a
            // later frame on the same or an earlier vsync is impossible.
            let fifo_ok =
                self.anchor.is_none_or(|a| (id > a.id && vsync > a.vsync) || (id < a.id && vsync < a.vsync));
            if !fifo_ok {
                self.anomalies += 1;
                Feedback::Anomalous { presented_s }
            } else {
                if self.fit.push(vsync, presented_s) {
                    self.chain_dirty = true;
                }
                if self.anchor.is_none_or(|a| id > a.id) {
                    self.anchor = Some(Anchor { id, vsync, presented: presented_s });
                    self.chain_dirty = true;
                }
                Feedback::Shown { presented_s, vsync }
            }
        };
        self.recs[id as usize].fb = fb;
    }

    /// Marks frames that have waited longer than the timeout as having no
    /// feedback.
    pub fn expire(&mut self, now_s: f64) {
        let limit = now_s - self.cfg.feedback_timeout_s;
        while let Some(r) = self.recs.get_mut(self.first_pending) {
            if r.render_start > limit {
                break;
            }
            if r.fb == Feedback::Pending {
                r.fb = Feedback::NoFeedback;
            }
            self.first_pending += 1;
        }
    }

    pub fn feedback(&self, id: u64) -> Option<Feedback> {
        self.recs.get(id as usize).map(|r| r.fb)
    }

    /// True once no frame up to and including `last_id` is still waiting for
    /// feedback (it came in, or the frame was given up on by `expire`).
    pub fn settled_through(&self, last_id: u64) -> bool {
        let end = (last_id as usize).saturating_add(1).min(self.recs.len());
        let start = self.first_pending.min(end);
        self.recs[start..end].iter().all(|r| r.fb != Feedback::Pending)
    }

    /// Time per frame between frames that reached the screen one after the
    /// other (since the last `restart_measuring`). With every frame shown,
    /// these are one refresh period each; if the display only takes every
    /// second frame (e.g. a mirrored 60 Hz screen), two.
    ///
    /// Frames whose reports were rejected (`Anomalous`) are bridged: the gap
    /// across them is divided by the number of frames it spans. They were
    /// drawn and presented, and when the nominal period is far off (say 60
    /// Hz assumed on a 144 Hz display) every other report can be rejected,
    /// which would otherwise leave no gaps at all. Frames never shown or
    /// never reported break the chain.
    pub fn shown_intervals_s(&self) -> Vec<f64> {
        let mut out = Vec::new();
        let mut last: Option<(usize, f64)> = None;
        for (k, r) in self.recs.iter().enumerate().skip(self.measure_from) {
            match r.fb {
                Feedback::Shown { presented_s, .. } => {
                    if let Some((j, t)) = last {
                        out.push((presented_s - t) / (k - j) as f64);
                    }
                    last = Some((k, presented_s));
                }
                Feedback::Anomalous { .. } => {}
                _ => last = None,
            }
        }
        out
    }

    /// The predicted vsync of the last frame we began, re-chained from the
    /// newest real feedback when that changed.
    fn last_prediction(&mut self) -> Option<i64> {
        if self.chain_dirty {
            self.chain_dirty = false;
            if let Some(a) = self.anchor {
                let mut v = a.vsync;
                for k in (a.id as usize + 1)..self.recs.len() {
                    let earliest = self.earliest_vsync(self.recs[k].render_start).unwrap_or(i64::MIN);
                    v = (v + 1).max(earliest);
                }
                self.chain_last = Some(v);
            }
        }
        self.chain_last
    }

    /// The first vsync at or after `t - guard`: a frame we started drawing
    /// at `t` can't appear any earlier. `None` until there's feedback.
    fn earliest_vsync(&self, t: f64) -> Option<i64> {
        let a = self.anchor?;
        let p = self.fit.period;
        let guard = self.cfg.guard_frac * p;
        Some(a.vsync + ((t - guard - a.presented) / p).ceil() as i64)
    }

    /// The host time of vsync `vsync`, counted along the grid from the
    /// newest frame whose real time we know. For a frame's
    /// `FramePlan::target_vsync`, that's when it should be on screen. `None`
    /// until some feedback has come in.
    pub fn vsync_time(&self, vsync: i64) -> Option<f64> {
        let a = self.anchor?;
        Some(a.presented + (vsync - a.vsync) as f64 * self.fit.period)
    }

    /// The stimulus index for a frame predicted to land on vsync `target`,
    /// plus how many indices it skips and whether it repeats the last one.
    fn plan_index(&mut self, id: u64, target: i64) -> (Option<u64>, u64, bool) {
        let target_time = self.vsync_time(target);
        if let Some(pulse_s) = self.pulse_pending.take() {
            self.run = Some(Run {
                pulse_s,
                first_id: id,
                n0: target,
                t_origin: target_time,
                last_planned: 0,
                drift_corr: 0,
            });
            return (Some(0), 0, false);
        }
        let n0 = match self.run {
            Some(run) => run.n0,
            None => return (None, 0, false),
        };
        let origin_now = self.vsync_time(n0);
        let pacing = self.cfg.pacing;
        let limit = 0.5 + self.cfg.hysteresis_frames;
        let run = self.run.as_mut().expect("run checked above");
        if run.t_origin.is_none() {
            run.t_origin = origin_now;
        }
        let mut desired = target - run.n0 + run.drift_corr;
        if let (Pacing::TimeLocked { schedule_period_s }, Some(t0), Some(t)) =
            (pacing, run.t_origin, target_time)
        {
            let err = (t - t0) / schedule_period_s - desired as f64;
            if err.abs() > limit {
                let step = err.round() as i64;
                run.drift_corr += step;
                desired += step;
            }
        }
        // Never go backwards: if we're ahead, show the same index again.
        let last = run.last_planned;
        let planned = desired.max(last as i64) as u64;
        run.last_planned = planned;
        (Some(planned), planned.saturating_sub(last + 1), planned == last)
    }

    /// Frame id of the run's frame 0, once it has been drawn.
    pub fn run_first_id(&self) -> Option<u64> {
        self.run.map(|r| r.first_id)
    }

    /// The per-frame log from frame 0 of the run on.
    pub fn rows(&self) -> Vec<FrameRow> {
        let Some(run) = self.run else {
            return Vec::new();
        };
        let period = self.fit.period;
        let mut out = Vec::new();
        let mut prev_shown: Option<i64> = None;
        // Frames since the last shown one whose fate is unknown: each may
        // have used up a refresh, so they aren't counted as misses.
        let mut unknown_between = 0i64;
        let mut prev_planned: Option<u64> = None;
        for (k, r) in self.recs.iter().enumerate() {
            if k == self.measure_from {
                // Vsync numbers before a restart are on the old grid.
                prev_shown = None;
                unknown_between = 0;
            }
            let missed = match r.fb {
                Feedback::Shown { vsync, .. } => {
                    prev_shown.map(|pv| (vsync - pv - 1 - unknown_between).max(0) as u64)
                }
                _ => None,
            };
            match r.fb {
                Feedback::Shown { vsync, .. } => {
                    prev_shown = Some(vsync);
                    unknown_between = 0;
                }
                Feedback::NotShown => {}
                _ => unknown_between += 1,
            }
            if (k as u64) < run.first_id {
                continue;
            }
            let planned = r.planned.expect("run frames have an index");
            let (skipped, held) = match prev_planned {
                Some(p) => (planned.saturating_sub(p + 1), planned == p),
                None => (0, false),
            };
            prev_planned = Some(planned);
            let (presented_s, vsync_index, late_vsyncs, late_s) = match r.fb {
                Feedback::Shown { presented_s, vsync } => {
                    let vi = vsync - run.n0;
                    let (lv, ls) = match self.cfg.pacing {
                        Pacing::VsyncCount => {
                            let lv = vi - planned as i64;
                            (Some(lv), Some(lv as f64 * period))
                        }
                        Pacing::TimeLocked { schedule_period_s } => match run.t_origin {
                            Some(t0) => {
                                let ls = presented_s - (t0 + planned as f64 * schedule_period_s);
                                (Some((ls / period).round() as i64), Some(ls))
                            }
                            None => (None, None),
                        },
                    };
                    (Some(presented_s - run.pulse_s), Some(vi), lv, ls)
                }
                _ => (None, None, None, None),
            };
            out.push(FrameRow {
                id: k as u64,
                planned_index: planned,
                planned_vsync: r.predicted - run.n0,
                render_start_s: r.render_start - run.pulse_s,
                presented_s,
                vsync_index,
                missed_vsyncs: missed,
                late_vsyncs,
                late_s,
                skipped,
                held,
                status: match r.fb {
                    Feedback::Shown { .. } => Status::Shown,
                    Feedback::NotShown => Status::NotShown,
                    Feedback::NoFeedback => Status::NoFeedback,
                    Feedback::Pending => Status::Pending,
                    Feedback::Anomalous { .. } => Status::Anomalous,
                },
            });
        }
        out
    }

    /// Counts over every frame from the run's frame 0 on.
    pub fn summary(&self) -> Summary {
        Summary::of(&self.rows(), self.fit.period)
    }

    /// Feedback ignored as impossible, at any time (waiting frames
    /// included).
    pub fn anomalies(&self) -> u64 {
        self.anomalies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 1.0 / 120.0;

    /// New frames per second, from the median gap between frames on screen
    /// (so a few dropped frames don't skew it).
    fn refresh_hz(gaps: &[f64]) -> f64 {
        assert!(!gaps.is_empty(), "no gaps between frames on screen");
        let mut sorted = gaps.to_vec();
        sorted.sort_by(f64::total_cmp);
        1.0 / sorted[sorted.len() / 2]
    }

    /// Begin a frame at `t` and report it shown at `shown_at`.
    fn shown_frame(planner: &mut Planner, t: f64, shown_at: f64) -> FramePlan {
        let plan = planner.begin_frame(t);
        planner.on_feedback(plan.id, shown_at);
        plan
    }

    #[test]
    fn settled_through_waits_for_pending_frames_only() {
        let mut planner = Planner::new(Config::new(P));
        let a = planner.begin_frame(100.0);
        let b = planner.begin_frame(100.0 + P);
        assert!(!planner.settled_through(b.id));
        planner.on_feedback(a.id, 100.0 + 0.5 * P);
        assert!(planner.settled_through(a.id));
        assert!(!planner.settled_through(b.id));
        // Given up on after the timeout.
        planner.expire(100.0 + P + FEEDBACK_TIMEOUT_S);
        assert!(planner.settled_through(b.id));
        assert_eq!(planner.feedback(b.id), Some(Feedback::NoFeedback));
    }

    #[test]
    fn shown_intervals_skip_frames_without_a_time() {
        let mut planner = Planner::new(Config::new(P));
        shown_frame(&mut planner, 100.0, 100.0 + P);
        shown_frame(&mut planner, 100.0 + P, 100.0 + 2.0 * P);
        shown_frame(&mut planner, 100.0 + 2.0 * P, 0.0); // never shown
        shown_frame(&mut planner, 100.0 + 3.0 * P, 100.0 + 4.0 * P);
        let gaps = planner.shown_intervals_s();
        assert_eq!(gaps.len(), 1);
        assert!((gaps[0] - P).abs() < 1e-12);
    }

    #[test]
    fn shown_intervals_bridge_rejected_reports() {
        let mut planner = Planner::new(Config::new(P));
        shown_frame(&mut planner, 100.0, 100.0 + P);
        // On screen before it was drawn: impossible, so rejected.
        shown_frame(&mut planner, 100.0 + P, 100.0);
        shown_frame(&mut planner, 100.0 + 2.0 * P, 100.0 + 3.0 * P);
        assert_eq!(planner.anomalies(), 1);
        let gaps = planner.shown_intervals_s();
        assert_eq!(gaps.len(), 1);
        assert!((gaps[0] - P).abs() < 1e-12, "{gaps:?}");
    }

    #[test]
    fn far_too_fast_displays_still_show_their_rate() {
        // Guessed 60 Hz on a 144 Hz display: every other report is
        // rejected, so no two frames in a row are "shown".
        let mut planner = Planner::new(Config::new(2.0 * P));
        run_display(&mut planner, 200, 1.0 / 144.0);
        assert!(planner.anomalies() > 50);
        let gaps = planner.shown_intervals_s();
        assert!(gaps.len() >= 30, "only {} gaps", gaps.len());
        let seen = refresh_hz(&gaps);
        assert!((seen - 144.0).abs() < 0.01, "{seen}");
    }

    /// Feed `frames` frames from a display really refreshing every
    /// `true_period`, each shown 0.6 of a period after we start drawing it.
    fn run_display(planner: &mut Planner, frames: usize, true_period: f64) {
        for k in 0..frames {
            let t = 100.0 + k as f64 * true_period;
            shown_frame(planner, t, t + 0.6 * true_period);
        }
    }

    #[test]
    fn a_display_far_off_its_reported_rate_never_warms_up() {
        // Claims 120 Hz, really 100 Hz: the fitted period is 20 % off the
        // claim, so it's rejected and the vsync grid isn't trusted.
        let mut planner = Planner::new(Config::new(P));
        run_display(&mut planner, 300, 1.0 / 100.0);
        assert!(!planner.is_warmed_up());
        assert_eq!(planner.period_s(), P);
    }

    #[test]
    fn a_display_twice_as_fast_as_reported_shows_in_the_frame_gaps() {
        // Claims 60 Hz, really 120 Hz. The planner needs the claim to be
        // within 10 %, so its numbers are wrong here, but it isn't silent:
        // some reports are rejected as impossible (two frames on one
        // refresh), and the gaps between frames on screen show the real rate,
        // which doesn't match the fitted one.
        let mut planner = Planner::new(Config::new(2.0 * P));
        run_display(&mut planner, 300, P);
        assert!(planner.anomalies() > 0);
        let seen_hz = refresh_hz(&planner.shown_intervals_s());
        assert!((seen_hz - 120.0).abs() < 0.01, "{seen_hz}");
        let fitted_hz = 1.0 / planner.period_s();
        assert!((fitted_hz - seen_hz).abs() / seen_hz > 0.005, "{fitted_hz}");
    }

    #[test]
    fn a_display_half_as_fast_as_reported_shows_in_the_frame_gaps() {
        // Claims 120 Hz, really 60 Hz: it looks like a 120 Hz display that
        // drops every other frame. The period fits the claim, but the gaps
        // between frames on screen give it away.
        let mut planner = Planner::new(Config::new(P));
        run_display(&mut planner, 300, 2.0 * P);
        assert!(planner.is_warmed_up());
        let gaps = planner.shown_intervals_s();
        let hz = refresh_hz(&gaps);
        assert!((hz - 60.0).abs() < 0.01, "{hz}");
        assert!((1.0 / planner.period_s() - 120.0).abs() < 0.01);
    }

    #[test]
    fn a_wrong_nominal_rate_is_fixed_by_measuring_again() {
        // The display reported no rate, so we guessed 60 Hz; it runs at 120.
        let mut planner = Planner::new(Config::new(2.0 * P));
        run_display(&mut planner, 120, P);
        let gaps = planner.shown_intervals_s();
        assert!(gaps.len() >= 30, "only {} gaps", gaps.len());
        let seen = refresh_hz(&gaps);
        assert!((seen - 120.0).abs() < 0.01, "{seen}");
        assert!(planner.anomalies() > 0);

        // Frame 119's report is still on its way when we restart.
        let in_flight = planner.begin_frame(100.0 + 120.0 * P);
        assert!(planner.restart_measuring(1.0 / seen));
        assert_eq!(planner.anomalies(), 0);
        assert_eq!(planner.measured_period(), None);
        assert!(planner.shown_intervals_s().is_empty());
        planner.on_feedback(in_flight.id, 100.0 + 120.6 * P);
        assert_eq!(planner.feedback(in_flight.id), Some(Feedback::Pending));

        for k in 121..400 {
            let t = 100.0 + k as f64 * P;
            if k == 300 {
                planner.start_run(t - 0.001);
            }
            shown_frame(&mut planner, t, t + 0.6 * P);
        }
        assert!(planner.is_warmed_up());
        assert_eq!(planner.anomalies(), 0);
        assert!((planner.period_s() / P - 1.0).abs() < 1e-9);
        let rows = planner.rows();
        assert_eq!(rows.len(), 100);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.planned_index, i as u64);
            assert_eq!(r.missed_vsyncs, Some(0), "frame {}", r.id);
            assert_eq!(r.late_vsyncs, Some(0), "frame {}", r.id);
        }
        // Too late once the run has started.
        assert!(!planner.restart_measuring(P));
    }

    #[test]
    fn restarting_refuses_a_period_that_cant_be_real() {
        let mut planner = Planner::new(Config::new(P));
        for bad in [0.0, -P, f64::NAN, f64::INFINITY] {
            assert!(!planner.restart_measuring(bad), "{bad}");
        }
        assert_eq!(planner.period_s(), P);
    }

    #[test]
    fn the_measured_period_comes_with_its_uncertainty() {
        let mut planner = Planner::new(Config::new(P));
        assert_eq!(planner.measured_period(), None);
        run_display(&mut planner, 300, P);
        let clean = planner.measured_period().unwrap();
        assert!((clean.period_s / P - 1.0).abs() < 1e-9);
        assert!(clean.std_err_s < 1e-12, "{clean:?}");

        // presentedTime scattered by up to ±1 ms (uniform, SD 0.577 ms).
        let mut planner = Planner::new(Config::new(P));
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let n = 1024;
        for k in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let noise = ((x >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2e-3;
            let t = 100.0 + k as f64 * P;
            shown_frame(&mut planner, t, t + 0.6 * P + noise);
        }
        let noisy = planner.measured_period().unwrap();
        // What least squares should give: SD / sqrt(Σ(v - mean)²).
        let nf = n as f64;
        let expected = 0.577e-3 / (nf * (nf * nf - 1.0) / 12.0).sqrt();
        let ratio = noisy.std_err_s / expected;
        assert!((0.7..1.4).contains(&ratio), "{noisy:?}, expected {expected:e}");
        let err = (noisy.period_s - P).abs();
        assert!(err < 4.0 * noisy.std_err_s, "{err:e} vs {noisy:?}");
    }

    #[test]
    fn without_feedback_only_a_real_stall_skips() {
        let mut planner = Planner::new(Config::new(P));
        let mut t = 100.0;
        planner.start_run(t - 0.001);
        let mut plans = Vec::new();
        for k in 0..60 {
            // Uneven hand-overs like the compositor's (4.5 ms, 12 ms, ...),
            // a 2-refresh hiccup at 20 and a 10-refresh stall at 40.
            t += match k {
                20 => 2.0 * P,
                40 => 10.0 * P,
                _ if k % 2 == 0 => 0.0045,
                _ => 2.0 * P - 0.0045,
            };
            plans.push(planner.begin_frame(t));
        }
        let skipped: Vec<(u64, u64)> =
            plans.iter().filter(|p| p.skipped > 0).map(|p| (p.id, p.skipped)).collect();
        // 10 refreshes went by: 9 more than usual, minus one to be safe.
        assert_eq!(skipped, vec![(40, 8)]);
        assert!(plans.iter().all(|p| !p.held));
    }

    #[test]
    fn rows_report_the_planned_vsync() {
        let mut planner = Planner::new(Config::new(P));
        for k in 0..100 {
            let t = 100.0 + k as f64 * P;
            if k == 80 {
                planner.start_run(t - 0.001);
            }
            shown_frame(&mut planner, t, t + 0.6 * P);
        }
        let rows = planner.rows();
        assert_eq!(rows.len(), 20);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.planned_index, i as u64);
            assert_eq!(r.planned_vsync, i as i64);
            assert_eq!(r.vsync_index, Some(i as i64));
        }
    }
}
