//! The run state machine: what to draw on each frame, and what happened.
//!
//! This has no GPU or hardware code so it can be tested with synthetic frame
//! and input times. All times are `f64` seconds on one monotonic clock (the
//! "run clock"); `t0` is the run-clock time of the first kept trigger.
//!
//! Frame timing model. Each frame, the caller says when it got the frame
//! buffer (`acquired`) and, if it knows, when that frame will be on screen
//! (`predicted_flip`, from the frame planner). The frame shows whatever is
//! planned for its flip.
//!
//! With feedback (`enable_feedback`), the display later reports when each
//! frame really was on screen (`presented`), and those times become the
//! measured onsets and ends. A frame that was never shown, or whose report
//! doesn't come within `FEEDBACK_TIMEOUT_S`, falls back to the estimate
//! below. Frame records are held until their report is in.
//!
//! Without feedback, the estimate is all there is: under vsync, acquiring
//! the next frame blocks until the previous one is on screen, so frame k's
//! flip is taken as the moment frame k+1 was acquired, and a frame is
//! predicted to flip one refresh period after it was acquired.
//!
//! Either way, the photodiode rig measures the true display latency on top.

use crate::experiment::Experiment;
use crate::scanner::{DEFAULT_QUIET_S, PulseWatch};
use crate::scene::{self, Item, Scene};
use crate::schedule::{PlannedEvent, Timeline};
use crate::template::Vars;
use std::collections::{HashMap, VecDeque};

/// With feedback, how long to wait for a frame's report (seconds after it
/// was acquired) before estimating its flip instead. Metal reports within a
/// few milliseconds; this only catches reports that never come.
pub const FEEDBACK_TIMEOUT_S: f64 = crate::planner::FEEDBACK_TIMEOUT_S;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Showing instructions until the scanner's first (kept) trigger starts
    /// the run, so nobody has to touch the stimulus computer. The operator
    /// can press Space to move on to the waiting screen early.
    Instructions,
    /// Waiting for the first (non-dummy) trigger.
    Waiting,
    Running,
    /// The stimulus is over but the scanner is still sending pulses: the
    /// final screen stays up, and pulses are still logged, until it stops.
    Holding,
    Done(EndReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    Aborted,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Completed => "completed",
            EndReason::Aborted => "aborted",
        }
    }
}

/// One finished event: what was planned vs. what actually happened.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub trial: usize,
    pub event: String,
    pub trial_type: String,
    pub stimulus: String,
    pub log: bool,
    /// Seconds after t0.
    pub planned_onset: f64,
    pub planned_duration: f64,
    /// Measured flip time of the first frame, seconds after t0 (None if the
    /// event was shorter than a frame and never reached the screen).
    pub onset: Option<f64>,
    pub duration: Option<f64>,
    pub frames: u64,
    pub response: Option<String>,
    /// Seconds from the measured onset to the response.
    pub response_time: Option<f64>,
    pub correct: Option<bool>,
    pub outcome: Option<String>,
    pub vars: Vars,
}

impl EventRecord {
    pub fn onset_delay_ms(&self) -> Option<f64> {
        self.onset.map(|o| (o - self.planned_onset) * 1000.0)
    }
}

/// One drawn frame.
#[derive(Debug, Clone)]
pub struct FrameRecord {
    pub index: u64,
    /// Run-clock time the swapchain image was acquired.
    pub acquired: f64,
    /// Time since the previous acquire, seconds.
    pub interval: f64,
    /// Refreshes missed before this frame (0 when on time). With feedback,
    /// from the gap since the previous frame on screen; without, from the
    /// gap between acquires.
    pub missed: u32,
    pub phase: Phase,
    /// Index into `Timeline::events` of the event drawn, if any.
    pub event: Option<usize>,
    pub photodiode: bool,
    /// What the display said about this frame.
    pub report: Report,
}

/// What the display reported about one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Report {
    /// No reports on this system: flips are estimated.
    Off,
    /// Still waiting for it.
    Pending,
    /// On screen at this run-clock time.
    Shown(f64),
    /// The display says this frame was never shown.
    NotShown,
    /// No report came within `FEEDBACK_TIMEOUT_S`.
    Missing,
}

impl Report {
    /// When the frame was on screen, if the display said so.
    pub fn presented(self) -> Option<f64> {
        match self {
            Report::Shown(t) => Some(t),
            _ => None,
        }
    }

    /// A short name for log files.
    pub fn label(self) -> &'static str {
        match self {
            Report::Off => "n/a",
            Report::Pending => "pending",
            Report::Shown(_) => "shown",
            Report::NotShown => "not_shown",
            Report::Missing => "no_report",
        }
    }
}

/// A frame record waiting for its report.
#[derive(Debug, Clone)]
struct Held {
    record: FrameRecord,
    /// This frame's predicted flip; its fallback if no later frame is acquired.
    predicted: f64,
    /// When the next frame was acquired: the estimated flip of this one.
    next_acquired: Option<f64>,
    /// False for frames drawn after the run was over (while waiting for the
    /// last reports): they aren't logged.
    logged: bool,
}

/// What `Run::input` did with an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputUse {
    /// Handled as usual.
    Normal,
    /// A scanner pulse from a scan that was already under way when Onset got
    /// ready (see `Run::ready`): not counted, and doesn't start the run.
    PreviousScan,
}

/// Something about the scanner the operator should hear about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Notice {
    /// A pulse was ignored because a scan was already under way when Onset
    /// got ready. The run starts on the first pulse after `quiet_s` without any.
    PulseIgnored { quiet_s: f64 },
    /// The scan that was under way has stopped: ready for the next one.
    ReadyAgain,
    /// No pulse for `quiet_s` seconds during the run (last one at `last_pulse`
    /// seconds after t0): the scan may have stopped before the stimulus.
    ScanQuiet { last_pulse: f64, quiet_s: f64 },
    /// The stimulus is over but pulses are still coming: holding the final
    /// screen until there's none for `quiet_s` seconds.
    Holding { quiet_s: f64 },
    /// The scanner stopped while the final screen was held.
    ScannerStopped,
    /// The operator stopped waiting for the scanner.
    HoldStopped,
}

/// How the scan lined up with the stimulus. Seconds after t0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanCoverage {
    /// The TR, from the gaps between pulses.
    pub tr: f64,
    /// The last pulse, plus one TR for its volume to be acquired.
    pub scanned_until: f64,
    /// When the stimulus was planned to end.
    pub stimulus_end: f64,
}

impl ScanCoverage {
    /// The scan stopped more than half a TR before the stimulus did, so the
    /// end of the run wasn't scanned.
    pub fn stopped_early(&self) -> bool {
        self.scanned_until < self.stimulus_end - 0.5 * self.tr
    }
}

/// What the renderer should draw for one frame.
pub struct FramePlan<'a> {
    pub scene: &'a Scene,
    pub photodiode: bool,
}

/// A trigger or button press, already mapped from raw keys/bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Trigger,
    Button(String),
    /// Operator: continue past the instructions.
    Continue,
    /// Operator: stop the run now (data is saved).
    Abort,
}

#[derive(Debug, Default, Clone)]
struct EventState {
    first_frame: Option<u64>,
    onset: Option<f64>,
    last_frame: Option<u64>,
    end: Option<f64>,
    shown: Option<String>,
    response: Option<(String, f64)>,
    window_open: bool,
    window_closed: bool,
    outcome: Option<String>,
    finalized: bool,
}

#[derive(Debug, Clone, Copy)]
enum FlipUse {
    Onset(usize),
    End(usize),
}

/// A response window that just opened (used by the simulated participant).
#[derive(Debug, Clone)]
pub struct StartedResponse {
    pub buttons: Vec<String>,
    pub correct: Option<String>,
    pub window: f64,
}

pub struct Status {
    pub phase: Phase,
    pub t: Option<f64>,
    pub end: f64,
    pub triggers: u32,
    pub trial: usize,
    pub trials: usize,
    pub event: Option<String>,
    pub missed_frames: u64,
    pub last_response: Option<(String, Option<f64>)>,
    pub responses: u32,
    pub correct: u32,
    pub scored: u32,
}

