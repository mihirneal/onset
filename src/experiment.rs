//! The experiment file format (`experiment.toml`).
//!
//! An experiment is portable: it says *what* to show and *when*, never which
//! monitor, serial port, or button box to use (that lives in the rig file).
//! See `docs/SCHEMA.md` for the full reference.

use crate::color::Color;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Experiment {
    pub experiment: Meta,
    #[serde(default)]
    pub scanner: Scanner,
    #[serde(default)]
    pub stimuli: BTreeMap<String, StimulusDef>,
    pub trial: TrialDef,
    pub schedule: ScheduleDef,

    /// Directory of the experiment file; relative paths resolve against it.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub name: String,
    /// BIDS task label, e.g. "flanker" -> `..._task-flanker_...`. Letters and digits only.
    pub task: String,
    #[serde(default = "default_background")]
    pub background: Color,
    /// Default color for text and fixation crosses.
    #[serde(default = "default_foreground")]
    pub color: Color,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// Font family name; falls back to the system sans-serif.
    pub font: Option<String>,
    /// Shown before the run; the operator presses Space to continue.
    pub instructions: Option<String>,
    /// Shown while waiting for the first scanner trigger.
    #[serde(default = "default_waiting_text")]
    pub waiting_text: String,
    /// Stimulus shown whenever no event is scheduled (gaps, after the last event).
    #[serde(default = "default_baseline")]
    pub baseline: String,
}

fn default_background() -> Color {
    Color::BLACK
}
fn default_foreground() -> Color {
    Color::WHITE
}
fn default_font_size() -> f32 {
    48.0
}
fn default_waiting_text() -> String {
    "Get ready…".into()
}
fn default_baseline() -> String {
    "blank".into()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scanner {
    /// Repetition time in seconds. Used to check trigger regularity and run length.
    pub tr: Option<f64>,
    /// Triggers to skip before t = 0 (volumes you will discard).
    #[serde(default)]
    pub dummy_volumes: u32,
    /// Volumes in the run; the run lasts at least `volumes * tr` seconds.
    pub volumes: Option<u32>,
}

/// A named thing that can be drawn. Strings may contain `{column}` templates.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StimulusDef {
    Blank,
    Fixation {
        #[serde(default = "default_fixation_size")]
        size: f32,
        #[serde(default = "default_fixation_thickness")]
        thickness: f32,
        color: Option<Color>,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
    },
    Text {
        text: String,
        size: Option<f32>,
        color: Option<Color>,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
    },
    Rect {
        width: f32,
        height: f32,
        color: Option<Color>,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
    },
    Circle {
        radius: f32,
        color: Option<Color>,
        /// Ring drawn inside the edge (the radius includes it).
        #[serde(default)]
        border_width: f32,
        #[serde(default = "default_checker_b")]
        border_color: Color,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
    },
    Image {
        path: String,
        /// Displayed size in pixels; defaults to the image's own size.
        width: Option<f32>,
        height: Option<f32>,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
    },
    /// Polar (dartboard) checkerboard that reverses contrast at `reversal_hz`.
    Checkerboard {
        #[serde(default = "default_checker_radius")]
        radius: f32,
        #[serde(default = "default_checker_inner")]
        inner_radius: f32,
        #[serde(default = "default_rings")]
        rings: u32,
        #[serde(default = "default_wedges")]
        wedges: u32,
        #[serde(default = "default_reversal_hz")]
        reversal_hz: f32,
        #[serde(default = "default_checker_a")]
        color_a: Color,
        #[serde(default = "default_checker_b")]
        color_b: Color,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
    },
    /// Several stimuli drawn together, in order (later ones on top).
    Group {
        items: Vec<String>,
    },
}

fn default_fixation_size() -> f32 {
    40.0
}
fn default_fixation_thickness() -> f32 {
    4.0
}
fn default_checker_radius() -> f32 {
    500.0
}
fn default_checker_inner() -> f32 {
    12.0
}
fn default_rings() -> u32 {
    8
}
fn default_wedges() -> u32 {
    24
}
fn default_reversal_hz() -> f32 {
    8.0
}
fn default_checker_a() -> Color {
    Color::WHITE
}
fn default_checker_b() -> Color {
    Color::BLACK
}

