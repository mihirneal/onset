//! Checkerboard block-design localizer: the first runnable Onset paradigm.
//!
//! Opens full screen on the BOLDscreen and shows a fixation dot until the
//! first scanner pulse (`t` from the fORP box, or press `t` yourself). Then it
//! runs rest / checkerboard blocks, ending on rest. The photodiode square in
//! the top-left corner turns white at the start of each checkerboard block,
//! flips on every reversal, and is black during rest.
//!
//!     cargo run --release --bin checkerboard
//!     cargo run --release --bin checkerboard -- --block-s 6 --blocks 2
//!     cargo run --release --bin checkerboard -- --windowed --no-wait
//!
//! Frame timing: on macOS, Metal reports when every frame really reached the
//! screen, and the planner (`onset::planner`) uses those reports to decide
//! which stimulus frame to draw. So CPU jitter can't skip frames, and after
//! a frame that really was late the run catches up instead of running late.
//! While waiting for the scanner it measures the display's real refresh
//! rate, and once that's precise (and close to the rate the display
//! reports) the run's schedule is built from it, so blocks line up with the
//! scanner's clock rather than the display's nominal rate. If the display
//! reports no rate, or one that frames on screen show is wrong, it switches
//! to the rate it sees.
//!
//! Esc stops early. Logs go to logs/<date>_<time>_checkerboard/:
//! events.tsv (BIDS; onsets are when each block reached the screen),
//! frames.tsv (every frame), keys.tsv (pulses and keys).

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::window::{Window, WindowId};

use onset::frames::{
    FRAME_RATE_MISMATCH, MAX_RATE_UNCERTAINTY, MIN_GAPS_FOR_RATE_CHECK, REFRESH_MISMATCH,
    RateProblem, measured_refresh_hz, nominal_rate_is_wrong, schedule_rate,
};
use onset::hostclock::{HostClock, host_now};
use onset::localizer::{BlockDesign, DrawnFrame, FrameSchedule, Screen};
use onset::planner::{self, FrameRow, Planner, Status, Summary};
use onset::render::{HAS_FEEDBACK, Look, Renderer};
use onset::scanner::{DEFAULT_QUIET_S, PulseWatch, tr_stats};
use onset::timing::frame_to_onset;

/// With --no-wait, start once the frame timing has settled, or after this
/// long at the latest.
const NO_WAIT_CAP: Duration = Duration::from_secs(2);

/// Warn if the frame timing still hasn't settled after this long.
const SETTLE_WARNING_AFTER: Duration = Duration::from_secs(3);

/// If the scanner pulse comes before `MIN_GAPS_FOR_RATE_CHECK` gaps between
/// frames on screen, check the refresh rate with as few as this many.
const MIN_GAPS_AT_PULSE: usize = 5;

/// End the run anyway once it has gone on this long past its planned
/// length (on top of `REFRESH_MISMATCH` of the length). Frame counting
/// normally ends it on time; this is the backstop if frames were lost
/// without being caught up (e.g. no reports from the display).
const OVERRUN_LIMIT_S: f64 = 1.0;

#[derive(Parser)]
#[command(about = "Checkerboard block-design localizer")]
struct Args {
    /// Which display: "external" (the first one that isn't the main screen),
    /// "main", or a number from the list printed at startup.
    #[arg(long, default_value = "external")]
    display: String,

    /// Use a normal window on the main screen instead of full screen.
    #[arg(long)]
    windowed: bool,

    /// Start without waiting for the first scanner pulse (as soon as the
    /// frame timing has settled, about half a second).
    #[arg(long)]
    no_wait: bool,

    /// The key the trigger box sends for each scanner pulse.
    #[arg(long, default_value_t = 't')]
    trigger_key: char,

    /// Length of each rest and checkerboard block, in seconds.
    #[arg(long, default_value_t = 20.0)]
    block_s: f64,

    /// Number of checkerboard blocks (the run has one more rest block).
    #[arg(long, default_value_t = 4)]
    blocks: u32,

    /// Checkerboard contrast reversals per second.
    #[arg(long, default_value_t = 8.0)]
    reversals_per_s: f64,

    /// Side of the photodiode square, in pixels.
    #[arg(long, default_value_t = 80)]
    diode_px: u32,

    /// Seconds without a pulse that mean the scanner isn't scanning, before
    /// its TR is known (afterwards 2.5 TRs, never less than this). A run only
    /// starts after this much quiet, and holds its last rest screen until
    /// the scanner has been quiet this long.
    #[arg(long, default_value_t = DEFAULT_QUIET_S)]
    quiet_s: f64,

    /// Where to save logs [default: logs/<date>_<time>_checkerboard]
    #[arg(long)]
    out_dir: Option<PathBuf>,
}

/// What's fixed once the run starts.
#[derive(Clone, Copy)]
struct Started {
    /// Time zero: the first scanner pulse (or the start, with --no-wait).
    zero: Instant,
    /// The run in frames, built for `hz`.
    schedule: FrameSchedule,
    /// The refresh rate the schedule was built for: the measured one if
    /// that could be trusted, otherwise the nominal one.
    hz: f64,
}

impl Started {
    /// The run's planned length in seconds.
    fn run_s(&self) -> f64 {
        frame_to_onset(self.schedule.total_frames(), self.hz)
    }

    /// The run has gone on well past its planned length at `now`.
    fn overran(&self, now: Instant) -> bool {
        let limit = self.run_s() * (1.0 + REFRESH_MISMATCH) + OVERRUN_LIMIT_S;
        now.saturating_duration_since(self.zero).as_secs_f64() >= limit
    }
}

/// Where the run is. `Copy` so we can match on it and then replace it
/// without fighting the borrow checker.
#[derive(Clone, Copy)]
enum Run {
    Waiting,
    Running(Started),
    /// The stimulus is over but the scanner is still sending pulses: keep
    /// the rest screen up and keep logging until it stops.
    Holding(Started),
    /// The run is over, but Metal hasn't reported on its last frames yet.
    /// Keep drawing rest frames until it has, or until `until`, then save.
    Draining {
        started: Started,
        stopped_early: bool,
        until: Instant,
    },
    Finished,
}

