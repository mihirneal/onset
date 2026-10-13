//! Compiling an experiment into a timeline of planned events.
//!
//! Every onset is an absolute time in seconds after t = 0 (the first kept
//! scanner trigger). Nothing at runtime ever accumulates durations, so a
//! late frame can't push later events off schedule.

use crate::experiment::{Experiment, NumOrTemplate, Order};
use crate::rng::Rng;
use crate::scene;
use crate::template::{RUNTIME_VARS, Vars, fill, references};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Timeline {
    pub trials: Vec<Trial>,
    /// All events of all trials, sorted by onset.
    pub events: Vec<PlannedEvent>,
    /// When the run ends (seconds after t = 0).
    pub end: f64,
    pub seed: u64,
    /// Schedule columns in file order (become extra events.tsv columns).
    pub columns: Vec<String>,
    /// Every image file the run can show, for preloading.
    pub images: BTreeSet<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Trial {
    pub index: usize,
    pub onset: f64,
    pub vars: Vars,
}

#[derive(Debug, Clone)]
pub struct PlannedEvent {
    pub trial: usize,
    pub name: String,
    pub onset: f64,
    pub duration: f64,
    /// Stimulus name; may still contain `{outcome}`, resolved at runtime.
    pub stimulus: String,
    pub trial_type: String,
    pub log: bool,
    pub response: Option<PlannedResponse>,
}

#[derive(Debug, Clone)]
pub struct PlannedResponse {
    pub buttons: Vec<String>,
    pub correct: Option<String>,
    /// Seconds after the event's onset during which responses count.
    pub window: f64,
    pub end_on_response: bool,
    pub after_response: String,
}

/// Outcomes a response window can produce. `{outcome}` takes one of these.
pub fn possible_outcomes(has_correct: bool) -> &'static [&'static str] {
    if has_correct { &["correct", "incorrect", "miss"] } else { &["response", "miss"] }
}

/// Read the schedule rows (file or inline) as string maps, plus column order.
pub fn load_rows(exp: &Experiment) -> Result<(Vec<Vars>, Vec<String>)> {
    let s = &exp.schedule;
    if let Some(file) = &s.file {
        let path = exp.resolve_path(file);
        let delim = match path.extension().and_then(|e| e.to_str()) {
            Some("csv") => b',',
            _ => b'\t',
        };
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delim)
            .trim(csv::Trim::All)
            .from_path(&path)
            .with_context(|| format!("can't read schedule file {}", path.display()))?;
        let headers: Vec<String> = rdr.headers()?.iter().map(str::to_string).collect();
        let mut rows = Vec::new();
        for (i, rec) in rdr.records().enumerate() {
            let rec = rec.with_context(|| format!("{}: row {}", path.display(), i + 2))?;
            rows.push(headers.iter().cloned().zip(rec.iter().map(str::to_string)).collect());
        }
        Ok((rows, headers))
    } else if let Some(inline) = &s.rows {
        let mut columns: Vec<String> = Vec::new();
        let rows = inline
            .iter()
            .map(|table| {
                table
                    .iter()
                    .map(|(k, v)| {
                        if !columns.contains(k) {
                            columns.push(k.clone());
                        }
                        let v = match v {
                            toml::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        (k.clone(), v)
                    })
                    .collect()
            })
            .collect();
        Ok((rows, columns))
    } else {
        // No schedule: run the trial template once.
        Ok((vec![Vars::new()], Vec::new()))
    }
}

fn seconds(spec: &NumOrTemplate, vars: &Vars, what: &str) -> Result<f64> {
    let v = match spec {
        NumOrTemplate::Num(n) => *n,
        NumOrTemplate::Template(t) => {
            let s = fill(t, vars).map_err(|e| anyhow!("{what}: {e}"))?;
            s.trim().parse::<f64>().map_err(|_| anyhow!("{what}: {s:?} is not a number of seconds"))?
        }
    };
    if !v.is_finite() || v < 0.0 {
        bail!("{what}: {v} is not a valid duration");
    }
    Ok(v)
}