pub struct Run<'e> {
    exp: &'e Experiment,
    timeline: Timeline,
    pub phase: Phase,
    pub t0: Option<f64>,
    /// Every trigger time (run clock), including dummies.
    pub triggers: Vec<f64>,
    period: f64,
    period_locked: bool,
    intervals: Vec<f64>,
    flash_frames: u64,
    photodiode_until: u64,

    states: Vec<EventState>,
    next_event: usize,
    current: Option<usize>,
    trial_outcome: HashMap<usize, String>,

    scenes: HashMap<(usize, String), Scene>,
    instructions: Scene,
    waiting: Scene,
    frame_scene: Scene,
    /// Flip time of the first frame; the welcome animation's clock starts here.
    first_flip: Option<f64>,

    last_acquire: Option<f64>,
    last_index: Option<u64>,
    /// Predicted flip of the last frame.
    last_flip: Option<f64>,
    /// Event onsets and ends waiting for the flip of frame N.
    flip_uses: Vec<(u64, FlipUse)>,
    pub missed_frames: u64,
    last_response: Option<(String, Option<f64>)>,
    pending_records: Vec<EventRecord>,
    pending_frames: Vec<FrameRecord>,
    started_responses: Vec<StartedResponse>,
    responses: u32,
    correct: u32,
    scored: u32,

    // Feedback: the display reports when each frame was on screen.
    feedback: bool,
    /// The refresh period measured from the display's reports, if any.
    measured_period: Option<f64>,
    /// Frame records waiting for their report (and those after them), oldest first.
    held: VecDeque<Held>,
    /// Last frame on screen, for counting missed refreshes...
    prev_shown: Option<f64>,
    /// ...and frames since then whose fate is unknown (each may have used a refresh).
    unknown_since_shown: i64,
    /// Onsets and ends that had to be estimated because a frame's report
    /// never came or the frame wasn't shown.
    pub flip_estimates: u64,

    // Following the scanner.
    /// When Onset could first hear pulses (see `ready`).
    ready_at: Option<f64>,
    /// Every pulse heard before the run's first, to tell if a scan was under way.
    before_first: PulseWatch,
    /// The run's own pulses, to tell when its scan stops.
    scan: PulseWatch,
    /// Pulses ignored as the tail of a scan that was under way at `ready`.
    pub ignored_triggers: u32,
    announce_ready: bool,
    quiet_warned: bool,
    notices: Vec<Notice>,
}