/// Where `App::nominal_hz` came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NominalFrom {
    /// The refresh rate the display reports.
    Display,
    /// 60 Hz, because the display reported none.
    Guess,
    /// How fast new frames reached the screen, because the other two were
    /// clearly wrong.
    FramesSeen,
}

/// One frame drawn during the run. Its timing is in the planner, under the
/// same id.
struct RunFrame {
    id: u64,
    screen: Screen,
}

struct App {
    args: Args,
    trigger_key: String,
    out_dir: PathBuf,
    design: BlockDesign,
    /// Converts `Instant`s (key presses) to the display's host time.
    clock: HostClock,

    // Set up once the window exists (in `resumed`).
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    planner: Option<Planner>,
    /// The schedule at `nominal_hz`, checked at startup. Used if the
    /// measured rate isn't available or can't be trusted.
    nominal_schedule: Option<FrameSchedule>,
    /// The refresh rate the display reports, if it reports one.
    reported_hz: Option<f64>,
    /// The refresh rate the planner counts with and the fallback schedule
    /// is built for: the reported one, or 60 Hz if there's none, until
    /// frames on screen show it's wrong (`check_nominal_rate`).
    nominal_hz: f64,
    nominal_from: NominalFrom,
    /// `check_nominal_rate` has compared `nominal_hz` with the frames on
    /// screen.
    rate_checked: bool,
    /// Feedback points needed before `check_nominal_rate` tries again.
    next_rate_check: usize,

    run: Run,
    waiting_since: Option<Instant>,
    settled_reported: bool,
    settle_warned: bool,
    last_drawn: Option<Instant>,
    skipping_since: Option<Instant>,
    skip_warned: bool,
    run_frames: Vec<RunFrame>,
    pulses: Vec<Instant>,
    keys: Vec<(Instant, String)>,
    /// Pulses that came while the scanner was already mid-scan, before the
    /// run (not counted as the run's pulses).
    ignored_pulses: Vec<Instant>,
    /// Fixed starting point for the pulse watches' seconds.
    origin: Instant,
    /// When the window was up and Onset could first hear a pulse.
    ready_s: Option<f64>,
    /// Every pulse heard before the run, to tell if a scan is under way.
    wait_watch: PulseWatch,
    /// The run's own pulses, to tell when its scan has stopped.
    run_watch: PulseWatch,
    /// Pulses were ignored and "ready" hasn't been announced again yet.
    announce_ready: bool,
    exit_code: ExitCode,
}

impl App {
    fn new(args: Args) -> Self {
        let out_dir = args.out_dir.clone().unwrap_or_else(|| {
            let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
            PathBuf::from(format!("logs/{stamp}_checkerboard"))
        });
        let quiet_s = args.quiet_s;
        App {
            trigger_key: args.trigger_key.to_string(),
            out_dir,
            design: BlockDesign {
                block_s: args.block_s,
                checker_blocks: args.blocks,
                reversals_per_s: args.reversals_per_s,
            },
            clock: HostClock::calibrate(1000),
            args,
            window: None,
            renderer: None,
            planner: None,
            nominal_schedule: None,
            reported_hz: None,
            nominal_hz: 60.0,
            nominal_from: NominalFrom::Guess,
            rate_checked: false,
            next_rate_check: 0,
            run: Run::Waiting,
            waiting_since: None,
            settled_reported: false,
            settle_warned: false,
            last_drawn: None,
            skipping_since: None,
            skip_warned: false,
            run_frames: Vec::new(),
            pulses: Vec::new(),
            keys: Vec::new(),
            ignored_pulses: Vec::new(),
            origin: Instant::now(),
            ready_s: None,
            wait_watch: PulseWatch::new(quiet_s),
            run_watch: PulseWatch::new(quiet_s),
            announce_ready: false,
            exit_code: ExitCode::SUCCESS,
        }
    }

    /// Stop with an error message before or during setup.
    fn fail(&mut self, event_loop: &ActiveEventLoop, message: &str) {
        eprintln!("Error: {message}");
        self.exit_code = ExitCode::FAILURE;
        self.run = Run::Finished;
        event_loop.exit();
    }

    /// "the display's 120.00 Hz", for messages.
    fn nominal_label(&self) -> String {
        let hz = self.nominal_hz;
        match self.nominal_from {
            NominalFrom::Display => format!("the display's {hz:.2} Hz"),
            NominalFrom::Guess => format!("an assumed {hz:.2} Hz"),
            NominalFrom::FramesSeen => format!("the {hz:.2} Hz seen on screen"),
        }
    }

