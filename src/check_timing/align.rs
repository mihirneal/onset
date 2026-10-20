//! Lining up two clocks from the trigger pulses both of them saw, and
//! matching expected onsets to photodiode edges.
//!
//! The Arduino and the stimulus computer each have their own clock. Both saw
//! the same scanner trigger pulses, so we pair the pulses up and fit
//!
//! ```text
//! stimulus_clock = slope * arduino_clock + intercept
//! ```
//!
//! by least squares. `slope` absorbs the crystal mismatch between the two
//! clocks (tens to thousands of ppm), `intercept` their arbitrary zero
//! points. Pairing tries every shift of one trigger list against the other
//! (the Arduino usually also saw triggers before and after the program was
//! listening) and keeps the shifts whose trigger intervals agree. With a
//! perfectly regular TR every shift agrees, so the photodiode decides: the
//! right alignment is the one where light edges line up with logged onsets.

use super::stats::{fit_line, median, range, std};
use std::collections::HashSet;

/// `dst = slope * src + intercept`, fitted on paired triggers.
#[derive(Debug, Clone)]
pub struct Alignment {
    pub slope: f64,
    pub intercept: f64,
    /// `(dst index, src index)` of each trigger pair, both into sorted lists.
    pub pairs: Vec<(usize, usize)>,
    /// dst - mapped src (s), per pair.
    pub residuals: Vec<f64>,
}

impl Alignment {
    pub fn map(&self, src_time: f64) -> f64 {
        self.slope * src_time + self.intercept
    }

    pub fn n(&self) -> usize {
        self.pairs.len()
    }

    pub fn rms(&self) -> f64 {
        if self.pairs.is_empty() {
            return f64::INFINITY;
        }
        (self.residuals.iter().map(|r| r * r).sum::<f64>() / self.residuals.len() as f64).sqrt()
    }

    pub fn drift_ppm(&self) -> f64 {
        (self.slope - 1.0) * 1e6
    }
}

/// Reject fits whose clock rates differ by more than this (2%; even a cheap
/// ceramic resonator is within 0.5%).
const MAX_SLOPE_DEV: f64 = 0.02;

pub fn sorted(x: &[f64]) -> Vec<f64> {
    let mut v = x.to_vec();
    v.sort_by(f64::total_cmp);
    v
}

/// Index of the first element `>= x` in a sorted slice (numpy's
/// `searchsorted`, side "left").
pub fn search_left(sorted: &[f64], x: f64) -> usize {
    sorted.partition_point(|&v| v < x)
}

/// Index of the first element `> x` (side "right").
pub fn search_right(sorted: &[f64], x: f64) -> usize {
    sorted.partition_point(|&v| v <= x)
}

/// Pair every dst trigger with the nearest mapped src trigger within `tol`.
/// Each src trigger is used at most once (by the closest dst trigger).
fn nearest_pairs(src: &[f64], dst: &[f64], slope: f64, intercept: f64, tol: f64) -> Vec<(usize, usize)> {
    let mapped: Vec<f64> = src.iter().map(|s| slope * s + intercept).collect();
    let mut found = Vec::new(); // (dst index, src index, distance)
    for (di, &d) in dst.iter().enumerate() {
        let si = if src.len() > 1 {
            let right = search_left(&mapped, d).clamp(1, src.len() - 1);
            let left = right - 1;
            if (d - mapped[left]).abs() <= (d - mapped[right]).abs() { left } else { right }
        } else {
            0
        };
        let dist = (d - mapped[si]).abs();
        if dist < tol {
            found.push((di, si, dist));
        }
    }
    let mut best: Vec<Option<usize>> = vec![None; src.len()]; // src index -> position in `found`
    for (k, &(_, si, dist)) in found.iter().enumerate() {
        if best[si].is_none_or(|b| dist < found[b].2) {
            best[si] = Some(k);
        }
    }
    found.iter().enumerate().filter(|&(k, f)| best[f.1] == Some(k)).map(|(_, f)| (f.0, f.1)).collect()
}

/// Re-pair triggers by proximity under the current model, refit, repeat.
///
/// Proximity pairing copes with a trigger missing on one side in the middle
/// of a run, which pairing by order would get wrong after the gap.
fn refine(src: &[f64], dst: &[f64], mut slope: f64, mut intercept: f64, tol: f64) -> Alignment {
    const MIN_SPAN: f64 = 1.0; // s of src triggers needed to fit a slope
    let mut pairs = Vec::new();
    for _ in 0..4 {
        pairs = nearest_pairs(src, dst, slope, intercept, tol);
        let x: Vec<f64> = pairs.iter().map(|&(_, s)| src[s]).collect();
        let y: Vec<f64> = pairs.iter().map(|&(d, _)| dst[d]).collect();
        if pairs.len() >= 2 && range(&x) >= MIN_SPAN {
            (slope, intercept) = fit_line(&x, &y);
        } else if !pairs.is_empty() {
            let offsets: Vec<f64> = x.iter().zip(&y).map(|(xi, yi)| yi - slope * xi).collect();
            intercept = median(&offsets);
        }
    }
    let residuals = pairs.iter().map(|&(d, s)| dst[d] - (slope * src[s] + intercept)).collect();
    Alignment { slope, intercept, pairs, residuals }
}

