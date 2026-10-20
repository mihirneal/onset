# Timing validation protocol

How to show, with a photodiode, that Onset's stimulus onsets are where its logs
say they are, and how to run the same check on other presentation software.

Hardware and wiring: [`hardware/README.md`](../hardware/README.md).
Tools: `tools/capture_probe.py` (record the probe), `tools/analyze_timing.py`
(compare), `tools/test_analyze_timing.py` (self-test with synthetic data).

```sh
python3 -m pip install -r tools/requirements.txt     # numpy pandas pyserial (+ matplotlib, pytest)
python3 -m pytest tools/test_analyze_timing.py       # the analysis recovers known lag/jitter
```

## What is being measured

Every stimulus event makes Onset's corner patch white for `flash_frames`
frames (default 3) starting on the event's first frame. So each event onset is
a **rising edge of light** under the photodiode. The probe timestamps those
edges and the scanner triggers on one clock (the Arduino's), and the analysis
maps that clock onto Onset's using the triggers both of them saw.

Per event you get:

| Column | Meaning |
|---|---|
| `logged_onset` | Onset's `onset` (s after its first kept trigger) |
| `photodiode_onset` | when light actually appeared, s after the scanner's first kept trigger, measured entirely by the probe — the "true" BIDS onset |
| `latency_ms` | light − logged flip, both on Onset's clock |
| `true_minus_logged_ms` | `photodiode_onset − logged_onset`: the error in events.tsv |
| `planned_error_ms` | `photodiode_onset − planned_onset` |
| `flash_ms`, `extra_frames` | how long the patch stayed white; +1 frame = a dropped frame |
| `status` | `ok`, `unmatched` (no light), `merged` (flash touched the previous one), `late_frame` / `early_frame` (a whole frame off the typical latency) |

**How to read latency.** `latency_ms` = the display's input lag (GPU → panel
or projector → light, usually 5–50 ms and constant) **plus** the average delay
with which Onset receives the trigger (e.g. the fORP's USB keyboard path, a few
ms). Neither matters much if constant: what matters is the **SD (jitter)**,
and that no event is a whole frame off. `true_minus_logged_ms` is the
number to report if you correct onsets; it equals the latency plus the delay of
the *first* trigger specifically (`t0_trigger_residual_ms`).

**Arduino clock.** An Uno R3's clock may be off by up to ~0.3%; the trigger fit
measures and removes this (`clock_drift_ppm`), so expect values anywhere from
tens to a few thousand ppm. `trigger_residual_sd_ms` is how well the triggers
fit a straight line — i.e. the jitter of Onset's trigger input path.

## 1. Desk test A — `onset timing-test` (display and frame pacing)

Goal: the display and Onset's frame loop run at a steady rate with no dropped
frames, independent of any experiment.

1. Use the stimulus PC, display, resolution and refresh rate you'll use in
   the scanner. Fullscreen, display at 100% brightness, notifications off.
2. Mount the sensor on the patch (hardware/README.md), tune the threshold.
3. `python3 tools/capture_probe.py --out tt_desk.tsv`
4. `onset timing-test --seconds 120` (the patch flips white/black every frame).
5. Ctrl-C the capture, then
   `python3 tools/analyze_timing.py --probe tt_desk.tsv --timing-test --hz 60 --plot tt_desk.png`

Report: `refresh_hz_arduino_clock` (approximate: Arduino clock), flashes,
`inferred_dropped_frames`, `intervals_longer`, `interval_sd_ms`,
`interval_dev_max_abs_ms`, and Onset's own console summary.

`onset timing-test` writes no log files, so this test checks frame pacing
only. Logged-vs-light comparison needs test B.

Note: some LCDs can't fully switch white↔black in one frame. If the flash
widths look odd or edges go missing, it's the panel's response time, not
Onset — tests B–D use 3-frame flashes, which every display can show.

## 2. Desk test B — real paradigm, fake scanner (full loop at the desk)

Goal: Onset's logged onsets match light onsets for a real experiment.

1. Flash the sketch with `FAKE_TR 1` and set `TR_MS` to your TR. Jumper
   **D9 → D3**. On a Leonardo/Uno R4 also set `FAKE_TR_KEYBOARD 1` so the
   board types `5` on every pulse; otherwise route D9 into your trigger box.
   (Onset's `--simulate-tr` can't be used: the probe can't see simulated
   triggers.)
2. Start the capture: `python3 tools/capture_probe.py --out probe_deskB.tsv`
3. Run the real experiment in Onset (trigger source keyboard, key `5`). Leave
   the Onset window focused.
4. After the run, stop the capture and analyse:

   ```sh
   python3 tools/analyze_timing.py --probe probe_deskB.tsv \
       --run data/sub-XX/onset --stem <stem> --out deskB.csv --plot deskB.png
   ```

5. Optional cross-check against Onset's per-frame log:
   add `--timing-test` with `--run` to match every patch transition in
   `frames.tsv` instead of event onsets.

Repeat 3 runs. Also try a "stress" run (other apps open, a large image set)
to see whether anything drops frames.

## 3. Scanner test — real trigger, real display, real paradigm