    /// Start the run at `zero`. The next frame drawn is frame 0.
    fn start(&mut self, zero: Instant) {
        // Last chance to check the rate, with whatever there is so far.
        self.check_nominal_rate(true);
        if HAS_FEEDBACK && self.nominal_from == NominalFrom::Guess && !self.rate_checked {
            println!(
                "WARNING: the display didn't report a refresh rate, and too few frames had \
                 reached the screen to measure it. Scheduling at an assumed {:.0} Hz: if the \
                 display runs at another rate, every block will be the wrong length. If so, \
                 stop (Esc), restart Onset and let it run for a second before the first pulse.",
                self.nominal_hz
            );
        }
        let nominal = self.nominal_hz;
        let nominal_label = self.nominal_label();
        let (Some(planner), Some(nominal_schedule)) =
            (self.planner.as_mut(), self.nominal_schedule)
        else {
            return;
        };
        planner.start_run(self.clock.instant_to_host(zero));
        let fallback = Started {
            zero,
            schedule: nominal_schedule,
            hz: nominal,
        };

        // Build the schedule from the refresh rate we measured, if that's
        // precise and believable. If the display really runs at 119.95 Hz
        // rather than 120, a 120 Hz schedule would finish 0.2 s late over 10
        // minutes.
        let measured = planner.measured_period().filter(|_| planner.is_warmed_up());
        let started = match schedule_rate(nominal, measured) {
            Ok(hz) => match FrameSchedule::new(&self.design, hz) {
                Ok(schedule) => {
                    println!(
                        "Refresh rate: measured {hz:.3} Hz ({nominal_label} nominal). \
                         Scheduling at the measured rate: blocks of {} frames, reversal every {} frames.",
                        schedule.block_frames, schedule.reversal_frames
                    );
                    Started { zero, schedule, hz }
                }
                Err(e) => {
                    println!(
                        "WARNING: can't schedule at the measured {hz:.3} Hz ({e}); \
                         using {nominal_label}."
                    );
                    fallback
                }
            },
            Err(_) if !HAS_FEEDBACK => {
                println!("Scheduling at {nominal_label}.");
                fallback
            }
            Err(RateProblem::NotMeasured) => {
                println!(
                    "WARNING: the frame timing hadn't settled yet ({} reports from the display). \
                     Scheduling at {nominal_label}; frame 0 may be a frame off.",
                    planner.feedback_points()
                );
                fallback
            }
            Err(RateProblem::Imprecise {
                measured_hz,
                uncertainty,
            }) => {
                println!(
                    "Refresh rate: measured {measured_hz:.3} Hz, but only to ±{:.0} ppm so far \
                     (needs ±{:.0}). Scheduling at {nominal_label}.",
                    uncertainty * 1e6,
                    MAX_RATE_UNCERTAINTY * 1e6
                );
                fallback
            }
            Err(RateProblem::Mismatch { measured_hz }) => {
                println!(
                    "WARNING: the measured refresh rate ({measured_hz:.3} Hz) is more than {:.1} % \
                     from {nominal_label}, so neither can be trusted. Scheduling at {nominal_label}; \
                     check the timing in frames.tsv. Is the display mirrored, or on the wrong screen?",
                    REFRESH_MISMATCH * 100.0
                );
                fallback
            }
        };
        self.run = Run::Running(started);
    }

    /// Before the run: compare `nominal_hz` with how fast new frames really
    /// reach the screen, once there are enough gaps between frames on
    /// screen to go by (fewer will do `at_pulse`, the last chance). If it's
    /// clearly wrong (no rate reported and the 60 Hz guess is off, or more
    /// new frames than the reported rate allows), switch to the rate seen
    /// and have the planner measure again on that grid. Its vsync
    /// numbering, and so its catch-up and dropped frame counts, are
    /// meaningless on a wrong grid.
    fn check_nominal_rate(&mut self, at_pulse: bool) {
        if self.rate_checked || !matches!(self.run, Run::Waiting) {
            return;
        }
        let Some(planner) = self.planner.as_mut() else {
            return;
        };
        let min_gaps = if at_pulse {
            MIN_GAPS_AT_PULSE
        } else {
            MIN_GAPS_FOR_RATE_CHECK
        };
        // Each try looks at every frame drawn so far, so it waits for more
        // reports before trying again (and there are no gaps without
        // reports). The count stops at the planner's fit window, so if that
        // many reports never gave enough gaps, only the pulse tries again.
        let points = planner.feedback_points();
        if points < min_gaps || (!at_pulse && points < self.next_rate_check) {
            return;
        }
        self.next_rate_check = points + MIN_GAPS_FOR_RATE_CHECK;
        let gaps = planner.shown_intervals_s();
        if gaps.len() < min_gaps {
            return;
        }
        let Some(seen) = measured_refresh_hz(&gaps) else {
            return;
        };
        self.rate_checked = true;
        let reported = self.reported_hz.is_some();
        if !nominal_rate_is_wrong(self.nominal_hz, seen, reported) {
            return;
        }
        let schedule = match FrameSchedule::new(&self.design, seen) {
            Ok(s) => s,
            Err(e) => {
                println!(
                    "WARNING: new frames reach the screen {seen:.1} times a second, not {:.2}, \
                     but the design can't run at {seen:.1} Hz ({e}). Keeping {:.2} Hz; the run's \
                     timing will be wrong.",
                    self.nominal_hz, self.nominal_hz
                );
                return;
            }
        };
        if !planner.restart_measuring(1.0 / seen) {
            return;
        }
        let said = match self.reported_hz {
            Some(hz) => format!("The display reports {hz:.2} Hz"),
            None => "The display didn't report a refresh rate".to_string(),
        };
        println!(
            "WARNING: {said}, but new frames reach the screen about {seen:.2} times a second. \
             Using {seen:.2} Hz and measuring the exact rate again: blocks of {} frames, \
             reversal every {} frames.",
            schedule.block_frames, schedule.reversal_frames
        );
        self.nominal_hz = seen;
        self.nominal_from = NominalFrom::FramesSeen;
        self.nominal_schedule = Some(schedule);
        // Report the settled timing again, on the new grid.
        self.settled_reported = false;
        self.settle_warned = false;
        self.waiting_since = None;
    }