/// All plausible ways to map src trigger times (Arduino) onto dst (the
/// stimulus program), best first (most pairs, then smallest RMS).
///
/// With a perfectly regular TR many candidates tie; [`choose_alignment`]
/// breaks the tie with photodiode evidence.
///
/// - `tol`: how far a paired trigger may be from the fit. It must cover the
///   trigger input path's jitter (USB keyboard or fORP: a few ms).
/// - `slope_hint`: used when one side has a single trigger (no slope can be
///   fitted), e.g. from the scanner TR (see [`slope_from_tr`]). Default 1.
pub fn align_triggers(src: &[f64], dst: &[f64], tol: f64, slope_hint: Option<f64>) -> Vec<Alignment> {
    let (src, dst) = (sorted(src), sorted(dst));
    let mut found: Vec<Alignment> = Vec::new();
    if src.is_empty() || dst.is_empty() {
        return found;
    }
    // Candidates are told apart by their trigger pairs.
    let mut seen: HashSet<Vec<(usize, usize)>> = HashSet::new();
    let mut consider = |slope: f64, intercept: f64| {
        if (slope - 1.0).abs() > MAX_SLOPE_DEV {
            return;
        }
        let al = refine(&src, &dst, slope, intercept, tol);
        if al.n() > 0 && (al.slope - 1.0).abs() <= MAX_SLOPE_DEV && seen.insert(al.pairs.clone()) {
            found.push(al);
        }
    };
    if src.len() == 1 || dst.len() == 1 {
        // One trigger on a side: every pairing is a candidate offset.
        let slope = slope_hint.unwrap_or(1.0);
        for &s in &src {
            for &d in &dst {
                consider(slope, d - slope * s);
            }
        }
    } else {
        // Pair dst[i] with src[i + shift] for every shift that overlaps.
        let (nd, ns) = (dst.len() as isize, src.len() as isize);
        for shift in -(nd - 1)..ns {
            let (i0, i1) = (0.max(-shift), nd.min(ns - shift));
            if i1 - i0 < 2 {
                continue;
            }
            let d_seg = &dst[i0 as usize..i1 as usize];
            let s_seg = &src[(i0 + shift) as usize..(i1 + shift) as usize];
            // Robust rate estimate: median ratio of consecutive intervals.
            let ratios: Vec<f64> = (1..s_seg.len())
                .filter(|&k| s_seg[k] - s_seg[k - 1] > 0.0)
                .map(|k| (d_seg[k] - d_seg[k - 1]) / (s_seg[k] - s_seg[k - 1]))
                .collect();
            if ratios.is_empty() {
                continue;
            }
            let slope = slope_hint.unwrap_or_else(|| median(&ratios));
            let offsets: Vec<f64> = s_seg.iter().zip(d_seg).map(|(s, d)| d - slope * s).collect();
            let intercept = median(&offsets);
            let inliers =
                s_seg.iter().zip(d_seg).filter(|&(s, d)| (d - (slope * s + intercept)).abs() < tol).count();
            if inliers as f64 >= (0.5 * d_seg.len() as f64).max(2.0) {
                consider(slope, intercept);
            }
        }
    }
    // Stable sort: equally good candidates keep the order they were found in.
    found.sort_by(|a, b| b.n().cmp(&a.n()).then(a.rms().total_cmp(&b.rms())));
    found
}

/// The Arduino's clock rate relative to the scanner, from its trigger train.
///
/// The scanner's TR is crystal-accurate, so regressing the Arduino's trigger
/// times on (volume number * TR) shows how fast the Arduino clock runs.
/// Returns the slope that maps Arduino seconds to true seconds.
pub fn slope_from_tr(src_triggers: &[f64], tr: f64) -> Option<f64> {
    let t = sorted(src_triggers);
    if t.len() < 3 {
        return None;
    }
    let intervals: Vec<f64> = t.windows(2).map(|w| w[1] - w[0]).collect();
    let period = median(&intervals);
    let true_t: Vec<f64> = t.iter().map(|ti| ((ti - t[0]) / period).round_ties_even() * tr).collect();
    let (a, _) = fit_line(&true_t, &t); // Arduino seconds per true second
    Some(1.0 / a)
}

