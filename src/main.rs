//! The `onset` command-line program.

mod console;
mod macos;

#[cfg(not(target_os = "macos"))]
compile_error!("Onset supports macOS only.");

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use console::{bold, dim, green, red, yellow};
use onset::experiment::Experiment;
use onset::hostclock::HostClock;
use onset::input::{self, RawInput, Source};
use onset::output::{self, LogMsg, Paths, WriterReport};
use onset::planner::{self, Feedback, Planner};
use onset::render::{PhotodiodePatch, Renderer};
use onset::rig::{MonitorSelect, Rig, TriggerSource};
use onset::run::{EndReason, FEEDBACK_TIMEOUT_S, Input, InputUse, Notice, Phase, Report, Run};
use onset::scanner::{DEFAULT_QUIET_S, PulseWatch};
use onset::schedule::{self, Timeline};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowId};

#[derive(Parser)]
#[command(name = "onset", version, about = "Frame-accurate stimulus presentation for fMRI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run an experiment.
    Run(RunArgs),
    /// Check an experiment (and optionally a rig file) without running it.
    Validate {
        experiment: PathBuf,
        #[arg(long)]
        rig: Option<PathBuf>,
        /// Use this schedule file instead of the experiment's.
        #[arg(long)]
        schedule: Option<PathBuf>,
    },
    /// Print the planned timeline, event by event.
    Schedule {
        experiment: PathBuf,
        /// Shuffle seed (if the experiment shuffles and doesn't fix one).
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Use this schedule file instead of the experiment's.
        #[arg(long)]
        schedule: Option<PathBuf>,
    },
    /// Measure refresh rate and dropped frames; flashes the photodiode patch every frame.
    TimingTest {
        #[arg(long)]
        rig: Option<PathBuf>,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Use a window instead of fullscreen.
        #[arg(long)]
        windowed: bool,
    },
    /// List connected monitors (for the rig's `[display] monitor`).
    Displays,
    /// List serial ports and USB input devices (for the rig's `[trigger]`).
    Ports,
    /// Print every trigger and button press as it arrives, without a window:
    /// a quick check of the trigger box and button pads before a run.
    Inputs {
        #[arg(long)]
        rig: Option<PathBuf>,
        /// Stop after this many seconds. Default: when a scan that sent
        /// pulses has stopped (no pulse for 2.5 TRs, at least 5 s), or Ctrl-C.
        #[arg(long)]
        seconds: Option<f64>,
    },
    /// Record the photodiode/trigger probe (Arduino) to a TSV file.
    Probe(onset::probe::Args),
    /// Compare logged onsets with photodiode-measured light onsets.
    CheckTiming(onset::check_timing::Args),
    /// Write an experiment's schedule TSV from a folder of images (see [schedule.generate]).
    MakeSchedule(onset::make_schedule::Args),
}

#[derive(clap::Args)]
struct RunArgs {
    experiment: PathBuf,
    /// Use this schedule file instead of the experiment's (e.g. one per run).
    #[arg(long)]
    schedule: Option<PathBuf>,
    /// Rig file (display, trigger, buttons). Defaults: keyboard trigger "5", buttons 1-4.
    #[arg(long)]
    rig: Option<PathBuf>,
    /// Subject label, e.g. 01. Asked for if missing.
    #[arg(long, short)]
    sub: Option<String>,
    /// Session label.
    #[arg(long)]
    ses: Option<String>,
    /// Participant's name for the welcome screen. Shown only; never saved.
    #[arg(long)]
    name: Option<String>,
    /// Run number. Defaults to the next one without data.
    #[arg(long, short)]
    run: Option<u32>,
    /// Output directory (overrides the rig's `[output] dir`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Use a window instead of fullscreen.
    #[arg(long)]
    windowed: bool,
    /// Fake scanner triggers every TR seconds (default: the experiment's TR, or 2).
    #[arg(long, value_name = "TR", num_args = 0..=1, default_missing_value = "0")]
    simulate: Option<f64>,
    /// Overwrite existing data for this run.
    #[arg(long)]
    force: bool,
    /// Shuffle seed (default: random, and recorded in the log).
    #[arg(long)]
    seed: Option<u64>,
    /// Simulated participant: presses buttons (mostly correct, 350-650 ms RTs)
    /// so you can dry-run the whole pipeline. Combine with --simulate.
    #[arg(long)]
    robot: bool,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run(args) => cmd_run(args),
        Cmd::Validate { experiment, rig, schedule } => {
            cmd_validate(&experiment, rig.as_deref(), schedule.as_deref())
        }
        Cmd::Schedule { experiment, seed, schedule } => cmd_schedule(&experiment, seed, schedule.as_deref()),
        Cmd::TimingTest { rig, seconds, windowed } => cmd_timing_test(rig.as_deref(), seconds, windowed),
        Cmd::Displays => cmd_displays(),
        Cmd::Ports => cmd_ports(),
        Cmd::Inputs { rig, seconds } => cmd_inputs(rig.as_deref(), seconds),
        Cmd::Probe(args) => onset::probe::run(args),
        Cmd::CheckTiming(args) => onset::check_timing::run(args),
        Cmd::MakeSchedule(args) => onset::make_schedule::run(args),
    };
    if let Err(e) = result {
        eprintln!("{} {e:#}", red("error:"));
        std::process::exit(1);
    }
}

fn load_rig(path: Option<&Path>) -> Result<Rig> {
    match path {
        Some(p) => Rig::load(p),
        None => Ok(Rig::default()),
    }
}

fn nominal_hz(rig: &Rig) -> f64 {
    rig.display.expected_hz.unwrap_or(60.0)
}

/// Load an experiment, optionally swapping in a different schedule file.
fn load_experiment(path: &Path, schedule: Option<&Path>) -> Result<Experiment> {
    let mut exp = Experiment::load(path)?;
    if let Some(file) = schedule {
        let abs = std::fs::canonicalize(file)
            .with_context(|| format!("can't find schedule file {}", file.display()))?;
        exp.schedule.file = Some(abs.display().to_string());
        exp.schedule.rows = None;
    }
    Ok(exp)
}

fn cmd_validate(exp_path: &Path, rig_path: Option<&Path>, schedule: Option<&Path>) -> Result<()> {
    let exp = load_experiment(exp_path, schedule)?;
    let rig = load_rig(rig_path)?;
    let tl = schedule::compile(&exp, 1, nominal_hz(&rig))?;
    if let Some(logo) = rig.site.logo.as_ref().filter(|p| !p.exists()) {
        println!("{} the rig's [site] logo {} doesn't exist", yellow("warning:"), logo.display());
    }
    for ev in &exp.trial.events {
        if let Some(r) = &ev.response {
            for b in &r.buttons {
                if !rig.is_known_button(b) {
                    println!(
                        "{} event {:?} accepts button {b:?}, but the rig has no key mapped to it (rig buttons: {})",
                        yellow("warning:"),
                        ev.name,
                        rig.buttons.values().cloned().collect::<Vec<_>>().join(", ")
                    );
                }
            }
        }
    }
    print_overview(&exp, &tl);
    println!("{}", green("✓ experiment is valid"));
    Ok(())
}

fn print_overview(exp: &Experiment, tl: &Timeline) {
    let logged = tl.events.iter().filter(|e| e.log).count();
    println!("{}  {}  task-{}", bold("▌"), bold(&exp.experiment.name), exp.experiment.task);
    println!(
        "   {} trials · {} events ({} in events.tsv) · {} · {} images",
        tl.trials.len(),
        tl.events.len(),
        logged,
        fmt_duration(tl.end),
        tl.images.len()
    );
    if let (Some(tr), Some(v)) = (exp.scanner.tr, exp.scanner.volumes) {
        println!("   scanner: {v} volumes × TR {tr} s, {} dummy", exp.scanner.dummy_volumes);
    }
    for w in &tl.warnings {
        println!("{} {w}", yellow("warning:"));
    }
}