    /// Seconds since `origin`, for the pulse watches.
    fn secs(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.origin).as_secs_f64()
    }

    fn on_key(&mut self, at: Instant, text: &str) {
        let zero = match self.run {
            Run::Running(s) | Run::Holding(s) | Run::Draining { started: s, .. } => Some(s.zero),
            Run::Waiting | Run::Finished => None,
        };
        if !text.eq_ignore_ascii_case(&self.trigger_key) {
            self.keys.push((at, text.to_string()));
            if let Some(zero) = zero {
                println!("key '{text}' at {:.3} s", signed_secs(at, zero));
            }
            return;
        }

        let t = self.secs(at);
        if matches!(self.run, Run::Waiting) {
            // A pulse that comes while a scan is already under way (the
            // tail of the previous scan, or Onset started mid-scan) must not
            // start the run.
            let ready = self.ready_s.unwrap_or(t);
            let quiet = self.wait_watch.quiet_since(ready, t);
            self.wait_watch.pulse(t);
            if !quiet {
                self.ignored_pulses.push(at);
                self.announce_ready = true;
                println!(
                    "Pulse ignored: the scanner was already running when Onset got ready. \
                     Waiting for {:.1} s without pulses before accepting the first one.",
                    self.wait_watch.stopped_after_s()
                );
                return;
            }
        }

        self.pulses.push(at);
        if matches!(self.run, Run::Running(_) | Run::Holding(_) | Run::Waiting) {
            self.run_watch.pulse(t);
        }
        match (self.run, zero) {
            (Run::Waiting, _) => {
                println!("Pulse 1: run started.");
                self.start(at);
            }
            (_, Some(zero)) => {
                println!(
                    "Pulse {} at {:.3} s",
                    self.pulses.len(),
                    signed_secs(at, zero)
                );
            }
            _ => {}
        }
    }

    /// Hand Metal's reports of frames reaching the screen to the planner,
    /// and give up on frames that have waited too long. `now_s` is host time.
    fn collect_feedback(&mut self, now_s: f64) {
        let (Some(renderer), Some(planner)) = (self.renderer.as_ref(), self.planner.as_mut())
        else {
            return;
        };
        for (id, presented_s) in renderer.presented() {
            planner.on_feedback(id, presented_s);
        }
        planner.expire(now_s);
    }

    /// Wait for the next free frame buffer, work out which frame of the run
    /// it is, and draw it. Returns false if there was no frame to draw.
    fn on_frame(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };
        let frame = match renderer.next_frame() {
            Ok(frame) => frame,
            Err(reason) => {
                self.collect_feedback(host_now());
                self.no_frames(event_loop, reason);
                return false;
            }
        };
        if self.skipping_since.take().is_some() && self.skip_warned {
            println!("Frames are being drawn again.");
            self.skip_warned = false;
        }
        let acquired = frame.acquired;
        self.last_drawn = Some(acquired);

        // When we got the frame buffer, on the display's clock. Read before
        // handling feedback, so it's as close to the wait as possible.
        let now_s = self.clock.instant_to_host(frame.acquired);
        self.collect_feedback(now_s);
        if matches!(self.run, Run::Waiting) {
            self.while_waiting(frame.acquired);
        }

        let Some(planner) = self.planner.as_mut() else {
            return false;
        };
        let plan = planner.begin_frame(now_s);
        // The renderer and the planner each count every frame from 0, so
        // Metal's reports (renderer ids) find the right planner record.
        debug_assert_eq!(
            plan.id,
            frame.id(),
            "renderer and planner frame counts drifted apart"
        );

        let screen = match self.run {
            Run::Running(started) => {
                let index = plan
                    .planned_index
                    .expect("start_run was called before this frame");
                let screen = started.schedule.screen_at(index);
                if screen != Screen::Done {
                    self.run_frames.push(RunFrame {
                        id: plan.id,
                        screen,
                    });
                }
                screen
            }
            Run::Waiting => Screen::Rest,
            Run::Holding(_) | Run::Draining { .. } | Run::Finished => Screen::Done,
        };

        let look = match screen {
            Screen::Checkerboard { phase } => Look {
                checkerboard: Some(phase),
                diode_white: !phase,
            },
            Screen::Rest | Screen::Done => Look {
                checkerboard: None,
                diode_white: false,
            },
        };
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.draw(frame, look);
        }

        match self.run {
            Run::Running(started) if screen == Screen::Done => {
                self.stimulus_done(event_loop, started)
            }
            Run::Holding(_) => self.check_scan_stopped(event_loop, acquired),
            Run::Running(started) if started.overran(acquired) => {
                println!(
                    "WARNING: the run has gone on more than {OVERRUN_LIMIT_S:.0} s past its planned \
                     {:.1} s: the stimulus fell behind the clock (frames lost without being caught \
                     up, or a display slower than the schedule). Ending it now, in step with the \
                     scanner; whatever was left of the run is cut.",
                    started.run_s()
                );
                self.end_run(event_loop, false);
            }
            Run::Draining { .. } => self.check_drained(event_loop),
            _ => {}
        }
        true
    }

    /// Before the first pulse: report the refresh rate once the frame timing
    /// has settled, and start by ourselves with --no-wait.
    fn while_waiting(&mut self, now: Instant) {
        if self.announce_ready
            && let Some(ready) = self.ready_s
            && self.wait_watch.quiet_since(ready, self.secs(now))
        {
            self.announce_ready = false;
            println!("The scanner has stopped. Ready for the first pulse of the next scan.");
        }
        // First, so a wrong nominal rate is fixed before the timing is
        // reported as settled.
        self.check_nominal_rate(false);
        let since = *self.waiting_since.get_or_insert(now);
        let Some(planner) = self.planner.as_ref() else {
            return;
        };
        let settled = planner.is_warmed_up();
        if settled && !self.settled_reported {
            self.settled_reported = true;
            self.report_refresh();
        } else if HAS_FEEDBACK
            && !settled
            && !self.settle_warned
            && now - since > SETTLE_WARNING_AFTER
        {
            self.settle_warned = true;
            println!(
                "WARNING: the frame timing hasn't settled after {} s ({} usable reports from \
                 the display, {} that made no sense). Is the window on screen, and is the \
                 display running at {}?",
                SETTLE_WARNING_AFTER.as_secs(),
                planner.feedback_points(),
                planner.anomalies(),
                self.nominal_label()
            );
        }

        if self.args.no_wait && (settled || !HAS_FEEDBACK || now - since >= NO_WAIT_CAP) {
            println!("Starting now (--no-wait). Esc stops.");
            self.start(now);
        }
    }

    /// Compare the refresh rate we're counting with to the one we see.
    fn report_refresh(&self) {
        let Some(planner) = self.planner.as_ref() else {
            return;
        };
        let nominal = self.nominal_hz;
        let nominal_label = self.nominal_label();
        let measured = 1.0 / planner.period_s();
        let precision = planner.measured_period().map_or(String::new(), |m| {
            format!(" ±{:.0} ppm", m.std_err_s / m.period_s * 1e6)
        });
        println!("Measured refresh rate: {measured:.3} Hz{precision} ({nominal_label} nominal).");
        let check_display =
            "Check that the display isn't mirrored and that Onset is on the right screen.";
        if (measured - nominal).abs() / nominal > REFRESH_MISMATCH {
            println!(
                "WARNING: those differ by more than {:.1} %. {check_display}",
                REFRESH_MISMATCH * 100.0
            );
        }
        // Gaps between frames on screen: one refresh each if every frame
        // is shown. Two would mean only every second refresh gets a new
        // frame, which the fitted period alone can't tell apart. More new
        // frames than refreshes is impossible, so it means the rate we're
        // counting with is wrong.
        if let Some(seen) = measured_refresh_hz(&planner.shown_intervals_s()) {
            if seen < measured * (1.0 - FRAME_RATE_MISMATCH) {
                println!(
                    "WARNING: only {seen:.1} new frames per second are reaching the screen. \
                     {check_display}"
                );
            } else if seen > measured * (1.0 + FRAME_RATE_MISMATCH) {
                println!(
                    "WARNING: {seen:.1} new frames per second are reaching the screen, more than \
                     the measured {measured:.1} Hz refresh rate allows; the frame timing can't be \
                     trusted. {check_display}"
                );
            }
        }
        if planner.anomalies() > 0 {
            println!(
                "WARNING: {} of the display's frame reports made no sense and were ignored. \
                 Is it really running at {nominal_label}?",
                planner.anomalies()
            );
        }
    }

    /// Called when the display gives us no frame (after up to 1 s of
    /// waiting). Warns, and ends the run on time even if no frame can be
    /// drawn, so a hidden window can't make it hang.
    fn no_frames(&mut self, event_loop: &ActiveEventLoop, reason: &str) {
        let now = Instant::now();
        let since = *self
            .skipping_since
            .get_or_insert(self.last_drawn.unwrap_or(now));
        if !self.skip_warned && now - since > Duration::from_millis(500) {
            println!("WARNING: no frames are being drawn ({reason}).");
            self.skip_warned = true;
        }
        match self.run {
            Run::Running(started) => {
                if now.saturating_duration_since(started.zero).as_secs_f64() >= started.run_s() {
                    println!("The run's time is up, but frames weren't being drawn at the end.");
                    self.stimulus_done(event_loop, started);
                }
            }
            Run::Holding(_) => self.check_scan_stopped(event_loop, now),
            Run::Draining { .. } => self.check_drained(event_loop),
            Run::Waiting | Run::Finished => {}
        }
    }

    /// The stimulus schedule has run out. If the scanner is still sending
    /// pulses, hold the rest screen until it stops, so the end of the scan
    /// isn't left without a stimulus and its last pulses can't start
    /// another run.
    fn stimulus_done(&mut self, event_loop: &ActiveEventLoop, started: Started) {
        let now = self.secs(Instant::now());
        let scanning = self
            .run_watch
            .last_s()
            .is_some_and(|last| now - last < self.run_watch.stopped_after_s());
        if scanning {
            println!(
                "Stimulus finished at {:.1} s, but the scanner is still going: holding the rest \
                 screen until it stops ({:.1} s without a pulse). Esc to stop now.",
                started.run_s(),
                self.run_watch.stopped_after_s()
            );
            self.run = Run::Holding(started);
        } else {
            self.end_run(event_loop, false);
        }
    }

    /// While holding: end once the scanner has stopped sending pulses.
    fn check_scan_stopped(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        let now_s = self.secs(now);
        let stopped = self
            .run_watch
            .last_s()
            .is_none_or(|last| now_s - last >= self.run_watch.stopped_after_s());
        if stopped {
            println!("The scanner has stopped.");
            self.end_run(event_loop, false);
        }
    }

    /// The run is over (or stopped). Wait briefly for Metal's last reports,
    /// then save. Asking again while waiting saves straight away.
    fn end_run(&mut self, event_loop: &ActiveEventLoop, stopped_early: bool) {
        match self.run {
            Run::Waiting => {
                println!("Stopped before the run started; nothing to save.");
                self.run = Run::Finished;
                event_loop.exit();
            }
            Run::Holding(started) => {
                if stopped_early {
                    println!("Stopped waiting for the scanner.");
                }
                self.run = Run::Draining {
                    started,
                    stopped_early: false,
                    until: Instant::now() + Duration::from_secs_f64(planner::FEEDBACK_TIMEOUT_S),
                };
                self.check_drained(event_loop);
            }
            Run::Running(started) => {
                self.run = Run::Draining {
                    started,
                    stopped_early,
                    until: Instant::now() + Duration::from_secs_f64(planner::FEEDBACK_TIMEOUT_S),
                };
                self.check_drained(event_loop);
            }
            Run::Draining { .. } => self.finish(event_loop),
            Run::Finished => {}
        }
    }

    /// Save once every frame of the run has its report, or time is up.
    fn check_drained(&mut self, event_loop: &ActiveEventLoop) {
        let Run::Draining { until, .. } = self.run else {
            return;
        };
        let settled = match (self.run_frames.last(), self.planner.as_ref()) {
            (Some(last), Some(planner)) => !HAS_FEEDBACK || planner.settled_through(last.id),
            _ => true,
        };
        if settled || Instant::now() >= until {
            self.finish(event_loop);
        }
    }

    fn finish(&mut self, event_loop: &ActiveEventLoop) {
        let Run::Draining {
            started,
            stopped_early,
            ..
        } = self.run
        else {
            return;
        };
        self.run = Run::Finished;
        event_loop.exit();
        // Last reports in; frames still without one are logged as such.
        self.collect_feedback(host_now());

        let rows = self.run_rows();
        self.print_summary(started, stopped_early, &rows);
        match self.save_logs(started, &rows) {
            Ok(()) => println!("Logs saved to {}/", self.out_dir.display()),
            Err(e) => {
                eprintln!(
                    "Error: couldn't save logs to {}: {e}",
                    self.out_dir.display()
                );
                self.exit_code = ExitCode::FAILURE;
            }
        }
    }

    /// The planner's timing for each frame drawn during the run, with what
    /// it showed. Leaves out the rest frames drawn after the run while
    /// waiting for the last reports.
    fn run_rows(&self) -> Vec<(FrameRow, Screen)> {
        let Some(planner) = self.planner.as_ref() else {
            return Vec::new();
        };
        let Some(first) = planner.run_first_id() else {
            return Vec::new();
        };
        // `rows()` has one row per frame from the run's frame 0 on.
        let rows = planner.rows();
        self.run_frames
            .iter()
            .filter_map(|f| {
                let row = rows.get(f.id.checked_sub(first)? as usize)?;
                debug_assert_eq!(row.id, f.id);
                Some((*row, f.screen))
            })
            .collect()
    }

    fn print_summary(&self, started: Started, stopped_early: bool, rows: &[(FrameRow, Screen)]) {
        println!();
        println!(
            "--- Run {} ---",
            if stopped_early {
                "stopped early"
            } else {
                "finished"
            }
        );
        let frames: Vec<FrameRow> = rows.iter().map(|(r, _)| *r).collect();
        let period_s = self
            .planner
            .as_ref()
            .map_or(1.0 / started.hz, |p| p.period_s());
        let s = Summary::of(&frames, period_s);
        if HAS_FEEDBACK {
            if s.shown == 0 {
                println!(
                    "Frames: {} drawn. Dropped frames unknown: the display timed none of them.",
                    s.frames
                );
            } else {
                let coverage = if s.shown < s.frames {
                    format!(", counted over the {} frames the display timed", s.shown)
                } else {
                    String::new()
                };
                println!(
                    "Frames: {} drawn, {} dropped (refreshes that showed the previous frame \
                     again{coverage}).",
                    s.frames, s.missed_vsyncs
                );
            }
            println!(
                "Reported by the display: {} on screen, {} never shown, {} with no report.",
                s.shown,
                s.not_shown,
                s.no_feedback + s.pending
            );
            if s.anomalous > 0 {
                println!("Reports that made no sense (ignored): {}.", s.anomalous);
            }
            if s.skipped_indices > 0 || s.holds > 0 {
                println!(
                    "Stimulus frames skipped to catch up: {}; repeated to wait: {}.",
                    s.skipped_indices, s.holds
                );
            }
            let reported = self
                .reported_hz
                .map_or("no rate".to_string(), |hz| format!("{hz:.2} Hz"));
            let measured = self.planner.as_ref().and_then(|p| p.measured_period());
            match measured {
                Some(m) => println!(
                    "Measured refresh rate: {:.3} Hz ±{:.0} ppm (display reports {reported}; \
                     the schedule used {:.3} Hz).",
                    1.0 / m.period_s,
                    m.std_err_s / m.period_s * 1e6,
                    started.hz
                ),
                None => println!(
                    "Refresh rate: not measured (display reports {reported}; the schedule \
                     used {:.3} Hz).",
                    started.hz
                ),
            }
        } else {
            println!(
                "Frames: {} drawn. This system doesn't report when frames reach the screen, \
                 so dropped frames aren't known.",
                s.frames
            );
        }

        let pulse_times: Vec<f64> = self
            .pulses
            .iter()
            .map(|p| signed_secs(*p, started.zero))
            .collect();
        match tr_stats(&pulse_times) {
            Some(s) => println!(
                "Scanner pulses: {}. TR median {:.4} s, SD {:.1} ms, likely missed {}.",
                s.pulses,
                s.median_tr_s,
                s.sd_s * 1000.0,
                s.likely_missed
            ),
            None => println!("Scanner pulses: {}.", self.pulses.len()),
        }
        self.print_scan_coverage(started, &pulse_times);
        if !self.ignored_pulses.is_empty() {
            println!(
                "Pulses ignored before the run (a scan was already under way): {}.",
                self.ignored_pulses.len()
            );
        }
        if !self.keys.is_empty() {
            println!("Other keys pressed: {}", self.keys.len());
        }
    }

    /// Did the scan cover the whole stimulus? Pulse times are seconds from
    /// the first pulse.
    fn print_scan_coverage(&self, started: Started, pulse_times: &[f64]) {
        let (Some(&last), Some(tr)) = (pulse_times.last(), self.run_watch.tr_s()) else {
            return;
        };
        let run_s = started.run_s();
        // The last volume is acquired from its pulse to one TR later.
        let scanned_until = last + tr;
        let volumes_needed = (run_s / tr).ceil() as u64;
        if scanned_until < run_s - 0.5 * tr {
            println!(
                "WARNING: the scan stopped at {scanned_until:.1} s (last pulse {last:.1} s + one TR), \
                 before the stimulus ended at {run_s:.1} s: the end of the run wasn't scanned. \
                 This design needs at least {volumes_needed} volumes at TR {tr:.2} s."
            );
        } else if last > run_s {
            println!(
                "The scan went on until {scanned_until:.1} s; the stimulus ended at {run_s:.1} s and \
                 the rest screen was held until the scanner stopped. (This design needs \
                 {volumes_needed} volumes at TR {tr:.2} s.)"
            );
        } else {
            println!(
                "The scan covered the whole stimulus ({run_s:.1} s; {volumes_needed} volumes at TR \
                 {tr:.2} s)."
            );
        }
    }

    fn save_logs(&self, started: Started, rows: &[(FrameRow, Screen)]) -> io::Result<()> {
        fs::create_dir_all(&self.out_dir)?;

        // BIDS events: one row per checkerboard block that actually started,
        // in seconds from the first pulse. `onset` is when the block first
        // reached the screen (1-3 frames after the planned onset, since
        // frame 0 is the first frame drawn after the pulse). `onset_source`
        // says how that was found: `presented` (Metal's time for the
        // block's first frame on screen), `estimated` (worked back from a
        // later frame of the block) or `planned` (no time known; the
        // schedule's onset). The last two columns say where the schedule
        // put the block.
        let drawn: Vec<DrawnFrame> = rows
            .iter()
            .map(|(r, _)| DrawnFrame {
                index: r.planned_index,
                presented_s: r.presented_s,
                not_shown: r.status == Status::NotShown,
            })
            .collect();
        write_tsv(
            &self.out_dir.join("events.tsv"),
            "onset\tduration\ttrial_type\tonset_source\tplanned_onset\tplanned_duration",
            started
                .schedule
                .block_events(&drawn, started.hz)
                .into_iter()
                .map(|e| {
                    format!(
                        "{:.4}\t{:.4}\tcheckerboard\t{}\t{:.4}\t{:.4}",
                        e.onset_s,
                        e.duration_s,
                        e.onset_source.label(),
                        e.planned_onset_s,
                        e.planned_duration_s
                    )
                }),
        )?;

        // Every frame of the run, counted from frame 0:
        // - `frame`: the stimulus frame drawn;
        // - `planned_vsync`: the refresh the planner predicted it would land on;
        // - `vsync`: the refresh it really landed on, from Metal's report;
        // - `acquired_s`: when we got its frame buffer and started drawing
        //   it (CPU time);
        // - `presented_s`: when it reached the screen;
        // - `missed`: refreshes just before it that repeated the previous
        //   frame (dropped frames);
        // - `late`: refreshes later than the schedule wanted its content.
        let na = |v: Option<String>| v.unwrap_or_else(|| "n/a".to_string());
        write_tsv(
            &self.out_dir.join("frames.tsv"),
            "frame\tplanned_vsync\tvsync\tacquired_s\tpresented_s\tmissed\tlate\tscreen\tfeedback",
            rows.iter().map(|(r, screen)| {
                format!(
                    "{}\t{}\t{}\t{:.6}\t{}\t{}\t{}\t{}\t{}",
                    r.planned_index,
                    r.planned_vsync,
                    na(r.vsync_index.map(|v| v.to_string())),
                    r.render_start_s,
                    na(r.presented_s.map(|t| format!("{t:.6}"))),
                    na(r.missed_vsyncs.map(|m| m.to_string())),
                    na(r.late_vsyncs.map(|l| l.to_string())),
                    screen.label(),
                    r.status.label()
                )
            }),
        )?;

        // Pulses and other keys, in seconds from time zero (the first pulse).
        let mut presses: Vec<(Instant, &str, &str)> = self
            .pulses
            .iter()
            .map(|p| (*p, "pulse", self.trigger_key.as_str()))
            .chain(self.keys.iter().map(|(at, k)| (*at, "key", k.as_str())))
            .chain(
                self.ignored_pulses
                    .iter()
                    .map(|p| (*p, "ignored_pulse", self.trigger_key.as_str())),
            )
            .collect();
        presses.sort_by_key(|(at, _, _)| *at);
        write_tsv(
            &self.out_dir.join("keys.tsv"),
            "time_s\tkind\tkey",
            presses.into_iter().map(|(at, kind, key)| {
                format!("{:.6}\t{kind}\t{key}", signed_secs(at, started.zero))
            }),
        )
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        // Draw continuously instead of waiting for the system to ask for a
        // redraw. macOS stops asking when a window is hidden, which would
        // stall the run; this way the loop is paced only by the display.
        event_loop.set_control_flow(ControlFlow::Poll);

        // Sleeping displays aren't listed. `keep_displays_awake` has asked
        // them to wake; give them up to 5 s to come back.
        let mut monitors: Vec<MonitorHandle> = Vec::new();
        for attempt in 0..50 {
            monitors = event_loop.available_monitors().collect();
            if !monitors.is_empty() {
                break;
            }
            if attempt == 0 {
                println!("Waiting for the displays to wake up...");
            }
            thread::sleep(Duration::from_millis(100));
        }
        let main = event_loop.primary_monitor();
        println!("Displays found:");
        for (i, m) in monitors.iter().enumerate() {
            let tag = if Some(m) == main.as_ref() {
                " (main)"
            } else {
                ""
            };
            println!("  {}. {}{tag}", i + 1, describe(m));
        }

        let choice = if self.args.windowed {
            "main"
        } else {
            self.args.display.as_str()
        };
        let monitor = match choice {
            "main" => main.clone(),
            "external" => monitors.iter().find(|m| Some(*m) != main.as_ref()).cloned(),
            n => n
                .parse::<usize>()
                .ok()
                .and_then(|n| monitors.get(n.wrapping_sub(1)))
                .cloned(),
        };
        let Some(monitor) = monitor else {
            let message = match choice {
                "external" => "no external display found. Is the BOLDscreen connected? \
                               (Use --display main or --windowed to test on the laptop.)"
                    .to_string(),
                other => format!("no display '{other}'; use external, main or a number above"),
            };
            return self.fail(event_loop, &message);
        };

        self.reported_hz = monitor
            .refresh_rate_millihertz()
            .filter(|mhz| *mhz > 0)
            .map(|mhz| f64::from(mhz) / 1000.0);
        match self.reported_hz {
            Some(hz) => {
                self.nominal_hz = hz;
                self.nominal_from = NominalFrom::Display;
            }
            None => println!(
                "WARNING: the display didn't report a refresh rate. Assuming 60 Hz until the \
                 frames reaching the screen show the real one."
            ),
        }

        // Checked here at the nominal rate, so a bad design fails before
        // the window opens. The run itself uses the measured rate.
        let schedule = match FrameSchedule::new(&self.design, self.nominal_hz) {
            Ok(s) => s,
            Err(e) => return self.fail(event_loop, &format!("can't run this design: {e}")),
        };
        self.nominal_schedule = Some(schedule);

        let mut attributes = Window::default_attributes().with_title("Onset");
        attributes = if self.args.windowed {
            attributes.with_inner_size(LogicalSize::new(1280.0, 720.0))
        } else {
            // Not macOS's own full-screen mode: that animates into a new
            // Space and can land on the wrong display. Instead, a borderless
            // window placed exactly over the chosen display. Positions are
            // given in that display's own points, so the laptop's Retina
            // scaling can't shift the window onto the wrong screen.
            let scale = monitor.scale_factor();
            let position: LogicalPosition<f64> = monitor.position().to_logical(scale);
            let size: LogicalSize<f64> = monitor.size().to_logical(scale);
            attributes
                .with_decorations(false)
                .with_resizable(false)
                .with_position(position)
                .with_inner_size(size)
        };
        let window = match event_loop.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, &format!("couldn't open a window: {e}")),
        };
        if !self.args.windowed {
            window.set_cursor_visible(false);
            cover_menu_bar(&window);
            window.focus_window();
        }
        match Renderer::new(window.clone(), self.args.diode_px) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => return self.fail(event_loop, &e),
        }
        // Starts from the nominal rate and measures the real one from
        // Metal's reports while we wait for the scanner.
        self.planner = Some(Planner::new(planner::Config::new(1.0 / self.nominal_hz)));

        let hz = self.nominal_hz;
        println!(
            "Showing on {} (one frame = {:.2} ms).",
            describe(&monitor),
            1000.0 / hz
        );
        println!(
            "Design: rest + {} × (checkerboard, rest), blocks of {:.3} s ({} frames). \
             Reversal every {} frames ({:.2}/s). Run length {:.1} s.",
            self.args.blocks,
            frame_to_onset(schedule.block_frames, hz),
            schedule.block_frames,
            schedule.reversal_frames,
            hz / schedule.reversal_frames as f64,
            frame_to_onset(schedule.total_frames(), hz),
        );
        if !HAS_FEEDBACK {
            println!(
                "Note: this system doesn't report when frames reach the screen, so frame \
                 timing isn't measured and dropped frames aren't caught up."
            );
        }

        if self.args.no_wait {
            println!("Starting as soon as the frame timing has settled (--no-wait).");
        } else {
            println!(
                "Waiting for the first '{}' pulse. Press {} yourself to start by hand; Esc quits.",
                self.trigger_key, self.trigger_key
            );
        }

        self.window = Some(window);
        self.ready_s = Some(self.secs(Instant::now()));
    }

    /// Runs whenever the event loop has handled all pending events (key
    /// presses included): draw the next frame. `next_frame` blocks until the
    /// display frees a frame buffer, so this runs once per refresh.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if matches!(self.run, Run::Finished) {
            return;
        }
        if !self.on_frame(event_loop) {
            // No frame this time (window hidden or being resized): don't spin.
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.end_run(event_loop, true),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed || event.repeat {
                    return;
                }
                let at = Instant::now();
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => self.end_run(event_loop, true),
                    Key::Character(text) => self.on_key(at, text),
                    _ => {}
                }
            }
            WindowEvent::Focused(focused) => {
                if focused {
                    println!("Onset window has keyboard focus.");
                } else if !matches!(self.run, Run::Finished) {
                    println!(
                        "WARNING: the Onset window lost keyboard focus, so scanner pulses \
                         won't reach it. Click on the Onset window."
                    );
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
            }
            _ => {}
        }
    }
}

