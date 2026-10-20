//! The comparisons: logged onsets vs photodiode light onsets, and the
//! flash intervals of `onset timing-test`.

use super::align::{align_triggers, argsort, choose_alignment, match_onsets, slope_from_tr, sorted};
use super::logs::{OnsetRun, Probe, Tsv, is_missing};
use super::report::{Column, Report, Summary, Table};
use super::stats::{fit_line, latency_stats, mean, median, not_nan, std};
use anyhow::{Result, bail};

/// What [`compare_onsets`] needs. Times called `*_ref` are on the stimulus
/// computer's clock (Onset's run clock, or E-Prime's), in seconds.
pub struct Comparison<'a> {
    pub probe: &'a Probe,
    /// Triggers the stimulus program logged.
    pub ref_triggers: &'a [f64],
    /// When the program received the first kept trigger.
    pub t0_ref: f64,
    /// Logged onsets.
    pub expected_ref: &'a [f64],
    /// Planned onsets (s after t0), if the log has them.
    pub planned_rel: Option<&'a [f64]>,
    /// Columns to copy into the report, one row per onset.
    pub labels: Table,
    /// Refresh period (default 1/60 s).
    pub period: Option<f64>,
    /// The rig's photodiode flash length, to spot dropped frames.
    pub flash_frames: Option<u32>,
    /// How early light may come (default: one frame before the logged flip).
    pub before: Option<f64>,
    /// How late light may come.
    pub after: f64,
    pub trigger_tol: f64,
    pub slope_hint: Option<f64>,
    /// Force which Arduino trigger (index into its sorted triggers) is the
    /// program's first trigger.
    pub forced_t0_index: Option<usize>,
    pub title: String,
}

