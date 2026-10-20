//! `onset make-schedule`: write an experiment's schedule TSV from a folder
//! of images, following its `[schedule.generate]` design.
//!
//! Each run gets the design's slot layout with blank slots placed at random,
//! and each image trial shows either an image seen before ("old", with
//! chance `repeat_prob`) or an unseen one ("new"). `--history` keeps old and
//! new honest across runs and sessions: images listed in it count as seen,
//! and each run's new images are appended to it. Use one history file per
//! participant.
//!
//! Columns written: `onset`, `stim_file`, `memory` (new/old), `answer`,
//! `image` (the file name without its extension), and the `name_id` column
//! if the design has one.

use crate::experiment::{Experiment, GenerateDef};
use crate::rng::PyRandom;
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct Args {
    /// Experiment file with a [schedule.generate] table.
    experiment: PathBuf,
    /// Run number within the session (a `trials` list cycles through runs).
    #[arg(long, default_value_t = 1)]
    run: u32,
    /// Random seed; with the run number it fixes the schedule.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Folder of PNG/JPEG images (default: the design's `images`).
    #[arg(long)]
    images: Option<PathBuf>,
    /// Per-participant list of images already shown (read, then appended).
    #[arg(long)]
    history: Option<PathBuf>,
    /// Output TSV (default: the experiment's [schedule] file).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Chance a trial repeats an earlier image (default: the design's `repeat_prob`).
    #[arg(long)]
    repeat_prob: Option<f64>,
}

/// One image trial.
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    pub slot: u32,
    /// Index into the image pool.
    pub image: usize,
    pub old: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub trials: Vec<Trial>,
    /// Images shown for the first time, in order (pool indices).
    pub new_images: Vec<usize>,
}

/// Which slots stay blank, for the design's largest trial count.
fn blank_slots(design: &GenerateDef, rng: &mut PyRandom) -> Result<Vec<bool>> {
    let slots = design.slots as usize;
    let images = design.max_trials();
    let blanks = slots - images as usize;
    let mut blank = vec![false; slots];
    match design.stretch {
        Some([lo, hi]) => {
            // Draw stretch lengths until they add up exactly.
            let lengths = (0..1_000_000)
                .find_map(|_| {
                    let lengths: Vec<u32> =
                        (0..=blanks).map(|_| lo + rng.below((hi - lo + 1) as u64) as u32).collect();
                    (lengths.iter().sum::<u32>() == images).then_some(lengths)
                })
                .context("[schedule.generate] stretch: couldn't place the blank slots; widen the range")?;
            let mut pos = 0;
            for length in &lengths[..blanks] {
                pos += *length as usize;
                blank[pos] = true;
                pos += 1;
            }
        }
        None => {
            let mut order: Vec<usize> = (0..slots).collect();
            rng.shuffle(&mut order);
            for &s in &order[..blanks] {
                blank[s] = true;
            }
        }
    }
    Ok(blank)
}

/// Plan run `run`'s trials. `stems` names the image pool (file names
/// without extensions); `history` lists stems shown in earlier runs.
pub fn plan(
    design: &GenerateDef,
    run: u32,
    repeat_prob: f64,
    stems: &[String],
    history: &[String],
    rng: &mut PyRandom,
) -> Result<Plan> {
    let blank = blank_slots(design, rng)?;
    let this_run = design.trials_in_run(run);

    let by_stem: HashMap<&str, usize> = stems.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let shown: HashSet<&str> = history.iter().map(String::as_str).collect();
    let mut seen: Vec<usize> = history.iter().filter_map(|h| by_stem.get(h.as_str()).copied()).collect();
    let mut unused: Vec<usize> = (0..stems.len()).filter(|&i| !shown.contains(stems[i].as_str())).collect();
    rng.shuffle(&mut unused);
    let first_new = seen.len();

    let mut trials = Vec::new();
    let image_slots = (0..design.slots).filter(|&s| !blank[s as usize]).take(this_run as usize);
    for slot in image_slots {
        if seen.is_empty() && unused.is_empty() {
            bail!("ran out of images: every image in the folder is already in the history");
        }
        let old = !seen.is_empty() && (rng.unit() < repeat_prob || unused.is_empty());
        let image = if old {
            seen[rng.below(seen.len() as u64) as usize]
        } else {
            let i = unused.pop().expect("checked above");
            seen.push(i);
            i
        };
        trials.push(Trial { slot, image, old });
    }
    Ok(Plan { trials, new_images: seen[first_new..].to_vec() })
}