impl<'e> Run<'e> {
    pub fn new(exp: &'e Experiment, timeline: Timeline, nominal_hz: f64, flash_frames: u32) -> Run<'e> {
        let instructions = exp.experiment.instructions.as_deref();
        let n = timeline.events.len();
        // With a [welcome] table, the screens before t0 get its animated background.
        let screen = |text: &str, waiting: bool| {
            Scene::welcome(exp, text, waiting).unwrap_or_else(|| Scene::text(exp, text))
        };
        Run {
            exp,
            phase: if instructions.is_some() { Phase::Instructions } else { Phase::Waiting },
            instructions: screen(instructions.unwrap_or(""), false),
            waiting: screen(&exp.experiment.waiting_text, true),
            frame_scene: Scene { background: exp.experiment.background, items: vec![] },
            first_flip: None,
            timeline,
            t0: None,
            triggers: Vec::new(),
            period: 1.0 / nominal_hz,
            period_locked: false,
            intervals: Vec::new(),
            flash_frames: flash_frames as u64,
            photodiode_until: 0,
            states: vec![EventState::default(); n],
            next_event: 0,
            current: None,
            trial_outcome: HashMap::new(),
            scenes: HashMap::new(),
            last_acquire: None,
            last_index: None,
            last_flip: None,
            flip_uses: Vec::new(),
            missed_frames: 0,
            last_response: None,
            pending_records: Vec::new(),
            pending_frames: Vec::new(),
            started_responses: Vec::new(),
            responses: 0,
            correct: 0,
            scored: 0,
            feedback: false,
            measured_period: None,
            held: VecDeque::new(),
            prev_shown: None,
            unknown_since_shown: 0,
            flip_estimates: 0,
            ready_at: None,
            before_first: PulseWatch::new(DEFAULT_QUIET_S),
            scan: PulseWatch::new(DEFAULT_QUIET_S),
            ignored_triggers: 0,
            announce_ready: false,
            quiet_warned: false,
            notices: Vec::new(),
        }
    }

    /// The display will report when each frame was on screen (`presented`).
    /// Call before the first frame.
    pub fn enable_feedback(&mut self) {
        self.feedback = true;
    }

    pub fn has_feedback(&self) -> bool {
        self.feedback
    }

    /// The refresh period measured from the display's reports. The run's
    /// period is locked to it at t0 (otherwise to the median gap between
    /// acquires).
    pub fn set_measured_period(&mut self, period: Option<f64>) {
        self.measured_period = period.filter(|p| p.is_finite() && *p > 0.0);
    }

    /// Onset can hear scanner pulses from run-clock time `t`. A pulse that
    /// comes before the run's first one and within the scanner's quiet time
    /// (`PulseWatch::stopped_after_s`) of `t` or of the pulse before it
    /// belongs to a scan that was already under way, and is ignored. Without
    /// this call (e.g. with a simulated scanner Onset starts itself), every
    /// pulse counts.
    pub fn ready(&mut self, t: f64) {
        self.ready_at = Some(t);
    }

    /// Things to tell the operator since the last call.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    /// How the scan lined up with the stimulus: `None` without t0 or with
    /// fewer than two pulses (no TR).
    pub fn scan_coverage(&self) -> Option<ScanCoverage> {
        let (t0, tr, last) = (self.t0?, self.scan.tr_s()?, self.scan.last_s()?);
        Some(ScanCoverage { tr, scanned_until: last - t0 + tr, stimulus_end: self.timeline.end })
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Show the participant's name on the welcome screen (if the experiment
    /// has one). The name is only drawn: no record, frame or log entry this
    /// run produces contains it.
    pub fn set_participant_name(&mut self, name: &str) {
        let name = name.trim();
        let name = (!name.is_empty()).then(|| name.to_string());
        for scene in [&mut self.instructions, &mut self.waiting] {
            for item in &mut scene.items {
                if let Item::Welcome(w) = item {
                    w.name.clone_from(&name);
                }
            }
        }
    }

    /// Current estimate of the refresh period (seconds).
    pub fn period(&self) -> f64 {
        self.period
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done(_))
    }

    /// Event records finished since the last call.
    pub fn take_records(&mut self) -> Vec<EventRecord> {
        std::mem::take(&mut self.pending_records)
    }

    /// Response windows opened since the last call.
    pub fn take_started_responses(&mut self) -> Vec<StartedResponse> {
        std::mem::take(&mut self.started_responses)
    }

    /// Every scene this run can show, so the renderer can prepare text and
    /// images before t = 0 instead of on a stimulus's first frame.
    pub fn all_scenes(&mut self) -> Vec<Scene> {
        let mut keys = vec![(0, self.exp.experiment.baseline.clone())];
        for e in &self.timeline.events {
            let mut names = vec![e.stimulus.clone()];
            if e.stimulus.contains("{outcome}") {
                names = ["correct", "incorrect", "miss", "response"]
                    .iter()
                    .map(|o| e.stimulus.replace("{outcome}", o))
                    .filter(|n| self.exp.stimulus(n).is_some())
                    .collect();
            }
            if let Some(r) = &e.response {
                names.push(r.after_response.clone());
            }
            keys.extend(names.into_iter().map(|n| (e.trial, n)));
        }
        let mut scenes = vec![self.instructions.clone(), self.waiting.clone()];
        for key in keys {
            self.ensure_scene(&key);
            scenes.push(self.scenes[&key].clone());
        }
        scenes
    }

    /// Frame records ready for the log since the last call, in order. With
    /// feedback, a frame's record waits until its report is in (or given up on).
    pub fn take_frames(&mut self) -> Vec<FrameRecord> {
        std::mem::take(&mut self.pending_frames)
    }

    /// Handle an input. `t` is the run-clock time it happened.
    pub fn input(&mut self, input: Input, t: f64) -> InputUse {
        match input {
            Input::Trigger => {
                let waiting = matches!(self.phase, Phase::Instructions | Phase::Waiting);
                // Until this run's scan has sent its first pulse: is a scan
                // already under way? Once one pulse is taken, the rest of
                // its scan (dummies included) belongs to this run.
                if waiting
                    && self.triggers.is_empty()
                    && let Some(ready) = self.ready_at
                {
                    let quiet = self.before_first.clear_to_start(ready, t);
                    self.before_first.pulse(t);
                    if !quiet {
                        self.ignored_triggers += 1;
                        // Once per run of ignored pulses; `ReadyAgain` ends it.
                        if !self.announce_ready {
                            let quiet_s = self.before_first.stopped_after_s();
                            self.notices.push(Notice::PulseIgnored { quiet_s });
                        }
                        self.announce_ready = true;
                        return InputUse::PreviousScan;
                    }
                }
                self.triggers.push(t);
                if !self.is_done() {
                    self.scan.pulse(t);
                }
                if waiting && self.triggers.len() as u32 > self.exp.scanner.dummy_volumes {
                    self.t0 = Some(t);
                    self.phase = Phase::Running;
                    self.lock_period();
                }
            }
            Input::Continue => {
                if self.phase == Phase::Instructions {
                    self.phase = Phase::Waiting;
                }
            }
            Input::Abort => match self.phase {
                // The stimulus already ran to the end: stopping the wait
                // for the scanner doesn't make the run aborted.
                Phase::Holding => {
                    self.phase = Phase::Done(EndReason::Completed);
                    self.notices.push(Notice::HoldStopped);
                }
                Phase::Done(_) => {}
                _ => self.finish(EndReason::Aborted, self.last_acquire.unwrap_or(t) + self.period),
            },
            Input::Button(button) => {
                if self.phase == Phase::Running {
                    self.response(button, t);
                }
            }
        }
        InputUse::Normal
    }

    fn response(&mut self, button: String, t: f64) {
        // The most recent started event whose window is open and accepts this button.
        let Some(i) = (0..self.states.len()).rev().find(|&i| {
            let s = &self.states[i];
            s.window_open
                && s.response.is_none()
                && self.timeline.events[i].response.as_ref().is_some_and(|r| r.buttons.contains(&button))
        }) else {
            return;
        };
        let ev = &self.timeline.events[i];
        let r = ev.response.as_ref().expect("window_open implies a response def");
        let outcome = match &r.correct {
            Some(c) if *c == button => "correct",
            Some(_) => "incorrect",
            None => "response",
        };
        self.responses += 1;
        if r.correct.is_some() {
            self.scored += 1;
            if outcome == "correct" {
                self.correct += 1;
            }
        }
        let rt = self.states[i].onset.map(|o| t - o);
        self.last_response = Some((button.clone(), rt));
        self.trial_outcome.insert(ev.trial, outcome.to_string());
        let s = &mut self.states[i];
        s.response = Some((button, t));
        s.outcome = Some(outcome.to_string());
        s.window_open = false;
        s.window_closed = true;
    }

    fn lock_period(&mut self) {
        if let Some(p) = self.measured_period {
            self.period = p;
        } else if self.intervals.len() >= 30 {
            let mut v: Vec<f64> = self.intervals.iter().rev().take(240).copied().collect();
            v.sort_by(f64::total_cmp);
            self.period = v[v.len() / 2];
        }
        self.period_locked = true;
    }

    /// Call right after acquiring frame `index` at run-clock time `acquired`.
    /// `predicted_flip` is when this frame will be on screen (run clock), if
    /// the frame planner knows; otherwise one period after `acquired` is
    /// assumed. Returns what to draw.
    pub fn frame(&mut self, index: u64, acquired: f64, predicted_flip: Option<f64>) -> FramePlan<'_> {
        let done_before = self.is_done();
        let mut missed = 0;
        let mut interval = 0.0;
        if let Some(prev) = self.last_acquire {
            interval = acquired - prev;
            // Without feedback, the previous frame reached the screen about
            // now, and a long gap means refreshes were missed. With
            // feedback, missed refreshes come from the reported times.
            if !self.feedback && self.phase == Phase::Running && interval > 1.5 * self.period {
                missed = ((interval / self.period).round() as u32).saturating_sub(1);
                self.missed_frames += missed as u64;
            }
            if !self.period_locked && interval > 0.002 && interval < 0.1 {
                self.intervals.push(interval);
                if self.intervals.len() > 600 {
                    self.intervals.drain(..300);
                }
            }
        }
        self.last_acquire = Some(acquired);
        self.last_index = Some(index);
        if self.feedback {
            if let Some(prev) = self.held.back_mut() {
                prev.next_acquired = Some(acquired);
            }
        } else {
            for (_, u) in std::mem::take(&mut self.flip_uses) {
                self.set_flip(u, acquired);
            }
        }
        self.update(acquired);

        let flip = predicted_flip.unwrap_or(acquired + self.period);
        self.last_flip = Some(flip);
        let mut photodiode = false;
        let mut shown_event = None;

        if self.phase == Phase::Running {
            let t0 = self.t0.expect("running implies t0");
            let t = flip - t0;
            let half = self.period / 2.0;

            self.close_windows(t);

            // Start every event whose onset is closest to this frame.
            let mut started = None;
            while self.next_event < self.timeline.events.len()
                && self.timeline.events[self.next_event].onset <= t + half
            {
                started = Some(self.next_event);
                self.next_event += 1;
            }
            if let Some(i) = started {
                self.end_current(index);
                self.start_event(i, index);
                photodiode = self.flash_frames > 0;
                self.photodiode_until = index + self.flash_frames;
            } else if let Some(i) = self.current {
                let e = &self.timeline.events[i];
                if t >= e.onset + e.duration - half {
                    self.end_current(index);
                }
            }
            photodiode |= index < self.photodiode_until;

            if t >= self.timeline.end - half && self.next_event >= self.timeline.events.len() {
                self.end_current(index);
                // Still scanning? Then the scan's last volumes get the final
                // screen, and its last pulses can't start another run.
                let scanning = self.scan.last_s().is_some() && !self.scan.stopped(acquired);
                self.finish(EndReason::Completed, flip);
                if scanning {
                    self.phase = Phase::Holding;
                    self.notices.push(Notice::Holding { quiet_s: self.scan.stopped_after_s() });
                }
            }
            shown_event = self.current;
        }

        let record = FrameRecord {
            index,
            acquired,
            interval,
            missed,
            phase: self.phase,
            event: shown_event,
            photodiode,
            report: if self.feedback { Report::Pending } else { Report::Off },
        };
        if self.feedback {
            self.held.push_back(Held { record, predicted: flip, next_acquired: None, logged: !done_before });
        } else {
            self.pending_frames.push(record);
        }

        let scene = match self.phase {
            Phase::Instructions | Phase::Waiting => {
                // Welcome animation time comes from flip times, like checkerboard reversals.
                let start = *self.first_flip.get_or_insert(flip);
                let screen = if self.phase == Phase::Instructions {
                    &mut self.instructions
                } else {
                    &mut self.waiting
                };
                screen.animate(flip - start);
                &*screen
            }
            Phase::Running | Phase::Holding | Phase::Done(_) => {
                let t = flip - self.t0.unwrap_or(flip);
                let (key, onset) = match shown_event {
                    Some(i) => {
                        let s = &self.states[i];
                        let e = &self.timeline.events[i];
                        let name = match (&e.response, &s.response) {
                            (Some(r), Some(_)) if r.end_on_response => r.after_response.clone(),
                            _ => s.shown.clone().unwrap_or_default(),
                        };
                        ((e.trial, name), e.onset)
                    }
                    None => ((0, self.exp.experiment.baseline.clone()), 0.0),
                };
                self.ensure_scene(&key);
                self.frame_scene.clone_from(&self.scenes[&key]);
                self.frame_scene.animate(t - onset);
                &self.frame_scene
            }
        };
        FramePlan { scene, photodiode }
    }

