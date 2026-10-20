//! Reading the files the timing check compares: the probe recording, an
//! Onset run's raw logs, and E-Prime tab-delimited exports.

use super::align::{search_right, sorted};
use super::stats::median;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// A table read from a tab-separated text file: a header and string cells.
#[derive(Debug, Clone, Default)]
pub struct Tsv {
    /// File name, for error messages.
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Tsv {
    /// Parse tab-separated text. `comments`: skip lines starting with `#`.
    /// `quotes`: allow "quoted" cells (exports from other programs; Onset
    /// never quotes). Blank lines are skipped.
    pub fn parse(name: &str, text: &str, comments: bool, quotes: bool) -> Result<Tsv> {
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .has_headers(false)
            .flexible(true)
            .quoting(quotes)
            .comment(comments.then_some(b'#'))
            .from_reader(text.as_bytes());
        let mut lines = Vec::new();
        for record in reader.records() {
            let record = record.with_context(|| format!("can't read {name}"))?;
            if record.iter().any(|cell| !cell.is_empty()) {
                lines.push(record.iter().map(str::to_string).collect::<Vec<_>>());
            }
        }
        if lines.is_empty() {
            bail!("{name} is empty");
        }
        let columns = lines.remove(0);
        Ok(Tsv { name: name.to_string(), columns, rows: lines })
    }

    /// Read one of Onset's own logs.
    pub fn read(path: &Path) -> Result<Tsv> {
        let text = std::fs::read_to_string(path).with_context(|| format!("can't read {}", path.display()))?;
        Tsv::parse(&path.display().to_string(), &text, false, false)
    }

    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    pub fn require(&self, name: &str) -> Result<usize> {
        self.column(name).with_context(|| format!("{} has no {name:?} column", self.name))
    }

    /// The cell at `row`, `col`; empty if the row is short.
    pub fn get(&self, row: usize, col: usize) -> &str {
        self.rows[row].get(col).map_or("", String::as_str)
    }

    /// A column as numbers: `n/a` and empty cells are NaN, anything else that
    /// isn't a number is an error.
    pub fn numbers(&self, name: &str) -> Result<Vec<f64>> {
        let col = self.require(name)?;
        (0..self.rows.len())
            .map(|r| match self.get(r, col).trim() {
                "" | "n/a" => Ok(f64::NAN),
                s => s
                    .parse()
                    .with_context(|| format!("{} row {}: {name} = {s:?} is not a number", self.name, r + 1)),
            })
            .collect()
    }

    /// A column as numbers, with NaN for anything that isn't one.
    pub fn numbers_or_nan(&self, col: usize) -> Vec<f64> {
        (0..self.rows.len()).map(|r| self.get(r, col).trim().parse().unwrap_or(f64::NAN)).collect()
    }

    /// The same table with only these rows, in this order.
    pub fn select(&self, rows: &[usize]) -> Tsv {
        Tsv {
            name: self.name.clone(),
            columns: self.columns.clone(),
            rows: rows.iter().map(|&r| self.rows[r].clone()).collect(),
        }
    }
}

// ─────────────────────────────── probe ───────────────────────────────

/// Everything the Arduino recorded, in seconds on the Arduino's clock.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// Trigger rising edges.
    pub triggers: Vec<f64>,
    /// Light-on times of accepted pulses, sorted.
    pub on: Vec<f64>,
    /// Matching light-off times (NaN if the log ended while lit).
    pub off: Vec<f64>,
    /// Pulses dropped as too short (noise blips).
    pub rejected: usize,
}

impl Probe {
    /// How long pulse `i` was lit (s).
    pub fn width(&self, i: usize) -> f64 {
        self.off[i] - self.on[i]
    }
}