/// Line up the clocks via triggers and match onsets to light edges.
///
/// Per event this measures:
/// - `latency_ms`: photodiode onset minus logged onset, both on the stimulus
///   computer's clock. Includes the display's input lag (a constant is
///   fine); its SD is the jitter, the number that matters most.
/// - `photodiode_onset`: light onset in seconds after the first kept scanner
///   trigger, measured on the Arduino alone (trigger pin to light). This is
///   the "true" BIDS onset.
/// - `true_minus_logged_ms`: photodiode_onset - logged onset, what you would
///   add to events.tsv to make it exact. It is the latency plus the delay
///   with which the program received the first trigger.
/// - `planned_error_ms`: photodiode_onset - planned onset.
pub fn compare_onsets(c: Comparison) -> Result<Report> {
    let probe = c.probe;
    let period = c.period.filter(|p| *p > 0.0).unwrap_or(1.0 / 60.0);
    let before = c.before.unwrap_or(period);
    let after = c.after;
    let expected = c.expected_ref;
    if probe.triggers.is_empty() {
        bail!("the probe log has no triggers (T lines); can't align clocks");
    }
    if c.ref_triggers.is_empty() {
        bail!("no logged triggers to align against (was the run's trigger simulated?)");
    }
    if expected.is_empty() {
        bail!("no onsets to compare");
    }

    let ref_sorted = sorted(c.ref_triggers);
    let src_sorted = &probe.triggers;
    let i_ref0 = (0..ref_sorted.len())
        .min_by(|&a, &b| (ref_sorted[a] - c.t0_ref).abs().total_cmp(&(ref_sorted[b] - c.t0_ref).abs()))
        .unwrap_or(0);
    let mut cands = align_triggers(src_sorted, c.ref_triggers, c.trigger_tol, c.slope_hint);
    if let Some(forced) = c.forced_t0_index {
        cands.retain(|al| al.pairs.contains(&(i_ref0, forced)));
    }
    let (al, mut notes) = choose_alignment(cands, expected, &probe.on, before, after);
    let Some(al) = al else {
        bail!(
            "could not line up Arduino and logged triggers; check that both saw the same pulses \
             (--trigger-tol-ms, --trigger-source)"
        );
    };

    // The Arduino's view of t = 0: its trigger paired with the program's t0.
    let ard_t0 = match al.pairs.iter().find(|p| p.0 == i_ref0) {
        Some(&(_, s)) => src_sorted[s],
        None => {
            notes
                .push("the first kept trigger has no Arduino partner; using the fitted clock instead".into());
            (c.t0_ref - al.intercept) / al.slope
        }
    };

    let edges_ref: Vec<f64> = probe.on.iter().map(|&t| al.map(t)).collect(); // light onsets, program clock
    let m = match_onsets(expected, &edges_ref, before, after);
    let n = expected.len();
    let nan = || vec![f64::NAN; n];
    let logged: Vec<f64> = expected.iter().map(|e| e - c.t0_ref).collect();
    let (mut photo, mut lat, mut width) = (nan(), nan(), nan());
    for (i, j) in m.iter().enumerate() {
        if let Some(j) = *j {
            photo[i] = al.slope * (probe.on[j] - ard_t0); // slope: Arduino s -> program s
            lat[i] = (edges_ref[j] - expected[i]) * 1000.0;
            width[i] = probe.width(j) * al.slope * 1000.0;
        }
    }
    let true_minus_logged: Vec<f64> = photo.iter().zip(&logged).map(|(p, l)| (p - l) * 1000.0).collect();

    let mut tab = c.labels;
    tab.push("logged_onset", Column::Num(logged.clone()));
    tab.push("photodiode_onset", Column::Num(photo.clone()));
    tab.push("latency_ms", Column::Num(lat.clone()));
    tab.push("true_minus_logged_ms", Column::Num(true_minus_logged.clone()));
    let planned_error: Option<Vec<f64>> = c.planned_rel.map(|planned| {
        tab.push("planned_onset", Column::Num(planned.to_vec()));
        let err: Vec<f64> = photo.iter().zip(planned).map(|(p, q)| (p - q) * 1000.0).collect();
        tab.push("planned_error_ms", Column::Num(err.clone()));
        err
    });
    tab.push("flash_ms", Column::Num(width.clone()));

    // A flash longer than flash_frames means a frame was repeated (a dropped
    // frame) while the patch was white. Two events whose flashes touch merge
    // into one pulse; skip those.
    let mut status: Vec<&str> = m.iter().map(|j| if j.is_some() { "ok" } else { "unmatched" }).collect();
    let flash_frames = c.flash_frames.filter(|&f| f > 0);
    let mut extra_frames = None;
    if let Some(ff) = flash_frames {
        let ff = ff as f64;
        let order = argsort(expected);
        let (mut gap_prev, mut gap_next) = (vec![f64::INFINITY; n], vec![f64::INFINITY; n]);
        for k in 1..n {
            let gap = expected[order[k]] - expected[order[k - 1]];
            gap_prev[order[k]] = gap;
            gap_next[order[k - 1]] = gap;
        }
        let touching = (ff + 0.5) * period;
        let mut extra = nan();
        for i in 0..n {
            if m[i].is_none() && gap_prev[i] < touching {
                status[i] = "merged";
            }
            if gap_next[i] >= touching {
                extra[i] = (width[i] / 1000.0 / period).round_ties_even() - ff;
            }
        }
        tab.push("extra_frames", Column::Count(extra.clone()));
        extra_frames = Some(extra);
    }
    let matched: Vec<f64> = not_nan(&lat);
    let med = if matched.is_empty() { f64::NAN } else { median(&matched) };
    let half_frame_ms = 0.5 * period * 1000.0;
    let (mut late, mut early) = (0, 0);
    for i in 0..n {
        if m[i].is_some() && lat[i] > med + half_frame_ms {
            status[i] = "late_frame";
            late += 1;
        } else if m[i].is_some() && lat[i] < med - half_frame_ms {
            status[i] = "early_frame";
            early += 1;
        }
    }
    let count = |s: &str| status.iter().filter(|x| **x == s).count();
    tab.push("status", Column::Text(status.iter().map(|s| s.to_string()).collect()));

    // Light edges inside the analysed span that no onset claimed.
    let lo = expected.iter().copied().fold(f64::INFINITY, f64::min) - before;
    let hi = expected.iter().copied().fold(f64::NEG_INFINITY, f64::max) + after;
    let mut used = vec![false; edges_ref.len()];
    for j in m.iter().flatten() {
        used[*j] = true;
    }
    let extra_edges =
        (0..edges_ref.len()).filter(|&j| !used[j] && edges_ref[j] >= lo && edges_ref[j] <= hi).count();

    let mut s = Summary::default();
    s.count("events", n);
    s.count("matched", m.iter().flatten().count());
    s.count("unmatched", count("unmatched"));
    s.count("merged_flashes", count("merged"));
    s.count("extra_edges", extra_edges);
    s.count("rejected_short_pulses", probe.rejected);
    s.num("frame_period_ms", period * 1000.0);
    if let Some(st) = latency_stats(&lat, Some(&logged)) {
        s.num("latency_mean_ms", st.mean);
        s.num("latency_sd_ms", st.sd);
        s.num("latency_median_ms", st.median);
        s.num("latency_min_ms", st.min);
        s.num("latency_max_ms", st.max);
        s.num("latency_p05_ms", st.p05);
        s.num("latency_p95_ms", st.p95);
        s.num("latency_max_abs_dev_ms", st.max_abs_dev);
        if let Some((per_min, detrended_sd)) = st.trend {
            s.num("latency_trend_ms_per_min", per_min);
            s.num("latency_detrended_sd_ms", detrended_sd);
        }
    }
    s.count("late_by_a_frame", late);
    s.count("early_by_a_frame", early);
    let tml = latency_stats(&true_minus_logged, None);
    s.num("true_minus_logged_mean_ms", tml.as_ref().map_or(f64::NAN, |t| t.mean));
    s.num("true_minus_logged_sd_ms", tml.as_ref().map_or(f64::NAN, |t| t.sd));
    // How much later than average the program received the t = 0 trigger
    // (true_minus_logged = latency + this).
    s.num("t0_trigger_residual_ms", (c.t0_ref - al.map(ard_t0)) * 1000.0);
    if let Some(err) = &planned_error {
        let pe = latency_stats(err, None);
        s.num("planned_error_mean_ms", pe.as_ref().map_or(f64::NAN, |p| p.mean));
        s.num("planned_error_sd_ms", pe.as_ref().map_or(f64::NAN, |p| p.sd));
        let abs = not_nan(&err.iter().map(|e| e.abs()).collect::<Vec<_>>());
        let max_abs = if abs.is_empty() { f64::NAN } else { abs.iter().copied().fold(0.0, f64::max) };
        s.num("planned_error_max_abs_ms", max_abs);
    }
    if let Some(extra) = &extra_frames {
        let dropped: f64 = not_nan(extra).iter().map(|e| e.max(0.0)).sum();
        s.count("inferred_dropped_frames", dropped as usize);
    }
    s.count("triggers_arduino", probe.triggers.len());
    s.count("triggers_logged", c.ref_triggers.len());
    s.count("triggers_paired", al.n());
    s.num("clock_drift_ppm", al.drift_ppm());
    s.num("trigger_residual_sd_ms", if al.n() > 1 { std(&al.residuals, 0) * 1000.0 } else { f64::NAN });
    let max_res = al.residuals.iter().map(|r| r.abs()).fold(0.0, f64::max);
    s.num("trigger_residual_max_ms", if al.n() > 0 { max_res * 1000.0 } else { f64::NAN });
    Ok(Report { title: c.title, table: tab, summary: s, notes })
}

