//! `onset check-timing` on synthetic Onset logs and probe recordings.
//!
//! We invent a "true" timeline (scanner time, seconds) and derive from it:
//!
//! - Onset's run clock   = true + 100 s              (PC clock taken as exact)
//! - the Arduino's clock = true * (1 + 50 ppm) + 7 s (a slightly fast board)
//! - light onset         = Onset's flip + 12 ms display lag + N(0, 1 ms) jitter
//!
//! and check that the analysis recovers the 12 ms lag, the 1 ms jitter and
//! the -50 ppm clock difference.

use onset::check_timing::report::Report;
use onset::check_timing::{
    EprimeOptions, RunOptions, analyze_eprime, analyze_onset_run, analyze_timing_test, load_onset_run,
    make_probe, read_eprime, read_probe,
};
use onset::rng::Rng;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const HZ: f64 = 60.0;
const FRAME: f64 = 1.0 / HZ;
const TR: f64 = 2.0;
const LAG: f64 = 0.012;
const JITTER: f64 = 0.001;
const DRIFT: f64 = 50e-6;
const FLASH_FRAMES: usize = 3;
const RUN_OFFSET: f64 = 100.0; // run clock = true + RUN_OFFSET
const ARD_OFFSET: f64 = 7.0; // arduino = true * (1 + DRIFT) + ARD_OFFSET
const STEM: &str = "sub-01_task-test_run-01";

fn to_arduino(t: f64) -> f64 {
    t * (1.0 + DRIFT) + ARD_OFFSET
}

/// Random numbers for the fake data.
struct Noise(Rng);