fn fmt_duration(s: f64) -> String {
    let s = s.round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn cmd_schedule(exp_path: &Path, seed: u64, schedule: Option<&Path>) -> Result<()> {
    let exp = load_experiment(exp_path, schedule)?;
    let tl = schedule::compile(&exp, seed, 60.0)?;
    print_overview(&exp, &tl);
    println!();
    println!(
        "{}",
        dim(&format!(
            "{:>9}  {:>8}  {:>5}  {:<12} {:<18} {:<14} {}",
            "onset", "dur", "trial", "event", "stimulus", "trial_type", "response"
        ))
    );
    for e in &tl.events {
        let resp = e.response.as_ref().map_or(String::new(), |r| {
            format!(
                "{}{} ({:.2}s)",
                r.buttons.join("/"),
                r.correct.as_ref().map_or(String::new(), |c| format!(" ✓{c}")),
                r.window
            )
        });
        let line = format!(
            "{:>9.3}  {:>8.3}  {:>5}  {:<12} {:<18} {:<14} {}",
            e.onset, e.duration, e.trial, e.name, e.stimulus, e.trial_type, resp
        );
        if e.log { println!("{line}") } else { println!("{}", dim(&line)) }
    }
    println!("{}", dim(&format!("{:>9.3}  end", tl.end)));
    Ok(())
}

fn cmd_ports() -> Result<()> {
    let ports = serialport::available_ports().context("can't list serial ports")?;
    if ports.is_empty() {
        println!("No serial ports found.");
    }
    for p in ports {
        let desc = match p.port_type {
            serialport::SerialPortType::UsbPort(u) => format!(
                "USB {:04x}:{:04x} {} {}",
                u.vid,
                u.pid,
                u.manufacturer.unwrap_or_default(),
                u.product.unwrap_or_default()
            ),
            other => format!("{other:?}"),
        };
        println!("{}  {}", bold(&p.port_name), dim(&desc));
    }

    println!();
    println!("{}", bold("USB input devices (for [trigger] source = \"hid\", device = \"vendor:product\"):"));
    let api = hidapi::HidApi::new().context("can't list USB input devices")?;
    let mut seen = std::collections::BTreeSet::new();
    for d in api.device_list() {
        // One line per device, however many parts (keyboard, mouse, ...) it has.
        if !seen.insert((d.vendor_id(), d.product_id())) || d.vendor_id() == 0 {
            continue;
        }
        println!(
            "{}  {}",
            bold(&format!("{:04x}:{:04x}", d.vendor_id(), d.product_id())),
            dim(&format!(
                "{} {}",
                d.manufacturer_string().unwrap_or_default(),
                d.product_string().unwrap_or_default()
            ))
        );
    }
    Ok(())
}

fn cmd_inputs(rig_path: Option<&Path>, seconds: Option<f64>) -> Result<()> {
    let rig = load_rig(rig_path)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    match rig.trigger.source {
        TriggerSource::Hid => {
            let (vendor, product) = rig
                .trigger
                .device
                .as_deref()
                .and_then(input::parse_usb_id)
                .context("[trigger] device is missing or not \"vendor:product\"")?;
            if !macos::request_input_monitoring() {
                println!(
                    "{} macOS hasn't granted Input Monitoring yet. Allow it for this terminal app in \
                     System Settings → Privacy & Security → Input Monitoring, then run this again.",
                    yellow("note:")
                );
            }
            let reader = input::spawn_hid(vendor, product, tx, stop.clone())?;
            let filtered = !reader.exclusive && onset::keyfilter::start();
            println!(
                "Opened {:04x}:{:04x} {}.",
                vendor,
                product,
                match (reader.exclusive, filtered) {
                    (true, _) => "exclusively (its keys don't reach other apps)",
                    (false, true) => "shared, with its keystrokes filtered out of other apps",
                    (false, false) => "shared (its keys also reach the app in front; allow Accessibility to filter them)",
                }
            );
        }
        TriggerSource::Serial => {
            input::spawn_serial(rig.trigger.port.as_deref().unwrap_or_default(), rig.trigger.baud, tx, stop.clone())?;
        }
        TriggerSource::Keyboard | TriggerSource::Simulated => {
            anyhow::bail!(
                "this rig's trigger comes through the window (keyboard) or is simulated; `onset inputs` reads \
                 USB (\"hid\") and serial trigger boxes"
            );
        }
    }
    println!("Listening. Press buttons or start the scanner; Ctrl-C stops.");
    let start = Instant::now();
    let mut pulses = PulseWatch::new(DEFAULT_QUIET_S);
    loop {
        let now = start.elapsed().as_secs_f64();
        if seconds.is_some_and(|s| now >= s) {
            break;
        }
        // Without --seconds, stop once the scan has ended, so the scanner's
        // last pulses are never left to type into whatever app is in front.
        if seconds.is_none()
            && let Some(tr) = pulses.tr_s()
            && pulses.stopped(now)
        {
            println!(
                "The scanner stopped ({} pulses, TR {tr:.3} s; no pulse for {:.1} s).",
                pulses.count(),
                pulses.stopped_after_s()
            );
            break;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(RawInput::Key { t, source, raw }) => {
                let at = t.saturating_duration_since(start).as_secs_f64();
                let (input, _, label) = App::classify(&rig, source, &raw);
                let what = match input {
                    Some(Input::Trigger) => {
                        let gap = pulses.last_s().map_or(String::new(), |p| format!("  (+{:.3} s)", at - p));
                        pulses.pulse(at);
                        green(&format!("trigger {}{gap}", pulses.count()))
                    }
                    Some(_) => format!("button {}", bold(&label)),
                    None => dim("not in the rig"),
                };
                println!("{at:9.3} s  key {raw:?}  {what}");
            }
            Ok(RawInput::Error(e)) => anyhow::bail!(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    Ok(())
}

// ───────────────────────────── displays ─────────────────────────────

fn cmd_displays() -> Result<()> {
    struct Lister;
    impl ApplicationHandler for Lister {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            let primary = el.primary_monitor();
            for (i, m) in el.available_monitors().enumerate() {
                let size = m.size();
                let hz = m
                    .refresh_rate_millihertz()
                    .map_or("? Hz".into(), |mhz| format!("{:.2} Hz", mhz as f64 / 1000.0));
                let star = if Some(&m) == primary.as_ref() { " (primary)" } else { "" };
                println!(
                    "{}  {}  {}×{} @ {hz}, scale {}{star}",
                    bold(&format!("[{i}]")),
                    m.name().unwrap_or_else(|| "unnamed".into()),
                    size.width,
                    size.height,
                    m.scale_factor()
                );
            }
            el.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    EventLoop::new()?.run_app(&mut Lister)?;
    Ok(())
}

fn pick_monitor(el: &ActiveEventLoop, sel: Option<&MonitorSelect>) -> Result<Option<MonitorHandle>> {
    // Sleeping displays aren't listed. `keep_displays_awake` has asked them
    // to wake; give them up to 5 s to come back.
    let mut monitors: Vec<MonitorHandle> = Vec::new();
    for _ in 0..50 {
        monitors = el.available_monitors().collect();
        if !monitors.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let primary = el.primary_monitor();
    Ok(match sel {
        Some(MonitorSelect::Index(i)) => Some(
            monitors
                .get(*i)
                .cloned()
                .with_context(|| format!("no monitor [{i}]; run `onset displays` to list them"))?,
        ),
        Some(MonitorSelect::Name(name)) => Some(
            monitors
                .iter()
                .find(|m| m.name().is_some_and(|n| n.to_lowercase().contains(&name.to_lowercase())))
                .cloned()
                .with_context(|| format!("no monitor named like {name:?}; run `onset displays`"))?,
        ),
        // Prefer a secondary monitor (the projector) when there is one.
        None => monitors.iter().find(|m| Some(*m) != primary.as_ref()).cloned().or(primary),
    })
}

// ───────────────────────────── run ─────────────────────────────

struct RunSetup {
    exp: &'static Experiment,
    rig: Rig,
    timeline: Timeline,
    paths: Paths,
    sub: String,
    ses: Option<String>,
    run: u32,
    simulate_tr: Option<f64>,
    exp_path: PathBuf,
    rig_path: Option<PathBuf>,
    robot: bool,
    /// For the welcome screen only: keep it out of run.json and every log.
    name: Option<String>,
}

fn prompt(label: &str) -> Result<String> {
    eprint!("{label}: ");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn random_seed() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (t.as_nanos() as u64) % 1_000_000_000
}

fn cmd_run(args: RunArgs) -> Result<()> {
    // Leaked on purpose: the experiment lives for the whole process, and a
    // 'static reference lets the run borrow it without lifetime plumbing.
    let exp: &'static Experiment =
        Box::leak(Box::new(load_experiment(&args.experiment, args.schedule.as_deref())?));
    let mut rig = load_rig(args.rig.as_deref())?;
    if args.windowed {
        rig.display.fullscreen = false;
    }
    let seed = args.seed.unwrap_or_else(random_seed);
    let timeline = schedule::compile(exp, seed, nominal_hz(&rig))?;

    let sub = match args.sub {
        Some(s) => s,
        None => prompt("Subject (e.g. 01)")?,
    };
    output::check_label("subject", &sub)?;
    if let Some(ses) = &args.ses {
        output::check_label("session", ses)?;
    }
    if args.robot {
        println!("   {}", yellow("robot participant: button presses are simulated"));
    }
    if args.name.is_some() && exp.welcome.is_none() {
        println!(
            "{} --name is shown on the [welcome] screen, and this experiment has none",
            yellow("warning:")
        );
    }
    let out_dir = args.out.clone().unwrap_or_else(|| rig.output.dir.clone());
    let task = exp.experiment.task.clone();
    let run = match args.run {
        Some(r) => {
            let p = Paths::new(&out_dir, &sub, args.ses.as_deref(), &task, r);
            if p.exists() && !args.force {
                let free = output::next_free_run(&out_dir, &sub, args.ses.as_deref(), &task, r);
                bail!(
                    "data for run {r} already exists ({}).\n  Use --run {free} for a new run, or --force to overwrite.",
                    p.events_tsv().display()
                );
            }
            r
        }
        None => output::next_free_run(&out_dir, &sub, args.ses.as_deref(), &task, 1),
    };
    let paths = Paths::new(&out_dir, &sub, args.ses.as_deref(), &task, run);

    let simulate_tr = match args.simulate {
        Some(tr) if tr > 0.0 => Some(tr),
        Some(_) => Some(exp.scanner.tr.unwrap_or(rig.trigger.simulated_tr)),
        None if rig.trigger.source == TriggerSource::Simulated => {
            Some(exp.scanner.tr.unwrap_or(rig.trigger.simulated_tr))
        }
        None => None,
    };

    print_overview(exp, &timeline);
    let trigger_desc = match (simulate_tr, rig.trigger.source) {
        (Some(tr), _) => format!("simulated scanner, TR {tr} s"),
        (None, TriggerSource::Keyboard) => format!("keyboard key {:?}", rig.trigger.key),
        (None, TriggerSource::Hid) => format!(
            "USB device {} key {:?} (read directly; window focus doesn't matter)",
            rig.trigger.device.as_deref().unwrap_or("?"),
            rig.trigger.key
        ),
        (None, TriggerSource::Serial) => format!(
            "serial {} @ {} baud, char {:?}",
            rig.trigger.port.as_deref().unwrap_or("?"),
            rig.trigger.baud,
            rig.trigger.serial_char
        ),
        (None, TriggerSource::Simulated) => unreachable!(),
    };
    println!(
        "   sub-{sub}{} run-{run:02} → {}",
        args.ses.as_ref().map_or(String::new(), |s| format!(" ses-{s}")),
        paths.events_tsv().display()
    );
    println!("   trigger: {trigger_desc}");
    println!("   {}", dim("the first scanner pulse starts the run · Ctrl-C here or Esc on the stimulus window aborts (data is saved)"));

    let setup = RunSetup {
        exp,
        rig,
        timeline,
        paths,
        sub,
        ses: args.ses,
        run,
        simulate_tr,
        exp_path: args.experiment,
        rig_path: args.rig,
        robot: args.robot,
        name: args.name,
    };
    present(Mode::Run(setup))
}

fn cmd_timing_test(rig_path: Option<&Path>, seconds: f64, windowed: bool) -> Result<()> {
    let mut rig = load_rig(rig_path)?;
    if windowed {
        rig.display.fullscreen = false;
    }
    println!("{} flashing for {seconds} s… (Esc to stop early)", bold("timing test:"));
    present(Mode::TimingTest { rig, seconds })
}

#[allow(clippy::large_enum_variant)]
enum Mode {
    Run(RunSetup),
    TimingTest { rig: Rig, seconds: f64 },
}

/// Shared slot so the panic hook can finish the logs if anything crashes.
type WriterSlot = Arc<Mutex<Option<(Sender<LogMsg>, JoinHandle<Result<WriterReport>>)>>>;

fn present(mode: Mode) -> Result<()> {
    macos::keep_displays_awake();
    let writer: WriterSlot = Arc::new(Mutex::new(None));
    {
        let writer = writer.clone();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(mut slot) = writer.lock() {
                if let Some((tx, handle)) = slot.take() {
                    let _ = tx.send(LogMsg::Finish(serde_json::json!({
                        "end_reason": "crashed",
                        "panic": info.to_string(),
                    })));
                    drop(tx);
                    let _ = handle.join();
                    eprintln!("\nonset crashed; data up to the crash was saved.");
                }
            }
            default_hook(info);
        }));
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
    }

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    macos::raise_priority();
    let (raw_tx, raw_rx) = channel();
    let mut app = App {
        mode: Some(mode),
        state: None,
        epoch: Instant::now(),
        clock: HostClock::calibrate(1000),
        raw_tx,
        raw_rx,
        stop: Arc::new(AtomicBool::new(false)),
        interrupted,
        writer,
        result: None,
        console: console::Status::new(),
        stalled_since: None,
        stall_warned: false,
        last_restore: None,
    };
    event_loop.run_app(&mut app)?;
    app.stop.store(true, Ordering::SeqCst);
    app.result.unwrap_or(Ok(()))
}

struct App {
    mode: Option<Mode>,
    state: Option<Active>,
    /// Zero of the run clock: every time in the run is seconds since this.
    epoch: Instant,
    /// Converts between `Instant`s and the host time Metal reports frames in.
    clock: HostClock,
    raw_tx: Sender<RawInput>,
    raw_rx: Receiver<RawInput>,
    stop: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    writer: WriterSlot,
    result: Option<Result<()>>,
    console: console::Status,
    stalled_since: Option<Instant>,
    stall_warned: bool,
    last_restore: Option<Instant>,
}

/// Everything that exists once the window is open.
struct Active {
    renderer: Renderer,
    window: Arc<Window>,
    /// Predicts each frame's flip from the display's reports; `None` when
    /// the renderer gets no reports.
    planner: Option<Planner>,
    kind: ActiveKind,
}

#[allow(clippy::large_enum_variant)]
enum ActiveKind {
    Run(Box<ActiveRun>),
    TimingTest(TimingTest),
}

struct ActiveRun {
    run: Run<'static>,
    setup: Box<RunSetup>,
    log: Option<Sender<LogMsg>>,
    t0_sent: bool,
    simulator_started: bool,
    started_wall: chrono::DateTime<chrono::Local>,
    last_status: Instant,
    warned_hz: bool,
    /// Every scene the run can show, re-prepared whenever the window size changes.
    scenes: Vec<onset::scene::Scene>,
    prewarmed_size: (u32, u32),
    robot: Option<Robot>,
    snapshot_frame: Option<u64>,
    /// Stops the fake scanner once the stimulus is over, as a scan
    /// programmed to match the experiment would.
    simulator_stop: Arc<AtomicBool>,
    /// Once the run is over: keep drawing until the last frames' reports are
    /// in, or until this time, then save.
    drain_until: Option<Instant>,
}

/// The simulated participant.
struct Robot {
    rng: onset::rng::Rng,
    /// (when to press, raw key)
    pending: Vec<(Instant, String)>,
}

impl Robot {
    fn respond(&mut self, rig: &Rig, r: &onset::run::StartedResponse, now: Instant) {
        let roll = self.rng.below(100);
        if roll < 5 || r.buttons.is_empty() {
            return; // miss
        }
        let label = match &r.correct {
            Some(c) if roll >= 15 => c.clone(),
            _ => r.buttons[self.rng.below(r.buttons.len() as u64) as usize].clone(),
        };
        let Some(raw) = rig.buttons.iter().find(|(_, l)| **l == label).map(|(k, _)| k.clone()) else {
            return;
        };
        let rt = (0.35 + self.rng.below(300) as f64 / 1000.0).min(r.window * 0.9);
        self.pending.push((now + Duration::from_secs_f64(rt), raw));
    }
}

struct TimingTest {
    rig: Rig,
    seconds: f64,
    start: Option<f64>,
    last: Option<f64>,
    intervals: Vec<f64>,
    /// Every frame of the test, for the report and the frames.tsv log.
    frames: Vec<TestFrame>,
    /// Once the time is up: keep drawing until the last reports are in, or
    /// until this time.
    drain_until: Option<Instant>,
    /// When the window opened, to end the test on time even if the display
    /// never takes a frame.
    opened: Instant,
}

struct TestFrame {
    index: u64,
    /// Run-clock time it was acquired, and the gap since the previous one.
    time: f64,
    interval: f64,
    photodiode: bool,
    report: Report,
}

impl TimingTest {
    /// The display's report on frame `index` (`None`: never shown).
    fn presented(&mut self, index: u64, at: Option<f64>) {
        if let Some(f) = self.frames.get_mut(index as usize).filter(|f| f.index == index)
            && f.report == Report::Pending
        {
            f.report = at.map_or(Report::NotShown, Report::Shown);
        }
    }

    fn waiting_for_reports(&self) -> bool {
        self.frames.iter().any(|f| f.report == Report::Pending)
    }

    /// Print the statistics and save the frame log.
    fn finish(&mut self) {
        for f in &mut self.frames {
            if f.report == Report::Pending {
                f.report = Report::Missing;
            }
        }
        let feedback = self.frames.first().is_some_and(|f| f.report != Report::Off);
        let shown_gaps: Vec<f64> = self
            .frames
            .windows(2)
            .filter_map(|w| Some(w[1].report.presented()? - w[0].report.presented()?))
            .collect();
        if feedback {
            let count = |r: fn(&Report) -> bool| self.frames.iter().filter(|f| r(&f.report)).count();
            println!("   timed by      the display's own reports of when each frame was on screen");
            println!(
                "   reports       {} shown · {} never shown · {} no report",
                count(|r| matches!(r, Report::Shown(_))),
                count(|r| *r == Report::NotShown),
                count(|r| *r == Report::Missing),
            );
            report_timing(self.frames.len(), &shown_gaps, self.rig.display.expected_hz);
        } else {
            println!("   timed by      when each frame buffer came back (no reports from the display)");
            report_timing(self.frames.len(), &self.intervals, self.rig.display.expected_hz);
        }
        match write_timing_log(&self.rig, &self.frames) {
            Ok(path) => println!("   log           {}", dim(&path.display().to_string())),
            Err(e) => println!("{} couldn't write the frame log: {e:#}", yellow("warning:")),
        }
    }
}

impl App {
    fn secs(&self, t: Instant) -> f64 {
        t.saturating_duration_since(self.epoch).as_secs_f64()
    }

    fn fail(&mut self, el: &ActiveEventLoop, e: anyhow::Error) {
        self.result = Some(Err(e));
        el.exit();
    }

    fn open(&mut self, el: &ActiveEventLoop, mode: Mode) -> Result<Active> {
        let rig = match &mode {
            Mode::Run(s) => &s.rig,
            Mode::TimingTest { rig, .. } => rig,
        };
        let monitor = pick_monitor(el, rig.display.monitor.as_ref())?;
        // Always on top: a stimulus window must never be covered (macOS also
        // refuses to draw to a covered window, which would stall the run).
        let mut attrs = Window::default_attributes()
            .with_title("Onset")
            .with_window_level(winit::window::WindowLevel::AlwaysOnTop);
        let cover = rig.display.fullscreen && monitor.is_some();
        if let (true, Some(m)) = (cover, &monitor) {
            // A borderless window placed exactly over the display, not
            // macOS fullscreen (see `macos::cover_screen`). Position and size
            // are in that display's own points, so the laptop's Retina
            // scaling can't shift the window onto the wrong screen.
            let scale = m.scale_factor();
            let position: winit::dpi::LogicalPosition<f64> = m.position().to_logical(scale);
            let size: winit::dpi::LogicalSize<f64> = m.size().to_logical(scale);
            attrs = attrs
                .with_decorations(false)
                .with_resizable(false)
                .with_position(position)
                .with_inner_size(size);
        } else if rig.display.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            let [w, h] = rig.display.window_size;
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(w, h));
        }
        let window = Arc::new(el.create_window(attrs)?);
        if rig.display.hide_cursor {
            window.set_cursor_visible(false);
        }
        window.focus_window();
        macos::bring_to_front(&window);
        if cover {
            macos::cover_screen(&window);
        }
        let mut renderer = Renderer::new(window.clone(), Box::new(el.owned_display_handle()))?;
        if rig.photodiode.enabled {
            renderer.photodiode =
                Some(PhotodiodePatch { corner: rig.photodiode.corner, size: rig.photodiode.size });
        }
        if let Some(logo) = &rig.site.logo {
            if let Err(e) = renderer.set_logo(logo) {
                eprintln!("{} {e:#}; the welcome screen will have no logo", yellow("warning:"));
            }
        }
        let monitor_hz =
            monitor.as_ref().and_then(|m| m.refresh_rate_millihertz()).map(|mhz| mhz as f64 / 1000.0);
        let hz = rig.display.expected_hz.or(monitor_hz).unwrap_or(60.0);
        // The planner starts from that rate and measures the real one from
        // the display's reports while the instructions are up.
        let planner = renderer.has_feedback().then(|| Planner::new(planner::Config::new(1.0 / hz)));
        if let Some(reason) = renderer.no_feedback_reason() {
            self.console.message(&format!(
                "{} the display won't report when frames reach the screen ({reason}); flip times \
                 are estimated from when each frame buffer comes back",
                yellow("warning:")
            ));
        }

        let kind = match mode {
            Mode::TimingTest { rig, seconds } => ActiveKind::TimingTest(TimingTest {
                rig,
                seconds,
                start: None,
                last: None,
                intervals: Vec::new(),
                frames: Vec::new(),
                drain_until: None,
                opened: Instant::now(),
            }),
            Mode::Run(setup) => {
                let setup = Box::new(setup);
                renderer.preload_images(&setup.timeline.images)?;

                // Provenance for run.json.
                let started_wall = chrono::Local::now();
                let size = window.inner_size();
                let header = serde_json::json!({
                    "onset_version": env!("CARGO_PKG_VERSION"),
                    "experiment_name": setup.exp.experiment.name,
                    "task": setup.exp.experiment.task,
                    "subject": setup.sub,
                    "session": setup.ses,
                    "run": setup.run,
                    "experiment_file": absolute(&setup.exp_path),
                    "rig_file": setup.rig_path.as_deref().map(absolute),
                    "seed": setup.timeline.seed,
                    "started": started_wall.to_rfc3339(),
                    "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                    "gpu": format!("{} ({:?}, {})", renderer.adapter_info.name, renderer.adapter_info.backend, renderer.adapter_info.driver),
                    "surface_format": format!("{:?}", renderer.format()),
                    "monitor": monitor.as_ref().and_then(|m| m.name()),
                    "monitor_refresh_hz": monitor_hz,
                    "frame_timing": if renderer.has_feedback() {
                        "presented: onsets are the display's reports of when frames were on screen"
                    } else {
                        "estimated: a frame's flip is taken as when the next frame was acquired"
                    },
                    "window_pixels": [size.width, size.height],
                    "scale_factor": window.scale_factor(),
                    "fullscreen": setup.rig.display.fullscreen,
                    "trigger": match setup.simulate_tr {
                        Some(tr) => format!("simulated TR {tr}"),
                        None => format!("{:?}", setup.rig.trigger.source).to_lowercase(),
                    },
                    "dummy_volumes": setup.exp.scanner.dummy_volumes,
                    "tr": setup.exp.scanner.tr,
                    "planned_end_s": setup.timeline.end,
                    "trials": setup.timeline.trials.len(),
                    "warnings": setup.timeline.warnings,
                    "clock": "times in raw logs are seconds on a monotonic clock; t = time - t0_run_clock",
                });
                let (tx, handle) = output::spawn_writer(
                    setup.paths.clone(),
                    header,
                    setup.timeline.columns.clone(),
                    setup.exp.schedule.onset_column.clone(),
                )?;
                copy_provenance(&setup);
                *self.writer.lock().unwrap() = Some((tx.clone(), handle));

                if setup.rig.trigger.source == TriggerSource::Hid && setup.simulate_tr.is_none() {
                    let (vendor, product) = setup
                        .rig
                        .trigger
                        .device
                        .as_deref()
                        .and_then(input::parse_usb_id)
                        .context("[trigger] device is missing or not \"vendor:product\"")?;
                    macos::request_input_monitoring();
                    let reader = input::spawn_hid(vendor, product, self.raw_tx.clone(), self.stop.clone())?;
                    if !reader.exclusive && !onset::keyfilter::start() {
                        println!(
                            "   {} the trigger box's keys also reach the app in front (stray keys or alert \
                             sounds; the data is unaffected). To stop that, allow Accessibility for this \
                             terminal app in System Settings → Privacy & Security → Accessibility.",
                            yellow("note:")
                        );
                    }
                }
                if setup.rig.trigger.source == TriggerSource::Serial && setup.simulate_tr.is_none() {
                    input::spawn_serial(
                        setup.rig.trigger.port.as_deref().unwrap_or_default(),
                        setup.rig.trigger.baud,
                        self.raw_tx.clone(),
                        self.stop.clone(),
                    )?;
                }

                let mut run =
                    Run::new(setup.exp, setup.timeline.clone(), hz, setup.rig.photodiode.flash_frames);
                if renderer.has_feedback() {
                    run.enable_feedback();
                }
                // From now on Onset hears real pulses, so it can tell the
                // tail of a scan that's already under way from a new one.
                // A simulated scanner is Onset's own, so it needs no check.
                if setup.simulate_tr.is_none() {
                    run.ready(self.secs(Instant::now()));
                }
                if let Some(name) = &setup.name {
                    run.set_participant_name(name);
                }
                let scenes = run.all_scenes();
                let robot = setup.robot.then(|| Robot {
                    rng: onset::rng::Rng::new(setup.timeline.seed ^ 0x5eed),
                    pending: Vec::new(),
                });
                ActiveKind::Run(Box::new(ActiveRun {
                    run,
                    setup,
                    log: Some(tx),
                    t0_sent: false,
                    simulator_started: false,
                    started_wall,
                    last_status: Instant::now(),
                    warned_hz: false,
                    scenes,
                    prewarmed_size: (0, 0),
                    robot,
                    snapshot_frame: None,
                    simulator_stop: Arc::new(AtomicBool::new(false)),
                    drain_until: None,
                }))
            }
        };
        Ok(Active { renderer, window, planner, kind })
    }

    /// Map a raw key/character to a run input according to the rig.
    fn classify(rig: &Rig, source: Source, raw: &str) -> (Option<Input>, &'static str, String) {
        let trigger = match source {
            Source::Simulated => true,
            Source::Keyboard => rig.trigger.source == TriggerSource::Keyboard && raw == rig.trigger.key,
            Source::Hid => raw == rig.trigger.key,
            Source::Serial => raw == rig.trigger.serial_char,
            Source::Robot => false,
        };
        if trigger {
            return (Some(Input::Trigger), "trigger", "trigger".into());
        }
        // With a USB trigger box read directly, its buttons come from there;
        // the same keys typed on the laptop are ignored, so a press can't
        // count twice.
        if source == Source::Keyboard && rig.trigger.source == TriggerSource::Hid {
            return (None, "ignored", String::new());
        }
        match rig.button_for(raw) {
            Some(label) => (Some(Input::Button(label.to_string())), "button", label.to_string()),
            None => (None, "ignored", String::new()),
        }
    }

    fn handle_raw(&mut self, raw: RawInput) {
        let epoch = self.epoch;
        let Some(active) = &mut self.state else { return };
        let ActiveKind::Run(ar) = &mut active.kind else { return };
        match raw {
            RawInput::Error(e) => {
                self.console.message(&format!("{} {e}", red("input error:")));
            }
            RawInput::Key { t, source, raw } => {
                let time = t.saturating_duration_since(epoch).as_secs_f64();
                let (input, kind, mut label) = Self::classify(&ar.setup.rig, source, &raw);
                if let Some(input) = input
                    && ar.run.input(input, time) == InputUse::PreviousScan
                {
                    // Still a real pulse (kind "trigger", so timing checks
                    // can match it), but not one of this run's.
                    label = "previous-scan".into();
                }
                if let Some(log) = &ar.log {
                    let _ = log.send(LogMsg::Input(output::InputRecord {
                        time,
                        source: source.as_str(),
                        raw,
                        kind,
                        label,
                    }));
                }
            }
        }
    }

    fn operator(&mut self, input: Input, t: Instant) {
        let time = self.secs(t);
        let Some(active) = &mut self.state else { return };
        match &mut active.kind {
            ActiveKind::Run(ar) => {
                if let Some(log) = &ar.log {
                    let label = format!("{input:?}").to_lowercase();
                    let _ = log.send(LogMsg::Input(output::InputRecord {
                        time,
                        source: "keyboard",
                        raw: label.clone(),
                        kind: "operator",
                        label,
                    }));
                }
                ar.run.input(input, time);
            }
            ActiveKind::TimingTest(tt) => {
                if input == Input::Abort {
                    tt.seconds = 0.0;
                }
            }
        }
    }

    /// Keyboard triggers and buttons only reach a focused window, so losing
    /// focus mid-session is the worst silent failure: warn loudly, log it,
    /// and try to take focus back.
    fn focus_changed(&mut self, focused: bool) {
        let time = self.secs(Instant::now());
        let Some(Active { kind: ActiveKind::Run(ar), window, .. }) = &mut self.state else { return };
        if matches!(ar.run.phase, Phase::Done(_)) {
            return;
        }
        // A USB trigger box read directly doesn't need focus; only the
        // operator keys (Space, Esc) do.
        let direct = ar.setup.rig.trigger.source == TriggerSource::Hid && ar.setup.simulate_tr.is_none();
        if let Some(log) = &ar.log {
            let label = if focused { "focus-gained" } else { "focus-lost" };
            let _ = log.send(LogMsg::Input(output::InputRecord {
                time,
                source: "window",
                raw: label.into(),
                kind: "operator",
                label: label.into(),
            }));
        }
        if direct {
            // Nothing to warn about: triggers and buttons don't need focus.
        } else if focused {
            self.console.message(&format!("{} Onset window has keyboard focus again", green("✓")));
        } else {
            self.console.message(&format!(
                "{} Onset LOST KEYBOARD FOCUS: keyboard triggers and button presses won't arrive until you click the stimulus window",
                red("WARNING:")
            ));
            window.focus_window();
            macos::bring_to_front(window);
        }
    }

    fn tick(&mut self, el: &ActiveEventLoop) {
        while let Ok(raw) = self.raw_rx.try_recv() {
            self.handle_raw(raw);
        }
        if self.interrupted.swap(false, Ordering::SeqCst) {
            self.operator(Input::Abort, Instant::now());
        }
        // Simulated participant presses that are due.
        let mut due = Vec::new();
        if let Some(Active { kind: ActiveKind::Run(ar), .. }) = &mut self.state {
            if let Some(robot) = &mut ar.robot {
                let now = Instant::now();
                robot.pending.retain(|(at, raw)| {
                    if *at <= now {
                        due.push(RawInput::Key { t: *at, source: Source::Robot, raw: raw.clone() });
                        false
                    } else {
                        true
                    }
                });
            }
        }
        for raw in due {
            self.handle_raw(raw);
        }

        let epoch = self.epoch;
        let clock = self.clock;
        let Some(active) = &mut self.state else { return };
        collect_feedback(active, &clock, epoch);

        // Prepare all text before the run (and again if the window resized).
        if let ActiveKind::Run(ar) = &mut active.kind {
            if ar.prewarmed_size != active.renderer.size() && ar.run.phase != Phase::Running {
                active.renderer.prewarm(&ar.scenes);
                ar.prewarmed_size = active.renderer.size();
            }
        }

        let Some(frame) = active.renderer.acquire() else {
            // No frame, but time goes on: give up on old reports, end a hold
            // once the scanner stops, and save a run that's over.
            if let ActiveKind::Run(ar) = &mut active.kind {
                ar.run.update(epoch.elapsed().as_secs_f64());
                if let Some(result) = after_frame(ar, &self.writer, &mut self.console) {
                    self.result = Some(result);
                    el.exit();
                    return;
                }
            }
            // A timing test ends on time even if the display stops taking
            // frames, or never takes one.
            if let ActiveKind::TimingTest(tt) = &mut active.kind {
                if tt.start.is_none() && tt.opened.elapsed().as_secs_f64() >= tt.seconds {
                    self.result = Some(Err(anyhow::anyhow!(
                        "the display never accepted a frame (screen locked, display asleep, or window hidden?)"
                    )));
                    el.exit();
                    return;
                }
                let late = tt.start.is_some_and(|start| {
                    epoch.elapsed().as_secs_f64() - start >= tt.seconds + FEEDBACK_TIMEOUT_S
                });
                if late || tt.drain_until.is_some_and(|until| Instant::now() >= until) {
                    tt.finish();
                    el.exit();
                    return;
                }
            }
            let since = *self.stalled_since.get_or_insert_with(Instant::now);
            // Minimized or hidden mid-session: put the window back.
            let restore_due = self.last_restore.is_none_or(|t| t.elapsed() > Duration::from_millis(500));
            if since.elapsed() > Duration::from_millis(200) && restore_due {
                self.last_restore = Some(Instant::now());
                active.window.set_minimized(false);
                active.window.focus_window();
                macos::bring_to_front(&active.window);
            }
            if since.elapsed() > Duration::from_secs(1) && !self.stall_warned {
                self.stall_warned = true;
                if std::env::var_os("ONSET_DEBUG").is_some() {
                    self.console.message(&macos::describe(&active.window));
                }
                self.console.message(&format!(
                    "{} the display isn't accepting frames (window hidden, minimized, or on another Space?)",
                    red("warning:")
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
            return;
        };
        if self.stalled_since.take().is_some() && self.stall_warned {
            self.stall_warned = false;
            self.console.message(&format!("{} display is updating again", green("✓")));
        }
        // The moment the display accepted a new frame (run clock).
        let now = frame.acquired.saturating_duration_since(epoch).as_secs_f64();
        let index = frame.id;
        // Which refresh this frame will land on, and when that is, from the
        // display's reports so far (run clock). Only once the planner trusts
        // its vsync grid (enough reports, a period close to the nominal one);
        // until then the run assumes one period after the acquire.
        let predicted_flip = active.planner.as_mut().and_then(|planner| {
            let host_now = clock.instant_to_host(frame.acquired);
            planner.expire(host_now);
            let plan = planner.begin_frame(host_now);
            // The renderer and the planner each count every frame from 0, so
            // the display's reports (renderer ids) find the right record.
            debug_assert_eq!(plan.id, frame.id, "renderer and planner frame counts drifted apart");
            let host = planner.vsync_time(plan.target_vsync).filter(|_| planner.is_warmed_up())?;
            clock.secs_since(epoch, host)
        });

        match &mut active.kind {
            ActiveKind::Run(ar) => {
                if let Some(planner) = &active.planner
                    && ar.run.t0.is_none()
                {
                    let measured = planner.measured_period().filter(|_| planner.is_warmed_up());
                    ar.run.set_measured_period(measured.map(|m| m.period_s));
                }
                let plan = ar.run.frame(index, now, predicted_flip);
                let photodiode = ar.setup.rig.photodiode.enabled.then_some(plan.photodiode);
                // Debugging/documentation aid: save each event's first frame.
                // (Slow: never set ONSET_SNAPSHOTS for a real session.)
                if let Some(dir) = std::env::var_os("ONSET_SNAPSHOTS") {
                    let scene = plan.scene.clone();
                    let onset_frame = plan.photodiode && ar.snapshot_frame != Some(index.wrapping_sub(1));
                    if plan.photodiode {
                        ar.snapshot_frame = Some(index);
                    }
                    if let Some(name) = ar.run.status().event.filter(|_| onset_frame) {
                        let path = PathBuf::from(dir).join(format!("frame{index:06}_{name}.png"));
                        match active.renderer.snapshot(&scene, photodiode) {
                            Ok(img) => {
                                let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
                                let _ = img.save(&path);
                            }
                            Err(e) => eprintln!("snapshot failed: {e:#}"),
                        }
                    }
                    active.renderer.draw(frame, &scene, photodiode);
                } else {
                    active.renderer.draw(frame, plan.scene, photodiode);
                }
                let started = ar.run.take_started_responses();
                if let Some(robot) = &mut ar.robot {
                    for r in &started {
                        robot.respond(&ar.setup.rig, r, Instant::now());
                    }
                }

                // Start the fake scanner once we're waiting for it (the run
                // starts on its first pulse, from the instructions too).
                if matches!(ar.run.phase, Phase::Instructions | Phase::Waiting) && !ar.simulator_started {
                    if let Some(tr) = ar.setup.simulate_tr {
                        ar.simulator_started = true;
                        input::spawn_simulated_scanner(
                            tr,
                            1.5,
                            "sim".into(),
                            self.raw_tx.clone(),
                            ar.simulator_stop.clone(),
                        );
                        self.console
                            .message(&format!("{} fake scanner starts in 1.5 s (TR {tr} s)", dim("·")));
                    }
                }
                if let (Some(t0), false) = (ar.run.t0, ar.t0_sent) {
                    ar.t0_sent = true;
                    if let Some(log) = &ar.log {
                        let _ = log.send(LogMsg::T0(t0));
                    }
                    let hz = 1.0 / ar.run.period();
                    self.console.message(&format!(
                        "{} trigger received, run started · display {hz:.3} Hz",
                        green("▶")
                    ));
                    if let Some(planner) = &active.planner
                        && !planner.is_warmed_up()
                    {
                        self.console.message(&format!(
                            "{} the display had sent only {} usable frame reports when the run started \
                             ({} needed): the first frames' flips are predicted from the CPU clock",
                            yellow("note:"),
                            planner.feedback_points(),
                            planner::WARM_UP_POINTS
                        ));
                    }
                    if let Some(expected) = ar.setup.rig.display.expected_hz {
                        if ((hz - expected) / expected).abs() > 0.01 && !ar.warned_hz {
                            ar.warned_hz = true;
                            self.console.message(&format!(
                                "{} measured {hz:.2} Hz but the rig expects {expected} Hz",
                                yellow("warning:")
                            ));
                        }
                    }
                }
                if ar.last_status.elapsed() > Duration::from_millis(250) {
                    ar.last_status = Instant::now();
                    self.console.update(&ar.run.status());
                }
                if let Some(result) = after_frame(ar, &self.writer, &mut self.console) {
                    self.result = Some(result);
                    el.exit();
                }
            }
            ActiveKind::TimingTest(tt) => {
                if let Some(until) = tt.drain_until {
                    // Time's up: plain black frames (not part of the test)
                    // until the display has reported on the last ones.
                    let black = onset::scene::Scene { background: onset::color::Color::BLACK, items: vec![] };
                    active.renderer.draw(frame, &black, tt.rig.photodiode.enabled.then_some(false));
                    if !tt.waiting_for_reports() || Instant::now() >= until {
                        tt.finish();
                        el.exit();
                    }
                    return;
                }
                let start = *tt.start.get_or_insert(now);
                if let Some(last) = tt.last {
                    tt.intervals.push(now - last);
                }
                tt.last = Some(now);
                let on = index % 2 == 0;
                let interval = tt.last.map_or(0.0, |_| *tt.intervals.last().unwrap_or(&0.0));
                tt.frames.push(TestFrame {
                    index,
                    time: now,
                    interval,
                    photodiode: on && tt.rig.photodiode.enabled,
                    report: if active.planner.is_some() { Report::Pending } else { Report::Off },
                });
                let scene = onset::scene::Scene {
                    background: onset::color::Color::BLACK,
                    items: vec![onset::scene::Item::Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 300.0,
                        h: 300.0,
                        color: if on { onset::color::Color::WHITE } else { onset::color::Color::BLACK },
                    }],
                };
                let photodiode = tt.rig.photodiode.enabled.then_some(on);
                active.renderer.draw(frame, &scene, photodiode);
                if now - start >= tt.seconds {
                    if active.planner.is_some() {
                        tt.drain_until = Some(Instant::now() + Duration::from_secs_f64(FEEDBACK_TIMEOUT_S));
                    } else {
                        tt.finish();
                        el.exit();
                    }
                }
            }
        }
    }
}

/// Hand the display's reports of frames reaching the screen to the planner,
/// and the ones it accepts to the run (or timing test), in run-clock seconds.
fn collect_feedback(active: &mut Active, clock: &HostClock, epoch: Instant) {
    let Active { renderer, planner: Some(planner), kind, .. } = active else { return };
    for (id, host_s) in renderer.presented() {
        planner.on_feedback(id, host_s);
        // `Some(None)`: never shown. Reports the planner rejects as
        // impossible are left to time out, like a missing one.
        let at = match planner.feedback(id) {
            Some(Feedback::Shown { presented_s, .. }) => Some(clock.secs_since(epoch, presented_s)),
            Some(Feedback::NotShown) => Some(None),
            _ => None,
        };
        let Some(at) = at else { continue };
        match kind {
            ActiveKind::Run(ar) => ar.run.presented(id, at),
            ActiveKind::TimingTest(tt) => tt.presented(id, at),
        }
    }
}

/// After each tick of a run: tell the operator what the run noticed, stop
/// the fake scanner once the stimulus is over, send finished records to the
/// log, and once the run is over and its last reports are in (or have been
/// waited for long enough), save it. Returns the result once saved.
fn after_frame(ar: &mut ActiveRun, writer: &WriterSlot, console: &mut console::Status) -> Option<Result<()>> {
    for notice in ar.run.take_notices() {
        console.message(&notice_text(notice));
    }
    if !matches!(ar.run.phase, Phase::Instructions | Phase::Waiting | Phase::Running) {
        ar.simulator_stop.store(true, Ordering::Relaxed);
    }
    if let Some(log) = &ar.log {
        for f in ar.run.take_frames() {
            let _ = log.send(LogMsg::Frame(f));
        }
        for r in ar.run.take_records() {
            let _ = log.send(LogMsg::Event(r));
        }
    }
    let Phase::Done(reason) = ar.run.phase else { return None };
    let until =
        *ar.drain_until.get_or_insert_with(|| Instant::now() + Duration::from_secs_f64(FEEDBACK_TIMEOUT_S));
    if ar.run.waiting_for_reports() && Instant::now() < until {
        return None;
    }
    Some(finish_run(ar, reason, writer, console))
}

fn notice_text(notice: Notice) -> String {
    match notice {
        Notice::PulseIgnored { quiet_s } => format!(
            "{} the scanner was already sending pulses when Onset got ready (an earlier scan still \
             running?). Ignoring them: the run starts on the first pulse after {quiet_s:.1} s without any.",
            yellow("pulse ignored:")
        ),
        Notice::ReadyAgain => {
            format!("{} the earlier scan has stopped; the next pulse starts the run", green("✓"))
        }
        Notice::ScanQuiet { last_pulse, quiet_s } => format!(
            "{} no scanner pulse for {quiet_s:.1} s (the last was at {last_pulse:.1} s): has the scan \
             stopped? The stimulus carries on; Esc stops it.",
            yellow("warning:")
        ),
        Notice::Holding { quiet_s } => format!(
            "{} stimulus finished, but the scanner is still sending pulses: holding the final screen until \
             it stops ({quiet_s:.1} s without a pulse). Esc stops waiting.",
            green("■")
        ),
        Notice::ScannerStopped => format!("{} the scanner has stopped", green("✓")),
        Notice::HoldStopped => format!("{} stopped waiting for the scanner", dim("·")),
    }
}

fn absolute(p: &Path) -> String {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).display().to_string()
}

/// Keep exact copies of the files that defined this run next to its logs.
fn copy_provenance(setup: &RunSetup) {
    let copy = |from: &Path, suffix: &str| {
        let _ = std::fs::copy(from, setup.paths.raw(suffix));
    };
    copy(&setup.exp_path, "experiment.toml");
    if let Some(rig) = &setup.rig_path {
        copy(rig, "rig.toml");
    }
    if let Some(file) = &setup.exp.schedule.file {
        let src = setup.exp.resolve_path(file);
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("tsv");
        copy(&src, &format!("schedule.{ext}"));
    }
}

fn finish_run(
    ar: &mut ActiveRun,
    reason: EndReason,
    writer: &WriterSlot,
    console: &mut console::Status,
) -> Result<()> {
    // Whatever is still waiting for a report is estimated now.
    ar.run.flush();
    ar.simulator_stop.store(true, Ordering::Relaxed);
    let status = ar.run.status();
    let coverage = ar.run.scan_coverage();
    if let Some(log) = ar.log.take() {
        for f in ar.run.take_frames() {
            let _ = log.send(LogMsg::Frame(f));
        }
        for r in ar.run.take_records() {
            let _ = log.send(LogMsg::Event(r));
        }
        let _ = log.send(LogMsg::Finish(serde_json::json!({
            "end_reason": reason.as_str(),
            "ended": chrono::Local::now().to_rfc3339(),
            "duration_wall_s": (chrono::Local::now() - ar.started_wall).num_milliseconds() as f64 / 1000.0,
            "measured_refresh_hz": 1.0 / ar.run.period(),
            "missed_frames": ar.run.missed_frames,
            "flip_estimates": ar.run.flip_estimates,
            "triggers": ar.run.triggers.len(),
            "ignored_triggers": ar.run.ignored_triggers,
            "scan_until_s": coverage.map(|c| c.scanned_until),
            "scan_stopped_early": coverage.map(|c| c.stopped_early()),
            "responses": status.responses,
            "correct": status.correct,
            "scored": status.scored,
        })));
    }
    console.clear();
    let Some((tx, handle)) = writer.lock().unwrap().take() else {
        bail!("log writer already finished");
    };
    drop(tx);
    let report = handle.join().map_err(|_| anyhow::anyhow!("log writer crashed"))??;

    let headline = match reason {
        EndReason::Completed => green("✓ run complete"),
        EndReason::Aborted => yellow("■ run aborted (data saved)"),
    };
    println!("\n{headline}");
    let t = status.t.unwrap_or(0.0).max(0.0);
    println!("   duration      {}  ({} triggers)", fmt_duration(t), status.triggers);
    if ar.run.ignored_triggers > 0 {
        println!(
            "   ignored       {} pulses from a scan already under way before the run",
            ar.run.ignored_triggers
        );
    }
    if let Some(c) = coverage {
        if c.stopped_early() && reason == EndReason::Completed {
            println!(
                "{} the scan stopped at {:.1} s (last pulse + one TR of {:.2} s), before the stimulus ended at \
                 {:.1} s: the end of the run wasn't scanned",
                yellow("warning:"),
                c.scanned_until,
                c.tr,
                c.stimulus_end
            );
        }
    }
    let mf = ar.run.missed_frames;
    let timed_by = if ar.run.has_feedback() { "timed by the display" } else { "estimated from the CPU" };
    let frames_line = format!("{mf} missed frames · {:.3} Hz · {timed_by}", 1.0 / ar.run.period());
    println!("   display       {}", if mf == 0 { green(&frames_line) } else { yellow(&frames_line) });
    if ar.run.has_feedback() && ar.run.flip_estimates > 0 {
        println!(
            "   {}",
            yellow(&format!(
                "{} onsets/ends estimated: the display didn't report those frames",
                ar.run.flip_estimates
            ))
        );
    }
    if status.scored > 0 {
        println!(
            "   accuracy      {}/{} ({:.0}%)",
            status.correct,
            status.scored,
            100.0 * status.correct as f64 / status.scored as f64
        );
    } else if status.responses > 0 {
        println!("   responses     {}", status.responses);
    }
    println!("   events.tsv    {} rows", report.events_written);
    for f in &report.files {
        println!("   {}", dim(&f.display().to_string()));
    }
    Ok(())
}

/// Save timing-test frames in the same format as a run's frames.tsv, so
/// they can be compared with photodiode recordings.
fn write_timing_log(rig: &Rig, frames: &[TestFrame]) -> Result<PathBuf> {
    let dir = rig.output.dir.join("timing-tests");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("timingtest_{stamp}_frames.tsv"));
    let mut out = String::from(
        "frame\ttime\tt\tinterval_ms\tmissed\tphase\tevent\tphotodiode\tpresented\tpresented_t\treport\n",
    );
    let t0 = frames.first().map_or(0.0, |f| f.time);
    // Refreshes missed before a frame: from the gap since the frame before
    // it, when the display reported both, rounded to whole refreshes.
    let mut gaps: Vec<f64> =
        frames.windows(2).filter_map(|w| Some(w[1].report.presented()? - w[0].report.presented()?)).collect();
    gaps.sort_by(f64::total_cmp);
    let period = gaps.get(gaps.len() / 2).copied();
    let mut prev: Option<f64> = None;
    for f in frames {
        let presented = f.report.presented();
        let missed = match (presented, prev, period) {
            (Some(p), Some(q), Some(period)) => (((p - q) / period).round() as i64 - 1).max(0),
            _ => 0,
        };
        prev = presented;
        let (abs, rel) = match presented {
            Some(p) => (format!("{p:.6}"), format!("{:.6}", p - t0)),
            None => ("n/a".into(), "n/a".into()),
        };
        out.push_str(&format!(
            "{}\t{:.6}\t{:.6}\t{:.3}\t{missed}\ttiming-test\tn/a\t{}\t{abs}\t{rel}\t{}\n",
            f.index,
            f.time,
            f.time - t0,
            f.interval * 1000.0,
            f.photodiode as u8,
            f.report.label()
        ));
    }
    std::fs::write(&path, out)?;
    Ok(path)
}

fn report_timing(frames: usize, intervals: &[f64], expected: Option<f64>) {
    if intervals.len() < 10 {
        println!("{}", yellow("too few frames to measure"));
        return;
    }
    let mut v: Vec<f64> = intervals.to_vec();
    v.sort_by(f64::total_cmp);
    let median = v[v.len() / 2];
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt();
    let missed: u64 = intervals
        .iter()
        .filter(|&&i| i > 1.5 * median)
        .map(|&i| ((i / median).round() as u64).saturating_sub(1))
        .sum();
    println!("   frames        {frames}");
    println!("   refresh       {:.3} Hz (median interval {:.3} ms)", 1.0 / median, median * 1000.0);
    println!(
        "   jitter        mean {:.3} ms · SD {:.3} ms · min {:.3} · max {:.3}",
        mean * 1000.0,
        sd * 1000.0,
        v[0] * 1000.0,
        v[v.len() - 1] * 1000.0
    );
    let line = format!("{missed} missed frames");
    println!("   dropped       {}", if missed == 0 { green(&line) } else { red(&line) });
    if let Some(e) = expected {
        if ((1.0 / median - e) / e).abs() > 0.01 {
            println!("{} expected {e} Hz", yellow("warning:"));
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let Some(mode) = self.mode.take() else { return };
        match self.open(el, mode) {
            Ok(active) => self.state = Some(active),
            Err(e) => self.fail(el, e),
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.operator(Input::Abort, Instant::now()),
            WindowEvent::Focused(focused) => self.focus_changed(focused),
            WindowEvent::Resized(size) => {
                if let Some(a) = &mut self.state {
                    let scale = a.window.scale_factor() as f32;
                    a.renderer.resize(size.width, size.height, scale);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(a) = &mut self.state {
                    let size = a.window.inner_size();
                    a.renderer.resize(size.width, size.height, scale_factor as f32);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Timestamp first, before doing anything else.
                let t = Instant::now();
                if event.state != ElementState::Pressed || event.repeat {
                    return;
                }
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => self.operator(Input::Abort, t),
                    Key::Named(NamedKey::Space) | Key::Named(NamedKey::Enter) => {
                        self.operator(Input::Continue, t)
                    }
                    Key::Character(c) => {
                        let raw = RawInput::Key { t, source: Source::Keyboard, raw: c.to_string() };
                        self.handle_raw(raw);
                    }
                    _ => {}
                }
                // A run that's now over is saved by the next tick, once the
                // display has reported its last frames.
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.result.is_some() {
            return;
        }
        self.tick(el);
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.stop.store(true, Ordering::SeqCst);
        // Window closed some other way: make sure logs are finished.
        if let Some(Active { kind: ActiveKind::Run(ar), .. }) = &mut self.state {
            if ar.log.is_some() {
                ar.run.input(Input::Abort, self.epoch.elapsed().as_secs_f64());
                // A run that had completed (e.g. while holding the final
                // screen) stays completed.
                let reason = match ar.run.phase {
                    Phase::Done(reason) => reason,
                    _ => EndReason::Aborted,
                };
                let r = finish_run(ar, reason, &self.writer, &mut self.console);
                if self.result.is_none() {
                    self.result = Some(r);
                }
            }
        }
    }
}
