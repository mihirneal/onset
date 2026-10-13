"""Tests for analyze_timing.py using synthetic Onset logs and probe data.

We invent a "true" timeline (scanner time, seconds) and derive from it:
  * Onset's run clock      = true + 100 s            (PC clock taken as exact)
  * the Arduino's clock    = true * (1 + 50 ppm) + 7 s (a slightly fast board)
  * light onset            = Onset's flip + 12 ms display lag + N(0, 1 ms) jitter
and check that the analysis recovers the 12 ms lag, the 1 ms jitter and the
-50 ppm clock difference.

Run:  python3 -m pytest tools/test_analyze_timing.py
 or:  python3 tools/test_analyze_timing.py
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import analyze_timing as at  # noqa: E402

HZ = 60.0
FRAME = 1 / HZ
TR = 2.0
LAG = 0.012
JITTER = 0.001
DRIFT = 50e-6
FLASH_FRAMES = 3
RUN_OFFSET = 100.0  # run clock = true + RUN_OFFSET
ARD_OFFSET = 7.0  # arduino = true * (1 + DRIFT) + ARD_OFFSET
STEM = "sub-01_task-test_run-01"


def to_arduino(true_t):
    return np.asarray(true_t) * (1 + DRIFT) + ARD_OFFSET


def synth_run(
    root: Path,
    *,
    seed: int = 1,
    n_events: int = 120,
    kb_latency=(0.0, 0.0),  # uniform range of the trigger input delay (s)
    drop_onset_trigger: int | None = None,
    dropped_frame_event: int | None = None,
    write_frames: bool = True,
):
    """Write Onset-format logs for a fake run to root/onset and a probe TSV.

    Returns (raw_dir, probe_path, truth dict).
    """
    rng = np.random.default_rng(seed)
    raw = root / "onset"
    raw.mkdir(parents=True)

    # Scanner: volumes every TR. The Arduino sees 6 volumes before Onset was
    # listening and 5 after it quit, as happens when the probe runs all session.
    k_all = np.arange(-6, 160 + 5)
    trig_true = 30.0 + k_all * TR
    onset_k = (k_all >= 0) & (k_all < 160)
    delay = rng.uniform(*kb_latency, size=onset_k.sum())
    onset_trig_run = trig_true[onset_k] + RUN_OFFSET + delay
    if drop_onset_trigger is not None:  # e.g. a missed keypress
        onset_trig_run = np.delete(onset_trig_run, drop_onset_trigger)
    t0_true = trig_true[k_all == 0][0]
    t0_run = onset_trig_run[0]  # Onset's t = 0 is when IT saw the first trigger

    # Events: random ITIs, flips land on the 60 Hz frame grid of the run clock.
    planned = np.cumsum(rng.uniform(1.5, 3.5, n_events)) + 2.0  # s after t0
    frame_phase = 0.004  # vsync isn't aligned to anything in particular
    flip_run = np.ceil((planned + t0_run - frame_phase) / FRAME) * FRAME + frame_phase
    onset_rel = flip_run - t0_run
    frames_on = np.full(n_events, 30)

    # Light: lag + jitter; off after flash_frames (+1 if a frame was dropped).
    light_true = flip_run - RUN_OFFSET + LAG + rng.normal(0, JITTER, n_events)
    flash = np.full(n_events, FLASH_FRAMES * FRAME)
    if dropped_frame_event is not None:
        flash[dropped_frame_event] += FRAME
    off_true = light_true + flash + rng.normal(0, 0.0003, n_events)

    # --- Onset files -----------------------------------------------------
    cols = [
        "onset", "duration", "trial_type", "response_time", "response", "correct", "stim",
        "trial", "event", "planned_onset", "planned_duration", "onset_delay_ms", "frames",
        "logged", "condition",
    ]  # fmt: skip
    rows = ["\t".join(cols)]
    for i in range(n_events):
        rows.append(
            "\t".join(
                [
                    f"{onset_rel[i]:.4f}", "0.5000", "face" if i % 2 else "house", "n/a", "n/a", "n/a",
                    f"img{i}.png", str(i + 1), "stim", f"{planned[i]:.4f}", "0.5000",
                    f"{(onset_rel[i] - planned[i]) * 1000:.2f}", str(frames_on[i]), "1", "a",
                ]
            )
        )  # fmt: skip
    # An event that never reached the screen (frames = 0) must be ignored.
    rows.append("\t".join(["9999.0000", "0.0000", "n/a", "n/a", "n/a", "n/a", "x", "999", "stim",
                           "9999.0000", "0.5000", "n/a", "0", "1", "a"]))  # fmt: skip
    (raw / f"{STEM}_eventlog.tsv").write_text("\n".join(rows) + "\n")

    inp = ["time\tt\tsource\traw\tkind\tlabel"]
    inp.append(f"{t0_run - 20:.6f}\tn/a\tkeyboard\tspace\toperator\tcontinue")
    for t in onset_trig_run:
        inp.append(f"{t:.6f}\t{t - t0_run:.6f}\tkeyboard\t5\ttrigger\ttrigger")
    inp.append(f"{t0_run + 50:.6f}\t50.000000\tkeyboard\t1\tbutton\tleft")
    (raw / f"{STEM}_inputs.tsv").write_text("\n".join(inp) + "\n")

    (raw / f"{STEM}_run.json").write_text(
        json.dumps({"t0_run_clock": t0_run, "measured_refresh_hz": HZ, "missed_frames": 0})
    )

    if write_frames:
        start = flip_run[0] - 5
        nfr = int((flip_run[-1] + 2 - start) / FRAME)
        ftime = start + np.arange(nfr) * FRAME
        photodiode = np.zeros(nfr, int)
        first = np.round((flip_run - start) / FRAME).astype(int)
        for f0 in first:
            photodiode[f0 : f0 + FLASH_FRAMES] = 1
        fr = ["frame\ttime\tt\tinterval_ms\tmissed\tphase\tevent\tphotodiode"]
        for i in range(nfr):
            fr.append(f"{i}\t{ftime[i]:.6f}\t{ftime[i] - t0_run:.6f}\t{FRAME * 1000:.3f}\t0\trunning\tn/a\t{photodiode[i]}")
        (raw / f"{STEM}_frames.tsv").write_text("\n".join(fr) + "\n")

    # --- probe file ---------------------------------------------------------
    rec = [("T", to_arduino(t), "n/a") for t in trig_true]
    rec += [("P", to_arduino(t), "1") for t in light_true]
    rec += [("P", to_arduino(t), "0") for t in off_true]
    # A 50 µs noise blip in the middle of nowhere: must be rejected.
    blip = light_true[10] + 0.7
    rec += [("P", to_arduino(blip), "1"), ("P", to_arduino(blip + 50e-6), "0")]
    rec.sort(key=lambda r: r[1])
    probe = root / "probe.tsv"
    lines = ["# onset_timing_probe v1", "kind\tarduino_us\tlevel\thost_time"]
    lines += [f"{k}\t{int(round(t * 1e6))}\t{lvl}\t0" for k, t, lvl in rec]
    probe.write_text("\n".join(lines) + "\n")

    truth = {"t0_true": t0_true, "t0_delay": delay[0], "mean_delay": delay.mean()}
    return raw, probe, truth


# -----------------------------------------------------------------------------


def test_recovers_lag_jitter_and_drift(tmp_path):
    raw, probe_path, truth = synth_run(tmp_path)
    run = at.load_onset_run(raw)
    rep = at.analyze_onset_run(run, at.read_probe(probe_path), flash_frames=FLASH_FRAMES)
    s = rep.summary
    assert s["events"] == 120  # the frames=0 row was dropped
    assert s["matched"] == 120 and s["unmatched"] == 0
    assert abs(s["latency_mean_ms"] - 12.0) < 0.3, s["latency_mean_ms"]
    assert 0.85 < s["latency_sd_ms"] < 1.15, s["latency_sd_ms"]
    # Arduino runs 50 ppm fast -> mapping Arduino -> Onset has slope 1 - 50 ppm.
    assert abs(s["clock_drift_ppm"] + 50) < 1, s["clock_drift_ppm"]
    assert s["triggers_paired"] == 160 and s["triggers_arduino"] == 171
    assert s["inferred_dropped_frames"] == 0
    assert s["extra_edges"] == 0  # the 50 µs blip was rejected as too short
    assert s["rejected_short_pulses"] == 1
    # No trigger input delay here, so the "true" onset error equals the lag too.
    assert abs(s["true_minus_logged_mean_ms"] - 12.0) < 0.3
    assert abs(s["planned_error_mean_ms"] - 12.0) < 17  # planned->flip adds up to a frame


def test_keyboard_trigger_latency_and_missing_trigger(tmp_path):
    """fORP-as-keyboard: triggers arrive 1-4 ms late; one keypress is lost."""
    raw, probe_path, truth = synth_run(tmp_path, seed=2, kb_latency=(0.001, 0.004), drop_onset_trigger=40)
    rep = at.analyze_onset_run(at.load_onset_run(raw), at.read_probe(probe_path))
    s = rep.summary
    assert s["matched"] == 120
    assert s["triggers_paired"] == 159
    # The fit absorbs the mean trigger delay into the latency...
    assert abs(s["latency_mean_ms"] - (12.0 + truth["mean_delay"] * 1000)) < 0.3
    # ...but the jitter is untouched.
    assert 0.85 < s["latency_sd_ms"] < 1.15
    # true - logged = lag + the delay of the t = 0 trigger specifically.
    assert abs(s["true_minus_logged_mean_ms"] - (12.0 + truth["t0_delay"] * 1000)) < 0.3
    assert 0.5 < s["trigger_residual_sd_ms"] < 1.2  # uniform 3 ms range -> SD ~0.87 ms


def test_dropped_frame_during_flash_is_detected(tmp_path):
    raw, probe_path, _ = synth_run(tmp_path, seed=3, dropped_frame_event=50)
    rep = at.analyze_onset_run(at.load_onset_run(raw), at.read_probe(probe_path))
    assert rep.summary["inferred_dropped_frames"] == 1
    assert rep.table.loc[50, "extra_frames"] == 1


def test_frames_tsv_mode(tmp_path):
    raw, probe_path, _ = synth_run(tmp_path, seed=4)
    rep = at.analyze_onset_run(at.load_onset_run(raw), at.read_probe(probe_path), use_frames=True)
    s = rep.summary
    assert s["events"] == 120 and s["matched"] == 120
    assert abs(s["latency_mean_ms"] - 12.0) < 0.3
    assert 0.85 < s["latency_sd_ms"] < 1.15


def test_timing_test_mode():
    rng = np.random.default_rng(5)
    period = 1 / 60
    shown = np.arange(1200)  # frame index actually on screen at each refresh
    # Refresh 500 repeats the previous image (a dropped frame).
    shown = np.concatenate([shown[:500], [499], shown[500:]])
    refresh_t = np.arange(len(shown)) * period
    white = shown % 2 == 0
    rises = refresh_t[1:][white[1:] & ~white[:-1]] + 0.012 + rng.normal(0, 0.0002, (white[1:] & ~white[:-1]).sum())
    falls = refresh_t[1:][~white[1:] & white[:-1]] + 0.014
    probe = at.make_probe([], to_arduino(rises), to_arduino(falls))
    rep = at.analyze_timing_test(probe)
    s = rep.summary
    assert s["inferred_dropped_frames"] == 1
    assert s["intervals_longer"] == 1
    assert abs(s["refresh_hz_arduino_clock"] - 60 / (1 + DRIFT)) < 0.01
    assert s["interval_dev_max_abs_ms"] < 1.0


def test_eprime_mode(tmp_path):
    """E-Prime logs one trigger time; --tr calibrates the Arduino's clock."""
    rng = np.random.default_rng(6)
    trig_true = 10.0 + np.arange(200) * TR  # the Arduino sees all volumes
    ep_offset = 55.0  # E-Prime clock = true + 55 s (milliseconds in the file)
    t0_true = trig_true[3]  # E-Prime started waiting after 3 volumes
    onsets_true = t0_true + np.cumsum(rng.uniform(2, 4, 100))
    light_true = onsets_true + LAG + rng.normal(0, JITTER, 100)
    rows = ["ExperimentName\tSubject\tTrial\tCondition\tWaitTrigger.RTTime\tStim.OnsetTime"]
    for i, t in enumerate(onsets_true):
        rows.append(f"test\t1\t{i + 1}\tA\t{(t0_true + ep_offset) * 1000:.0f}\t{(t + ep_offset) * 1000:.0f}")
    ep = tmp_path / "eprime.txt"
    ep.write_bytes(("\r\n".join(rows) + "\r\n").encode("utf-16"))  # E-DataAid's Unicode export
    probe = at.make_probe(to_arduino(trig_true), to_arduino(light_true), to_arduino(light_true + 0.1))
    rep = at.analyze_eprime(
        at.read_eprime(ep),
        probe,
        onset_cols=["Stim.OnsetTime"],
        trigger_col="WaitTrigger.RTTime",
        label_col="Condition",
        hz=60,
        tr=TR,
    )
    s = rep.summary
    assert s["matched"] == 100
    # E-Prime logs whole ms, which adds ~0.3 ms rounding noise.
    assert abs(s["latency_mean_ms"] - 12.0) < 0.3
    assert 0.85 < s["latency_sd_ms"] < 1.25
    assert abs(s["clock_drift_ppm"] + 50) < 2