/// What happens in every trial, as a sequence of back-to-back events.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialDef {
    /// BIDS `trial_type` for this trial's events, e.g. "{condition}".
    /// Defaults to the event name.
    pub trial_type: Option<String>,
    pub events: Vec<EventDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventDef {
    pub name: String,
    /// Name of a stimulus, may be templated: "{stim}", "feedback_{outcome}".
    pub stimulus: String,
    pub duration: NumOrTemplate,
    pub response: Option<ResponseDef>,
    /// Overrides the trial's `trial_type` for this event.
    pub trial_type: Option<String>,
    /// Write this event to the BIDS events.tsv (it's always in the raw log).
    #[serde(default = "yes")]
    pub log: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseDef {
    /// Button labels (from the rig's `[buttons]` table) that count as responses.
    pub buttons: Vec<String>,
    /// The correct button, usually a template like "{correct}".
    pub correct: Option<String>,
    /// How long after this event's onset responses are accepted. Defaults to the
    /// event's duration; may be longer to keep listening during later events.
    pub window: Option<NumOrTemplate>,
    /// Replace the stimulus with `after_response` once a response arrives.
    /// Timing of later events is unaffected (the schedule never shifts).
    #[serde(default)]
    pub end_on_response: bool,
    /// Stimulus shown after an `end_on_response` response; defaults to the baseline.
    pub after_response: Option<String>,
}

/// `0.5` or `"{iti}"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum NumOrTemplate {
    Num(f64),
    Template(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDef {
    /// TSV or CSV with one row per trial. Relative to the experiment file.
    pub file: Option<String>,
    /// Rows written inline instead of a file.
    pub rows: Option<Vec<toml::Table>>,
    /// Column holding each trial's absolute onset (seconds from t = 0),
    /// e.g. from optseq2. If absent, trials run back to back.
    pub onset_column: Option<String>,
    #[serde(default)]
    pub order: Order,
    #[serde(default = "one")]
    pub repeat: u32,
    /// Seed for shuffling; a random seed is chosen and logged if omitted.
    pub seed: Option<u64>,
    /// Delay before the first trial when trials run back to back.
    #[serde(default)]
    pub start_delay: f64,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    Sequential,
    Shuffle,
}

impl Experiment {
    pub fn load(path: &Path) -> Result<Experiment> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("can't read experiment file {}", path.display()))?;
        let mut exp: Experiment =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}:\n{e}", path.display()))?;
        exp.base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        exp.check()?;
        Ok(exp)
    }

    pub fn resolve_path(&self, p: &str) -> PathBuf {
        self.base_dir.join(p)
    }

    /// Structural checks that don't need the schedule rows.
    fn check(&self) -> Result<()> {
        let task = &self.experiment.task;
        if task.is_empty() || !task.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("[experiment] task = {task:?}: BIDS task labels may only contain letters and digits");
        }
        if self.trial.events.is_empty() {
            bail!("[trial] needs at least one [[trial.events]] entry");
        }
        for (name, def) in &self.stimuli {
            if name == "blank" {
                bail!("[stimuli.blank] is built in; pick another name");
            }
            if let StimulusDef::Group { items } = def {
                for item in items {
                    if item == name {
                        bail!("stimulus group {name:?} contains itself");
                    }
                }
            }
        }
        let s = &self.schedule;
        if s.file.is_some() && s.rows.is_some() {
            bail!("[schedule] has both `file` and `rows`; use one");
        }
        if s.onset_column.is_some() && (s.order == Order::Shuffle || s.repeat != 1) {
            bail!(
                "[schedule] onset_column gives fixed onsets, so it can't be combined with \
                 order = \"shuffle\" or repeat > 1 (shuffle the rows when you generate the file)"
            );
        }
        if s.repeat == 0 {
            bail!("[schedule] repeat must be at least 1");
        }
        Ok(())
    }

    /// Look up a stimulus by name; `blank` always exists.
    pub fn stimulus(&self, name: &str) -> Option<&StimulusDef> {
        static BLANK: StimulusDef = StimulusDef::Blank;
        if name == "blank" { Some(&BLANK) } else { self.stimuli.get(name) }
    }
}
