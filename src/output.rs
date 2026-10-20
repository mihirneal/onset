//! Writing results: BIDS events files plus raw timing logs.
//!
//! All disk I/O happens on a dedicated writer thread fed by a channel, so a
//! slow disk can never delay a frame. Raw logs are streamed and flushed
//! several times a second; if Onset crashes mid-run, everything up to the
//! last fraction of a second is on disk, and the BIDS events.tsv is still
//! written from whatever events finished.

use crate::run::{EventRecord, FrameRecord, Phase};
use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Where one run's files go.
#[derive(Debug, Clone)]
pub struct Paths {
    pub func_dir: PathBuf,
    pub raw_dir: PathBuf,
    /// `sub-01_ses-01_task-x_run-01`
    pub stem: String,
}

impl Paths {
    pub fn new(out: &Path, sub: &str, ses: Option<&str>, task: &str, run: u32) -> Paths {
        let mut dir = out.join(format!("sub-{sub}"));
        let mut stem = format!("sub-{sub}");
        if let Some(ses) = ses {
            dir = dir.join(format!("ses-{ses}"));
            stem.push_str(&format!("_ses-{ses}"));
        }
        stem.push_str(&format!("_task-{task}_run-{run:02}"));
        Paths { func_dir: dir.join("func"), raw_dir: dir.join("onset"), stem }
    }

    pub fn events_tsv(&self) -> PathBuf {
        self.func_dir.join(format!("{}_events.tsv", self.stem))
    }
    pub fn events_json(&self) -> PathBuf {
        self.func_dir.join(format!("{}_events.json", self.stem))
    }
    pub fn raw(&self, suffix: &str) -> PathBuf {
        self.raw_dir.join(format!("{}_{suffix}", self.stem))
    }

    pub fn exists(&self) -> bool {
        self.events_tsv().exists() || self.raw("run.json").exists()
    }
}

/// First run number (from `start`) whose files don't exist yet.
pub fn next_free_run(out: &Path, sub: &str, ses: Option<&str>, task: &str, start: u32) -> u32 {
    (start..).find(|&r| !Paths::new(out, sub, ses, task, r).exists()).unwrap_or(start)
}

/// BIDS labels: letters and digits only.
pub fn check_label(what: &str, label: &str) -> Result<()> {
    if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("{what} {label:?} must be letters and digits only (BIDS labels can't contain - or _)");
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct InputRecord {
    /// Run-clock seconds.
    pub time: f64,
    pub source: &'static str,
    pub raw: String,
    /// trigger / button / operator / ignored
    pub kind: &'static str,
    pub label: String,
}

pub enum LogMsg {
    /// The run clock time of t = 0, once known.
    T0(f64),
    Frame(FrameRecord),
    Input(InputRecord),
    Event(EventRecord),
    /// Final summary fields merged into run.json.
    Finish(serde_json::Value),
}

pub struct WriterReport {
    pub events_written: usize,
    pub files: Vec<PathBuf>,
}

/// Start the writer thread. `header` is the provenance written to run.json.
pub fn spawn_writer(
    paths: Paths,
    header: serde_json::Value,
    columns: Vec<String>,
    onset_column: Option<String>,
) -> Result<(Sender<LogMsg>, JoinHandle<Result<WriterReport>>)> {
    fs::create_dir_all(&paths.func_dir)
        .with_context(|| format!("can't create {}", paths.func_dir.display()))?;
    fs::create_dir_all(&paths.raw_dir)
        .with_context(|| format!("can't create {}", paths.raw_dir.display()))?;
    write_json(&paths.raw("run.json"), &header)?;

    let (tx, rx) = channel();
    let handle = std::thread::Builder::new()
        .name("onset-writer".into())
        .spawn(move || writer(rx, paths, header, columns, onset_column))?;
    Ok((tx, handle))
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(value)? + "\n")?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn num(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.decimals$}"),
        _ => "n/a".into(),
    }
}

/// TSV cells can't contain tabs or newlines.
fn cell(s: &str) -> String {
    if s.is_empty() { "n/a".into() } else { s.replace(['\t', '\n', '\r'], " ") }
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Instructions => "instructions",
        Phase::Waiting => "waiting",
        Phase::Running => "running",
        Phase::Holding => "holding",
        Phase::Done(_) => "done",
    }
}