/// Pair each light-on with the first light-off after it.
///
/// The sketch always alternates on and off, but a recording may start or
/// stop mid-pulse: an off with no on before it is dropped, an on with no off
/// after it gets NaN.
pub fn pair_pulses(rises: &[f64], falls: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let (rises, falls) = (sorted(rises), sorted(falls));
    let off = (0..rises.len())
        .map(|i| {
            let next_rise = rises.get(i + 1).copied().unwrap_or(f64::INFINITY);
            match falls.get(search_right(&falls, rises[i])) {
                Some(&f) if f <= next_rise => f,
                _ => f64::NAN,
            }
        })
        .collect();
    (rises, off)
}

/// Build a [`Probe`] from raw edge times (s), dropping pulses shorter than
/// `min_pulse_s`.
pub fn make_probe(triggers: &[f64], rises: &[f64], falls: &[f64], min_pulse_s: f64) -> Probe {
    let (on, off) = pair_pulses(rises, falls);
    // Keep pulses that are long enough, and those with no end yet (NaN width).
    let keep: Vec<bool> = on.iter().zip(&off).map(|(a, b)| b.is_nan() || b - a >= min_pulse_s).collect();
    let pick = |v: &[f64]| v.iter().zip(&keep).filter(|k| *k.1).map(|k| *k.0).collect();
    Probe {
        triggers: sorted(triggers),
        on: pick(&on),
        off: pick(&off),
        rejected: keep.iter().filter(|k| !**k).count(),
    }
}

/// Read a probe recording made by `onset probe`.
pub fn read_probe(path: &Path, min_pulse_s: f64) -> Result<Probe> {
    let text = std::fs::read_to_string(path).with_context(|| format!("can't read {}", path.display()))?;
    let t = Tsv::parse(&path.display().to_string(), &text, true, false)?;
    let (Some(kind), Some(us)) = (t.column("kind"), t.column("arduino_us")) else {
        bail!("{}: not a probe log (it needs the columns kind and arduino_us)", path.display());
    };
    let level = t.column("level");
    let (mut triggers, mut rises, mut falls) = (Vec::new(), Vec::new(), Vec::new());
    for r in 0..t.rows.len() {
        let cell = t.get(r, us).trim();
        let micros: i64 = cell.parse().with_context(|| {
            format!("{} row {}: arduino_us = {cell:?} is not a whole number", path.display(), r + 1)
        })?;
        // Integer microseconds to float seconds: exact for ~285 years.
        let s = micros as f64 / 1e6;
        match (t.get(r, kind), level.map(|l| t.get(r, l))) {
            ("T", _) => triggers.push(s),
            ("P", Some("1")) => rises.push(s),
            ("P", Some("0")) => falls.push(s),
            _ => {}
        }
    }
    Ok(make_probe(&triggers, &rises, &falls, min_pulse_s))
}

// ───────────────────────────── Onset runs ─────────────────────────────

/// The parts of an Onset run's raw logs the timing check uses.
#[derive(Debug, Clone)]
pub struct OnsetRun {
    pub raw_dir: PathBuf,
    pub stem: String,
    /// Run-clock time of the first kept trigger.
    pub t0: f64,
    pub hz: Option<f64>,
    /// Run-clock trigger times.
    pub triggers: Vec<f64>,
    /// Eventlog rows that reached the screen (frames > 0), by onset.
    pub events: Tsv,
    pub frames: Option<Tsv>,
    /// run.json (`null` if there is none).
    pub info: serde_json::Value,
}