Goal: the same numbers under real conditions (scanner trigger path, the
participant display, the stimulus PC in its normal place).

**Safety:** follow hardware/README.md: nothing enters the magnet room, the
trigger comes from the existing trigger box output in the control room via
the opto-isolator, and the sensor goes on the display outside the magnet room
(or the identical display on the bench). Clear it with the MR
physicist/engineer first.

1. Sketch: `FAKE_TR 0`. Trigger box TTL output → opto-isolator → D3.
2. Phantom in the scanner; the real sequence for your paradigm, including
   dummy volumes.
3. Start the capture before starting Onset; stop it after the scan ends.
   (It's fine that the probe also records triggers before/after Onset was
   listening — the analysis finds the right alignment.)
4. Run the paradigm 2–3 times; analyse each run as in test B.

Also check: `triggers_logged` equals the number of volumes Onset was running
for, and `trigger_residual_sd_ms` (fORP/USB path) is small (< 1–2 ms).

## 4. Comparing with other software — same paradigm, same rig

Goal: a like-for-like measurement of another program on the same rig. The
analysis reads E-Prime's tab-delimited export; the steps below use it as the
example.

1. Implement the same paradigm in E-Prime, on the same PC, display and
   trigger box. Add the **same patch**: a white rectangle in the same corner
   and size, on the first slide/frame of every stimulus for 3 frames (or on
   for the whole stimulus — then the analysis's `flash_ms` won't mean
   anything), black otherwise. Use E-Prime's recommended timing settings
   (event mode, PreRelease, refresh-aligned durations, "OnsetSync: vertical
   blank").
2. Make sure E-Prime logs the trigger time (e.g. the `RTTime` of the object
   waiting for the scanner `5`) and each stimulus's `OnsetTime`.
3. Record the probe for the run exactly as for Onset.
4. In E-DataAid: File → Export → "Other", tab-delimited (Unicode is fine).
5. Analyse:

   ```sh
   python3 tools/analyze_timing.py --probe probe_eprime.tsv \
       --eprime export.txt \
       --eprime-onset-col Stimulus.OnsetTime \
       --eprime-trigger-col WaitForScanner.RTTime \
       --eprime-label-col Condition \
       --tr 2.0 --hz 60 --out eprime.csv
   ```

   Repeat `--eprime-onset-col` for each stimulus object that shows the patch.
   E-Prime normally logs a single trigger, so a clock rate can't be fitted from
   triggers; `--tr` calibrates the Arduino's clock against the scanner's TR
   from the probe's own trigger train (the E-Prime PC's clock is assumed
   accurate, ~10–50 ppm). If the automatic choice of which probe trigger
   E-Prime started on looks wrong, force it with `--eprime-trigger-index N`
   (0-based count of `T` lines).

Compare the two runs on: latency SD, worst event, frame-off events,
dropped frames, and `true_minus_logged` mean/SD.

## What to report

For each test and run:

* PC, OS, GPU, display/projector model, resolution, refresh rate; Onset
  version (run.json); probe board + sensor.
* N events, matched, unmatched, merged, extra light edges.
* Latency: mean, **SD**, median, min, max, 5th/95th percentile, worst
  deviation from the mean, trend (ms/min).
* `true_minus_logged` mean and SD; `planned_error` mean / SD / max.
* Frames off: `late_by_a_frame`, `early_by_a_frame`.
* Dropped frames: photodiode-inferred and Onset-reported.
* Trigger path: triggers paired, `trigger_residual_sd_ms`, `clock_drift_ppm`.
* The `--plot` figure (histogram + latency over the run).

## Pass criteria

| Criterion | Target |
|---|---|
| Latency jitter (`latency_sd_ms`) | **< 1 ms** |
| Worst single event (`latency_max_abs_dev_ms`) | < 4 ms (no event near a frame off) |
| Events a whole frame off (`late_by_a_frame` + `early_by_a_frame`) | **0** |
| Unmatched events (no light) | **0** |
| Dropped frames (photodiode-inferred and Onset-reported) | **0** |
| Latency trend | < 0.1 ms/min (no slow drift between logs and reality) |
| Constant lag | measured and reported (mean `latency_ms`, `true_minus_logged_ms`); repeatable within ~1 ms across runs |
| `planned_error_ms` − mean lag | within 0 … 1 frame (onsets snap to the next refresh) |

`analyze_timing.py` prints PASS/CHECK lines for the first five automatically.
A CHECK is a prompt to look at the table and plot, not automatically a
failure — e.g. a single late event right after the run starts may be the
display waking up.

## Limits of this method

* Displays update top to bottom, so a bottom-left patch lights up almost a
  full frame after the top of the screen and about half a frame after the
  centre, where stimuli usually are. That is a constant offset; if you need
  the absolute value for central stimuli, measure it once with the sensor over
  the centre of a full-screen white stimulus.
* The probe's microsecond clock is only as good as its crystal; the fit removes
  constant rate error, not temperature wander (negligible over a run).
* Comparator mode resolves ~4 µs (Uno R3) / ~1 µs (R4); analog mode ~0.1–0.5 ms.
