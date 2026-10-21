//! The welcome screen: drawn offscreen (no window opens), gone from t0 on,
//! and the participant's name never reaching any file a run writes.
//!
//! `ONSET_WELCOME_SNAPSHOTS=<dir> cargo test --test welcome` also saves the
//! rendered screens of every example experiment as PNGs in `<dir>`.

use onset::experiment::Experiment;
use onset::output::{self, InputRecord, LogMsg, Paths};
use onset::render::{PhotodiodePatch, Renderer};
use onset::rig::{Corner, Rig};
use onset::run::{Input, Phase, Run};
use onset::scene::{Item, Scene};
use onset::schedule;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

const HZ: f64 = 120.0;
const NAME: &str = "Testy McTestface";

fn example(name: &str) -> Experiment {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(name).join("experiment.toml");
    Experiment::load(&path).unwrap()
}

fn shown_name(scene: &Scene) -> Option<String> {
    scene.items.iter().find_map(|i| match i {
        Item::Welcome(w) => w.name.clone(),
        _ => None,
    })
}

/// The example scanner rig's logo, which ships with the examples.
fn scanner_logo() -> PathBuf {
    let rig =
        Rig::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/rigs/scanner-forp.toml")).unwrap();
    let logo = rig.site.logo.expect("the scanner rig has a [site] logo");
    assert!(logo.exists(), "{}", logo.display());
    logo
}

fn has_welcome(scene: &Scene) -> bool {
    scene.items.iter().any(|i| matches!(i, Item::Welcome(_)))
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() { files(&path, out) } else { out.push(path) }
    }
}

/// Send an input to the run and log it the way `onset run` does.
fn input(run: &mut Run, log: &Sender<LogMsg>, input: Input, time: f64, kind: &'static str, raw: &str) {
    let _ = log.send(LogMsg::Input(InputRecord {
        time,
        source: "keyboard",
        raw: raw.into(),
        kind,
        label: raw.into(),
    }));
    run.input(input, time);
}

#[test]
fn participant_name_is_shown_but_never_written() {
    let exp = example("flanker");
    let tl = schedule::compile(&exp, 1, HZ).unwrap();
    let run_length = tl.end;
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("welcome-name");
    let _ = std::fs::remove_dir_all(&out);
    let paths = Paths::new(&out, "99", None, &exp.experiment.task, 1);
    let header = serde_json::json!({ "experiment_name": exp.experiment.name, "subject": "99", "run": 1 });
    let (log, writer) =
        output::spawn_writer(paths.clone(), header, tl.columns.clone(), exp.schedule.onset_column.clone())
            .unwrap();

    let mut run = Run::new(&exp, tl, HZ, 3);
    run.set_participant_name(NAME);
    let mut greeted = 0;
    let mut presses: Vec<(f64, String)> = Vec::new();
    let mut next_trigger = 1.0;
    let mut t0_sent = false;
    let mut frame = 0;
    while !run.is_done() {
        let t = frame as f64 / HZ;
        assert!(t < run_length + 60.0, "the run never ended");
        if frame == 60 {
            input(&mut run, &log, Input::Continue, t, "operator", "continue");
        }
        // The scan covers the run and then stops, so the run can end.
        let scanning = run.t0.is_none_or(|t0| next_trigger <= t0 + run_length);
        if scanning && t >= next_trigger {
            input(&mut run, &log, Input::Trigger, next_trigger, "trigger", "t");
            next_trigger += 2.0;
        }
        presses.retain(|(at, button)| {
            if *at > t {
                return true;
            }
            input(&mut run, &log, Input::Button(button.clone()), *at, "button", button);
            false
        });

        let plan = run.frame(frame, t, None);
        if shown_name(plan.scene).as_deref() == Some(NAME) {
            greeted += 1;
        }
        for r in run.take_started_responses() {
            presses.push((t + 0.4, r.correct.unwrap_or_else(|| r.buttons[0].clone())));
        }
        if let (Some(t0), false) = (run.t0, t0_sent) {
            t0_sent = true;
            log.send(LogMsg::T0(t0)).unwrap();
        }
        for f in run.take_frames() {
            log.send(LogMsg::Frame(f)).unwrap();
        }
        for r in run.take_records() {
            log.send(LogMsg::Event(r)).unwrap();
        }
        frame += 1;
    }
    log.send(LogMsg::Finish(serde_json::json!({ "end_reason": "completed" }))).unwrap();
    drop(log);
    let report = writer.join().unwrap().unwrap();

    assert!(greeted > 100, "the welcome screen should greet {NAME} before t0 ({greeted} frames did)");
    assert!(report.events_written > 0);
    let mut written = Vec::new();
    files(&out, &mut written);
    assert!(written.len() >= 6, "{written:?}");
    for path in written {
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).to_lowercase();
        for part in NAME.split(' ').chain([NAME]) {
            assert!(!text.contains(&part.to_lowercase()), "{} contains {part:?}", path.display());
        }
    }
}

/// Luminance (0-1) of pixel (x, y).
fn lum(img: &image::RgbaImage, x: u32, y: u32) -> f32 {
    let p = img.get_pixel(x, y).0;
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn percentile(mut v: Vec<f32>, p: f32) -> f32 {
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * p) as usize]
}