    fn ensure_scene(&mut self, key: &(usize, String)) {
        if self.scenes.contains_key(key) {
            return;
        }
        let exp = self.exp;
        let trials = &self.timeline.trials;
        self.scenes.entry(key.clone()).or_insert_with_key(|(trial, name)| {
            let empty = Vars::new();
            let vars = trials.get(trial.wrapping_sub(1)).map_or(&empty, |t| &t.vars);
            // The schedule compiler already checked every stimulus builds.
            scene::build(exp, name, vars).unwrap_or_else(|e| {
                eprintln!("onset: {e}");
                Scene { background: exp.experiment.background, items: vec![] }
            })
        });
    }

    fn start_event(&mut self, i: usize, frame: u64) {
        let e: &PlannedEvent = &self.timeline.events[i];
        let trial = e.trial;
        let has_response = e.response.is_some();
        let mut shown = e.stimulus.clone();
        if shown.contains("{outcome}") {
            // Settle this trial's outcome now; an unanswered window is a miss.
            for j in 0..self.states.len() {
                if self.timeline.events[j].trial == trial && self.states[j].window_open {
                    self.close_window(j);
                }
            }
            let outcome = self.trial_outcome.get(&trial).cloned().unwrap_or_else(|| "miss".into());
            shown = shown.replace("{outcome}", &outcome);
        }
        let s = &mut self.states[i];
        s.first_frame = Some(frame);
        s.shown = Some(shown);
        s.window_open = has_response;
        if let Some(r) = &self.timeline.events[i].response {
            self.started_responses.push(StartedResponse {
                buttons: r.buttons.clone(),
                correct: r.correct.clone(),
                window: r.window,
            });
        }
        self.flip_uses.push((frame, FlipUse::Onset(i)));
        self.current = Some(i);

        // Events squeezed out by this one never reached the screen.
        for j in 0..i {
            if self.states[j].first_frame.is_none() && !self.states[j].finalized {
                let sj = &mut self.states[j];
                sj.shown = Some(self.timeline.events[j].stimulus.clone());
                sj.window_closed = true;
            }
        }
    }

    fn end_current(&mut self, frame: u64) {
        if let Some(i) = self.current.take() {
            self.states[i].last_frame = Some(frame.saturating_sub(1));
            self.flip_uses.push((frame, FlipUse::End(i)));
        }
    }

    fn set_flip(&mut self, u: FlipUse, t: f64) {
        match u {
            FlipUse::Onset(i) => self.states[i].onset = Some(t),
            FlipUse::End(i) => self.states[i].end = Some(t),
        }
    }

    fn close_window(&mut self, i: usize) {
        let s = &mut self.states[i];
        if s.window_open {
            s.window_open = false;
            s.window_closed = true;
            if s.response.is_none() {
                s.outcome = Some("miss".into());
                let trial = self.timeline.events[i].trial;
                self.trial_outcome.entry(trial).or_insert_with(|| "miss".into());
                if self.timeline.events[i].response.as_ref().is_some_and(|r| r.correct.is_some()) {
                    self.scored += 1;
                }
            }
        }
    }

    fn close_windows(&mut self, t: f64) {
        for i in 0..self.states.len() {
            if self.states[i].window_open {
                let e = &self.timeline.events[i];
                let window = e.response.as_ref().map_or(0.0, |r| r.window);
                if t >= e.onset + window - self.period / 2.0 {
                    self.close_window(i);
                }
            }
        }
    }

    fn finalize_ready(&mut self) {
        let t0 = match self.t0 {
            Some(t0) => t0,
            None => return,
        };
        for i in 0..self.states.len() {
            let s = &self.states[i];
            let shown_and_ended = s.first_frame.is_some() && s.end.is_some() && s.onset.is_some();
            let never_shown = s.first_frame.is_none() && s.shown.is_some();
            if s.finalized || !(shown_and_ended || never_shown) {
                continue;
            }
            let has_window = self.timeline.events[i].response.is_some();
            if has_window && !s.window_closed && !never_shown {
                continue;
            }
            self.emit(i, t0);
        }
    }

    fn emit(&mut self, i: usize, t0: f64) {
        let e = &self.timeline.events[i];
        let s = &mut self.states[i];
        s.finalized = true;
        let frames = match (s.first_frame, s.last_frame) {
            (Some(a), Some(b)) if b >= a => b - a + 1,
            _ => 0,
        };
        let onset = s.onset.map(|o| o - t0);
        let duration = match (s.onset, s.end) {
            (Some(a), Some(b)) => Some(b - a),
            _ => None,
        };
        let r = e.response.as_ref();
        let (response, response_time) = match (&s.response, s.onset) {
            (Some((b, t)), Some(o)) => (Some(b.clone()), Some(t - o)),
            (Some((b, _)), None) => (Some(b.clone()), None),
            _ => (None, None),
        };
        let correct = match (r.and_then(|r| r.correct.as_ref()), &s.outcome) {
            (Some(_), Some(o)) => Some(o == "correct"),
            _ => None,
        };
        let vars = self.timeline.trials.get(e.trial - 1).map(|t| t.vars.clone()).unwrap_or_default();
        self.pending_records.push(EventRecord {
            trial: e.trial,
            event: e.name.clone(),
            trial_type: e.trial_type.clone(),
            stimulus: s.shown.clone().unwrap_or_default(),
            log: e.log,
            planned_onset: e.onset,
            planned_duration: e.duration,
            onset,
            duration,
            frames,
            response,
            response_time,
            correct,
            outcome: if r.is_some() { s.outcome.clone() } else { None },
            vars,
        });
    }

    /// End the stimulus. Without feedback, every onset and end still waiting
    /// is set to `flip`; with feedback, they wait for their frames' reports
    /// (the current event ends on the next frame drawn).
    fn finish(&mut self, reason: EndReason, flip: f64) {
        if let Some(i) = self.current.take() {
            self.states[i].last_frame = self.last_index;
            let next = self.last_index.map_or(0, |f| f + 1);
            self.flip_uses.push((next, FlipUse::End(i)));
        }
        if !self.feedback {
            for (_, u) in std::mem::take(&mut self.flip_uses) {
                self.set_flip(u, flip);
            }
        }
        for i in 0..self.states.len() {
            if self.states[i].window_open {
                self.close_window(i);
            }
        }
        self.phase = Phase::Done(reason);
        self.finalize_ready();
    }

    /// The display's report on frame `index`: on screen at run-clock time
    /// `at`, or `None` if it was never shown. Reports may come in any order;
    /// one for a frame already given up on (`FEEDBACK_TIMEOUT_S`) is ignored.
    pub fn presented(&mut self, index: u64, at: Option<f64>) {
        if !self.feedback {
            return;
        }
        let Some(h) = self.held.iter_mut().find(|h| h.record.index == index) else { return };
        if h.record.report != Report::Pending {
            return;
        }
        let flip = match at {
            Some(t) => {
                h.record.report = Report::Shown(t);
                t
            }
            None => {
                h.record.report = Report::NotShown;
                h.next_acquired.unwrap_or(h.predicted)
            }
        };
        self.resolve_flips(index, flip, at.is_none());
        self.release();
        self.finalize_ready();
    }

    /// Time passes even without a frame (e.g. while the window is hidden):
    /// give up on reports older than `FEEDBACK_TIMEOUT_S`, and end the hold
    /// once the scanner has stopped. `frame()` calls this too.
    pub fn update(&mut self, now: f64) {
        if self.feedback {
            let expired: Vec<(u64, f64)> = self
                .held
                .iter_mut()
                .filter(|h| {
                    h.record.report == Report::Pending && now - h.record.acquired >= FEEDBACK_TIMEOUT_S
                })
                .map(|h| {
                    h.record.report = Report::Missing;
                    (h.record.index, h.next_acquired.unwrap_or(h.predicted))
                })
                .collect();
            for (index, flip) in expired {
                self.resolve_flips(index, flip, true);
            }
            self.release();
        }
        self.follow_scanner(now);
        self.finalize_ready();
    }

