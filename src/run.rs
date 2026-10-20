//! The run state machine: what to draw on each frame, and what happened.
//!
//! This has no GPU or hardware code so it can be tested with synthetic frame
//! and input times. All times are `f64` seconds on one monotonic clock (the
//! "run clock"); `t0` is the run-clock time of the first kept trigger.
//!
//! Frame timing model: under vsync, acquiring the next swapchain image blocks
//! until the previous frame is on screen. So the frame being drawn now will
//! appear about one refresh period after it was acquired, and frame k's
//! actual flip time is (approximately) when frame k+1 was acquired. The
//! photodiode rig measures the true display latency on top of this.

use crate::experiment::Experiment;
use crate::scene::{self, Scene};
use crate::schedule::{PlannedEvent, Timeline};
use crate::template::Vars;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Showing instructions until the scanner's first (kept) trigger starts
    /// the run, so nobody has to touch the stimulus computer. The operator
    /// can press Space to move on to the waiting screen early.
    Instructions,
    /// Waiting for the first (non-dummy) trigger.
    Waiting,
    Running,
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
    /// Refreshes missed before this frame (0 when on time).
    pub missed: u32,
    pub phase: Phase,
    /// Index into `Timeline::events` of the event drawn, if any.
    pub event: Option<usize>,
    pub photodiode: bool,
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

    last_acquire: Option<f64>,
    last_index: Option<u64>,
    flip_uses: Vec<FlipUse>,
    pub missed_frames: u64,
    last_response: Option<(String, Option<f64>)>,
    pending_records: Vec<EventRecord>,
    pending_frames: Vec<FrameRecord>,
    started_responses: Vec<StartedResponse>,
    responses: u32,
    correct: u32,
    scored: u32,
}

impl<'e> Run<'e> {
    pub fn new(exp: &'e Experiment, timeline: Timeline, nominal_hz: f64, flash_frames: u32) -> Run<'e> {
        let instructions = exp.experiment.instructions.as_deref();
        let n = timeline.events.len();
        Run {
            exp,
            phase: if instructions.is_some() { Phase::Instructions } else { Phase::Waiting },
            instructions: Scene::text(exp, instructions.unwrap_or("")),
            waiting: Scene::text(exp, &exp.experiment.waiting_text),
            frame_scene: Scene { background: exp.experiment.background, items: vec![] },
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
            flip_uses: Vec::new(),
            missed_frames: 0,
            last_response: None,
            pending_records: Vec::new(),
            pending_frames: Vec::new(),
            started_responses: Vec::new(),
            responses: 0,
            correct: 0,
            scored: 0,
        }
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
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

    pub fn take_frames(&mut self) -> Vec<FrameRecord> {
        std::mem::take(&mut self.pending_frames)
    }

    /// Handle an input. `t` is the run-clock time it happened.
    pub fn input(&mut self, input: Input, t: f64) {
        match input {
            Input::Trigger => {
                self.triggers.push(t);
                let waiting = matches!(self.phase, Phase::Instructions | Phase::Waiting);
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
            Input::Abort => {
                if !self.is_done() {
                    self.finish(EndReason::Aborted, self.last_acquire.unwrap_or(t) + self.period);
                }
            }
            Input::Button(button) => {
                if self.phase == Phase::Running {
                    self.response(button, t);
                }
            }
        }
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
        if self.intervals.len() >= 30 {
            let mut v: Vec<f64> = self.intervals.iter().rev().take(240).copied().collect();
            v.sort_by(f64::total_cmp);
            self.period = v[v.len() / 2];
        }
        self.period_locked = true;
    }

    /// Call right after acquiring frame `index` at run-clock time `acquired`.
    /// Returns what to draw.
    pub fn frame(&mut self, index: u64, acquired: f64) -> FramePlan<'_> {
        // The previous frame reached the screen about now.
        let mut missed = 0;
        let mut interval = 0.0;
        if let Some(prev) = self.last_acquire {
            interval = acquired - prev;
            if self.phase == Phase::Running && interval > 1.5 * self.period {
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
        for u in std::mem::take(&mut self.flip_uses) {
            self.set_flip(u, acquired);
        }
        self.finalize_ready();

        // This frame will reach the screen about one period from now.
        let flip = acquired + self.period;
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
                self.finish(EndReason::Completed, flip);
            }
            shown_event = self.current;
        }

        self.pending_frames.push(FrameRecord {
            index,
            acquired,
            interval,
            missed,
            phase: self.phase,
            event: shown_event,
            photodiode,
        });

        let scene = match self.phase {
            Phase::Instructions => &self.instructions,
            Phase::Waiting => &self.waiting,
            Phase::Running | Phase::Done(_) => {
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
        self.flip_uses.push(FlipUse::Onset(i));
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
            self.flip_uses.push(FlipUse::End(i));
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

    fn finish(&mut self, reason: EndReason, flip: f64) {
        if let Some(i) = self.current.take() {
            self.states[i].last_frame = self.last_index;
            self.flip_uses.push(FlipUse::End(i));
        }
        for u in std::mem::take(&mut self.flip_uses) {
            self.set_flip(u, flip);
        }
        for i in 0..self.states.len() {
            if self.states[i].window_open {
                self.close_window(i);
            }
        }
        self.phase = Phase::Done(reason);
        self.finalize_ready();
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
        let mut e: Experiment = toml::from_str(
            r#"
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
            "#,
        )
        .unwrap();
        e.base_dir = PathBuf::from(".");
        e
    }

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
            run.frame(*frames, t);
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
            run.frame(f, 9.0 + i as f64 * P);
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
            run.frame(f, t);
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
        let mut e = exp();
        e.experiment.instructions = Some("Keep your eyes on the dot.".into());
        let tl = schedule::compile(&e, 1).unwrap();
        let mut run = Run::new(&e, tl, 60.0, 3);
        assert_eq!(run.phase, Phase::Instructions);
        run.input(Input::Trigger, 1.0);
        assert_eq!(run.phase, Phase::Running);
        assert_eq!(run.t0, Some(1.0));
    }
}