/// Settings for [`analyze_onset_run`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Refresh rate (default: the run's measured rate).
    pub hz: Option<f64>,
    pub flash_frames: u32,
    pub max_latency_s: f64,
    pub trigger_tol: f64,
    /// Match the patch's 0 -> 1 transitions in frames.tsv instead of the
    /// eventlog's onsets.
    pub use_frames: bool,
}

impl Default for RunOptions {
    fn default() -> RunOptions {
        RunOptions { hz: None, flash_frames: 3, max_latency_s: 0.100, trigger_tol: 0.020, use_frames: false }
    }
}

/// Copy these columns of `t` (those it has) into a report table.
fn label_columns(t: &Tsv, names: &[&str]) -> Table {
    let mut tab = Table::default();
    for name in names {
        if let Some(col) = t.column(name) {
            tab.push(name, Column::Text((0..t.rows.len()).map(|r| t.get(r, col).to_string()).collect()));
        }
    }
    tab
}

/// An Onset run vs the photodiode.
pub fn analyze_onset_run(run: &OnsetRun, probe: &Probe, opts: &RunOptions) -> Result<Report> {
    let hz = opts.hz.filter(|h| *h > 0.0).or(run.hz.filter(|h| *h > 0.0));
    let period = hz.map(|h| 1.0 / h);
    let mut rep = if opts.use_frames {
        let Some(frames) = &run.frames else { bail!("no frames.tsv for this run") };
        let lit: Vec<bool> = frames.numbers("photodiode")?.iter().map(|v| *v == 1.0).collect();
        let rises: Vec<usize> = (0..lit.len()).filter(|&i| lit[i] && (i == 0 || !lit[i - 1])).collect();
        let times = frames.numbers("time")?;
        let expected: Vec<f64> = rises.iter().map(|&i| times[i]).collect();
        let mut rep = compare_onsets(Comparison {
            probe,
            ref_triggers: &run.triggers,
            t0_ref: run.t0,
            expected_ref: &expected,
            planned_rel: None,
            labels: label_columns(&frames.select(&rises), &["frame", "phase", "event"]),
            period,
            flash_frames: None,
            before: period,
            after: opts.max_latency_s,
            trigger_tol: opts.trigger_tol,
            slope_hint: None,
            forced_t0_index: None,
            title: format!("{}: patch transitions in frames.tsv vs photodiode", run.stem),
        })?;
        rep.notes.push(
            "frames.tsv 'time' is when the frame was acquired, not flipped: latency includes that offset"
                .into(),
        );
        rep
    } else {
        let ev = &run.events;
        let expected: Vec<f64> = ev.numbers("onset")?.iter().map(|o| o + run.t0).collect();
        let planned =
            if ev.column("planned_onset").is_some() { Some(ev.numbers("planned_onset")?) } else { None };
        compare_onsets(Comparison {
            probe,
            ref_triggers: &run.triggers,
            t0_ref: run.t0,
            expected_ref: &expected,
            planned_rel: planned.as_deref(),
            labels: label_columns(ev, &["trial", "event", "trial_type", "stim", "frames", "logged"]),
            period,
            flash_frames: Some(opts.flash_frames),
            before: period,
            after: opts.max_latency_s,
            trigger_tol: opts.trigger_tol,
            slope_hint: None,
            forced_t0_index: None,
            title: format!("{}: logged onsets vs photodiode", run.stem),
        })?
    };
    if let Some(col) = run.frames.as_ref().and_then(|f| f.column("missed").map(|c| (f, c))) {
        let missed: f64 = not_nan(&col.0.numbers_or_nan(col.1)).iter().sum();
        rep.summary.count("onset_reported_missed_frames", missed as usize);
    } else if let Some(missed) = run.info.get("missed_frames").and_then(serde_json::Value::as_f64) {
        rep.summary.count("onset_reported_missed_frames", missed as usize);
    }
    if hz.is_none() {
        rep.notes.push("refresh rate unknown; assumed 60 Hz (pass --hz)".into());
    }
    Ok(rep)
}