/// Compile the experiment. `seed` is used when the file doesn't fix one.
/// `refresh_hz` is only used for frame-alignment warnings.
pub fn compile(exp: &Experiment, seed: u64, refresh_hz: f64) -> Result<Timeline> {
    let (mut rows, columns) = load_rows(exp)?;
    let seed = exp.schedule.seed.unwrap_or(seed);
    let s = &exp.schedule;

    if let Some(col) = &s.onset_column {
        if !columns.contains(col) {
            bail!(
                "[schedule] onset_column = {col:?}, but the schedule has no such column (columns: {})",
                columns.join(", ")
            );
        }
    }
    let repeated: Vec<Vars> = (0..s.repeat).flat_map(|_| rows.iter().cloned()).collect();
    rows = repeated;
    if s.order == Order::Shuffle {
        Rng::new(seed).shuffle(&mut rows);
    }

    let mut warnings = Vec::new();
    let mut trials = Vec::new();
    let mut events = Vec::new();
    let mut images = BTreeSet::new();
    let mut cursor = s.start_delay;
    let mut prev_end = 0.0_f64;
    let frame = 1.0 / refresh_hz;

    for (index, mut vars) in rows.into_iter().enumerate() {
        let n = index + 1;
        vars.insert("trial".into(), n.to_string());
        let onset = match &s.onset_column {
            Some(col) => {
                let raw = &vars[col];
                let t: f64 = raw
                    .parse()
                    .map_err(|_| anyhow!("trial {n}: onset {raw:?} in column {col:?} is not a number"))?;
                if t + 1e-9 < prev_end {
                    bail!(
                        "trial {n} starts at {t:.3} s, but trial {} doesn't end until {prev_end:.3} s; \
                         trials can't overlap",
                        n - 1
                    );
                }
                t
            }
            None => cursor,
        };

        let mut t = onset;
        for ev in &exp.trial.events {
            let what = format!("trial {n}, event {:?}", ev.name);
            let duration = seconds(&ev.duration, &vars, &format!("{what} duration"))?;
            let trial_type_template =
                ev.trial_type.as_deref().or(exp.trial.trial_type.as_deref()).unwrap_or(&ev.name);
            let trial_type =
                fill(trial_type_template, &vars).map_err(|e| anyhow!("{what} trial_type: {e}"))?;

            let response = match &ev.response {
                None => None,
                Some(r) => {
                    for b in &r.buttons {
                        if b.contains('{') {
                            bail!("{what}: response buttons can't be templates ({b:?})");
                        }
                    }
                    let correct = match &r.correct {
                        Some(c) => Some(fill(c, &vars).map_err(|e| anyhow!("{what} correct: {e}"))?),
                        None => None,
                    };
                    if let Some(c) = &correct {
                        if !r.buttons.contains(c) {
                            bail!(
                                "{what}: correct response {c:?} isn't one of the allowed buttons {:?}",
                                r.buttons
                            );
                        }
                    }
                    let window = match &r.window {
                        Some(w) => seconds(w, &vars, &format!("{what} response window"))?,
                        None => duration,
                    };
                    Some(PlannedResponse {
                        buttons: r.buttons.clone(),
                        correct,
                        window,
                        end_on_response: r.end_on_response,
                        after_response: r
                            .after_response
                            .clone()
                            .unwrap_or_else(|| exp.experiment.baseline.clone()),
                    })
                }
            };

            // Resolve the stimulus name now, except runtime variables like {outcome}.
            let mut stim_vars = vars.clone();
            for rv in RUNTIME_VARS {
                stim_vars.insert(rv.to_string(), format!("{{{rv}}}"));
            }
            let stimulus = fill(&ev.stimulus, &stim_vars).map_err(|e| anyhow!("{what} stimulus: {e}"))?;

            // Check every stimulus this event could show actually builds.
            let mut candidates = vec![stimulus.clone()];
            if references(&stimulus).iter().any(|r| r == "outcome") {
                let has_correct =
                    exp.trial.events.iter().any(|e| e.response.as_ref().is_some_and(|r| r.correct.is_some()));
                candidates =
                    possible_outcomes(has_correct).iter().map(|o| stimulus.replace("{outcome}", o)).collect();
            }
            if let Some(r) = &response {
                candidates.push(r.after_response.clone());
            }
            for name in &candidates {
                let scene = scene::build(exp, name, &vars).map_err(|e| anyhow!("{what}: {e}"))?;
                for item in scene.items {
                    if let scene::Item::Image { path, .. } = item {
                        if !path.exists() {
                            bail!("{what}: image file {} doesn't exist", path.display());
                        }
                        images.insert(path);
                    }
                }
            }

            let frames = duration / frame;
            if duration > 0.0 && (frames - frames.round()).abs() > 0.05 {
                warnings.push(format!(
                    "{what}: {:.1} ms is {frames:.2} frames at {refresh_hz:.0} Hz; it will be rounded to {} frames",
                    duration * 1000.0,
                    frames.round()
                ));
            }

            events.push(PlannedEvent {
                trial: n,
                name: ev.name.clone(),
                onset: t,
                duration,
                stimulus,
                trial_type,
                log: ev.log,
                response,
            });
            t += duration;
        }
        prev_end = t;
        cursor = t;
        trials.push(Trial { index: n, onset, vars });
    }

    events.sort_by(|a, b| a.onset.total_cmp(&b.onset));
    let mut end = events
        .iter()
        .map(|e| e.onset + e.duration.max(e.response.as_ref().map_or(0.0, |r| r.window)))
        .fold(0.0, f64::max);

    if let (Some(tr), Some(vols)) = (exp.scanner.tr, exp.scanner.volumes) {
        let scan_end = tr * vols as f64;
        if end > scan_end + 1e-9 {
            warnings.push(format!(
                "the last event ends at {end:.2} s, after the scan ends ({vols} volumes × {tr} s = {scan_end:.2} s)"
            ));
        }
        end = end.max(scan_end);
    }

    // Deduplicate warnings that repeat for every trial.
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for w in warnings {
        let key = w.split_once(": ").map(|(_, rest)| rest.to_string()).unwrap_or(w.clone());
        if seen.insert(key) {
            unique.push(w);
        }
    }

    Ok(Timeline { trials, events, end, seed, columns, images, warnings: unique })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(toml_text: &str) -> Experiment {
        let mut e: Experiment = toml::from_str(toml_text).unwrap();
        e.base_dir = PathBuf::from(".");
        e
    }

    const BASE: &str = r#"
        [experiment]
        name = "test"
        task = "test"
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
    "#;

    #[test]
    fn back_to_back_trials_have_absolute_onsets() {
        let e = exp(&format!(
            r#"{BASE}
            [trial]
            [[trial.events]]
            name = "cue"
            stimulus = "fix"
            duration = 0.5
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = 1.0
            [schedule]
            start_delay = 2.0
            rows = [{{ word = "a" }}, {{ word = "b" }}, {{ word = "c" }}]
            "#
        ));
        let tl = compile(&e, 1, 60.0).unwrap();
        let onsets: Vec<f64> = tl.events.iter().map(|e| e.onset).collect();
        assert_eq!(onsets, vec![2.0, 2.5, 3.5, 4.0, 5.0, 5.5]);
        assert_eq!(tl.end, 6.5);
    }

    #[test]
    fn onset_column_and_templated_durations() {
        let e = exp(&format!(
            r#"{BASE}
            [trial]
            trial_type = "{{cond}}"
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = "{{dur}}"
            [schedule]
            onset_column = "onset"
            rows = [
              {{ onset = 0.0, dur = 1.0, word = "x", cond = "a" }},
              {{ onset = 4.0, dur = 2.0, word = "y", cond = "b" }},
            ]
            "#
        ));
        let tl = compile(&e, 1, 60.0).unwrap();
        assert_eq!(tl.events[1].onset, 4.0);
        assert_eq!(tl.events[1].duration, 2.0);
        assert_eq!(tl.events[1].trial_type, "b");
    }

    #[test]
    fn overlapping_onsets_are_rejected() {
        let e = exp(&format!(
            r#"{BASE}
            [trial]
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = 3.0
            [schedule]
            onset_column = "onset"
            rows = [{{ onset = 0.0, word = "x" }}, {{ onset = 2.0, word = "y" }}]
            "#
        ));
        let err = compile(&e, 1, 60.0).unwrap_err().to_string();
        assert!(err.contains("can't overlap"), "{err}");
    }

    #[test]
    fn feedback_outcome_stimuli_must_all_exist() {
        let e = exp(&format!(
            r#"{BASE}
            [stimuli.fb_only_one]
            type = "blank"
            [trial]
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = 1.0
            response = {{ buttons = ["1", "2"], correct = "{{answer}}" }}
            [[trial.events]]
            name = "feedback"
            stimulus = "fb_{{outcome}}"
            duration = 0.5
            [schedule]
            rows = [{{ word = "x", answer = "1" }}]
            "#
        ));
        let tl = compile(&e, 1, 60.0).unwrap();
        assert_eq!(tl.events[1].stimulus, "fb_{outcome}");

        let bad = exp(&format!(
            r#"{BASE}
            [trial]
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = 1.0
            response = {{ buttons = ["1", "2"], correct = "{{answer}}" }}
            [[trial.events]]
            name = "feedback"
            stimulus = "oops_{{outcome}}"
            duration = 0.5
            [schedule]
            rows = [{{ word = "x", answer = "1" }}]
            "#
        ));
        assert!(compile(&bad, 1, 60.0).is_err());
    }

    #[test]
    fn shuffle_is_reproducible_from_the_seed() {
        let text = format!(
            r#"{BASE}
            [trial]
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = 1.0
            [schedule]
            order = "shuffle"
            repeat = 3
            rows = [{{ word = "a" }}, {{ word = "b" }}, {{ word = "c" }}, {{ word = "d" }}]
            "#
        );
        let words = |seed| {
            compile(&exp(&text), seed, 60.0)
                .unwrap()
                .trials
                .iter()
                .map(|t| t.vars["word"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(words(5), words(5));
        assert_eq!(words(5).len(), 12);
        assert_ne!(words(5), words(6));
    }

    #[test]
    fn warns_about_durations_that_are_not_whole_frames() {
        let e = exp(&format!(
            r#"{BASE}
            [trial]
            [[trial.events]]
            name = "target"
            stimulus = "word"
            duration = 0.03
            [schedule]
            rows = [{{ word = "a" }}, {{ word = "b" }}]
            "#
        ));
        let tl = compile(&e, 1, 60.0).unwrap();
        assert_eq!(tl.warnings.len(), 1, "{:?}", tl.warnings);
    }
}