impl Noise {
    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.0.unit()
    }

    /// Normal with mean 0 (Box–Muller).
    fn normal(&mut self, sd: f64) -> f64 {
        let (u1, u2) = (1.0 - self.0.unit(), self.0.unit());
        sd * (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// A fresh, empty scratch folder for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("check_timing").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Synth {
    seed: u64,
    n_events: usize,
    /// Uniform range of the trigger input delay (s).
    kb_latency: (f64, f64),
    /// A trigger Onset never saw (e.g. a missed keypress).
    drop_onset_trigger: Option<usize>,
    /// An event whose flash lasts a frame longer.
    dropped_frame_event: Option<usize>,
    write_frames: bool,
}

impl Synth {
    fn new(seed: u64) -> Synth {
        Synth {
            seed,
            n_events: 120,
            kb_latency: (0.0, 0.0),
            drop_onset_trigger: None,
            dropped_frame_event: None,
            write_frames: true,
        }
    }
}

struct Truth {
    /// Input delay of the t = 0 trigger (s).
    t0_delay: f64,
    /// Mean input delay of Onset's triggers (s).
    mean_delay: f64,
}

/// Write Onset-format logs for a fake run to root/onset, and a probe TSV.
/// Returns (raw log dir, probe path, truth).
fn synth_run(root: &Path, o: &Synth) -> (PathBuf, PathBuf, Truth) {
    let mut rng = Noise(Rng::new(o.seed));
    let raw = root.join("onset");
    std::fs::create_dir_all(&raw).unwrap();

    // Scanner: volumes every TR. The Arduino sees 6 volumes before Onset was
    // listening and 5 after it quit, as happens when the probe runs all session.
    let trig_true: Vec<f64> = (-6..165).map(|k| 30.0 + k as f64 * TR).collect();
    let delays: Vec<f64> = (0..160).map(|_| rng.uniform(o.kb_latency.0, o.kb_latency.1)).collect();
    let mut onset_trig_run: Vec<f64> = (0..160).map(|k| trig_true[k + 6] + RUN_OFFSET + delays[k]).collect();
    if let Some(i) = o.drop_onset_trigger {
        onset_trig_run.remove(i);
    }
    let t0_run = onset_trig_run[0]; // Onset's t = 0 is when IT saw the first trigger

    // Events: random ITIs, flips land on the 60 Hz frame grid of the run clock.
    let n = o.n_events;
    let mut planned = Vec::new();
    let mut t = 2.0;
    for _ in 0..n {
        t += rng.uniform(1.5, 3.5);
        planned.push(t);
    }
    let phase = 0.004; // vsync isn't aligned to anything in particular
    let flip_run: Vec<f64> =
        planned.iter().map(|p| ((p + t0_run - phase) / FRAME).ceil() * FRAME + phase).collect();
    let onset_rel: Vec<f64> = flip_run.iter().map(|f| f - t0_run).collect();

    // Light: lag + jitter; off after FLASH_FRAMES (+1 if a frame was dropped).
    let light_true: Vec<f64> = flip_run.iter().map(|f| f - RUN_OFFSET + LAG + rng.normal(JITTER)).collect();
    let off_true: Vec<f64> = (0..n)
        .map(|i| {
            let frames = FLASH_FRAMES + usize::from(o.dropped_frame_event == Some(i));
            light_true[i] + frames as f64 * FRAME + rng.normal(0.0003)
        })
        .collect();

    // Onset's files.
    let mut log = String::from(
        "onset\tduration\ttrial_type\tresponse_time\tresponse\tcorrect\tstim\ttrial\tevent\t\
         planned_onset\tplanned_duration\tonset_delay_ms\tframes\tlogged\tcondition\n",
    );
    for i in 0..n {
        let kind = if i % 2 == 1 { "face" } else { "house" };
        let delay_ms = (onset_rel[i] - planned[i]) * 1000.0;
        let (on, pl) = (onset_rel[i], planned[i]);
        writeln!(log, "{on:.4}\t0.5000\t{kind}\tn/a\tn/a\tn/a\timg{i}.png\t{}\tstim\t{pl:.4}\t0.5000\t{delay_ms:.2}\t30\t1\ta", i + 1)
            .unwrap();
    }
    // An event that never reached the screen (frames = 0) must be ignored.
    log.push_str("9999.0000\t0.0000\tn/a\tn/a\tn/a\tn/a\tx\t999\tstim\t9999.0000\t0.5000\tn/a\t0\t1\ta\n");
    std::fs::write(raw.join(format!("{STEM}_eventlog.tsv")), log).unwrap();

    let mut inputs = String::from("time\tt\tsource\traw\tkind\tlabel\n");
    writeln!(inputs, "{:.6}\tn/a\tkeyboard\tspace\toperator\tcontinue", t0_run - 20.0).unwrap();
    for t in &onset_trig_run {
        writeln!(inputs, "{t:.6}\t{:.6}\tkeyboard\t5\ttrigger\ttrigger", t - t0_run).unwrap();
    }
    writeln!(inputs, "{:.6}\t50.000000\tkeyboard\t1\tbutton\tleft", t0_run + 50.0).unwrap();
    std::fs::write(raw.join(format!("{STEM}_inputs.tsv")), inputs).unwrap();

    let info = serde_json::json!({"t0_run_clock": t0_run, "measured_refresh_hz": HZ, "missed_frames": 0});
    std::fs::write(raw.join(format!("{STEM}_run.json")), info.to_string()).unwrap();

    if o.write_frames {
        let start = flip_run[0] - 5.0;
        let nfr = ((flip_run[n - 1] + 2.0 - start) / FRAME) as usize;
        let mut lit = vec![false; nfr];
        for f in &flip_run {
            let first = ((f - start) / FRAME).round() as usize;
            lit[first..first + FLASH_FRAMES].fill(true);
        }
        let mut frames = String::from("frame\ttime\tt\tinterval_ms\tmissed\tphase\tevent\tphotodiode\n");
        for (i, on) in lit.iter().enumerate() {
            let time = start + i as f64 * FRAME;
            writeln!(
                frames,
                "{i}\t{time:.6}\t{:.6}\t{:.3}\t0\trunning\tn/a\t{}",
                time - t0_run,
                FRAME * 1000.0,
                *on as u8
            )
            .unwrap();
        }
        std::fs::write(raw.join(format!("{STEM}_frames.tsv")), frames).unwrap();
    }

    // The probe file.
    let mut rec: Vec<(&str, f64, &str)> = trig_true.iter().map(|&t| ("T", to_arduino(t), "n/a")).collect();
    rec.extend(light_true.iter().map(|&t| ("P", to_arduino(t), "1")));
    rec.extend(off_true.iter().map(|&t| ("P", to_arduino(t), "0")));
    // A 50 µs noise blip in the middle of nowhere: must be rejected.
    let blip = light_true[10] + 0.7;
    rec.push(("P", to_arduino(blip), "1"));
    rec.push(("P", to_arduino(blip + 50e-6), "0"));
    let probe = root.join("probe.tsv");
    write_probe(&probe, &mut rec);

    let truth = Truth { t0_delay: delays[0], mean_delay: delays.iter().sum::<f64>() / delays.len() as f64 };
    (raw, probe, truth)
}

fn write_probe(path: &Path, rec: &mut [(&str, f64, &str)]) {
    rec.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut text = String::from("# onset_timing_probe v1\nkind\tarduino_us\tlevel\thost_time\n");
    for (kind, t, level) in rec.iter() {
        writeln!(text, "{kind}\t{}\t{level}\t0", (t * 1e6).round() as i64).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn analyze(raw: &Path, probe: &Path, opts: &RunOptions) -> Report {
    let run = load_onset_run(raw, None, &[]).unwrap();
    analyze_onset_run(&run, &read_probe(probe, 0.0005).unwrap(), opts).unwrap()
}

fn near(value: f64, target: f64, tol: f64) -> bool {
    (value - target).abs() < tol
}

#[test]
fn recovers_lag_jitter_and_drift() {
    let root = scratch("lag_jitter_drift");
    let (raw, probe, _) = synth_run(&root, &Synth::new(1));
    let s = analyze(&raw, &probe, &RunOptions { flash_frames: 3, ..Default::default() }).summary;
    assert_eq!(s["events"], 120.0); // the frames = 0 row was dropped
    assert_eq!((s["matched"], s["unmatched"]), (120.0, 0.0));
    assert!(near(s["latency_mean_ms"], 12.0, 0.3), "{}", s["latency_mean_ms"]);
    assert!((0.85..1.15).contains(&s["latency_sd_ms"]), "{}", s["latency_sd_ms"]);
    // The Arduino runs 50 ppm fast, so mapping it onto Onset's clock has slope 1 - 50 ppm.
    assert!(near(s["clock_drift_ppm"], -50.0, 1.0), "{}", s["clock_drift_ppm"]);
    assert_eq!((s["triggers_paired"], s["triggers_arduino"]), (160.0, 171.0));
    assert_eq!(s["inferred_dropped_frames"], 0.0);
    assert_eq!(s["extra_edges"], 0.0); // the 50 µs blip was rejected as too short
    assert_eq!(s["rejected_short_pulses"], 1.0);
    // No trigger input delay here, so the "true" onset error equals the lag too.
    assert!(near(s["true_minus_logged_mean_ms"], 12.0, 0.3));
    assert!(near(s["planned_error_mean_ms"], 12.0, 17.0)); // planned -> flip adds up to a frame
}

#[test]
fn keyboard_trigger_latency_and_missing_trigger() {
    // A fORP sending keypresses: triggers arrive 1-4 ms late; one is lost.
    let root = scratch("keyboard_latency");
    let o = Synth { kb_latency: (0.001, 0.004), drop_onset_trigger: Some(40), ..Synth::new(2) };
    let (raw, probe, truth) = synth_run(&root, &o);
    let s = analyze(&raw, &probe, &RunOptions::default()).summary;
    assert_eq!(s["matched"], 120.0);
    assert_eq!(s["triggers_paired"], 159.0);
    // The fit absorbs the mean trigger delay into the latency...
    assert!(near(s["latency_mean_ms"], 12.0 + truth.mean_delay * 1000.0, 0.3));
    // ...but the jitter is untouched.
    assert!((0.85..1.15).contains(&s["latency_sd_ms"]), "{}", s["latency_sd_ms"]);
    // true - logged = lag + the delay of the t = 0 trigger specifically.
    assert!(near(s["true_minus_logged_mean_ms"], 12.0 + truth.t0_delay * 1000.0, 0.3));
    // A uniform 3 ms range has SD ~0.87 ms.
    assert!((0.5..1.2).contains(&s["trigger_residual_sd_ms"]), "{}", s["trigger_residual_sd_ms"]);
}

#[test]
fn dropped_frame_during_flash_is_detected() {
    let root = scratch("dropped_frame");
    let (raw, probe, _) = synth_run(&root, &Synth { dropped_frame_event: Some(50), ..Synth::new(3) });
    let rep = analyze(&raw, &probe, &RunOptions::default());
    assert_eq!(rep.summary["inferred_dropped_frames"], 1.0);
    assert_eq!(rep.table.num("extra_frames")[50], 1.0);
    assert_eq!(rep.table.text("status")[50], "ok");
}

#[test]
fn frames_tsv_mode() {
    let root = scratch("frames_mode");
    let (raw, probe, _) = synth_run(&root, &Synth::new(4));
    let s = analyze(&raw, &probe, &RunOptions { use_frames: true, ..Default::default() }).summary;
    assert_eq!((s["events"], s["matched"]), (120.0, 120.0));
    assert!(near(s["latency_mean_ms"], 12.0, 0.3), "{}", s["latency_mean_ms"]);
    assert!((0.85..1.15).contains(&s["latency_sd_ms"]), "{}", s["latency_sd_ms"]);
}

#[test]
fn timing_test_mode() {
    let mut rng = Noise(Rng::new(5));
    // The frame index on screen at each refresh; refresh 500 repeats the
    // previous image (a dropped frame).
    let mut shown: Vec<usize> = (0..1200).collect();
    shown.insert(500, 499);
    let white: Vec<bool> = shown.iter().map(|f| f % 2 == 0).collect();
    let (mut rises, mut falls) = (Vec::new(), Vec::new());
    for i in 1..shown.len() {
        let t = i as f64 * FRAME;
        if white[i] && !white[i - 1] {
            rises.push(to_arduino(t + 0.012 + rng.normal(0.0002)));
        } else if !white[i] && white[i - 1] {
            falls.push(to_arduino(t + 0.014));
        }
    }
    let probe = make_probe(&[], &rises, &falls, 0.0005);
    let s = analyze_timing_test(&probe, None).unwrap().summary;
    assert_eq!(s["inferred_dropped_frames"], 1.0);
    assert_eq!(s["intervals_longer"], 1.0);
    assert!(
        near(s["refresh_hz_arduino_clock"], 60.0 / (1.0 + DRIFT), 0.01),
        "{}",
        s["refresh_hz_arduino_clock"]
    );
    assert!(s["interval_dev_max_abs_ms"] < 1.0);
}

#[test]
fn eprime_mode() {
    // E-Prime logs one trigger time; the TR calibrates the Arduino's clock.
    let mut rng = Noise(Rng::new(6));
    let root = scratch("eprime");
    let trig_true: Vec<f64> = (0..200).map(|k| 10.0 + k as f64 * TR).collect(); // the Arduino sees all volumes
    let ep_offset = 55.0; // E-Prime clock = true + 55 s (milliseconds in the file)
    let t0_true = trig_true[3]; // E-Prime started waiting after 3 volumes
    let mut onsets = Vec::new();
    let mut t = t0_true;
    for _ in 0..100 {
        t += rng.uniform(2.0, 4.0);
        onsets.push(t);
    }
    let light: Vec<f64> = onsets.iter().map(|o| o + LAG + rng.normal(JITTER)).collect();
    let mut rows =
        String::from("ExperimentName\tSubject\tTrial\tCondition\tWaitTrigger.RTTime\tStim.OnsetTime\r\n");
    for (i, o) in onsets.iter().enumerate() {
        let (trig_ms, onset_ms) = ((t0_true + ep_offset) * 1000.0, (o + ep_offset) * 1000.0);
        write!(rows, "test\t1\t{}\tA\t{trig_ms:.0}\t{onset_ms:.0}\r\n", i + 1).unwrap();
    }
    // E-DataAid's Unicode export: UTF-16 with a byte-order mark.
    let utf16: Vec<u8> =
        [0xFF, 0xFE].into_iter().chain(rows.encode_utf16().flat_map(u16::to_le_bytes)).collect();
    let export = root.join("eprime.txt");
    std::fs::write(&export, utf16).unwrap();

    let triggers: Vec<f64> = trig_true.iter().map(|&t| to_arduino(t)).collect();
    let rises: Vec<f64> = light.iter().map(|&t| to_arduino(t)).collect();
    let falls: Vec<f64> = light.iter().map(|&t| to_arduino(t + 0.1)).collect();
    let probe = make_probe(&triggers, &rises, &falls, 0.0005);
    let opts = EprimeOptions {
        onset_cols: vec!["Stim.OnsetTime".into()],
        trigger_col: "WaitTrigger.RTTime".into(),
        label_col: Some("Condition".into()),
        hz: Some(60.0),
        tr: Some(TR),
        flash_frames: None,
        max_latency_s: 0.1,
        trigger_tol: 0.02,
        trigger_index: None,
    };
    let s = analyze_eprime(&read_eprime(&export).unwrap(), &probe, &opts).unwrap().summary;
    assert_eq!(s["matched"], 100.0);
    // E-Prime logs whole ms, which adds ~0.3 ms rounding noise.
    assert!(near(s["latency_mean_ms"], 12.0, 0.3), "{}", s["latency_mean_ms"]);
    assert!((0.85..1.25).contains(&s["latency_sd_ms"]), "{}", s["latency_sd_ms"]);
    assert!(near(s["clock_drift_ppm"], -50.0, 2.0), "{}", s["clock_drift_ppm"]);
}

#[test]
fn finds_the_run_from_any_of_its_paths() {
    let root = scratch("paths");
    let (raw, probe, _) = synth_run(&root, &Synth { write_frames: false, ..Synth::new(8) });
    // A BIDS func/ events.tsv points at the raw logs in ../onset.
    std::fs::create_dir_all(root.join("func")).unwrap();
    let events = root.join("func").join(format!("{STEM}_events.tsv"));
    std::fs::write(&events, "onset\n").unwrap();
    let eventlog = raw.join(format!("{STEM}_eventlog.tsv"));
    for path in [&root, &raw, &eventlog, &events] {
        let run = load_onset_run(path, None, &[]).unwrap();
        assert_eq!((run.stem.as_str(), run.events.rows.len()), (STEM, 120), "{}", path.display());
    }
    // Without t0 in run.json, t = 0 comes from rows that have both clocks.
    let s = analyze(&raw, &probe, &RunOptions::default()).summary;
    std::fs::write(raw.join(format!("{STEM}_run.json")), r#"{"measured_refresh_hz": 60.0}"#).unwrap();
    let recovered = analyze(&raw, &probe, &RunOptions::default()).summary;
    assert_eq!(recovered["matched"], 120.0);
    assert!(near(recovered["latency_mean_ms"], s["latency_mean_ms"], 1e-6));
}

#[test]
fn cli_end_to_end() {
    let root = scratch("cli");
    let (raw, probe, _) = synth_run(&root, &Synth::new(7));
    let eventlog = raw.join(format!("{STEM}_eventlog.tsv"));
    let (csv, tsv) = (root.join("timing.csv"), root.join("timing.tsv"));
    for out in [&csv, &tsv] {
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_onset"))
            .args(["check-timing", "--no-table"])
            .arg("--probe")
            .arg(&probe)
            .arg("--run")
            .arg(&eventlog)
            .arg("--out")
            .arg(out)
            .output()
            .unwrap();
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(stdout.contains("[PASS] every onset produced light (0 unmatched)"), "{stdout}");
    }
    let text = std::fs::read_to_string(&csv).unwrap();
    let mut rows = text.lines();
    let header: Vec<&str> = rows.next().unwrap().split(',').collect();
    let lat = header.iter().position(|c| *c == "latency_ms").unwrap();
    let latencies: Vec<f64> = rows.map(|r| r.split(',').nth(lat).unwrap().parse().unwrap()).collect();
    assert_eq!(latencies.len(), 120);
    assert!(near(latencies.iter().sum::<f64>() / 120.0, 12.0, 0.3));
    // A .tsv name gives a tab-separated table.
    let tsv_text = std::fs::read_to_string(&tsv).unwrap();
    assert!(tsv_text.lines().next().unwrap().contains("\tlatency_ms\t"));
    assert_eq!(tsv_text.lines().count(), 121);
}