/// `onset timing-test`: the patch is white on even frames, black on odd.
///
/// Ideally there is a rising edge every 2 frames and each flash lasts 1
/// frame. A frame shown twice (a dropped frame) makes one interval 3 frames
/// long. Only the light is analysed.
pub fn analyze_timing_test(probe: &Probe, hz: Option<f64>) -> Result<Report> {
    let order = argsort(&probe.on);
    let on: Vec<f64> = order.iter().map(|&i| probe.on[i]).collect();
    if on.len() < 3 {
        bail!("fewer than 3 light flashes in the probe log");
    }
    let mut notes = Vec::new();
    let iv: Vec<f64> = on.windows(2).map(|w| w[1] - w[0]).collect();
    let period = match hz.filter(|h| *h > 0.0) {
        Some(h) => 1.0 / h,
        None => {
            notes.push(
                "frame period estimated from the median flash interval / 2 (pass --hz to fix it)".into(),
            );
            median(&iv) / 2.0
        }
    };
    let nfr: Vec<f64> = iv.iter().map(|i| (i / period).round_ties_even()).collect();
    // Refine the period by regressing flash times on their frame numbers;
    // this averages all the jitter away. (In Arduino seconds: an Uno's clock
    // can be off by up to ~0.5%, so treat the Hz as approximate.)
    let mut frame_no = vec![0.0];
    for f in &nfr {
        frame_no.push(frame_no[frame_no.len() - 1] + f);
    }
    let (fitted_period, _) = fit_line(&frame_no, &on);
    let dev_ms: Vec<f64> = iv.iter().zip(&nfr).map(|(i, f)| (i - f * fitted_period) * 1000.0).collect();
    let widths: Vec<f64> = order[..order.len() - 1].iter().map(|&i| probe.width(i) * 1000.0).collect();

    let mut table = Table::default();
    table.push("flash", Column::Count((1..=iv.len()).map(|k| k as f64).collect()));
    table.push("t_s", Column::Num(on[..on.len() - 1].iter().map(|t| t - on[0]).collect()));
    table.push("interval_ms", Column::Num(iv.iter().map(|i| i * 1000.0).collect()));
    table.push("frames", Column::Count(nfr.clone()));
    table.push("deviation_ms", Column::Num(dev_ms.clone()));
    table.push("flash_ms", Column::Num(widths.clone()));

    let regular: Vec<f64> =
        iv.iter().zip(&nfr).filter(|(_, f)| **f == 2.0).map(|(i, _)| i * 1000.0).collect();
    let lit = not_nan(&widths);
    let mut s = Summary::default();
    s.count("flashes", on.len());
    s.num("duration_s", on[on.len() - 1] - on[0]);
    s.num("refresh_hz_arduino_clock", 1.0 / fitted_period);
    s.count("intervals_2_frames", regular.len());
    s.count("intervals_longer", nfr.iter().filter(|f| **f > 2.0).count());
    s.count("intervals_shorter", nfr.iter().filter(|f| **f < 2.0).count());
    s.count("inferred_dropped_frames", nfr.iter().map(|f| (f - 2.0).max(0.0)).sum::<f64>() as usize);
    s.num("interval_sd_ms", if regular.len() > 1 { std(&regular, 1) } else { f64::NAN });
    s.num("interval_dev_max_abs_ms", dev_ms.iter().map(|d| d.abs()).fold(0.0, f64::max));
    s.num("flash_mean_ms", mean(&lit));
    s.num("flash_sd_ms", std(&lit, 1));
    s.count("rejected_short_pulses", probe.rejected);
    Ok(Report { title: "timing-test: photodiode flash intervals".into(), table, summary: s, notes })
}