/// Columns Onset itself writes to events.tsv, in order.
const EVENT_COLUMNS: &[&str] = &[
    "onset",
    "duration",
    "trial_type",
    "response_time",
    "response",
    "correct",
    "stim",
    "trial",
    "event",
    "planned_onset",
    "planned_duration",
    "onset_delay_ms",
    "frames",
];

fn writer(
    rx: Receiver<LogMsg>,
    paths: Paths,
    mut header: serde_json::Value,
    columns: Vec<String>,
    onset_column: Option<String>,
) -> Result<WriterReport> {
    let open = |suffix: &str| -> Result<BufWriter<File>> {
        let p = paths.raw(suffix);
        Ok(BufWriter::new(File::create(&p).with_context(|| format!("can't create {}", p.display()))?))
    };
    let mut frames = open("frames.tsv")?;
    let mut inputs = open("inputs.tsv")?;
    let mut eventlog = open("eventlog.tsv")?;
    writeln!(
        frames,
        "frame\ttime\tt\tinterval_ms\tmissed\tphase\tevent\tphotodiode\tpresented\tpresented_t\treport"
    )?;
    writeln!(inputs, "time\tt\tsource\traw\tkind\tlabel")?;

    // Schedule columns become extra events.tsv columns; rename clashes with ours.
    let extra: Vec<(String, String)> = columns
        .iter()
        .filter(|c| Some(*c) != onset_column.as_ref())
        .map(|c| {
            let out = if EVENT_COLUMNS.contains(&c.as_str()) { format!("row_{c}") } else { c.clone() };
            (c.clone(), out)
        })
        .collect();
    let mut log_header: Vec<String> = EVENT_COLUMNS.iter().map(|s| s.to_string()).collect();
    log_header.push("logged".into());
    log_header.extend(extra.iter().map(|(_, o)| o.clone()));
    writeln!(eventlog, "{}", log_header.join("\t"))?;

    let mut t0: Option<f64> = None;
    let mut events: Vec<EventRecord> = Vec::new();
    let mut finish: Option<serde_json::Value> = None;
    let mut last_flush = Instant::now();
    let rel = |t: f64, t0: Option<f64>| num(t0.map(|z| t - z), 6);

    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Some(LogMsg::T0(t)) => t0 = Some(t),
            Some(LogMsg::Frame(f)) => writeln!(
                frames,
                "{}\t{:.6}\t{}\t{:.3}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                f.index,
                f.acquired,
                rel(f.acquired, t0),
                f.interval * 1000.0,
                f.missed,
                phase_name(f.phase),
                f.event.map_or("n/a".into(), |e| e.to_string()),
                f.photodiode as u8,
                num(f.report.presented(), 6),
                f.report.presented().map_or("n/a".into(), |p| rel(p, t0)),
                f.report.label()
            )?,
            Some(LogMsg::Input(i)) => writeln!(
                inputs,
                "{:.6}\t{}\t{}\t{}\t{}\t{}",
                i.time,
                rel(i.time, t0),
                i.source,
                cell(&i.raw),
                i.kind,
                cell(&i.label)
            )?,
            Some(LogMsg::Event(e)) => {
                let mut row = event_cells(&e);
                row.push((e.log as u8).to_string());
                row.extend(extra.iter().map(|(c, _)| cell(e.vars.get(c).map_or("", String::as_str))));
                writeln!(eventlog, "{}", row.join("\t"))?;
                eventlog.flush()?;
                events.push(e);
            }
            Some(LogMsg::Finish(summary)) => {
                finish = Some(summary);
                break;
            }
            None => {}
        }
        if last_flush.elapsed() > Duration::from_millis(250) {
            frames.flush()?;
            inputs.flush()?;
            last_flush = Instant::now();
        }
    }
    frames.flush()?;
    inputs.flush()?;
    eventlog.flush()?;

    // BIDS events.tsv: logged events that reached the screen, by onset.
    let mut bids: Vec<&EventRecord> = events.iter().filter(|e| e.log && e.onset.is_some()).collect();
    bids.sort_by(|a, b| a.onset.unwrap().total_cmp(&b.onset.unwrap()));
    let mut out = BufWriter::new(File::create(paths.events_tsv())?);
    let mut head: Vec<String> = EVENT_COLUMNS.iter().map(|s| s.to_string()).collect();
    head.extend(extra.iter().map(|(_, o)| o.clone()));
    writeln!(out, "{}", head.join("\t"))?;
    for e in &bids {
        let mut row = event_cells(e);
        row.extend(extra.iter().map(|(c, _)| cell(e.vars.get(c).map_or("", String::as_str))));
        writeln!(out, "{}", row.join("\t"))?;
    }
    out.flush()?;
    write_json(&paths.events_json(), &sidecar(&extra))?;

    let status = if finish.is_some() { "finished" } else { "crashed" };
    if let Some(obj) = header.as_object_mut() {
        obj.insert("writer_status".into(), status.into());
        obj.insert("t0_run_clock".into(), t0.into());
        if let Some(serde_json::Value::Object(summary)) = finish {
            obj.extend(summary);
        }
    }
    write_json(&paths.raw("run.json"), &header)?;

    Ok(WriterReport {
        events_written: bids.len(),
        files: vec![
            paths.events_tsv(),
            paths.events_json(),
            paths.raw("eventlog.tsv"),
            paths.raw("frames.tsv"),
            paths.raw("inputs.tsv"),
            paths.raw("run.json"),
        ],
    })
}

