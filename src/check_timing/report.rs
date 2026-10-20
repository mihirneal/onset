//! What a timing check produces: a per-event table, a summary, notes and
//! pass/check lines, printed to the terminal and optionally saved.

use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::path::Path;

/// Pass criteria (see docs/VALIDATION.md).
pub const PASS_JITTER_SD_MS: f64 = 1.0;
/// No single event further than this from the mean latency.
pub const PASS_MAX_ABS_DEV_MS: f64 = 4.0;

#[derive(Debug, Clone)]
pub enum Column {
    /// Text; `n/a` (or empty) means no value.
    Text(Vec<String>),
    /// Numbers; NaN means no value.
    Num(Vec<f64>),
    /// Whole numbers; NaN means no value.
    Count(Vec<f64>),
}

/// The per-event table, column by column.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub columns: Vec<(String, Column)>,
}

impl Table {
    pub fn push(&mut self, name: &str, col: Column) {
        self.columns.push((name.to_string(), col));
    }

    pub fn rows(&self) -> usize {
        match self.columns.first().map(|c| &c.1) {
            Some(Column::Text(v)) => v.len(),
            Some(Column::Num(v) | Column::Count(v)) => v.len(),
            None => 0,
        }
    }

    /// A numeric column. Panics if there is none by that name (for tests).
    pub fn num(&self, name: &str) -> &[f64] {
        match self.columns.iter().find(|c| c.0 == name).map(|c| &c.1) {
            Some(Column::Num(v) | Column::Count(v)) => v,
            _ => panic!("no numeric column {name:?}"),
        }
    }

    /// A text column. Panics if there is none by that name (for tests).
    pub fn text(&self, name: &str) -> &[String] {
        match self.columns.iter().find(|c| c.0 == name).map(|c| &c.1) {
            Some(Column::Text(v)) => v,
            _ => panic!("no text column {name:?}"),
        }
    }

    /// One cell; `None` for no value. Numbers keep full precision, or
    /// `decimals` places for the terminal.
    fn cell(&self, col: usize, row: usize, decimals: Option<usize>) -> Option<String> {
        match &self.columns[col].1 {
            Column::Text(v) => Some(v[row].clone()).filter(|s| !s.is_empty() && s != "n/a"),
            Column::Num(v) => Some(v[row]).filter(|x| !x.is_nan()).map(|x| match decimals {
                Some(d) => format!("{x:.d$}"),
                None => x.to_string(),
            }),
            // `+ 0.0` turns -0 into 0.
            Column::Count(v) => Some(v[row]).filter(|x| !x.is_nan()).map(|x| format!("{:.0}", x + 0.0)),
        }
    }

    /// The table with aligned columns and 3 decimals.
    pub fn to_text(&self) -> String {
        let cells: Vec<Vec<String>> = (0..self.rows())
            .map(|r| {
                (0..self.columns.len())
                    .map(|c| self.cell(c, r, Some(3)).unwrap_or_else(|| "n/a".into()))
                    .collect()
            })
            .collect();
        let widths: Vec<usize> = (0..self.columns.len())
            .map(|c| cells.iter().map(|row| row[c].len()).chain([self.columns[c].0.len()]).max().unwrap_or(0))
            .collect();
        let mut out = String::new();
        let line = |out: &mut String, items: Vec<&str>| {
            let parts: Vec<String> = items.iter().zip(&widths).map(|(s, w)| format!("{s:>w$}")).collect();
            out.push_str(&parts.join(" "));
            out.push('\n');
        };
        line(&mut out, self.columns.iter().map(|c| c.0.as_str()).collect());
        for row in &cells {
            line(&mut out, row.iter().map(String::as_str).collect());
        }
        out
    }

    /// Save as CSV (empty cells for no value), or as TSV with `n/a` when
    /// the file name ends in `.tsv`.
    pub fn write(&self, path: &Path) -> Result<()> {
        let tsv = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
        let mut w = csv::WriterBuilder::new()
            .delimiter(if tsv { b'\t' } else { b',' })
            .from_path(path)
            .with_context(|| format!("can't create {}", path.display()))?;
        w.write_record(self.columns.iter().map(|c| &c.0))?;
        let missing = if tsv { "n/a" } else { "" };
        for r in 0..self.rows() {
            let row: Vec<String> = (0..self.columns.len())
                .map(|c| self.cell(c, r, None).unwrap_or_else(|| missing.into()))
                .collect();
            w.write_record(&row)?;
        }
        w.flush()?;
        Ok(())
    }
}

