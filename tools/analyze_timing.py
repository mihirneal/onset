#!/usr/bin/env python3
"""Compare Onset's logged stimulus onsets with photodiode-measured light onsets.

Inputs
------
* a probe log from tools/capture_probe.py (Arduino microsecond timestamps of
  photodiode edges "P" and scanner triggers "T"), and
* one of
    - an Onset run (its onset/ folder + stem, or the path to <stem>_eventlog.tsv),
    - nothing else, with --timing-test (analyse `onset timing-test` flashing),
    - an E-Prime tab-delimited export (--eprime), to run the same comparison
      on E-Prime.

How the two clocks are lined up
-------------------------------
The Arduino and the stimulus computer each have their own clock. Both saw the
same scanner trigger pulses (the Arduino on its trigger pin, Onset in
inputs.tsv rows with kind=trigger), so we pair the pulses up and fit

    onset_clock = slope * arduino_clock + intercept

by least squares. `slope` absorbs the crystal mismatch between the two clocks
(tens to thousands of ppm), `intercept` the arbitrary zero points. Pairing is
done by trying every shift of one trigger list against the other (the Arduino
usually also saw triggers before/after Onset was listening) and keeping
shifts whose trigger intervals agree; if the TR is perfectly regular every
shift agrees, so the photodiode decides: the right alignment is the one where
light edges line up with logged onsets.

What is measured per event
--------------------------
* latency_ms         photodiode onset - Onset's logged onset, both on Onset's
                     clock. Includes the display's input lag (constant is fine);
                     its SD is the jitter — the number that matters most.
* photodiode_onset   light onset in seconds after the first kept scanner trigger,
                     measured on the Arduino alone (trigger pin -> light).
                     This is the "true" BIDS onset.
* true_minus_logged_ms   photodiode_onset - logged onset: what you would have to
                     add to events.tsv to make it exact. = latency + the delay
                     with which Onset received the first trigger.
* planned_error_ms   photodiode_onset - planned onset.

Usage
-----
    # a normal run (photodiode taped over the patch, real or desk trigger)
    python3 tools/analyze_timing.py --probe probe.tsv \\
        --run data/sub-01/onset --stem sub-01_task-faces_run-01 --plot

    # or point at the eventlog directly
    python3 tools/analyze_timing.py --probe probe.tsv \\
        --run data/sub-01/onset/sub-01_task-faces_run-01_eventlog.tsv --out timing.csv

    # `onset timing-test` (patch flips every frame; Onset writes no logs)
    python3 tools/analyze_timing.py --probe probe.tsv --timing-test --hz 60

    # E-Prime, same paradigm and photodiode patch
    python3 tools/analyze_timing.py --probe probe_eprime.tsv --eprime export.txt \\
        --eprime-onset-col Stimulus.OnsetTime --eprime-trigger-col WaitTrigger.RTTime --tr 2.0

Dependencies: numpy, pandas; matplotlib only for --plot.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np
import pandas as pd

# Pass criteria printed at the end of a report (see docs/VALIDATION.md).
PASS_JITTER_SD_MS = 1.0
PASS_MAX_ABS_DEV_MS = 4.0  # no single event further than this from the mean latency


# =============================================================================
# Probe (Arduino) log
# =============================================================================


@dataclass
class Probe:
    """Everything the Arduino recorded, in seconds on the Arduino's clock."""

    triggers: np.ndarray  # trigger rising edges
    on: np.ndarray  # light-on times of accepted pulses
    off: np.ndarray  # matching light-off times (NaN if the log ended while lit)
    rejected: int = 0  # pulses dropped as too short (noise blips)

    @property
    def width(self) -> np.ndarray:
        return self.off - self.on