/// Settings for [`analyze_eprime`].
#[derive(Debug, Clone)]
pub struct EprimeOptions {
    /// OnsetTime columns (ms).
    pub onset_cols: Vec<String>,
    /// Column with the time E-Prime registered the scanner trigger (ms),
    /// e.g. WaitForScanner.RTTime.
    pub trigger_col: String,
    pub label_col: Option<String>,
    pub hz: Option<f64>,
    /// Scanner TR (s): calibrates the Arduino's clock when the export has
    /// only one trigger.
    pub tr: Option<f64>,
    pub flash_frames: Option<u32>,
    pub max_latency_s: f64,
    pub trigger_tol: f64,
    /// Index of the Arduino trigger E-Prime started on (forces the alignment).
    pub trigger_index: Option<usize>,
}

/// An E-Prime export vs the photodiode, aligned on the scanner trigger.
///
/// E-Prime times are milliseconds on its own clock. An export usually holds
/// only ONE trigger, so the clock rate can't be fitted from triggers; with
/// `tr` the Arduino's clock is calibrated against the scanner's TR instead
/// (the stimulus computer's clock is taken as accurate).
pub fn analyze_eprime(df: &Tsv, probe: &Probe, o: &EprimeOptions) -> Result<Report> {
    let label_col = o.label_col.as_deref();
    for name in o.onset_cols.iter().map(String::as_str).chain([o.trigger_col.as_str()]).chain(label_col) {
        if df.column(name).is_none() {
            bail!("E-Prime export has no column {name:?}; columns are: {}", df.columns.join(", "));
        }
    }
    let numbers = |name: &str| df.numbers_or_nan(df.column(name).unwrap_or_default());
    let mut trig: Vec<f64> = not_nan(&numbers(&o.trigger_col)).iter().map(|t| t / 1000.0).collect();
    trig.sort_by(f64::total_cmp);
    trig.dedup();
    if trig.is_empty() {
        bail!("column {:?} has no numbers", o.trigger_col);
    }
    // One row per onset: (export row, column, label, time in s).
    let mut onsets: Vec<(usize, &str, String, f64)> = Vec::new();
    for col in &o.onset_cols {
        let label = label_col.and_then(|l| df.column(l));
        for (r, v) in numbers(col).iter().enumerate() {
            if !v.is_nan() {
                let text = label.map(|l| df.get(r, l)).filter(|t| !is_missing(t)).unwrap_or("n/a");
                onsets.push((
                    r + 1,
                    col,
                    if label.is_some() { text.into() } else { String::new() },
                    v / 1000.0,
                ));
            }
        }
    }
    if onsets.is_empty() {
        bail!("no onset times found in the E-Prime columns");
    }
    onsets.sort_by(|a, b| a.3.total_cmp(&b.3));

    let mut slope_hint = None;
    let mut notes = Vec::new();
    if trig.len() == 1 {
        if let Some(tr) = o.tr {
            slope_hint = slope_from_tr(&probe.triggers, tr);
            if let Some(slope) = slope_hint {
                notes.push(format!(
                    "Arduino clock calibrated against TR={tr:?}s: {:+.0} ppm",
                    (slope - 1.0) * 1e6
                ));
            }
        }
        if slope_hint.is_none() {
            notes.push(
                "one E-Prime trigger and no --tr: assuming Arduino and PC clocks run at the same rate".into(),
            );
        }
    }
    let mut labels = Table::default();
    labels.push("row", Column::Count(onsets.iter().map(|x| x.0 as f64).collect()));
    labels.push("column", Column::Text(onsets.iter().map(|x| x.1.to_string()).collect()));
    labels.push("label", Column::Text(onsets.iter().map(|x| x.2.clone()).collect()));
    let expected: Vec<f64> = onsets.iter().map(|x| x.3).collect();
    let period = o.hz.filter(|h| *h > 0.0).map(|h| 1.0 / h);
    let mut rep = compare_onsets(Comparison {
        probe,
        ref_triggers: &trig,
        t0_ref: trig[0],
        expected_ref: &expected,
        planned_rel: None,
        labels,
        period,
        flash_frames: o.flash_frames,
        before: period,
        after: o.max_latency_s,
        trigger_tol: o.trigger_tol,
        slope_hint,
        forced_t0_index: o.trigger_index,
        title: "E-Prime: OnsetTime vs photodiode".into(),
    })?;
    notes.append(&mut rep.notes);
    rep.notes = notes;
    if trig.len() == 1 {
        rep.summary.num("clock_drift_ppm", slope_hint.map_or(f64::NAN, |s| (s - 1.0) * 1e6));
    }
    Ok(rep)
}