    /// With feedback: true while an onset, an end or a logged frame is still
    /// waiting for its report. After the run, keep drawing (briefly) until
    /// this is false, then `flush`.
    pub fn waiting_for_reports(&self) -> bool {
        self.feedback
            && (!self.flip_uses.is_empty()
                || self.held.iter().any(|h| h.logged && h.record.report == Report::Pending))
    }

    /// Stop waiting for reports: estimate whatever is still waiting and
    /// release every frame record. Call once the run is over, before taking
    /// the last records.
    pub fn flush(&mut self) {
        if self.feedback {
            let pending: Vec<(u64, f64)> = self
                .held
                .iter_mut()
                .filter(|h| h.record.report == Report::Pending)
                .map(|h| {
                    h.record.report = Report::Missing;
                    (h.record.index, h.next_acquired.unwrap_or(h.predicted))
                })
                .collect();
            for (index, flip) in pending {
                self.resolve_flips(index, flip, true);
            }
            // Ends on a frame that was never drawn: when it would have flipped.
            let next_flip = self.last_flip.map_or(0.0, |f| f + self.period);
            for (_, u) in std::mem::take(&mut self.flip_uses) {
                self.set_flip(u, next_flip);
                self.flip_estimates += 1;
            }
            self.release();
        }
        self.finalize_ready();
    }

