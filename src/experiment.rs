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
    /// Animated title screen behind the instructions and the wait for the
    /// scanner. Off unless the experiment has a `[welcome]` table.
    pub welcome: Option<Welcome>,
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

/// The welcome screen shown before t = 0 (`[welcome]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Welcome {
    #[serde(default)]
    pub style: WelcomeStyle,
    /// Study title; defaults to `[experiment] name`.
    pub title: Option<String>,
    /// Small line above the participant's name (`onset run --name`), or
    /// above the title when no name is given.
    #[serde(default = "default_greeting")]
    pub greeting: String,
    /// Color of the light, the halo ring and the greeting.
    #[serde(default = "default_accent")]
    pub accent: Color,
    /// Color of all text on the welcome screens, including the instructions.
    #[serde(default = "default_foreground")]
    pub color: Color,
    /// Font of the welcome screens; the instructions use it too unless
    /// `[experiment] font` is set.
    #[serde(default = "default_welcome_font")]
    pub font: String,
    /// "Powered by Onset" in the bottom-right corner.
    #[serde(default = "yes")]
    pub powered_by: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WelcomeStyle {
    /// Slowly flowing light on near-black, a thin halo ring and drifting dust.
    #[default]
    Halo,
}

fn default_greeting() -> String {
    "Welcome".into()
}
fn default_accent() -> Color {
    Color::rgb8(64, 200, 255)
}
/// The Mac's system font (SF Pro).
fn default_welcome_font() -> String {
    "System Font".into()
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
    /// How `onset make-schedule` writes `file` from a folder of images.
    pub generate: Option<GenerateDef>,
}

fn one() -> u32 {
    1
}

/// `[schedule.generate]`: a design for `onset make-schedule`, which writes
/// the schedule file from a folder of images. Running ignores it.
///
/// Trials sit in fixed slots (`start + k * slot` seconds after t = 0); some
/// slots stay blank. With `repeat_prob`, some trials show an image seen
/// before ("old") and the others unseen ones ("new").
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerateDef {
    /// Folder of PNG/JPEG images, relative to the experiment file.
    pub images: Option<String>,
    /// Onset of the first slot (s).
    #[serde(default)]
    pub start: f64,
    /// Seconds from one slot's onset to the next.
    pub slot: f64,
    /// Slots per run.
    pub slots: u32,
    /// Image trials per run (default: every slot). A list cycles through
    /// runs: blanks are placed for its largest entry, and runs with fewer
    /// trials leave their last image slots blank.
    pub trials: Option<Counts>,
    /// Blank slots split the image trials into stretches of `[min, max]`
    /// trials. Default: blanks may fall anywhere.
    pub stretch: Option<[u32; 2]>,
    /// Chance that a trial shows an image seen before.
    #[serde(default)]
    pub repeat_prob: f64,
    /// The `answer` column for unseen images.
    #[serde(default = "default_new_answer")]
    pub new_answer: String,
    /// The `answer` column for images seen before.
    #[serde(default = "default_old_answer")]
    pub old_answer: String,
    /// Copy the number that follows `prefix` in each file name into its own
    /// column, e.g. an image database's ID.
    pub name_id: Option<NameId>,
}

/// `63` or `[63, 62]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Counts {
    One(u32),
    Cycle(Vec<u32>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameId {
    pub column: String,
    pub prefix: String,
}

fn default_new_answer() -> String {
    "1".into()
}
fn default_old_answer() -> String {
    "2".into()
}

impl GenerateDef {
    fn trial_counts(&self) -> Vec<u32> {
        match &self.trials {
            None => vec![self.slots],
            Some(Counts::One(n)) => vec![*n],
            Some(Counts::Cycle(v)) => v.clone(),
        }
    }

    /// Image trials in run `run` (1-based).
    pub fn trials_in_run(&self, run: u32) -> u32 {
        let counts = self.trial_counts();
        counts[(run.max(1) as usize - 1) % counts.len()]
    }

    /// The most image trials any run has (what blanks are placed for).
    pub fn max_trials(&self) -> u32 {
        self.trial_counts().into_iter().max().unwrap_or(0)
    }

    fn check(&self) -> Result<()> {
        let at = "[schedule.generate]";
        if !(self.slot > 0.0 && self.slot.is_finite() && self.start.is_finite()) {
            bail!("{at} slot must be a positive number of seconds and start a number");
        }
        if self.slots == 0 {
            bail!("{at} slots must be at least 1");
        }
        let counts = self.trial_counts();
        if counts.is_empty() || counts.iter().any(|&n| n > self.slots) {
            bail!("{at} trials must be one or more counts, each at most slots ({})", self.slots);
        }
        if !(0.0..=1.0).contains(&self.repeat_prob) {
            bail!("{at} repeat_prob must be between 0 and 1");
        }
        if let Some([lo, hi]) = self.stretch {
            let (images, stretches) = (self.max_trials(), self.slots - self.max_trials() + 1);
            if lo == 0 || lo > hi {
                bail!("{at} stretch must be [min, max] with 1 <= min <= max");
            }
            if stretches * lo > images || stretches * hi < images {
                bail!(
                    "{at} stretch = [{lo}, {hi}] can't split {images} image trials into {stretches} stretches \
                     (one more than the {} blank slots): it needs min <= {} and max >= {}",
                    stretches - 1,
                    images / stretches,
                    images.div_ceil(stretches)
                );
            }
        }
        if let Some(id) = &self.name_id {
            if id.column.is_empty() || id.prefix.is_empty() {
                bail!("{at} name_id needs a column and a prefix");
            }
        }
        Ok(())
    }
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
        if let Some(g) = &s.generate {
            g.check()?;
        }
        Ok(())
    }

    /// Look up a stimulus by name; `blank` always exists.
    pub fn stimulus(&self, name: &str) -> Option<&StimulusDef> {
        static BLANK: StimulusDef = StimulusDef::Blank;
        if name == "blank" { Some(&BLANK) } else { self.stimuli.get(name) }
    }
}
