//! `onset check-timing`: compare logged stimulus onsets with the light onsets
//! a photodiode measured.
//!
//! Inputs: a probe recording from `onset probe` (Arduino microsecond
//! timestamps of photodiode edges and scanner triggers), plus one of
//! - an Onset run (its onset/ folder, or a `*_eventlog.tsv` path),
//! - nothing else, with `--timing-test` (the flashing of `onset timing-test`),
//! - an E-Prime tab-delimited export (`--eprime`), to run the same
//!   comparison for a paradigm presented with E-Prime.
//!
//! The two clocks are lined up on the scanner triggers both sides saw
//! ([`align`]), each logged onset is matched to a light edge, and the
//! result is a per-event table plus summary statistics ([`analyze`]).
//! See docs/VALIDATION.md for how to set up and read a measurement.

pub mod align;
pub mod analyze;
pub mod logs;
pub mod report;
pub mod stats;

pub use analyze::{EprimeOptions, RunOptions, analyze_eprime, analyze_onset_run, analyze_timing_test};
pub use logs::{OnsetRun, Probe, load_onset_run, make_probe, read_eprime, read_probe};
pub use report::{Report, format_report};

use anyhow::{Result, bail};
use std::path::PathBuf;

#[derive(clap::Args, Debug, Default)]
pub struct Args {
    /// Probe recording from `onset probe`.
    #[arg(long)]
    pub probe: PathBuf,
    /// An Onset run: its onset/ folder, or a *_eventlog.tsv or func/*_events.tsv file.
    #[arg(long)]
    pub run: Option<PathBuf>,
    /// Run stem, e.g. sub-01_task-faces_run-01 (when the --run folder holds several runs).
    #[arg(long)]
    pub stem: Option<String>,
    /// Analyse `onset timing-test` flashing (no logs needed). With --run, match
    /// the patch transitions in frames.tsv instead of the eventlog's onsets.
    #[arg(long)]
    pub timing_test: bool,
    /// Display refresh rate in Hz (default: from the run's run.json).
    #[arg(long)]
    pub hz: Option<f64>,
    /// The rig's photodiode flash_frames.
    #[arg(long, default_value_t = 3)]
    pub flash_frames: u32,
    /// How long after each onset to look for light (ms).
    #[arg(long, default_value_t = 100.0)]
    pub max_latency_ms: f64,
    /// Ignore light pulses shorter than this (ms).
    #[arg(long, default_value_t = 0.5)]
    pub min_pulse_ms: f64,
    /// Largest trigger mismatch allowed once the clocks are lined up (ms).
    #[arg(long, default_value_t = 20.0)]
    pub trigger_tol_ms: f64,
    /// Only use Onset triggers from this inputs.tsv source (keyboard, serial). Repeatable.
    #[arg(long)]
    pub trigger_source: Vec<String>,
    /// Scanner TR in seconds: calibrates the Arduino's clock when the other log has only one trigger.
    #[arg(long)]
    pub tr: Option<f64>,
    /// E-Prime tab-delimited export (from E-DataAid) to compare instead of an Onset run.
    #[arg(long, help_heading = "E-Prime export")]
    pub eprime: Option<PathBuf>,
    /// OnsetTime column (ms). Repeatable.
    #[arg(long, help_heading = "E-Prime export")]
    pub eprime_onset_col: Vec<String>,
    /// Column with the trigger time (ms), e.g. WaitForScanner.RTTime.
    #[arg(long, help_heading = "E-Prime export")]
    pub eprime_trigger_col: Option<String>,
    /// Column to show as a label, e.g. Procedure or Condition.
    #[arg(long, help_heading = "E-Prime export")]
    pub eprime_label_col: Option<String>,
    /// Force the alignment: index of the probe's trigger (T line) the export started on.
    #[arg(long, help_heading = "E-Prime export")]
    pub eprime_trigger_index: Option<usize>,
    /// Save the per-event table: CSV, or TSV if the name ends in .tsv.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Print only the summary, not the per-event table.
    #[arg(long)]
    pub no_table: bool,
}

/// Run the check the arguments ask for.
pub fn report(args: &Args) -> Result<Report> {
    let probe = read_probe(&args.probe, args.min_pulse_ms / 1000.0)?;
    let trigger_tol = args.trigger_tol_ms / 1000.0;
    let max_latency_s = args.max_latency_ms / 1000.0;
    if let Some(export) = &args.eprime {
        let (false, Some(trigger_col)) = (args.eprime_onset_col.is_empty(), &args.eprime_trigger_col) else {
            bail!("--eprime needs --eprime-onset-col and --eprime-trigger-col");
        };
        let opts = EprimeOptions {
            onset_cols: args.eprime_onset_col.clone(),
            trigger_col: trigger_col.clone(),
            label_col: args.eprime_label_col.clone(),
            hz: args.hz,
            tr: args.tr,
            flash_frames: None,
            max_latency_s,
            trigger_tol,
            trigger_index: args.eprime_trigger_index,
        };
        return analyze_eprime(&read_eprime(export)?, &probe, &opts);
    }
    if let Some(run) = &args.run {
        let run = load_onset_run(run, args.stem.as_deref(), &args.trigger_source)?;
        let opts = RunOptions {
            hz: args.hz,
            flash_frames: args.flash_frames,
            max_latency_s,
            trigger_tol,
            use_frames: args.timing_test,
        };
        return analyze_onset_run(&run, &probe, &opts);
    }
    if args.timing_test {
        return analyze_timing_test(&probe, args.hz);
    }
    bail!("give --run (an Onset run), --timing-test, or --eprime");
}

pub fn run(args: Args) -> Result<()> {
    let rep = report(&args)?;
    print!("{}", format_report(&rep, !args.no_table));
    if let Some(out) = &args.out {
        rep.table.write(out)?;
        eprintln!("\ntable written to {}", out.display());
    }
    Ok(())
}