/// macOS turns the displays off after a few idle minutes (2 on the scanner
/// laptop), which would black out the BOLDscreen in the middle of a run.
/// Wake them now and keep them on until Onset exits, using macOS's own
/// `caffeinate` tool.
#[cfg(target_os = "macos")]
fn keep_displays_awake() {
    use std::process::{Command, Stdio};

    let pid = std::process::id().to_string();
    let jobs: [&[&str]; 2] = [
        // Count as user activity for 2 s, which wakes sleeping displays.
        &["-u", "-t", "2"],
        // Block display and idle sleep until this process exits.
        &["-d", "-i", "-w", &pid],
    ];
    for args in jobs {
        match Command::new("caffeinate")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            // Reap it when it exits, from a thread so we don't block.
            Ok(mut child) => {
                thread::spawn(move || child.wait());
            }
            Err(e) => {
                eprintln!(
                    "WARNING: couldn't keep the displays awake ({e}); they may sleep mid-run."
                )
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn keep_displays_awake() {}

/// Lift the window above the menu bar, the Dock and notifications, and bring
/// it to the front even if macOS hasn't made Onset the active app.
#[cfg(target_os = "macos")]
fn cover_menu_bar(window: &Window) {
    use objc2_app_kit::{NSScreenSaverWindowLevel, NSView};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit hands us a valid NSView pointer for a window that is
    // alive for this whole call, and we're on the main thread (the event
    // loop), which AppKit requires.
    let view: &NSView = unsafe { appkit.ns_view.cast().as_ref() };
    if let Some(ns_window) = view.window() {
        ns_window.setLevel(NSScreenSaverWindowLevel);
        unsafe { ns_window.orderFrontRegardless() };
    }
}

#[cfg(not(target_os = "macos"))]
fn cover_menu_bar(_window: &Window) {}

fn describe(m: &MonitorHandle) -> String {
    let size = m.size();
    let hz = m
        .refresh_rate_millihertz()
        .map_or("unknown Hz".to_string(), |mhz| {
            format!("{:.2} Hz", f64::from(mhz) / 1000.0)
        });
    format!(
        "{}: {}×{} @ {hz}",
        m.name().unwrap_or_else(|| "(unnamed)".into()),
        size.width,
        size.height
    )
}

/// Seconds from `zero` to `at`, negative if `at` came first.
fn signed_secs(at: Instant, zero: Instant) -> f64 {
    if at >= zero {
        (at - zero).as_secs_f64()
    } else {
        -(zero - at).as_secs_f64()
    }
}

fn write_tsv(path: &Path, header: &str, rows: impl Iterator<Item = String>) -> io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(out, "{header}")?;
    for row in rows {
        writeln!(out, "{row}")?;
    }
    out.flush()
}

fn main() -> ExitCode {
    let args = Args::parse();
    keep_displays_awake();
    let event_loop = match EventLoop::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: couldn't start the window system: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut app = App::new(args);
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("Error: {e}");
        return ExitCode::FAILURE;
    }
    app.exit_code
}