fn eventlogs(dir: &Path) -> Vec<String> {
    let mut stems: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| {
                    e.ok()?.file_name().to_str()?.strip_suffix("_eventlog.tsv").map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    stems.sort();
    stems
}

/// Accept an eventlog path, a BIDS func/ events.tsv path, or a directory
/// (plus a stem when it holds several runs). Returns (raw log dir, stem).
pub fn resolve_run(run: &Path, stem: Option<&str>) -> Result<(PathBuf, String)> {
    if run.is_file() {
        let name = run.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let parent = run.parent().unwrap_or(Path::new(""));
        if let Some(s) = name.strip_suffix("_eventlog.tsv") {
            return Ok((parent.to_path_buf(), s.to_string()));
        }
        if let Some(s) = name.strip_suffix("_events.tsv") {
            // A BIDS func/ file: the raw logs live in ../onset.
            return Ok((parent.parent().unwrap_or(Path::new("")).join("onset"), s.to_string()));
        }
        bail!("{}: expected a *_eventlog.tsv or *_events.tsv file", run.display());
    }
    if !run.is_dir() {
        bail!("{} doesn't exist", run.display());
    }
    let mut dir = run.to_path_buf();
    if dir.join("onset").is_dir() && eventlogs(&dir).is_empty() {
        dir = dir.join("onset");
    }
    if let Some(stem) = stem {
        return Ok((dir, stem.to_string()));
    }
    match eventlogs(&dir).as_slice() {
        [one] => Ok((dir, one.clone())),
        logs => {
            let names: Vec<String> = logs.iter().map(|s| format!("{s}_eventlog.tsv")).collect();
            let names = if names.is_empty() { "(none)".to_string() } else { names.join("\n  ") };
            bail!("{}: pass --stem to choose one of these eventlogs:\n  {names}", dir.display())
        }
    }
}

/// Median of `time - t` over the rows that have both: recovers t = 0 from
/// a run whose run.json didn't get its final summary.
fn t0_from_rows(table: &Tsv) -> Result<Option<f64>> {
    if table.column("t").is_none() {
        return Ok(None);
    }
    let (time, t) = (table.numbers("time")?, table.numbers("t")?);
    let diffs: Vec<f64> = time.iter().zip(&t).map(|(a, b)| a - b).filter(|d| !d.is_nan()).collect();
    Ok((!diffs.is_empty()).then(|| median(&diffs)))
}

/// Load a run's logs. Triggers come from inputs.tsv: those from
/// `trigger_sources` if given, otherwise all but simulated ones (the
/// Arduino can't see triggers Onset made up itself).
pub fn load_onset_run(run: &Path, stem: Option<&str>, trigger_sources: &[String]) -> Result<OnsetRun> {
    let (raw_dir, stem) = resolve_run(run, stem)?;
    let path = |suffix: &str| raw_dir.join(format!("{stem}_{suffix}"));

    let info: serde_json::Value = match std::fs::read_to_string(path("run.json")) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("can't parse {}", path("run.json").display()))?,
        Err(_) => serde_json::Value::Null,
    };

    let inputs = Tsv::read(&path("inputs.tsv"))?;
    let (kind, source) = (inputs.require("kind")?, inputs.require("source")?);
    let times = inputs.numbers("time")?;
    let triggers = (0..inputs.rows.len())
        .filter(|&r| inputs.get(r, kind) == "trigger")
        .filter(|&r| {
            let src = inputs.get(r, source);
            if trigger_sources.is_empty() {
                src != "simulated"
            } else {
                trigger_sources.iter().any(|s| s == src)
            }
        })
        .map(|r| times[r])
        .collect();

    let frames = if path("frames.tsv").exists() { Some(Tsv::read(&path("frames.tsv"))?) } else { None };

    let mut t0 = info.get("t0_run_clock").and_then(serde_json::Value::as_f64);
    if t0.is_none() {
        t0 = t0_from_rows(&inputs)?;
    }
    if let (None, Some(f)) = (t0, &frames) {
        t0 = t0_from_rows(f)?;
    }
    let Some(t0) = t0 else {
        bail!("{stem}: can't find t0 (no trigger was received?)");
    };

    let mut hz = info.get("measured_refresh_hz").and_then(serde_json::Value::as_f64);
    if let (None, Some(f)) = (hz, &frames) {
        if f.rows.len() > 10 {
            let intervals: Vec<f64> =
                f.numbers("interval_ms")?.into_iter().skip(1).filter(|v| !v.is_nan()).collect();
            hz = Some(1000.0 / median(&intervals));
        }
    }

    let log = Tsv::read(&path("eventlog.tsv"))?;
    let shown = log.numbers("frames")?;
    let onsets = log.numbers("onset")?;
    let mut keep: Vec<usize> = (0..log.rows.len()).filter(|&r| shown[r] > 0.0).collect();
    keep.sort_by(|&a, &b| onsets[a].total_cmp(&onsets[b]));
    let events = log.select(&keep);

    Ok(OnsetRun { raw_dir, stem, t0, hz, triggers, events, frames, info })
}