    /// Set every onset and end waiting for frame `index`'s flip.
    fn resolve_flips(&mut self, index: u64, flip: f64, estimated: bool) {
        let (now, later): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.flip_uses).into_iter().partition(|(f, _)| *f == index);
        self.flip_uses = later;
        for (_, u) in now {
            self.set_flip(u, flip);
            if estimated {
                self.flip_estimates += 1;
            }
        }
    }

    /// Move frame records whose reports are in (and every one before them)
    /// to the log, counting missed refreshes on the way.
    fn release(&mut self) {
        while self.held.front().is_some_and(|h| h.record.report != Report::Pending) {
            let Some(mut h) = self.held.pop_front() else { break };
            match h.record.report {
                Report::Shown(t) => {
                    // Refreshes between this frame and the last one on screen
                    // showed the old image again, except those that frames
                    // with no report may have used.
                    if let Some(prev) = self.prev_shown
                        && h.record.phase == Phase::Running
                    {
                        let refreshes = ((t - prev) / self.period).round() as i64;
                        let missed = (refreshes - 1 - self.unknown_since_shown).max(0) as u32;
                        h.record.missed = missed;
                        if h.logged {
                            self.missed_frames += missed as u64;
                        }
                    }
                    self.prev_shown = Some(t);
                    self.unknown_since_shown = 0;
                }
                Report::Missing => self.unknown_since_shown += 1,
                _ => {}
            }
            if h.logged {
                self.pending_frames.push(h.record);
            }
        }
    }

    /// Before the run, say when a scan that was under way has stopped;
    /// during it, warn if the pulses stop; while holding, end once the
    /// scanner has stopped.
    fn follow_scanner(&mut self, now: f64) {
        match self.phase {
            Phase::Instructions | Phase::Waiting => {
                if self.announce_ready
                    && let Some(ready) = self.ready_at
                    && self.before_first.clear_to_start(ready, now)
                {
                    self.announce_ready = false;
                    self.notices.push(Notice::ReadyAgain);
                }
            }
            Phase::Running => {
                if !self.quiet_warned
                    && self.scan.tr_s().is_some()
                    && self.scan.stopped(now)
                    && let (Some(t0), Some(last)) = (self.t0, self.scan.last_s())
                {
                    self.quiet_warned = true;
                    let quiet_s = self.scan.stopped_after_s();
                    self.notices.push(Notice::ScanQuiet { last_pulse: last - t0, quiet_s });
                }
            }
            Phase::Holding => {
                if self.scan.stopped(now) {
                    self.phase = Phase::Done(EndReason::Completed);
                    self.notices.push(Notice::ScannerStopped);
                }
            }
            Phase::Done(_) => {}
        }
    }

    pub fn status(&self) -> Status {
        let trial =
            self.current.or(self.next_event.checked_sub(1)).map_or(0, |i| self.timeline.events[i].trial);
        Status {
            phase: self.phase,
            t: match (self.t0, self.last_acquire) {
                (Some(t0), Some(a)) => Some(a + self.period - t0),
                _ => None,
            },
            end: self.timeline.end,
            triggers: self.triggers.len() as u32,
            trial,
            trials: self.timeline.trials.len(),
            event: self.current.map(|i| self.timeline.events[i].name.clone()),
            missed_frames: self.missed_frames,
            last_response: self.last_response.clone(),
            responses: self.responses,
            correct: self.correct,
            scored: self.scored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::compile;
    use std::path::PathBuf;

    const HZ: f64 = 60.0;
    const P: f64 = 1.0 / HZ;

    fn experiment() -> Experiment {
        parse(EXPERIMENT)
    }

    fn parse(text: &str) -> Experiment {
        let mut e: Experiment = toml::from_str(text).unwrap();
        e.base_dir = PathBuf::from(".");
        e
    }

    const EXPERIMENT: &str = r#"
            [experiment]
            name = "t"
            task = "t"
            baseline = "fix"
            [stimuli.fix]
            type = "fixation"
            [stimuli.word]
            type = "text"
            text = "{word}"
            [stimuli.fb_correct]
            type = "text"
            text = "yes"
            [stimuli.fb_incorrect]
            type = "text"
            text = "no"
            [stimuli.fb_miss]
            type = "text"
            text = "?"
            [trial]
            trial_type = "{cond}"
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = 0.5
            response = { buttons = ["1", "2"], correct = "{answer}", window = 1.0 }
            [[trial.events]]
            name = "gap"
            stimulus = "fix"
            duration = 0.5
            log = false
            [[trial.events]]
            name = "feedback"
            stimulus = "fb_{outcome}"
            duration = 0.5
            [schedule]
            onset_column = "onset"
            rows = [
              { onset = 1.0, word = "a", answer = "1", cond = "x" },
              { onset = 3.0, word = "b", answer = "2", cond = "y" },
            ]
            "#;

    /// Drive the run with perfect vsync frames from `start` until done.
    fn drive(run: &mut Run, frames: &mut u64, start: f64, inputs: &[(f64, Input)]) -> Vec<EventRecord> {
        let mut records = Vec::new();
        let mut pending: Vec<(f64, Input)> = inputs.to_vec();
        let mut t = start;
        while !run.is_done() && *frames < 100_000 {
            pending.retain(|(at, input)| {
                if *at <= t {
                    run.input(input.clone(), *at);
                    false
                } else {
                    true
                }
            });
            run.frame(*frames, t, None);
            records.extend(run.take_records());
            *frames += 1;
            t += P;
        }
        records.extend(run.take_records());
        records
    }

    fn start_run(e: &Experiment) -> (Run<'_>, u64) {
        let tl = compile(e, 1, HZ).unwrap();
        let mut run = Run::new(e, tl, HZ, 3);
        assert_eq!(run.phase, Phase::Waiting);
        let mut f = 0;
        // Some waiting frames, then a trigger at t = 10.0 + a bit.
        for i in 0..60 {
            run.frame(f, 9.0 + i as f64 * P, None);
            f += 1;
        }
        run.input(Input::Trigger, 10.0);
        assert_eq!(run.phase, Phase::Running);
        (run, f)
    }

    #[test]
    fn onsets_land_within_half_a_frame_of_plan() {
        let e = experiment();
        let (mut run, mut f) = start_run(&e);
        // Frames continue on the same vsync grid (last waiting frame was 9.0 + 59P).
        let recs = drive(&mut run, &mut f, 9.0 + 60.0 * P, &[]);
        assert_eq!(run.phase, Phase::Done(EndReason::Completed));
        assert_eq!(recs.len(), 6);
        for r in &recs {
            let delay = r.onset_delay_ms().unwrap();
            assert!(delay.abs() <= 1000.0 * P / 2.0 + 1e-6, "{} delay {delay} ms", r.event);
            assert_eq!(r.frames, (r.planned_duration * HZ).round() as u64, "{}", r.event);
        }
    }

    #[test]
    fn responses_score_and_drive_feedback() {
        let e = experiment();
        let (mut run, mut f) = start_run(&e);
        // Trial 1 (target at t0+1.0): press "1" (correct) 0.4 s later.
        // Trial 2 (target at t0+3.0): no response -> miss.
        let recs = drive(&mut run, &mut f, 9.0 + 60.0 * P, &[(10.0 + 1.4, Input::Button("1".into()))]);
        let target1 = recs.iter().find(|r| r.trial == 1 && r.event == "target").unwrap();
        assert_eq!(target1.response.as_deref(), Some("1"));
        assert_eq!(target1.correct, Some(true));
        let rt = target1.response_time.unwrap();
        assert!((rt - 0.4).abs() < P, "rt {rt}");
        let fb1 = recs.iter().find(|r| r.trial == 1 && r.event == "feedback").unwrap();
        assert_eq!(fb1.stimulus, "fb_correct");
        let fb2 = recs.iter().find(|r| r.trial == 2 && r.event == "feedback").unwrap();
        assert_eq!(fb2.stimulus, "fb_miss");
        let target2 = recs.iter().find(|r| r.trial == 2 && r.event == "target").unwrap();
        assert_eq!(target2.outcome.as_deref(), Some("miss"));
        assert_eq!(target2.correct, Some(false));
    }

    #[test]
    fn a_dropped_frame_does_not_shift_later_onsets() {
        let e = experiment();
        let (mut run, mut f) = start_run(&e);
        let mut t = 9.0 + 60.0 * P;
        let mut recs = Vec::new();
        while !run.is_done() {
            run.frame(f, t, None);
            recs.extend(run.take_records());
            f += 1;
            // Stall for 5 extra refreshes once, half a second in.
            t += if (t - 10.5).abs() < P / 2.0 { 6.0 * P } else { P };
        }
        recs.extend(run.take_records());
        assert!(run.missed_frames >= 5, "missed {}", run.missed_frames);
        let later = recs.iter().find(|r| r.trial == 2 && r.event == "target").unwrap();
        assert!(later.onset_delay_ms().unwrap().abs() <= 1000.0 * P / 2.0 + 1e-6);
    }

    #[test]
    fn dummy_volumes_delay_t0() {
        let mut e = experiment();
        e.scanner.dummy_volumes = 2;
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.input(Input::Trigger, 1.0);
        run.input(Input::Trigger, 3.0);
        assert_eq!(run.phase, Phase::Waiting);
        run.input(Input::Trigger, 5.0);
        assert_eq!(run.t0, Some(5.0));
    }

    fn welcome(scene: &Scene) -> Option<crate::scene::WelcomeScreen> {
        scene.items.iter().find_map(|i| match i {
            Item::Welcome(w) => Some(w.clone()),
            _ => None,
        })
    }

    #[test]
    fn welcome_screen_runs_on_the_frame_clock_until_t0_and_never_after() {
        let text = EXPERIMENT.replace("baseline = \"fix\"", "baseline = \"fix\"\ninstructions = \"Hi\"");
        let e = parse(&format!("{text}\n[welcome]\ngreeting = \"Hello\"\n"));
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.set_participant_name("  Sam Example ");
        assert_eq!(run.phase, Phase::Instructions);

        let w0 = welcome(run.frame(0, 9.0, None).scene).expect("instructions have the welcome screen");
        let w1 = welcome(run.frame(1, 9.0 + P, None).scene).unwrap();
        assert!(!w0.waiting);
        assert_eq!(w0.title, "t", "title defaults to the experiment name");
        assert_eq!(w0.name.as_deref(), Some("Sam Example"));
        assert_eq!(w0.text.as_deref(), Some("Hi"));
        assert_eq!(w0.greeting, "Hello");
        assert_eq!(w0.time, 0.0);
        assert!((w1.time - P as f32).abs() < 1e-6, "time advances one refresh per frame");

        run.input(Input::Continue, 9.0 + 1.5 * P);
        let w2 = welcome(run.frame(2, 9.0 + 2.0 * P, None).scene).expect("waiting screen has it too");
        assert!(w2.waiting);
        assert_eq!(w2.name.as_deref(), Some("Sam Example"));
        assert!((w2.time - 2.0 * P as f32).abs() < 1e-6, "one clock across both screens");

        run.input(Input::Trigger, 9.0 + 2.5 * P);
        let (mut f, mut t) = (3, 9.0 + 3.0 * P);
        while !run.is_done() {
            assert!(welcome(run.frame(f, t, None).scene).is_none(), "welcome screen visible after t0 (frame {f})");
            f += 1;
            t += P;
        }
        assert!(welcome(run.frame(f, t, None).scene).is_none(), "nor once the run is done");
    }

    #[test]
    fn without_a_welcome_table_the_screens_are_unchanged() {
        let e = experiment();
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.set_participant_name("Sam Example");
        let scene = run.frame(0, 9.0, None).scene.clone();
        assert_eq!(scene, Scene::text(&e, &e.experiment.waiting_text));
    }

    #[test]
    fn abort_still_reports_started_events() {
        let e = experiment();
        let (mut run, mut f) = start_run(&e);
        let recs = drive(&mut run, &mut f, 9.0 + 60.0 * P, &[(11.2, Input::Abort)]);
        assert_eq!(run.phase, Phase::Done(EndReason::Aborted));
        assert_eq!(recs.len(), 1, "{recs:?}");
        assert_eq!(recs[0].event, "target");
        assert_eq!(recs[0].outcome.as_deref(), Some("miss"));
    }

    #[test]
    fn the_first_trigger_starts_the_run_from_the_instructions() {
        let mut e = experiment();
        e.experiment.instructions = Some("Keep your eyes on the dot.".into());
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        assert_eq!(run.phase, Phase::Instructions);
        run.input(Input::Trigger, 1.0);
        assert_eq!(run.phase, Phase::Running);
        assert_eq!(run.t0, Some(1.0));
    }

    // ── With feedback: the display reports when each frame was on screen ──

    /// When frame k is on screen: a perfect vsync grid, one period after its
    /// nominal acquire. The planner's prediction, and the display's report.
    fn flip_of(k: u64) -> f64 {
        9.0 + (k + 1) as f64 * P
    }

    /// When frame k's buffer comes back: 0.5 ms after the previous flip, but
    /// every 10th frame 10 ms later still, like a compositor's uneven
    /// hand-overs. The gaps between acquires are then 26.7 ms and 6.7 ms
    /// around those, though the display shows every refresh.
    fn acquire_of(k: u64) -> f64 {
        9.0 + k as f64 * P + 0.0005 + if k % 10 == 0 { 0.010 } else { 0.0 }
    }

    /// The display's report on frame k: `Some(Some(t))` on screen at t,
    /// `Some(None)` never shown, `None` no report at all.
    type Reports = dyn Fn(u64) -> Option<Option<f64>>;

    fn on_time(k: u64) -> Option<Option<f64>> {
        Some(Some(flip_of(k)))
    }

    #[derive(Default)]
    struct Drawn {
        records: Vec<EventRecord>,
        frames: Vec<FrameRecord>,
    }

    /// Drive a run the way the frame loop does with feedback: frame k is
    /// acquired at `acquire_of(k)` and predicted to flip at `flip_of(k)`, and
    /// its report arrives two frames later. Keeps drawing after the run is
    /// over until the last reports are in, then flushes.
    fn drive_feedback(run: &mut Run, f: &mut u64, inputs: &[(f64, Input)], report: &Reports) -> Drawn {
        let mut out = Drawn::default();
        let mut pending: Vec<(f64, Input)> = inputs.to_vec();
        while (!run.is_done() || run.waiting_for_reports()) && *f < 100_000 {
            let k = *f;
            let t = acquire_of(k);
            pending.retain(|(at, input)| {
                if *at <= t {
                    run.input(input.clone(), *at);
                    false
                } else {
                    true
                }
            });
            if k >= 2
                && let Some(r) = report(k - 2)
            {
                run.presented(k - 2, r);
            }
            run.frame(k, t, Some(flip_of(k)));
            out.records.extend(run.take_records());
            out.frames.extend(run.take_frames());
            *f += 1;
        }
        run.flush();
        out.records.extend(run.take_records());
        out.frames.extend(run.take_frames());
        out
    }

    /// 60 waiting frames with feedback, then the first pulse at t = 10.0.
    fn start_feedback_run(e: &Experiment) -> (Run<'_>, u64) {
        let tl = compile(e, 1, HZ).unwrap();
        let mut run = Run::new(e, tl, HZ, 3);
        run.enable_feedback();
        run.set_measured_period(Some(P));
        for k in 0..60 {
            if k >= 2 {
                run.presented(k - 2, Some(flip_of(k - 2)));
            }
            run.frame(k, acquire_of(k), Some(flip_of(k)));
        }
        run.input(Input::Trigger, 10.0);
        assert_eq!(run.phase, Phase::Running);
        (run, 60)
    }

    #[test]
    fn with_feedback_onsets_are_when_frames_were_on_screen() {
        let e = experiment();
        let (mut run, mut f) = start_feedback_run(&e);
        let out = drive_feedback(&mut run, &mut f, &[], &on_time);
        assert_eq!(run.phase, Phase::Done(EndReason::Completed));
        assert_eq!(out.records.len(), 6);
        for r in &out.records {
            // The reported flips sit exactly on the planned refreshes.
            assert!(r.onset_delay_ms().unwrap().abs() < 1e-6, "{} delay {:?}", r.event, r.onset_delay_ms());
            assert_eq!(r.frames, (r.planned_duration * HZ).round() as u64, "{}", r.event);
            assert!((r.duration.unwrap() - r.planned_duration).abs() < 1e-6, "{}", r.event);
        }
        // Uneven hand-overs aren't drops: the display showed every refresh.
        assert_eq!(run.missed_frames, 0);
        assert_eq!(run.flip_estimates, 0);
        // Every frame logged once, in order, with its reported time; frames
        // drawn after the run while waiting for reports aren't logged.
        let last = out.frames.last().unwrap().index;
        assert_eq!(out.frames.iter().map(|r| r.index).collect::<Vec<_>>(), (0..=last).collect::<Vec<_>>());
        assert!(out.frames.iter().all(|r| r.report == Report::Shown(flip_of(r.index))));
        assert_eq!(out.frames.iter().filter(|r| r.phase == Phase::Done(EndReason::Completed)).count(), 1);
        assert!(f > last + 1, "kept drawing for the last reports");

        // The same frames timed from the acquires alone: false drops, and
        // onsets off by the late hand-overs (10.5 ms is more than half a frame).
        let tl = compile(&e, 1, HZ).unwrap();
        let mut old = Run::new(&e, tl, HZ, 3);
        let mut recs = Vec::new();
        let mut k = 0;
        while !old.is_done() {
            if k == 60 {
                old.input(Input::Trigger, 10.0);
            }
            old.frame(k, acquire_of(k), None);
            recs.extend(old.take_records());
            k += 1;
        }
        assert!(old.missed_frames > 20, "missed {}", old.missed_frames);
        assert!(recs.iter().any(|r| r.onset_delay_ms().unwrap().abs() > 1000.0 * P / 2.0));
    }

    #[test]
    fn a_frame_never_shown_or_never_reported_falls_back_to_the_next_acquire() {
        let e = experiment();
        let (mut run, mut f) = start_feedback_run(&e);
        // Trial 1's target starts on frame 119 (flip 11.0 = t0 + 1.0), trial
        // 2's on frame 239. The display says 119 was never shown, and never
        // reports on 239.
        let report = |k: u64| match k {
            119 => Some(None),
            239 => None,
            _ => on_time(k),
        };
        let out = drive_feedback(&mut run, &mut f, &[], &report);
        let first_frame = |event: usize| out.frames.iter().find(|r| r.event == Some(event)).unwrap().index;
        assert_eq!((first_frame(0), first_frame(3)), (119, 239));
        let target =
            |trial: usize| out.records.iter().find(|r| r.trial == trial && r.event == "target").unwrap();
        assert!((target(1).onset.unwrap() - (acquire_of(120) - 10.0)).abs() < 1e-9);
        assert!((target(2).onset.unwrap() - (acquire_of(240) - 10.0)).abs() < 1e-9);
        assert_eq!(run.flip_estimates, 2);
        let report_of = |k: u64| out.frames.iter().find(|r| r.index == k).unwrap().report;
        assert_eq!((report_of(119), report_of(239)), (Report::NotShown, Report::Missing));
        // The refresh frame 119 should have been on showed frame 118 again: a
        // drop. Frame 239 may have used its refresh, so it isn't counted.
        assert_eq!(run.missed_frames, 1);
        assert_eq!(out.frames.iter().find(|r| r.index == 120).unwrap().missed, 1);
    }

    #[test]
    fn missed_frames_come_from_gaps_between_frames_on_screen() {
        let e = experiment();
        let (mut run, mut f) = start_feedback_run(&e);
        // From frame 150 on, every frame reaches the screen one refresh late:
        // the display really dropped one.
        let report = |k: u64| Some(Some(flip_of(k) + if k >= 150 { P } else { 0.0 }));
        let out = drive_feedback(&mut run, &mut f, &[], &report);
        assert_eq!(run.missed_frames, 1);
        let dropped: Vec<u64> = out.frames.iter().filter(|r| r.missed > 0).map(|r| r.index).collect();
        assert_eq!(dropped, vec![150]);
    }

    #[test]
    fn frame_records_wait_for_their_reports() {
        let e = experiment();
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.enable_feedback();
        run.frame(0, 1.0, None);
        run.frame(1, 1.0 + P, None);
        assert!(run.take_frames().is_empty());
        // Out of order: frame 1's report first. Frame 0 still holds it back.
        run.presented(1, Some(1.0 + 2.0 * P));
        assert!(run.take_frames().is_empty());
        run.presented(0, Some(1.0 + P));
        let got: Vec<(u64, Report)> = run.take_frames().iter().map(|r| (r.index, r.report)).collect();
        assert_eq!(got, vec![(0, Report::Shown(1.0 + P)), (1, Report::Shown(1.0 + 2.0 * P))]);
        // No report within the timeout: logged without one.
        run.frame(2, 1.0 + 2.0 * P, None);
        run.update(1.0 + 2.0 * P + FEEDBACK_TIMEOUT_S - 0.01);
        assert!(run.take_frames().is_empty());
        run.update(1.0 + 2.0 * P + FEEDBACK_TIMEOUT_S);
        let late = run.take_frames();
        assert_eq!((late.len(), late[0].report), (1, Report::Missing));
        // A report after that is ignored.
        run.presented(2, Some(1.0 + 3.0 * P));
        assert!(run.take_frames().is_empty());
    }

    #[test]
    fn the_period_is_locked_from_the_measured_one() {
        let e = experiment();
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.enable_feedback();
        for k in 0..60 {
            run.frame(k, 9.0 + k as f64 * P, None);
        }
        let measured = 1.0 / 59.94;
        run.set_measured_period(Some(measured));
        run.input(Input::Trigger, 10.0);
        assert_eq!(run.period(), measured);
        // Locked: later measurements don't change it.
        run.set_measured_period(Some(P));
        run.input(Input::Trigger, 12.0);
        assert_eq!(run.period(), measured);
    }

    #[test]
    fn with_feedback_an_abort_ends_the_event_on_the_next_frame_shown() {
        let e = experiment();
        let (mut run, mut f) = start_feedback_run(&e);
        // During trial 1's target (frames 119-148). Frame 132 is the first
        // drawn after the abort; it shows the baseline.
        let out = drive_feedback(&mut run, &mut f, &[(11.2, Input::Abort)], &on_time);
        assert_eq!(run.phase, Phase::Done(EndReason::Aborted));
        assert_eq!(out.records.len(), 1, "{:?}", out.records);
        let target = &out.records[0];
        assert!((target.duration.unwrap() - (flip_of(132) - flip_of(119))).abs() < 1e-9);
        assert_eq!(run.flip_estimates, 0);
        assert_eq!(out.frames.last().unwrap().index, 131, "frames after the abort aren't logged");
    }

    #[test]
    fn flush_estimates_what_never_came() {
        let e = experiment();
        let (mut run, mut f) = start_feedback_run(&e);
        while acquire_of(f) < 11.2 {
            if let Some(r) = on_time(f - 2) {
                run.presented(f - 2, r);
            }
            run.frame(f, acquire_of(f), Some(flip_of(f)));
            f += 1;
        }
        // Stopped, and no frame is drawn after (e.g. the window closed).
        run.input(Input::Abort, 11.2);
        assert!(run.waiting_for_reports());
        run.flush();
        assert!(!run.waiting_for_reports());
        let recs = run.take_records();
        assert_eq!(recs.len(), 1);
        // Its end: when the next frame would have flipped.
        assert!((recs[0].duration.unwrap() - (flip_of(f) - flip_of(119))).abs() < 1e-9);
        assert!(run.flip_estimates >= 1);
        assert_eq!(run.take_frames().last().unwrap().index, f - 1);
    }

    // ── Following the scanner ──

    #[test]
    fn the_final_screen_holds_until_the_scanner_stops() {
        let e = experiment();
        let (mut run, mut f) = start_run(&e);
        // The stimulus ends 4.5 s after t0 (14.5), but the scan goes on with
        // a pulse every 2 s until 18.0. Stopped = 5 s without a pulse.
        let pulses: Vec<(f64, Input)> =
            [12.0, 14.0, 16.0, 18.0].iter().map(|&t| (t, Input::Trigger)).collect();
        let recs = drive(&mut run, &mut f, 9.0 + 60.0 * P, &pulses);
        assert_eq!(run.phase, Phase::Done(EndReason::Completed));
        assert_eq!(recs.len(), 6);
        assert_eq!(run.triggers.len(), 5, "pulses while holding are still counted");
        let frames = run.take_frames();
        let holding: Vec<&FrameRecord> = frames.iter().filter(|r| r.phase == Phase::Holding).collect();
        // From the frame whose flip (one period after its acquire) is nearest the end.
        let flip = holding[0].acquired + P;
        assert!((flip - 14.5).abs() <= P / 2.0 + 1e-9, "held from {flip}");
        assert!(holding.iter().all(|r| r.event.is_none()), "the final screen is the baseline");
        let done = frames.iter().find(|r| matches!(r.phase, Phase::Done(_))).unwrap();
        assert!((23.0..23.0 + P).contains(&done.acquired), "ended at {}", done.acquired);
        assert_eq!(run.take_notices(), vec![Notice::Holding { quiet_s: 5.0 }, Notice::ScannerStopped]);
        let c = run.scan_coverage().unwrap();
        assert!(!c.stopped_early(), "{c:?}");
    }

    #[test]
    fn a_run_ends_on_time_when_the_scanner_has_stopped() {
        let mut e = experiment();
        // The stimulus runs to t0 + 8 s.
        e.scanner.tr = Some(1.0);
        e.scanner.volumes = Some(8);
        let (mut run, mut f) = start_run(&e);
        // One pulse only (t0): 8 s of quiet by the end, more than the 5 s
        // that mean "stopped" while the TR isn't known.
        drive(&mut run, &mut f, 9.0 + 60.0 * P, &[]);
        let done = run.take_frames().into_iter().find(|r| matches!(r.phase, Phase::Done(_))).unwrap();
        assert!((done.acquired + P - 18.0).abs() <= P / 2.0 + 1e-9, "ended at {}", done.acquired);
        assert!(run.take_notices().is_empty());
        assert_eq!(run.scan_coverage(), None, "one pulse: no TR");
    }

    #[test]
    fn stopping_the_hold_completes_the_run() {
        let e = experiment();
        let (mut run, mut f) = start_run(&e);
        let pulses: Vec<(f64, Input)> = [12.0, 14.0].iter().map(|&t| (t, Input::Trigger)).collect();
        let mut t = 9.0 + 60.0 * P;
        let mut pending = pulses.clone();
        while run.phase != Phase::Holding {
            pending.retain(|(at, i)| {
                if *at <= t {
                    run.input(i.clone(), *at);
                    false
                } else {
                    true
                }
            });
            run.frame(f, t, None);
            f += 1;
            t += P;
        }
        run.input(Input::Abort, t);
        assert_eq!(run.phase, Phase::Done(EndReason::Completed));
        assert_eq!(run.take_notices().last(), Some(&Notice::HoldStopped));
    }

    #[test]
    fn a_scan_that_stops_before_the_stimulus_is_reported() {
        let mut e = experiment();
        // A 20 volume × 1 s scan planned, so the stimulus runs to t0 + 20.
        e.scanner.tr = Some(1.0);
        e.scanner.volumes = Some(20);
        let (mut run, mut f) = start_run(&e);
        // The scan stops after three volumes.
        let pulses: Vec<(f64, Input)> = [11.0, 12.0].iter().map(|&t| (t, Input::Trigger)).collect();
        drive(&mut run, &mut f, 9.0 + 60.0 * P, &pulses);
        assert_eq!(run.phase, Phase::Done(EndReason::Completed), "no hold: the scanner had stopped");
        // Warned live once the pulses had stopped for 5 s...
        assert_eq!(run.take_notices(), vec![Notice::ScanQuiet { last_pulse: 2.0, quiet_s: 5.0 }]);
        // ...and at the end: scanned until 2 + 1 TR, stimulus until 20.
        let c = run.scan_coverage().unwrap();
        assert!(c.stopped_early());
        assert_eq!((c.tr, c.scanned_until, c.stimulus_end), (1.0, 3.0, 20.0));
    }

    #[test]
    fn pulses_from_a_scan_already_under_way_are_ignored() {
        let e = experiment();
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.ready(0.0);
        // The tail of an earlier scan: a pulse every 2 s until 7.0.
        for t in [1.0, 3.0, 5.0, 7.0] {
            assert_eq!(run.input(Input::Trigger, t), InputUse::PreviousScan);
        }
        assert_eq!((run.phase, run.triggers.len(), run.ignored_triggers), (Phase::Waiting, 0, 4));
        assert_eq!(run.take_notices(), vec![Notice::PulseIgnored { quiet_s: 5.0 }], "said once");
        // 5 s (2.5 TRs, at least 5 s) after the last of them: ready again.
        run.update(11.9);
        assert!(run.take_notices().is_empty());
        run.update(12.0);
        assert_eq!(run.take_notices(), vec![Notice::ReadyAgain]);
        // The next scan's first pulse starts the run.
        assert_eq!(run.input(Input::Trigger, 20.0), InputUse::Normal);
        assert_eq!((run.phase, run.t0), (Phase::Running, Some(20.0)));
    }

    #[test]
    fn a_scan_started_soon_after_launch_is_not_ignored() {
        let e = experiment();
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.ready(0.0);
        // The operator starts the scan right away: first pulse 3 s after
        // Onset got ready, well inside the 5 s scanner-stopped rule.
        assert_eq!(run.input(Input::Trigger, 3.0), InputUse::Normal);
        assert_eq!((run.phase, run.t0, run.ignored_triggers), (Phase::Running, Some(3.0), 0));
    }

    #[test]
    fn after_a_quiet_spell_every_pulse_of_the_new_scan_counts() {
        let mut e = experiment();
        e.scanner.dummy_volumes = 2;
        let tl = compile(&e, 1, HZ).unwrap();
        let mut run = Run::new(&e, tl, HZ, 3);
        run.ready(0.0);
        // Quiet since ready, then a scan: the dummies 2 s apart count too.
        for t in [10.0, 12.0, 14.0] {
            assert_eq!(run.input(Input::Trigger, t), InputUse::Normal);
        }
        assert_eq!(run.t0, Some(14.0));
        assert_eq!(run.ignored_triggers, 0);
    }
}