def pair_pulses(rises: np.ndarray, falls: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Pair each light-on with the first light-off after it.

    The sketch always alternates on/off, but a capture may start or stop
    mid-pulse; an off with no preceding on is dropped, an on with no
    following off gets NaN.
    """
    rises = np.sort(rises)
    falls = np.sort(falls)
    off = np.full(len(rises), np.nan)
    j = np.searchsorted(falls, rises, side="right")
    nxt = np.append(rises[1:], np.inf)
    for i, k in enumerate(j):
        if k < len(falls) and falls[k] <= nxt[i]:
            off[i] = falls[k]
    return rises, off


def make_probe(triggers, rises, falls, min_pulse_s: float = 0.0005) -> Probe:
    """Build a Probe from raw edge times (seconds), dropping too-short pulses."""
    on, off = pair_pulses(np.asarray(rises, float), np.asarray(falls, float))
    keep = ~(off - on < min_pulse_s)  # NaN widths (no off yet) are kept
    return Probe(
        triggers=np.sort(np.asarray(triggers, float)),
        on=on[keep],
        off=off[keep],
        rejected=int((~keep).sum()),
    )


def read_probe(path: Path, min_pulse_s: float = 0.0005) -> Probe:
    """Read a capture_probe.py TSV (comment lines start with '#')."""
    df = pd.read_csv(path, sep="\t", comment="#", dtype={"kind": str, "level": str})
    missing = {"kind", "arduino_us"} - set(df.columns)
    if missing:
        raise ValueError(f"{path}: not a probe log (missing columns {sorted(missing)})")
    # Integer microseconds -> float seconds. float64 holds µs exactly for ~285 years.
    t = df["arduino_us"].astype("int64").to_numpy() / 1e6
    kind = df["kind"].to_numpy()
    level = df.get("level", pd.Series(["n/a"] * len(df))).astype(str).to_numpy()
    return make_probe(
        triggers=t[kind == "T"],
        rises=t[(kind == "P") & (level == "1")],
        falls=t[(kind == "P") & (level == "0")],
        min_pulse_s=min_pulse_s,
    )


# =============================================================================
# Clock alignment from shared trigger pulses
# =============================================================================


@dataclass
class Alignment:
    """dst_time = slope * src_time + intercept, fitted on paired triggers."""

    slope: float
    intercept: float
    pairs: np.ndarray  # shape (n, 2): [dst_index, src_index]
    residuals: np.ndarray  # dst - mapped src, seconds, per pair

    def map(self, src_times):
        return self.slope * np.asarray(src_times, float) + self.intercept

    @property
    def n(self) -> int:
        return len(self.pairs)

    @property
    def rms(self) -> float:
        return float(np.sqrt(np.mean(self.residuals**2))) if self.n else math.inf

    @property
    def drift_ppm(self) -> float:
        return (self.slope - 1.0) * 1e6

    def key(self) -> tuple:
        return tuple(map(tuple, self.pairs.tolist()))


def fit_line(x: np.ndarray, y: np.ndarray) -> tuple[float, float]:
    """Least-squares y = a*x + b (centred for numerical stability)."""
    xm, ym = x.mean(), y.mean()
    dx = x - xm
    denom = float(np.dot(dx, dx))
    a = float(np.dot(dx, y - ym) / denom) if denom > 0 else 1.0
    return a, float(ym - a * xm)


def _nearest_pairs(src, dst, slope, intercept, tol):
    """Pair every dst trigger with the nearest mapped src trigger within tol."""
    mapped = slope * src + intercept
    idx = np.clip(np.searchsorted(mapped, dst), 1, len(src) - 1) if len(src) > 1 else np.zeros(len(dst), int)
    if len(src) > 1:
        left = idx - 1
        use_left = np.abs(dst - mapped[left]) <= np.abs(dst - mapped[idx])
        idx = np.where(use_left, left, idx)
    d = dst - mapped[idx]
    ok = np.abs(d) < tol
    di, si = np.nonzero(ok)[0], idx[ok]
    # A src trigger may only be used once: keep the closest dst for it.
    if len(np.unique(si)) != len(si):
        order = np.argsort(np.abs(d[ok]))
        _, first = np.unique(si[order], return_index=True)
        keep = np.sort(order[first])
        di, si = di[keep], si[keep]
    return di, si


def refine(src, dst, slope, intercept, tol, min_span=1.0, iters=4) -> Alignment:
    """Re-pair triggers by proximity under the current model, refit, repeat.

    Proximity pairing copes with a trigger missing on one side in the middle
    of a run, which pure order-based pairing would get wrong after the gap.
    """
    di = si = np.zeros(0, int)
    for _ in range(iters):
        di, si = _nearest_pairs(src, dst, slope, intercept, tol)
        if len(di) >= 2 and np.ptp(src[si]) >= min_span:
            slope, intercept = fit_line(src[si], dst[di])
        elif len(di) >= 1:
            intercept = float(np.median(dst[di] - slope * src[si]))
    res = dst[di] - (slope * src[si] + intercept)
    return Alignment(slope, intercept, np.column_stack([di, si]).astype(int), res)


def align_triggers(
    src: np.ndarray,
    dst: np.ndarray,
    tol: float = 0.020,
    slope_hint: float | None = None,
    max_slope_dev: float = 0.02,
) -> list[Alignment]:
    """All plausible ways to map src trigger times (Arduino) onto dst (Onset).

    Returns distinct candidates, best first (most pairs, then smallest RMS).
    With a perfectly regular TR many candidates tie; the caller breaks the tie
    with photodiode evidence (see choose_alignment).

    tol: how far a paired trigger may be from the fit. It must cover the
         trigger input path's jitter (USB keyboard / fORP: a few ms).
    slope_hint: used when one side has a single trigger (no slope can be fit),
         e.g. from the scanner TR (see slope_from_tr). Default 1.0.
    max_slope_dev: reject fits whose clock rates differ by more than this
         (2% — even a cheap ceramic resonator is within 0.5%).
    """
    src = np.sort(np.asarray(src, float))
    dst = np.sort(np.asarray(dst, float))
    if len(src) == 0 or len(dst) == 0:
        return []
    found: dict[tuple, Alignment] = {}

    def consider(slope, intercept):
        if abs(slope - 1.0) > max_slope_dev:
            return
        al = refine(src, dst, slope, intercept, tol)
        if al.n and abs(al.slope - 1.0) <= max_slope_dev:
            found.setdefault(al.key(), al)

    if len(src) == 1 or len(dst) == 1:
        # One trigger on a side: every pairing is a candidate offset.
        slope = slope_hint if slope_hint is not None else 1.0
        for s in src:
            for d in dst:
                consider(slope, d - slope * s)
    else:
        # Pair dst[i] with src[i + shift] for every shift that overlaps.
        for shift in range(-(len(dst) - 1), len(src)):
            i0, i1 = max(0, -shift), min(len(dst), len(src) - shift)
            if i1 - i0 < 2:
                continue
            d_seg = dst[i0:i1]
            s_seg = src[i0 + shift : i1 + shift]
            # Robust rate estimate: median ratio of consecutive intervals.
            ds, dd = np.diff(s_seg), np.diff(d_seg)
            good = ds > 0
            if not good.any():
                continue
            slope = float(np.median(dd[good] / ds[good])) if slope_hint is None else slope_hint
            intercept = float(np.median(d_seg - slope * s_seg))
            inl = np.abs(d_seg - (slope * s_seg + intercept)) < tol
            if inl.sum() >= max(2, 0.5 * len(d_seg)):
                consider(slope, intercept)

    cands = sorted(found.values(), key=lambda a: (-a.n, a.rms))
    return cands


def slope_from_tr(src_triggers: np.ndarray, tr: float) -> float | None:
    """Arduino clock rate relative to the scanner, from its trigger train.

    The scanner's TR is crystal-accurate, so regressing the Arduino's trigger
    times on (volume number * TR) gives how fast the Arduino clock runs.
    Returns the slope that maps Arduino seconds -> true seconds.
    """
    t = np.sort(np.asarray(src_triggers, float))
    if len(t) < 3:
        return None
    period = np.median(np.diff(t))
    k = np.round((t - t[0]) / period)
    a, _ = fit_line(k * tr, t)  # arduino seconds per true second
    return 1.0 / a


# =============================================================================
# Matching expected onsets to photodiode edges
# =============================================================================


def match_onsets(expected: np.ndarray, edges: np.ndarray, before: float, after: float) -> np.ndarray:
    """For each expected onset, the index of the first edge in [t-before, t+after].

    Each edge is used at most once and matches keep their time order.
    Returns -1 where nothing matched. `expected` need not be sorted;
    `edges` must be.
    """
    order = np.argsort(expected, kind="stable")
    out = np.full(len(expected), -1, int)
    next_free = 0
    for i in order:
        t = expected[i]
        j = max(int(np.searchsorted(edges, t - before, side="left")), next_free)
        if j < len(edges) and edges[j] <= t + after:
            out[i] = j
            next_free = j + 1
    return out


def quick_match_score(expected, edges, before, after) -> tuple[int, float]:
    """Fast, approximate (count matched, latency SD) used to rank alignments."""
    if len(edges) == 0 or len(expected) == 0:
        return 0, math.inf
    j = np.searchsorted(edges, expected - before)
    ok = j < len(edges)
    lat = np.full(len(expected), np.nan)
    lat[ok] = edges[j[ok]] - expected[ok]
    hit = ok & (lat <= after)
    sd = float(np.std(lat[hit])) if hit.sum() > 1 else math.inf
    return int(hit.sum()), sd


def choose_alignment(
    cands: list[Alignment], expected: np.ndarray, edges_src: np.ndarray, before: float, after: float
) -> tuple[Alignment | None, list[str]]:
    """Pick the trigger alignment under which light edges best match onsets."""
    notes: list[str] = []
    if not cands:
        return None, ["no consistent trigger alignment found"]
    best_n = cands[0].n
    plausible = [c for c in cands if c.n >= max(1, 0.5 * best_n)]
    scored = []
    for c in plausible:
        m, sd = quick_match_score(expected, np.sort(c.map(edges_src)), before, after)
        scored.append((m, -c.n, sd, c.rms, c))
    scored.sort(key=lambda s: (-s[0], s[1], s[2], s[3]))
    chosen = scored[0][4]
    if len(scored) > 1:
        notes.append(
            f"{len(scored)} trigger alignments were consistent (regular TR); "
            f"picked the one matching {scored[0][0]} onsets (next best: {scored[1][0]})"
        )
        if scored[0][0] == scored[1][0] and scored[0][0] > 0:
            notes.append("WARNING: alignment is ambiguous — check the trigger counts")
    return chosen, notes


# =============================================================================
# Summary statistics
# =============================================================================


def latency_stats(lat_ms: np.ndarray, times_s: np.ndarray | None = None) -> dict:
    """Mean / SD / range / percentiles, plus the linear trend over the run."""
    lat = np.asarray(lat_ms, float)
    lat = lat[np.isfinite(lat)]
    if len(lat) == 0:
        return {"n": 0}
    s = {
        "n": len(lat),
        "mean_ms": float(lat.mean()),
        "sd_ms": float(lat.std(ddof=1)) if len(lat) > 1 else 0.0,
        "median_ms": float(np.median(lat)),
        "min_ms": float(lat.min()),
        "max_ms": float(lat.max()),
        "p05_ms": float(np.percentile(lat, 5)),
        "p95_ms": float(np.percentile(lat, 95)),
        "max_abs_dev_ms": float(np.max(np.abs(lat - lat.mean()))),
    }
    if times_s is not None and len(lat) > 2:
        t = np.asarray(times_s, float)
        t = t[np.isfinite(np.asarray(lat_ms, float))]
        if np.ptp(t) > 0:
            a, b = fit_line(t, lat)
            s["trend_ms_per_min"] = a * 60.0
            s["detrended_sd_ms"] = float(np.std(lat - (a * t + b), ddof=1))
    return s


# =============================================================================
# Onset run files
# =============================================================================


@dataclass
class OnsetRun:
    raw_dir: Path
    stem: str
    t0: float  # run-clock time of the first kept trigger
    hz: float | None
    triggers: np.ndarray  # run-clock trigger times
    events: pd.DataFrame  # eventlog rows that reached the screen (frames > 0)
    frames: pd.DataFrame | None
    info: dict = field(default_factory=dict)


def resolve_run(run: Path, stem: str | None) -> tuple[Path, str]:
    """Accept an eventlog path, a func/ events.tsv path, or a directory (+ stem)."""
    run = Path(run)
    if run.is_file():
        name = run.name
        if name.endswith("_eventlog.tsv"):
            return run.parent, name[: -len("_eventlog.tsv")]
        if name.endswith("_events.tsv"):  # BIDS func/ file: raw logs live in ../onset
            return run.parent.parent / "onset", name[: -len("_events.tsv")]
        raise ValueError(f"{run}: expected a *_eventlog.tsv or *_events.tsv file")
    if not run.is_dir():
        raise FileNotFoundError(run)
    if (run / "onset").is_dir() and not list(run.glob("*_eventlog.tsv")):
        run = run / "onset"
    if stem:
        return run, stem
    logs = sorted(run.glob("*_eventlog.tsv"))
    if len(logs) == 1:
        return run, logs[0].name[: -len("_eventlog.tsv")]
    names = "\n  ".join(p.name for p in logs) or "(none)"
    raise ValueError(f"{run}: pass --stem to choose one of these eventlogs:\n  {names}")


def _read_tsv(path: Path) -> pd.DataFrame:
    return pd.read_csv(path, sep="\t", na_values=["n/a"], keep_default_na=False)


def load_onset_run(run: Path, stem: str | None = None, trigger_sources: list[str] | None = None) -> OnsetRun:
    raw_dir, stem = resolve_run(run, stem)
    p = lambda suffix: raw_dir / f"{stem}_{suffix}"  # noqa: E731

    info = json.loads(p("run.json").read_text()) if p("run.json").exists() else {}

    inputs = _read_tsv(p("inputs.tsv"))
    trig = inputs[inputs["kind"] == "trigger"]
    if trigger_sources:
        trig = trig[trig["source"].isin(trigger_sources)]
    else:
        # The Arduino cannot see triggers Onset simulated itself.
        trig = trig[trig["source"] != "simulated"]

    frames = _read_tsv(p("frames.tsv")) if p("frames.tsv").exists() else None

    t0 = info.get("t0_run_clock")
    if t0 is None:  # older/crashed run: recover t0 from any row with both clocks
        for df in (inputs, frames):
            if df is not None and df["t"].notna().any():
                t0 = float(np.median(df["time"] - df["t"]))
                break
    if t0 is None:
        raise ValueError(f"{stem}: can't find t0 (no trigger was received?)")

    hz = info.get("measured_refresh_hz")
    if hz is None and frames is not None and len(frames) > 10:
        hz = 1000.0 / float(frames["interval_ms"].iloc[1:].median())

    ev = _read_tsv(p("eventlog.tsv"))
    ev = ev[ev["frames"].fillna(0) > 0].copy()
    ev = ev.sort_values("onset", kind="stable").reset_index(drop=True)

    return OnsetRun(raw_dir, stem, float(t0), hz, trig["time"].to_numpy(float), ev, frames, info)


# =============================================================================
# Analyses
# =============================================================================


@dataclass
class Report:
    title: str
    table: pd.DataFrame
    summary: dict
    notes: list[str]


def _alignment_summary(al: Alignment, n_src: int, n_dst: int) -> dict:
    return {
        "triggers_arduino": n_src,
        "triggers_logged": n_dst,
        "triggers_paired": al.n,
        "clock_drift_ppm": al.drift_ppm,
        "trigger_residual_sd_ms": float(np.std(al.residuals) * 1000) if al.n > 1 else float("nan"),
        "trigger_residual_max_ms": float(np.max(np.abs(al.residuals)) * 1000) if al.n else float("nan"),
    }


def compare_onsets(
    *,
    probe: Probe,
    ref_triggers: np.ndarray,
    t0_ref: float,
    expected_ref: np.ndarray,
    planned_rel: np.ndarray | None,
    labels: pd.DataFrame,
    period: float | None,
    flash_frames: int | None,
    before: float | None,
    after: float,
    trigger_tol: float,
    slope_hint: float | None = None,
    forced_t0_index: int | None = None,
    title: str = "",
) -> Report:
    """Shared core: align clocks via triggers, match onsets to light edges.

    All *_ref times are on the stimulus computer's clock (Onset run clock, or
    E-Prime's clock), in seconds. expected_ref = logged onsets on that clock;
    t0_ref = when that program received the first kept trigger.
    """
    notes: list[str] = []
    period = period or 1 / 60
    if before is None:
        before = period  # light can't come earlier than one frame before the logged flip
    if len(probe.triggers) == 0:
        raise ValueError("the probe log has no triggers (T lines); can't align clocks")
    if len(ref_triggers) == 0:
        raise ValueError("no logged triggers to align against (was the run's trigger simulated?)")

    cands = align_triggers(probe.triggers, ref_triggers, tol=trigger_tol, slope_hint=slope_hint)
    if forced_t0_index is not None:
        # User says which Arduino trigger is the program's first trigger.
        i_ref0 = int(np.argmin(np.abs(np.sort(ref_triggers) - t0_ref)))
        cands = [c for c in cands if any((c.pairs[:, 0] == i_ref0) & (c.pairs[:, 1] == forced_t0_index))]
    al, n2 = choose_alignment(cands, expected_ref, probe.on, before, after)
    notes += n2
    if al is None:
        raise ValueError(
            "could not line up Arduino and logged triggers; check that both saw the same "
            "pulses (--trigger-tol-ms, --trigger-source)"
        )

    # The Arduino's view of "t = 0": its trigger paired with the program's t0.
    ref_sorted = np.sort(ref_triggers)
    src_sorted = np.sort(probe.triggers)
    i_ref0 = int(np.argmin(np.abs(ref_sorted - t0_ref)))
    hit = al.pairs[al.pairs[:, 0] == i_ref0]
    if len(hit):
        ard_t0 = src_sorted[hit[0, 1]]
    else:
        ard_t0 = (t0_ref - al.intercept) / al.slope
        notes.append("the first kept trigger has no Arduino partner; using the fitted clock instead")

    edges_ref = al.map(probe.on)  # light onsets on the program's clock
    m = match_onsets(expected_ref, edges_ref, before, after)
    ok = m >= 0

    tab = labels.reset_index(drop=True).copy()
    tab["logged_onset"] = expected_ref - t0_ref
    photo_rel = np.full(len(tab), np.nan)  # light onset re. Arduino's first trigger
    lat = np.full(len(tab), np.nan)
    width = np.full(len(tab), np.nan)
    photo_rel[ok] = al.slope * (probe.on[m[ok]] - ard_t0)  # slope: Arduino s -> PC s
    lat[ok] = (edges_ref[m[ok]] - expected_ref[ok]) * 1000
    width[ok] = probe.width[m[ok]] * al.slope * 1000
    tab["photodiode_onset"] = photo_rel
    tab["latency_ms"] = lat
    tab["true_minus_logged_ms"] = (photo_rel - tab["logged_onset"].to_numpy()) * 1000
    if planned_rel is not None:
        tab["planned_onset"] = planned_rel
        tab["planned_error_ms"] = (photo_rel - planned_rel) * 1000
    tab["flash_ms"] = width

    # Flash length in frames: longer than flash_frames means a frame was
    # repeated (a dropped frame) while the patch was white. Two events whose
    # flashes touch merge into one pulse; skip those.
    status = np.where(ok, "ok", "unmatched")
    if flash_frames:
        # Time to the previous / next onset, in the table's (original) order.
        order = np.argsort(expected_ref, kind="stable")
        gaps = np.diff(expected_ref[order])
        gap_prev, gap_next = np.empty(len(order)), np.empty(len(order))
        gap_prev[order] = np.insert(gaps, 0, np.inf)
        gap_next[order] = np.append(gaps, np.inf)
        merged_into_prev = gap_prev < (flash_frames + 0.5) * period
        status = np.where(~ok & merged_into_prev, "merged", status)
        extra = np.round(width / 1000 / period) - flash_frames
        extra = np.where(gap_next < (flash_frames + 0.5) * period, np.nan, extra)
        tab["extra_frames"] = extra
    med = np.nanmedian(lat) if ok.any() else np.nan
    late = ok & (lat > med + 0.5 * period * 1000)
    early = ok & (lat < med - 0.5 * period * 1000)
    status = np.where(late, "late_frame", np.where(early, "early_frame", status))
    tab["status"] = status

    # Light edges inside the analysed span that no onset claimed.
    lo, hi = expected_ref.min() - before, expected_ref.max() + after
    in_span = (edges_ref >= lo) & (edges_ref <= hi)
    used = np.zeros(len(edges_ref), bool)
    used[m[ok]] = True
    extra_edges = int((in_span & ~used).sum())

    st = latency_stats(lat, tab["logged_onset"].to_numpy())
    tml = latency_stats(tab["true_minus_logged_ms"].to_numpy())
    summary = {
        "events": len(tab),
        "matched": int(ok.sum()),
        "unmatched": int((status == "unmatched").sum()),
        "merged_flashes": int((status == "merged").sum()),
        "extra_edges": extra_edges,
        "rejected_short_pulses": probe.rejected,
        "frame_period_ms": period * 1000,
        **{f"latency_{k}": v for k, v in st.items() if k != "n"},
        "late_by_a_frame": int(late.sum()),
        "early_by_a_frame": int(early.sum()),
        "true_minus_logged_mean_ms": tml.get("mean_ms", float("nan")),
        "true_minus_logged_sd_ms": tml.get("sd_ms", float("nan")),
        # How much later than average the program received the t = 0 trigger
        # (true_minus_logged = latency + this).
        "t0_trigger_residual_ms": float((t0_ref - al.map(ard_t0)) * 1000),
    }
    if planned_rel is not None:
        pe = latency_stats(tab["planned_error_ms"].to_numpy())
        summary["planned_error_mean_ms"] = pe.get("mean_ms", float("nan"))
        summary["planned_error_sd_ms"] = pe.get("sd_ms", float("nan"))
        summary["planned_error_max_abs_ms"] = (
            float(np.nanmax(np.abs(tab["planned_error_ms"]))) if ok.any() else float("nan")
        )
    if flash_frames:
        summary["inferred_dropped_frames"] = int(np.nansum(np.clip(tab["extra_frames"], 0, None)))
    summary.update(_alignment_summary(al, len(probe.triggers), len(ref_triggers)))
    return Report(title, tab, summary, notes)


def analyze_onset_run(
    run: OnsetRun,
    probe: Probe,
    *,
    hz: float | None = None,
    flash_frames: int = 3,
    max_latency_s: float = 0.100,
    trigger_tol: float = 0.020,
    use_frames: bool = False,
) -> Report:
    """Onset run vs photodiode. use_frames: match the patch's 0->1 transitions
    from frames.tsv instead of eventlog onsets."""
    hz = hz or run.hz
    period = 1 / hz if hz else None
    if use_frames:
        if run.frames is None:
            raise ValueError("no frames.tsv for this run")
        f = run.frames.reset_index(drop=True)
        pd_on = f["photodiode"].fillna(0).astype(int).to_numpy()
        rise = np.nonzero((pd_on[1:] == 1) & (pd_on[:-1] == 0))[0] + 1
        if pd_on[0] == 1:
            rise = np.insert(rise, 0, 0)
        labels = f.loc[rise, ["frame", "phase", "event"]]
        expected = f.loc[rise, "time"].to_numpy(float)
        rep = compare_onsets(
            probe=probe,
            ref_triggers=run.triggers,
            t0_ref=run.t0,
            expected_ref=expected,
            planned_rel=None,
            labels=labels,
            period=period,
            flash_frames=None,
            before=period,
            after=max_latency_s,
            trigger_tol=trigger_tol,
            title=f"{run.stem}: patch transitions in frames.tsv vs photodiode",
        )
        rep.notes.append("frames.tsv 'time' is when the frame was acquired, not flipped: latency includes that offset")
    else:
        ev = run.events
        keep = [c for c in ("trial", "event", "trial_type", "stim", "frames", "logged") if c in ev.columns]
        rep = compare_onsets(
            probe=probe,
            ref_triggers=run.triggers,
            t0_ref=run.t0,
            expected_ref=ev["onset"].to_numpy(float) + run.t0,
            planned_rel=ev["planned_onset"].to_numpy(float) if "planned_onset" in ev else None,
            labels=ev[keep],
            period=period,
            flash_frames=flash_frames,
            before=period,
            after=max_latency_s,
            trigger_tol=trigger_tol,
            title=f"{run.stem}: logged onsets vs photodiode",
        )
    if run.frames is not None and "missed" in run.frames:
        rep.summary["onset_reported_missed_frames"] = int(pd.to_numeric(run.frames["missed"], errors="coerce").fillna(0).sum())
    elif "missed_frames" in run.info:
        rep.summary["onset_reported_missed_frames"] = int(run.info["missed_frames"])
    if not hz:
        rep.notes.append("refresh rate unknown; assumed 60 Hz (pass --hz)")
    return rep


def analyze_timing_test(probe: Probe, hz: float | None = None) -> Report:
    """`onset timing-test`: the patch is white on even frames, black on odd.

    Ideal: a rising edge every 2 frames and each flash 1 frame long. A frame
    shown twice (dropped/missed vsync) makes one interval 3 frames long.
    Onset writes no log files in this mode, so only the light is analysed.
    """
    on = np.sort(probe.on)
    notes: list[str] = []
    if len(on) < 3:
        raise ValueError("fewer than 3 light flashes in the probe log")
    iv = np.diff(on)
    if hz:
        period = 1 / hz
    else:
        period = float(np.median(iv)) / 2
        notes.append("frame period estimated from the median flash interval / 2 (pass --hz to fix it)")
    nfr = np.round(iv / period).astype(int)
    # Refine the period by regressing flash times on their frame numbers;
    # this averages all the jitter away. (In Arduino seconds: an Uno's clock
    # can be off by up to ~0.5%, so treat the Hz as approximate.)
    frame_no = np.concatenate([[0], np.cumsum(nfr)])
    fitted_period, _ = fit_line(frame_no.astype(float), on)
    dev_ms = (iv - nfr * fitted_period) * 1000
    widths = probe.width[np.argsort(probe.on)][:-1] * 1000
    table = pd.DataFrame(
        {
            "flash": np.arange(1, len(iv) + 1),
            "t_s": on[:-1] - on[0],
            "interval_ms": iv * 1000,
            "frames": nfr,
            "deviation_ms": dev_ms,
            "flash_ms": widths,
        }
    )
    regular = nfr == 2
    summary = {
        "flashes": len(on),
        "duration_s": float(on[-1] - on[0]),
        "refresh_hz_arduino_clock": 1 / fitted_period,
        "intervals_2_frames": int(regular.sum()),
        "intervals_longer": int((nfr > 2).sum()),
        "intervals_shorter": int((nfr < 2).sum()),
        "inferred_dropped_frames": int(np.clip(nfr - 2, 0, None).sum()),
        "interval_sd_ms": float(np.std(iv[regular] * 1000, ddof=1)) if regular.sum() > 1 else float("nan"),
        "interval_dev_max_abs_ms": float(np.max(np.abs(dev_ms))),
        "flash_mean_ms": float(np.nanmean(widths)),
        "flash_sd_ms": float(np.nanstd(widths, ddof=1)),
        "rejected_short_pulses": probe.rejected,
    }
    return Report("timing-test: photodiode flash intervals", table, summary, notes)


def read_eprime(path: Path) -> pd.DataFrame:
    """Read an E-DataAid tab-delimited export (UTF-8 or UTF-16)."""
    raw = Path(path).read_bytes()
    text = None
    for enc in ("utf-8-sig", "utf-16"):
        try:
            text = raw.decode(enc)
            if "\t" in text:
                break
        except UnicodeDecodeError:
            continue
    if text is None:
        raise ValueError(f"{path}: can't decode (expected a tab-delimited text export)")
    lines = text.splitlines()
    # Some exports start with a line holding just the file name.
    skip = 1 if lines and "\t" not in lines[0] and len(lines) > 1 and "\t" in lines[1] else 0
    from io import StringIO

    return pd.read_csv(StringIO("\n".join(lines[skip:])), sep="\t", na_values=["NULL", "", "n/a"])


def analyze_eprime(
    df: pd.DataFrame,
    probe: Probe,
    *,
    onset_cols: list[str],
    trigger_col: str,
    label_col: str | None = None,
    hz: float | None = None,
    tr: float | None = None,
    flash_frames: int | None = None,
    max_latency_s: float = 0.100,
    trigger_tol: float = 0.020,
    trigger_index: int | None = None,
) -> Report:
    """E-Prime export vs photodiode, aligned on the scanner trigger.

    E-Prime times are milliseconds on E-Prime's clock. `trigger_col` holds the
    time E-Prime registered the scanner trigger (e.g. WaitForScanner.RTTime);
    each `onset_cols` column holds stimulus OnsetTimes. Usually E-Prime logs
    only ONE trigger, so the clock rate can't be fitted from triggers; give
    --tr and the Arduino's clock is calibrated against the scanner's TR
    instead (E-Prime's PC clock is assumed accurate).
    """
    for c in onset_cols + [trigger_col] + ([label_col] if label_col else []):
        if c not in df.columns:
            raise ValueError(f"E-Prime export has no column {c!r}; columns are: {', '.join(df.columns)}")
    trig = np.unique(pd.to_numeric(df[trigger_col], errors="coerce").dropna().to_numpy(float)) / 1000
    if len(trig) == 0:
        raise ValueError(f"column {trigger_col!r} has no numbers")
    rows = []
    for c in onset_cols:
        v = pd.to_numeric(df[c], errors="coerce")
        for i in np.nonzero(v.notna().to_numpy())[0]:
            rows.append(
                {
                    "row": i + 1,
                    "column": c,
                    "label": df[label_col].iloc[i] if label_col else "",
                    "t": v.iloc[i] / 1000,
                }
            )
    if not rows:
        raise ValueError("no onset times found in the E-Prime columns")
    on = pd.DataFrame(rows).sort_values("t", kind="stable").reset_index(drop=True)
    slope_hint = None
    notes = []
    if len(trig) == 1:
        if tr:
            slope_hint = slope_from_tr(probe.triggers, tr)
            if slope_hint is not None:
                notes.append(f"Arduino clock calibrated against TR={tr}s: {(slope_hint - 1) * 1e6:+.0f} ppm")
        if slope_hint is None:
            notes.append("one E-Prime trigger and no --tr: assuming Arduino and PC clocks run at the same rate")
    rep = compare_onsets(
        probe=probe,
        ref_triggers=trig,
        t0_ref=float(trig[0]),
        expected_ref=on["t"].to_numpy(float),
        planned_rel=None,
        labels=on[["row", "column", "label"]],
        period=1 / hz if hz else None,
        flash_frames=flash_frames,
        before=1 / hz if hz else None,
        after=max_latency_s,
        trigger_tol=trigger_tol,
        slope_hint=slope_hint,
        forced_t0_index=trigger_index,
        title="E-Prime: OnsetTime vs photodiode",
    )
    rep.notes = notes + rep.notes
    if len(trig) == 1:
        rep.summary["clock_drift_ppm"] = float("nan") if slope_hint is None else (slope_hint - 1) * 1e6
    return rep


# =============================================================================
# Output
# =============================================================================


def verdict(rep: Report) -> list[str]:
    """Pass/fail lines for the criteria in docs/VALIDATION.md."""
    s = rep.summary
    out = []

    def line(ok, text):
        out.append(f"  [{'PASS' if ok else 'CHECK'}] {text}")

    if "latency_sd_ms" in s:
        line(s["latency_sd_ms"] < PASS_JITTER_SD_MS, f"jitter SD {s['latency_sd_ms']:.3f} ms < {PASS_JITTER_SD_MS} ms")
        line(
            s["latency_max_abs_dev_ms"] < PASS_MAX_ABS_DEV_MS,
            f"worst event {s['latency_max_abs_dev_ms']:.2f} ms from the mean < {PASS_MAX_ABS_DEV_MS} ms",
        )
        line(s["unmatched"] == 0, f"every onset produced light ({s['unmatched']} unmatched)")
        line(s["late_by_a_frame"] + s["early_by_a_frame"] == 0, "no onset a whole frame off")
    if "interval_dev_max_abs_ms" in s:
        line(s["interval_dev_max_abs_ms"] < 1.0, f"flash intervals within 1 ms of whole frames ({s['interval_dev_max_abs_ms']:.3f} ms)")
    if "inferred_dropped_frames" in s:
        line(s["inferred_dropped_frames"] == 0, f"no dropped frames seen by the photodiode ({s['inferred_dropped_frames']})")
    if "onset_reported_missed_frames" in s:
        line(s["onset_reported_missed_frames"] == 0, f"Onset reported {s['onset_reported_missed_frames']} missed frames")
    return out


def format_report(rep: Report, show_table: bool = True) -> str:
    lines = [f"== {rep.title} =="]
    if show_table:
        with pd.option_context("display.max_rows", None, "display.width", 200, "display.max_columns", None):
            lines.append(rep.table.to_string(index=False, float_format=lambda v: f"{v:.3f}"))
    lines.append("")
    lines.append("summary:")
    for k, v in rep.summary.items():
        lines.append(f"  {k:32s} {v:.3f}" if isinstance(v, float) else f"  {k:32s} {v}")
    for n in rep.notes:
        lines.append(f"note: {n}")
    lines.append("")
    lines += verdict(rep)
    return "\n".join(lines)


def plot_report(rep: Report, dest: str) -> None:
    """Histogram of latency + latency over the run (or intervals for timing-test)."""
    try:
        import matplotlib

        if dest != "show":
            matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("--plot needs matplotlib:  python3 -m pip install matplotlib", file=sys.stderr)
        return
    t = rep.table
    if "latency_ms" in t:
        y, x, ylab, xlab = t["latency_ms"], t["logged_onset"], "latency (ms)", "logged onset (s)"
    else:
        y, x, ylab, xlab = t["deviation_ms"], t["t_s"], "interval - whole frames (ms)", "time (s)"
    y = y.to_numpy(float)
    fin = np.isfinite(y)
    color = "#3b6ea8"
    fig, (a1, a2) = plt.subplots(1, 2, figsize=(11, 4), gridspec_kw={"width_ratios": [1, 2]})
    a1.hist(y[fin], bins=40, color=color, edgecolor="white", linewidth=0.5)
    a1.set_xlabel(ylab)
    a1.set_ylabel("count")
    a2.plot(x[fin], y[fin], "o", ms=3, color=color)
    a2.set_xlabel(xlab)
    a2.set_ylabel(ylab)
    for a in (a1, a2):
        a.grid(alpha=0.25, linewidth=0.5)
        a.spines[["top", "right"]].set_visible(False)
    fig.suptitle(rep.title, fontsize=10)
    fig.tight_layout()
    if dest == "show":
        plt.show()
    else:
        fig.savefig(dest, dpi=150)
        print(f"plot saved to {dest}", file=sys.stderr)


# =============================================================================
# Command line
# =============================================================================


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--probe", type=Path, required=True, help="TSV from capture_probe.py")
    ap.add_argument("--run", type=Path, help="Onset onset/ folder, or a *_eventlog.tsv / func/*_events.tsv path")
    ap.add_argument("--stem", help="run stem, e.g. sub-01_task-faces_run-01 (if --run is a folder)")
    ap.add_argument(
        "--timing-test",
        action="store_true",
        help="analyse `onset timing-test` flashing (no logs needed); with --run, "
        "match frames.tsv patch transitions instead of eventlog onsets",
    )
    ap.add_argument("--hz", type=float, help="display refresh rate (default: from run.json)")
    ap.add_argument("--flash-frames", type=int, default=3, help="rig photodiode.flash_frames (default 3)")
    ap.add_argument("--max-latency-ms", type=float, default=100.0, help="search window after each onset")
    ap.add_argument("--min-pulse-ms", type=float, default=0.5, help="ignore light pulses shorter than this")
    ap.add_argument("--trigger-tol-ms", type=float, default=20.0, help="max trigger mismatch after alignment")
    ap.add_argument(
        "--trigger-source",
        action="append",
        help="only use Onset triggers from this inputs.tsv source (keyboard/serial); repeatable",
    )
    ap.add_argument("--tr", type=float, help="scanner TR in s (calibrates the Arduino clock in E-Prime mode)")
    eg = ap.add_argument_group("E-Prime comparison")
    eg.add_argument("--eprime", type=Path, help="E-DataAid tab-delimited export")
    eg.add_argument("--eprime-onset-col", action="append", help="OnsetTime column (ms); repeatable")
    eg.add_argument("--eprime-trigger-col", help="column with the trigger time (ms), e.g. WaitForScanner.RTTime")
    eg.add_argument("--eprime-label-col", help="column to show as a label, e.g. Procedure or Condition")
    eg.add_argument("--eprime-trigger-index", type=int, help="force: index of the Arduino T line E-Prime started on")
    ap.add_argument("--out", type=Path, help="write the per-event table to this CSV")
    ap.add_argument("--plot", nargs="?", const="show", help="plot (to a PNG path, or a window if no path)")
    ap.add_argument("--no-table", action="store_true", help="print only the summary")
    return ap


def run_cli(args: argparse.Namespace) -> Report:
    probe = read_probe(args.probe, min_pulse_s=args.min_pulse_ms / 1000)
    tol = args.trigger_tol_ms / 1000
    window = args.max_latency_ms / 1000
    if args.eprime:
        if not args.eprime_onset_col or not args.eprime_trigger_col:
            raise SystemExit("--eprime needs --eprime-onset-col and --eprime-trigger-col")
        return analyze_eprime(
            read_eprime(args.eprime),
            probe,
            onset_cols=args.eprime_onset_col,
            trigger_col=args.eprime_trigger_col,
            label_col=args.eprime_label_col,
            hz=args.hz,
            tr=args.tr,
            max_latency_s=window,
            trigger_tol=tol,
            trigger_index=args.eprime_trigger_index,
        )
    if args.run:
        run = load_onset_run(args.run, args.stem, args.trigger_source)
        return analyze_onset_run(
            run,
            probe,
            hz=args.hz,
            flash_frames=args.flash_frames,
            max_latency_s=window,
            trigger_tol=tol,
            use_frames=args.timing_test,
        )
    if args.timing_test:
        return analyze_timing_test(probe, hz=args.hz)
    raise SystemExit("give --run (an Onset run), --timing-test, or --eprime")


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        rep = run_cli(args)
    except (ValueError, FileNotFoundError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(format_report(rep, show_table=not args.no_table))
    if args.out:
        rep.table.to_csv(args.out, index=False)
        print(f"\ntable written to {args.out}", file=sys.stderr)
    if args.plot:
        plot_report(rep, args.plot)
    return 0


if __name__ == "__main__":
    sys.exit(main())