// ───────────────────────────── E-Prime ─────────────────────────────

/// Text as UTF-8 (with or without a byte-order mark) if that works and has
/// tabs, otherwise as UTF-16 (E-DataAid's "Unicode" export).
fn decode_text(raw: &[u8]) -> Option<String> {
    let mut text = None;
    if let Ok(s) = std::str::from_utf8(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw)) {
        if s.contains('\t') {
            return Some(s.to_string());
        }
        text = Some(s.to_string());
    }
    if raw.len() % 2 == 0 {
        let (big_endian, body) = match raw {
            [0xFE, 0xFF, rest @ ..] => (true, rest),
            [0xFF, 0xFE, rest @ ..] => (false, rest),
            _ => (false, raw),
        };
        let units: Vec<u16> =
            body.chunks_exact(2)
                .map(|c| {
                    if big_endian {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
        if let Ok(s) = String::from_utf16(&units) {
            text = Some(s);
        }
    }
    text
}

/// Read an E-Prime (E-DataAid) tab-delimited export, UTF-8 or UTF-16.
pub fn read_eprime(path: &Path) -> Result<Tsv> {
    let raw = std::fs::read(path).with_context(|| format!("can't read {}", path.display()))?;
    let text = decode_text(&raw).with_context(|| {
        format!("{}: can't decode (expected a tab-delimited text export)", path.display())
    })?;
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    // Some exports start with a line holding just the file name.
    let skip = usize::from(lines.len() > 1 && !lines[0].contains('\t') && lines[1].contains('\t'));
    Tsv::parse(&path.display().to_string(), &lines[skip..].join("\n"), false, true)
}

/// Cells an E-Prime export uses for "no value" (plus the usual spellings).
pub fn is_missing(cell: &str) -> bool {
    const MISSING: [&str; 19] = [
        "", "#N/A", "#N/A N/A", "#NA", "-1.#IND", "-1.#QNAN", "-NaN", "-nan", "1.#IND", "1.#QNAN", "<NA>",
        "N/A", "NA", "NULL", "NaN", "None", "n/a", "nan", "null",
    ];
    MISSING.contains(&cell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_pairing_and_filter() {
        let (_, off) = pair_pulses(&[1.0, 2.0, 3.0], &[0.5, 1.05, 2.0001]);
        assert_eq!(off[..2], [1.05, 2.0001]);
        assert!(off[2].is_nan());
        let p = make_probe(&[], &[1.0, 2.0, 3.0], &[0.5, 1.05, 2.0001], 0.0005);
        assert_eq!(p.on, [1.0, 3.0]);
        assert_eq!(p.rejected, 1);
    }

    #[test]
    fn decodes_utf8_and_utf16() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFa\tb").unwrap(), "a\tb");
        let utf16: Vec<u8> =
            [0xFF, 0xFE].into_iter().chain("a\tb".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode_text(&utf16).unwrap(), "a\tb");
        let be: Vec<u8> =
            [0xFE, 0xFF].into_iter().chain("x\ty".encode_utf16().flat_map(u16::to_be_bytes)).collect();
        assert_eq!(decode_text(&be).unwrap(), "x\ty");
    }

    #[test]
    fn parses_tables() {
        let t = Tsv::parse("x", "# comment\na\tb\n\n1\tn/a\n2\n", true, false).unwrap();
        assert_eq!(t.columns, ["a", "b"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.get(1, 1), "");
        let b = t.numbers("b").unwrap();
        assert!(b[0].is_nan() && b[1].is_nan());
        assert!(Tsv::parse("x", "a\nzz\n", false, false).unwrap().numbers("a").is_err());
    }
}