/// The instructions screen, the same 3 s later, the waiting screen, and the
/// first frame after t0, with the frame clock running at `HZ`.
fn screens(exp: &Experiment, name: Option<&str>) -> [Scene; 4] {
    let tl = schedule::compile(exp, 1, HZ).unwrap();
    let mut run = Run::new(exp, tl, HZ, 3);
    if let Some(name) = name {
        run.set_participant_name(name);
    }
    let mut frame = 0;
    let mut next = |run: &mut Run| {
        let scene = run.frame(frame, frame as f64 / HZ, None).scene.clone();
        frame += 1;
        scene
    };
    let instructions = next(&mut run);
    for _ in 0..359 {
        next(&mut run);
    }
    let later = next(&mut run);
    run.input(Input::Continue, 3.01);
    for _ in 0..120 {
        next(&mut run);
    }
    let waiting = next(&mut run);
    assert_eq!(run.phase, Phase::Waiting);
    run.input(Input::Trigger, 4.02);
    let running = next(&mut run);
    assert_eq!(run.phase, Phase::Running);
    [instructions, later, waiting, running]
}

#[test]
fn welcome_screen_renders_offscreen() {
    let mut r = match Renderer::headless(1920, 1080, 1.0) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no GPU for offscreen rendering ({e:#})");
            return;
        }
    };
    r.photodiode = Some(PhotodiodePatch { corner: Corner::BottomLeft, size: 40.0 });
    r.set_logo(&scanner_logo()).unwrap();
    let snapshots = std::env::var_os("ONSET_WELCOME_SNAPSHOTS").map(PathBuf::from);
    if let Some(dir) = &snapshots {
        std::fs::create_dir_all(dir).unwrap();
    }

    let exp = example("flanker");
    let [instructions, later, waiting, running] = screens(&exp, Some("Sam Example"));
    assert!(has_welcome(&instructions) && has_welcome(&waiting));
    assert!(!has_welcome(&running));
    let a = r.snapshot(&instructions, Some(false)).unwrap();
    let b = r.snapshot(&later, Some(false)).unwrap();
    let w = r.snapshot(&waiting, Some(false)).unwrap();
    let run = r.snapshot(&running, Some(false)).unwrap();
    assert!(a.as_raw() != b.as_raw(), "the background should move");

    // Readable: in the instructions' column the background is dark and the text bright.
    let mut column = Vec::new();
    for y in (0.38 * 1080.0) as u32..(0.85 * 1080.0) as u32 {
        for x in (0.07 * 1920.0) as u32..(0.6 * 1920.0) as u32 {
            column.push(lum(&a, x, y));
        }
    }
    let (bg, text) = (percentile(column.clone(), 0.5), percentile(column, 0.999));
    assert!(bg < 0.06, "background behind the instructions is too bright: {bg}");
    assert!(text > 0.8, "instruction text should be bright: {text}");

    // The photodiode patch is drawn over the welcome screen exactly as before.
    for img in [&a, &w] {
        for y in 1080 - 40..1080 {
            for x in 0..40 {
                assert_eq!(img.get_pixel(x, y).0, [0, 0, 0, 255], "photodiode patch at ({x}, {y})");
            }
        }
    }
    // Once running, only the experiment's own screen: flanker's black background.
    let corner: f32 =
        (0..100).flat_map(|y| (0..100).map(move |x| (x, y))).map(|(x, y)| lum(&run, x, y)).sum();
    assert_eq!(corner, 0.0, "nothing of the welcome screen may remain after t0");
    // The brain's dots on the right of the waiting screen.
    let brain = (1150..1700).flat_map(|x| (350..750).map(move |y| (x, y)));
    assert!(brain.map(|(x, y)| lum(&w, x, y)).fold(0.0, f32::max) > 0.3, "no brain");
    // The (white) logo top left, "POWERED BY ONSET" bottom right.
    for img in [&a, &w] {
        let logo = (144..244).flat_map(|x| (65..157).map(move |y| (x, y)));
        assert!(logo.map(|(x, y)| lum(img, x, y)).fold(0.0, f32::max) > 0.9, "no logo");
        let footer = (1920 - 450..1920 - 144).flat_map(|x| (1080 - 100..1080 - 60).map(move |y| (x, y)));
        assert!(footer.map(|(x, y)| lum(img, x, y)).fold(0.0, f32::max) > 0.3, "no footer text");
    }

    let Some(dir) = snapshots else { return };
    a.save(dir.join("flanker_instructions.png")).unwrap();
    b.save(dir.join("flanker_instructions_3s.png")).unwrap();
    w.save(dir.join("flanker_waiting.png")).unwrap();
    run.save(dir.join("flanker_after_t0.png")).unwrap();
    for name in ["checkerboard-localizer", "nsd", "nsd-resting"] {
        let exp = example(name);
        let [instructions, _, waiting, _] = screens(&exp, Some("Sam Example"));
        r.snapshot(&instructions, Some(false))
            .unwrap()
            .save(dir.join(format!("{name}_instructions.png")))
            .unwrap();
        r.snapshot(&waiting, Some(false)).unwrap().save(dir.join(format!("{name}_waiting.png"))).unwrap();
    }
    // No --name, and a Retina laptop window (1280 × 800 points at 2×).
    let [_, _, waiting, _] = screens(&exp, None);
    r.snapshot(&waiting, Some(false)).unwrap().save(dir.join("flanker_waiting_no_name.png")).unwrap();
    let mut retina = Renderer::headless(2560, 1600, 2.0).unwrap();
    retina.set_logo(&scanner_logo()).unwrap();
    let [instructions, ..] = screens(&exp, Some("Sam Example"));
    retina.snapshot(&instructions, None).unwrap().save(dir.join("flanker_instructions_retina.png")).unwrap();
}