/// Indices that sort `x` (stable, NaN last).
pub fn argsort(x: &[f64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..x.len()).collect();
    order.sort_by(|&a, &b| x[a].total_cmp(&x[b]));
    order
}

/// For each expected onset, the index of the first edge in
/// `[t - before, t + after]`. Each edge is used at most once and matches keep
/// their time order. `expected` needn't be sorted; `edges` must be.
pub fn match_onsets(expected: &[f64], edges: &[f64], before: f64, after: f64) -> Vec<Option<usize>> {
    let mut out = vec![None; expected.len()];
    let mut next_free = 0;
    for i in argsort(expected) {
        let t = expected[i];
        let j = search_left(edges, t - before).max(next_free);
        if j < edges.len() && edges[j] <= t + after {
            out[i] = Some(j);
            next_free = j + 1;
        }
    }
    out
}

/// Fast, approximate (onsets matched, latency SD), to rank alignments.
fn quick_match_score(expected: &[f64], edges: &[f64], before: f64, after: f64) -> (usize, f64) {
    if edges.is_empty() || expected.is_empty() {
        return (0, f64::INFINITY);
    }
    let lat: Vec<f64> = expected
        .iter()
        .filter_map(|&t| edges.get(search_left(edges, t - before)).map(|e| e - t))
        .filter(|&l| l <= after)
        .collect();
    let sd = if lat.len() > 1 { std(&lat, 0) } else { f64::INFINITY };
    (lat.len(), sd)
}

/// Pick the trigger alignment under which light edges best match onsets.
/// Returns it with notes for the report.
pub fn choose_alignment(
    cands: Vec<Alignment>,
    expected: &[f64],
    edges_src: &[f64],
    before: f64,
    after: f64,
) -> (Option<Alignment>, Vec<String>) {
    let Some(best_n) = cands.first().map(Alignment::n) else {
        return (None, vec!["no consistent trigger alignment found".into()]);
    };
    let mut scored: Vec<(usize, f64, Alignment)> = cands
        .into_iter()
        .filter(|c| c.n() as f64 >= (0.5 * best_n as f64).max(1.0))
        .map(|c| {
            let edges = sorted(&edges_src.iter().map(|&e| c.map(e)).collect::<Vec<_>>());
            let (matched, sd) = quick_match_score(expected, &edges, before, after);
            (matched, sd, c)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.2.n().cmp(&a.2.n()))
            .then(a.1.total_cmp(&b.1))
            .then(a.2.rms().total_cmp(&b.2.rms()))
    });
    let mut notes = Vec::new();
    if scored.len() > 1 {
        notes.push(format!(
            "{} trigger alignments were consistent (regular TR); picked the one matching {} onsets (next best: {})",
            scored.len(),
            scored[0].0,
            scored[1].0
        ));
        if scored[0].0 == scored[1].0 && scored[0].0 > 0 {
            notes.push("WARNING: alignment is ambiguous; check the trigger counts".into());
        }
    }
    (scored.into_iter().next().map(|s| s.2), notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_onsets_uses_each_edge_once() {
        let m = match_onsets(&[1.0, 1.001, 2.0], &[1.01, 2.01], 0.016, 0.1);
        assert_eq!(m, vec![Some(0), None, Some(1)]);
    }

    #[test]
    fn aligns_offset_drifting_clocks_with_a_gap() {
        // The Arduino saw 3 extra triggers first; one is missing on the other
        // side. Irregular intervals, so only one shift fits.
        let src: Vec<f64> = (0..50).map(|k| 5.0 + 2.0 * k as f64 + 0.1 * ((k * 7) % 5) as f64).collect();
        let mut dst: Vec<f64> = src[3..].iter().map(|s| 0.9999 * s + 40.0).collect();
        dst.remove(20);
        let best = &align_triggers(&src, &dst, 0.02, None)[0];
        assert_eq!(best.n(), 46);
        assert!((best.slope - 0.9999).abs() < 1e-9 && (best.intercept - 40.0).abs() < 1e-6);
        assert_eq!(best.pairs[0], (0, 3));
        assert_eq!(best.pairs[20], (20, 24)); // after the gap
    }

    #[test]
    fn tr_calibrates_the_slope() {
        let src: Vec<f64> = (0..20).map(|k| 1.5 * k as f64 * (1.0 + 50e-6)).collect();
        assert!((slope_from_tr(&src, 1.5).unwrap() - 1.0 / (1.0 + 50e-6)).abs() < 1e-12);
        assert_eq!(slope_from_tr(&src[..2], 1.5), None);
    }
}