fn event_cells(e: &EventRecord) -> Vec<String> {
    vec![
        num(e.onset.or(Some(e.planned_onset)), 4),
        num(e.duration.or(Some(0.0)), 4),
        cell(&e.trial_type),
        num(e.response_time, 4),
        cell(e.response.as_deref().unwrap_or("")),
        e.correct.map_or("n/a".into(), |c| (c as u8).to_string()),
        cell(&e.stimulus),
        e.trial.to_string(),
        cell(&e.event),
        num(Some(e.planned_onset), 4),
        num(Some(e.planned_duration), 4),
        num(e.onset_delay_ms(), 2),
        e.frames.to_string(),
    ]
}

fn sidecar(extra: &[(String, String)]) -> serde_json::Value {
    let mut v = serde_json::json!({
        "onset": {"Description": "Measured time the event first appeared on screen, relative to the first kept scanner trigger: when its first frame was on screen, as reported by the display (estimated from the frame loop for a frame it did not report; see frame_timing in run.json).", "Units": "s"},
        "duration": {"Description": "Measured time from this event's first frame on screen to the first frame that no longer showed it.", "Units": "s"},
        "trial_type": {"Description": "Condition label from the experiment's trial_type template."},
        "response_time": {"Description": "Time from the measured onset to the first accepted button press.", "Units": "s"},
        "response": {"Description": "Button label of the first accepted response, n/a if none."},
        "correct": {"Description": "1 if the response matched the correct button, 0 if wrong or missed, n/a if not scored.", "Levels": {"1": "correct", "0": "incorrect or miss"}},
        "stim": {"Description": "Name of the stimulus that was shown."},
        "trial": {"Description": "Trial number (1-based, in presentation order)."},
        "event": {"Description": "Event name within the trial, from the experiment file."},
        "planned_onset": {"Description": "Scheduled onset relative to the first kept trigger.", "Units": "s"},
        "planned_duration": {"Description": "Scheduled duration.", "Units": "s"},
        "onset_delay_ms": {"Description": "onset minus planned_onset.", "Units": "ms"},
        "frames": {"Description": "Number of display refreshes the event was on screen."},
        "StimulusPresentation": {
            "SoftwareName": "Onset",
            "SoftwareVersion": env!("CARGO_PKG_VERSION"),
            "OperatingSystem": std::env::consts::OS,
        }
    });
    for (orig, out) in extra {
        v[out] = serde_json::json!({"Description": format!("Value of the schedule column {orig:?} for this trial.")});
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bids_paths() {
        let p = Paths::new(Path::new("data"), "01", Some("pre"), "flanker", 2);
        assert_eq!(
            p.events_tsv(),
            Path::new("data/sub-01/ses-pre/func/sub-01_ses-pre_task-flanker_run-02_events.tsv")
        );
        assert_eq!(
            p.raw("frames.tsv"),
            Path::new("data/sub-01/ses-pre/onset/sub-01_ses-pre_task-flanker_run-02_frames.tsv")
        );
        let p = Paths::new(Path::new("data"), "7", None, "rest", 1);
        assert_eq!(p.events_tsv(), Path::new("data/sub-7/func/sub-7_task-rest_run-01_events.tsv"));
    }

    #[test]
    fn labels_must_be_alphanumeric() {
        assert!(check_label("subject", "01").is_ok());
        assert!(check_label("subject", "sub-01").is_err());
        assert!(check_label("subject", "").is_err());
    }
}