/// PNG and JPEG files in `dir`, sorted by name (hidden files skipped).
pub fn list_images(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("can't read the image folder {}", dir.display()))?;
    let mut images: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or(".");
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            !name.starts_with('.') && matches!(ext.as_str(), "png" | "jpg" | "jpeg")
        })
        .collect();
    images.sort();
    if images.is_empty() {
        bail!("no PNG/JPEG images in {}", dir.display());
    }
    Ok(images)
}

/// The number after the first `prefix` that's followed by digits, without
/// leading zeros: `shared0001_nsd02951` with prefix `nsd` gives `2951`.
pub fn name_number(name: &str, prefix: &str) -> Option<String> {
    name.match_indices(prefix).find_map(|(at, _)| {
        let digits: String = name[at + prefix.len()..].chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let trimmed = digits.trim_start_matches('0');
        Some(if trimmed.is_empty() { "0".into() } else { trimmed.into() })
    })
}

/// Seconds with as many decimals as needed (at least one): `12.0`, `4.25`.
fn seconds(x: f64) -> String {
    let s = format!("{x:.6}");
    let s = s.trim_end_matches('0');
    if s.ends_with('.') { format!("{s}0") } else { s.to_string() }
}

fn read_history(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("can't read {}", path.display()))?;
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

fn append_history(path: &Path, stems: &[&str]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let ends_cleanly = std::fs::read(path).map_or(true, |b| b.is_empty() || b.ends_with(b"\n"));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("can't write {}", path.display()))?;
    let mut text = if ends_cleanly { String::new() } else { "\n".into() };
    for stem in stems {
        text.push_str(stem);
        text.push('\n');
    }
    f.write_all(text.as_bytes())?;
    Ok(())
}