/// Summary numbers in a fixed order. Counts print without decimals.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    entries: Vec<(String, f64, bool)>,
}

impl Summary {
    fn set(&mut self, key: &str, value: f64, count: bool) {
        match self.entries.iter_mut().find(|e| e.0 == key) {
            Some(e) => *e = (key.to_string(), value, count),
            None => self.entries.push((key.to_string(), value, count)),
        }
    }

    pub fn count(&mut self, key: &str, n: usize) {
        self.set(key, n as f64, true);
    }

    pub fn num(&mut self, key: &str, value: f64) {
        self.set(key, value, false);
    }

    pub fn get(&self, key: &str) -> Option<f64> {
        self.entries.iter().find(|e| e.0 == key).map(|e| e.1)
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

/// `summary["latency_sd_ms"]`; panics if the key is missing.
impl std::ops::Index<&str> for Summary {
    type Output = f64;
    fn index(&self, key: &str) -> &f64 {
        &self.entries.iter().find(|e| e.0 == key).unwrap_or_else(|| panic!("no summary value {key:?}")).1
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub title: String,
    pub table: Table,
    pub summary: Summary,
    pub notes: Vec<String>,
}

/// `nan` and `inf` spelled the way numpy prints them.
fn fixed3(x: f64) -> String {
    if x.is_nan() { "nan".into() } else { format!("{x:.3}") }
}

/// Pass/check lines for the criteria in docs/VALIDATION.md.
pub fn verdict(rep: &Report) -> Vec<String> {
    let s = &rep.summary;
    let mut out = Vec::new();
    let mut line =
        |ok: bool, text: String| out.push(format!("  [{}] {text}", if ok { "PASS" } else { "CHECK" }));
    if s.has("latency_sd_ms") {
        let (sd, worst) = (s["latency_sd_ms"], s["latency_max_abs_dev_ms"]);
        line(sd < PASS_JITTER_SD_MS, format!("jitter SD {sd:.3} ms < {PASS_JITTER_SD_MS:.1} ms"));
        line(
            worst < PASS_MAX_ABS_DEV_MS,
            format!("worst event {worst:.2} ms from the mean < {PASS_MAX_ABS_DEV_MS:.1} ms"),
        );
        let unmatched = s["unmatched"];
        line(unmatched == 0.0, format!("every onset produced light ({unmatched} unmatched)"));
        line(s["late_by_a_frame"] + s["early_by_a_frame"] == 0.0, "no onset a whole frame off".into());
    }
    if let Some(dev) = s.get("interval_dev_max_abs_ms") {
        line(dev < 1.0, format!("flash intervals within 1 ms of whole frames ({} ms)", fixed3(dev)));
    }
    if let Some(dropped) = s.get("inferred_dropped_frames") {
        line(dropped == 0.0, format!("no dropped frames seen by the photodiode ({dropped})"));
    }
    if let Some(missed) = s.get("onset_reported_missed_frames") {
        line(missed == 0.0, format!("Onset reported {missed} missed frames"));
    }
    out
}

/// The whole report as printed: table (optional), summary, notes, verdict.
pub fn format_report(rep: &Report, show_table: bool) -> String {
    let mut out = format!("== {} ==\n", rep.title);
    if show_table {
        out.push_str(&rep.table.to_text());
    }
    out.push_str("\nsummary:\n");
    for (key, value, count) in &rep.summary.entries {
        let value = if *count { format!("{value}") } else { fixed3(*value) };
        let _ = writeln!(out, "  {key:32} {value}");
    }
    for note in &rep.notes {
        let _ = writeln!(out, "note: {note}");
    }
    out.push('\n');
    for line in verdict(rep) {
        let _ = writeln!(out, "{line}");
    }
    out
}