def test_pulse_pairing_and_filter():
    on, off = at.pair_pulses(np.array([1.0, 2.0, 3.0]), np.array([0.5, 1.05, 2.0001]))
    assert np.allclose(off[:2], [1.05, 2.0001]) and np.isnan(off[2])
    p = at.make_probe([], [1.0, 2.0, 3.0], [0.5, 1.05, 2.0001], min_pulse_s=0.0005)
    assert list(p.on) == [1.0, 3.0] and p.rejected == 1


def test_match_onsets_uses_each_edge_once():
    m = at.match_onsets(np.array([1.0, 1.001, 2.0]), np.array([1.01, 2.01]), before=0.016, after=0.1)
    assert list(m) == [0, -1, 1]


def test_cli_end_to_end(tmp_path, capsys=None):
    raw, probe_path, _ = synth_run(tmp_path, seed=7)
    out = tmp_path / "timing.csv"
    eventlog = raw / f"{STEM}_eventlog.tsv"
    assert at.main(["--probe", str(probe_path), "--run", str(eventlog), "--out", str(out), "--no-table"]) == 0
    import pandas as pd

    df = pd.read_csv(out)
    assert len(df) == 120 and abs(df["latency_ms"].mean() - 12) < 0.3


# -----------------------------------------------------------------------------

if __name__ == "__main__":
    # Minimal runner for machines without pytest.
    failed = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            with tempfile.TemporaryDirectory() as d:
                try:
                    if fn.__code__.co_argcount:
                        fn(Path(d))
                    else:
                        fn()
                    print(f"ok    {name}")
                except Exception as e:  # noqa: BLE001
                    failed += 1
                    print(f"FAIL  {name}: {e!r}")
    sys.exit(1 if failed else 0)