pub fn run(args: Args) -> Result<()> {
    if args.run == 0 {
        bail!("--run starts at 1");
    }
    let exp = Experiment::load(&args.experiment)?;
    let Some(design) = &exp.schedule.generate else {
        bail!("{} has no [schedule.generate] table (see docs/SCHEMA.md)", args.experiment.display());
    };
    let repeat_prob = args.repeat_prob.unwrap_or(design.repeat_prob);
    if !(0.0..=1.0).contains(&repeat_prob) {
        bail!("--repeat-prob must be between 0 and 1");
    }
    let out = match (&args.out, &exp.schedule.file) {
        (Some(out), _) => out.clone(),
        (None, Some(file)) => exp.resolve_path(file),
        (None, None) => bail!("the experiment has no [schedule] file; pass --out"),
    };
    let folder = match (&args.images, &design.images) {
        (Some(dir), _) => dir.clone(),
        (None, Some(dir)) => exp.resolve_path(dir),
        (None, None) => bail!("no images: pass --images or set images in [schedule.generate]"),
    };
    let pool = list_images(&folder)?;
    let stems: Vec<String> =
        pool.iter().map(|p| p.file_stem().unwrap_or_default().to_string_lossy().into_owned()).collect();
    let history = match &args.history {
        Some(h) => read_history(h)?,
        None => Vec::new(),
    };

    let mut rng = PyRandom::new(args.seed.wrapping_mul(1000).wrapping_add(args.run as u64));
    let plan = plan(design, args.run, repeat_prob, &stems, &history, &mut rng)?;

    // Images inside the experiment's folder are listed relative to it (so
    // the folder can move); others by absolute path.
    let base = std::fs::canonicalize(if exp.base_dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        &exp.base_dir
    })?;
    let folder = std::fs::canonicalize(&folder)?;
    let stim_dir = folder.strip_prefix(&base).map_or(folder.clone(), Path::to_path_buf);

    let mut text = String::from("onset\tstim_file\tmemory\tanswer\timage");
    if let Some(id) = &design.name_id {
        text.push('\t');
        text.push_str(&id.column);
    }
    text.push('\n');
    for t in &plan.trials {
        let file = pool[t.image].file_name().unwrap_or_default();
        let (memory, answer) = if t.old { ("old", &design.old_answer) } else { ("new", &design.new_answer) };
        let stem = &stems[t.image];
        let onset = design.start + t.slot as f64 * design.slot;
        text.push_str(&format!(
            "{}\t{}\t{memory}\t{answer}\t{stem}",
            seconds(onset),
            stim_dir.join(file).display()
        ));
        if let Some(id) = &design.name_id {
            text.push('\t');
            text.push_str(&name_number(stem, &id.prefix).unwrap_or_else(|| "n/a".into()));
        }
        text.push('\n');
    }
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
    }
    std::fs::write(&out, text).with_context(|| format!("can't write {}", out.display()))?;
    if let Some(h) = &args.history {
        let new: Vec<&str> = plan.new_images.iter().map(|&i| stems[i].as_str()).collect();
        append_history(h, &new)?;
    }
    let n_old = plan.trials.iter().filter(|t| t.old).count();
    println!(
        "wrote {}: {} image trials ({n_old} old), {} blank slots, run {}",
        out.display(),
        plan.trials.len(),
        design.slots as usize - plan.trials.len(),
        args.run
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn design(toml: &str) -> GenerateDef {
        toml::from_str(toml).unwrap()
    }

    fn stems(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("img{i:03}")).collect()
    }

    const BLOCKED: &str =
        "slot = 4.0\nslots = 68\nstart = 12.0\ntrials = [63, 62]\nstretch = [9, 14]\nrepeat_prob = 0.3";

    #[test]
    fn blanks_split_the_images_into_stretches() {
        let d = design(BLOCKED);
        for seed in 0..50 {
            let blank = blank_slots(&d, &mut PyRandom::new(seed)).unwrap();
            assert_eq!(blank.iter().filter(|b| **b).count(), 5);
            let stretches: Vec<usize> = blank.split(|b| *b).map(<[bool]>::len).collect();
            assert_eq!(stretches.len(), 6);
            assert!(stretches.iter().all(|&n| (9..=14).contains(&n)), "{stretches:?}");
        }
    }

    #[test]
    fn odd_and_even_runs() {
        let d = design(BLOCKED);
        let odd = plan(&d, 1, 0.3, &stems(80), &[], &mut PyRandom::new(1001)).unwrap();
        let even = plan(&d, 2, 0.3, &stems(80), &[], &mut PyRandom::new(1001)).unwrap();
        assert_eq!(odd.trials.len(), 63);
        assert_eq!(even.trials.len(), 62);
        // Same seed, same layout: the even run only drops the last image.
        assert_eq!(odd.trials[..62], even.trials[..]);
        assert_eq!(odd.trials.last().unwrap().slot, 67);
    }

    #[test]
    fn old_means_seen_before_and_history_carries_over() {
        let d = design(BLOCKED);
        let pool = stems(200);
        let run1 = plan(&d, 1, 0.3, &pool, &[], &mut PyRandom::new(7)).unwrap();
        let mut seen = HashSet::new();
        for t in &run1.trials {
            assert_eq!(t.old, !seen.insert(t.image), "old exactly when shown before");
        }
        assert!(run1.trials.iter().any(|t| t.old));
        assert_eq!(run1.new_images.len(), seen.len());

        // Run 2 counts run 1's images as seen.
        let history: Vec<String> = run1.new_images.iter().map(|&i| pool[i].clone()).collect();
        let run2 = plan(&d, 2, 0.3, &pool, &history, &mut PyRandom::new(8)).unwrap();
        assert!(run2.trials.iter().any(|t| t.old && run1.new_images.contains(&t.image)));
        for t in &run2.trials {
            assert_eq!(t.old, !seen.insert(t.image));
        }
    }

    #[test]
    fn small_pools_repeat_and_empty_ones_fail() {
        let d = design("slot = 1.0\nslots = 10");
        let p = plan(&d, 1, 0.0, &stems(3), &[], &mut PyRandom::new(1)).unwrap();
        assert_eq!(p.trials.len(), 10);
        assert_eq!(p.trials.iter().filter(|t| !t.old).count(), 3, "repeats once the pool runs out");
        let p = plan(&d, 1, 0.0, &stems(3), &stems(3), &mut PyRandom::new(1)).unwrap();
        assert!(p.trials.iter().all(|t| t.old) && p.new_images.is_empty());
        assert!(plan(&d, 1, 0.0, &[], &[], &mut PyRandom::new(1)).is_err());
    }

    #[test]
    fn no_stretch_places_blanks_anywhere() {
        let d = design("slot = 2.0\nslots = 20\ntrials = 15");
        let p = plan(&d, 3, 0.0, &stems(40), &[], &mut PyRandom::new(3)).unwrap();
        assert_eq!(p.trials.len(), 15);
        assert!(p.trials.windows(2).all(|w| w[0].slot < w[1].slot));
    }

    #[test]
    fn numbers_from_names() {
        assert_eq!(name_number("shared0001_nsd02951", "nsd").as_deref(), Some("2951"));
        assert_eq!(name_number("nsdx_nsd000", "nsd").as_deref(), Some("0"));
        assert_eq!(name_number("placeholder_052", "nsd"), None);
        assert_eq!(seconds(12.0), "12.0");
        assert_eq!(seconds(4.25), "4.25");
    }
}
