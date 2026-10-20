# Onset

Frame-accurate, open-source stimulus presentation for fMRI and beyond, written in Rust.

Developed and tested at the Center for Advanced Magnetic Resonance Imaging (CAMRI), Baylor College of Medicine, Houston, TX.

- **Experiments are TOML files**, not code. Trial lists come from a TSV, so schedules from optseq2 / AFNI drop straight in.
- **Scanner-synced timing.** Every onset is scheduled as an absolute time after the first kept trigger, and snaps to the nearest refresh. A late frame can never push the rest of the run off schedule.
- **Honest logs.** Every frame, trigger, and button press is timestamped. Each event records its planned onset, its measured onset, and the delay between them.
- **BIDS output.** `sub-01_task-x_run-01_events.tsv` + `events.json` are ready for fMRIPrep, nilearn, FSL, or SPM.
- **Built-in timing checks.** A photodiode patch flashes in a screen corner at every onset. `onset timing-test` measures frame pacing, and with a small Arduino rig `onset probe` and `onset check-timing` measure when light really appeared, onset by onset.
- **One program.** Running, checking timing and building schedules are all `onset` commands, with nothing else to install.

## Quick start

```bash
# install Rust once: https://rustup.rs
cargo build --release

# Run the flanker example in a window with a fake scanner (TR 2 s)
# and a simulated participant pressing buttons:
./target/release/onset run examples/flanker/experiment.toml \
    --rig examples/rigs/dev.toml --simulate --robot --sub 01
```

Press **Space** to get past the instructions; the fake scanner starts 1.5 s later. **Esc** aborts at any time, and data recorded so far is always saved. Results go to `data/sub-01/`.

Other commands:

```bash
onset validate examples/flanker/experiment.toml   # check without running
onset schedule examples/flanker/experiment.toml   # print every planned event
onset timing-test --seconds 10                    # refresh rate, jitter, dropped frames
onset displays                                    # list monitors for the rig file
onset ports                                       # list serial ports (trigger boxes)
onset make-schedule examples/nsd/experiment.toml  # build a trial list from a folder of images
onset probe --out probe.tsv                       # record the photodiode/trigger probe
onset check-timing --probe probe.tsv --run data/sub-01/onset   # logged vs measured onsets
```

`onset <command> --help` describes each command's options.

## Examples

| | |
|---|---|
| [`examples/flanker`](examples/flanker) | Event-related flanker task: jittered onsets from a TSV, button responses, ✓/✗ feedback |
| [`examples/checkerboard-localizer`](examples/checkerboard-localizer) | Block-design 8 Hz radial checkerboard vs. fixation |
| [`examples/nsd`](examples/nsd) | Image paradigm modeled on the Natural Scenes Dataset core run: 3 s images + 1 s gap, continuous old/new recognition. Ships with placeholder images; see its [README](examples/nsd/README.md) for using real images |
| [`examples/nsd-resting`](examples/nsd-resting) | Resting-state run with an instructed-breath cue |

`--schedule FILE` (on `run`, `validate` and `schedule`) swaps in a different trial list without editing the experiment; a copy is saved with the run's logs.

Set `ONSET_SNAPSHOTS=some/folder` to save a PNG of every event's first frame. That's handy for figures and for checking stimuli, but it disturbs timing, so never use it in a real session.

## At the scanner

1. Copy `examples/rigs/scanner-forp.toml` (a 120 Hz MR display and a fORP trigger box read over USB) and adjust it. Onset uses the first display that isn't the laptop's own screen; set `[display] monitor` to override it (`onset displays` lists them). Set `[trigger]` to match your trigger box: a fORP in keyboard mode types `t` (BYGRT mode) or `5` (12345 mode) on each TR; serial boxes use `source = "serial"`. Connect the stimulus display as a separate display, not mirrored.
2. Run `onset inputs --rig your-rig.toml` and press each button (and let the scanner send a few pulses) to check every input arrives. Then run `onset timing-test --rig your-rig.toml` on the stimulus display. You want 0 dropped frames.
3. Run the experiment:

   ```bash
   onset run task.toml --rig your-rig.toml --sub 07 --ses 1
   ```

   The run number is chosen automatically: Onset picks the next run without data, and never overwrites a run unless you pass `--force`. The terminal shows a live status line: elapsed time, TR count, trial, dropped frames, and the last response.

   The run follows the scanner at both ends. If the scan is still going when the stimulus ends, Onset holds the final screen until the pulses stop (Esc stops waiting), and it warns if the scan stopped before the stimulus did. Pulses that are already arriving when Onset gets ready (the end of an earlier scan) are ignored rather than starting the run; Onset says so and waits for that scan to stop.

## Output

```
data/sub-07/ses-1/
├── func/
│   ├── sub-07_ses-1_task-flanker_run-01_events.tsv    BIDS events (measured onsets)
│   └── sub-07_ses-1_task-flanker_run-01_events.json   column descriptions
└── onset/
    ├── …_eventlog.tsv     every event: planned vs measured onset, delay, frames, response, RT
    ├── …_frames.tsv       every frame: time, when it was on screen, missed refreshes, photodiode state
    ├── …_inputs.tsv       every trigger and key/button press with timestamps
    ├── …_run.json         provenance: versions, GPU, display Hz, seed, start/end, summary
    └── …_experiment.toml, …_rig.toml, …_schedule.tsv   exact copies of the inputs
```

Logs are written on a background thread and flushed continuously, so a crash or Ctrl-C mid-run keeps everything up to that moment.

## Writing experiments

See [docs/SCHEMA.md](docs/SCHEMA.md) for the full format. The basic idea:

```toml
[experiment]
name = "Flanker"
task = "flanker"            # BIDS task label
baseline = "fixation"       # shown whenever nothing else is scheduled

[stimuli.fixation]
type = "fixation"

[stimuli.arrows]
type = "text"
text = "{arrows}"           # filled from the schedule's "arrows" column

[[trial.events]]
name = "target"
stimulus = "arrows"
duration = 0.8
response = { buttons = ["1", "2"], correct = "{answer}", window = 1.5 }

[[trial.events]]
name = "feedback"
stimulus = "fb_{outcome}"   # fb_correct / fb_incorrect / fb_miss
duration = 0.5

[schedule]
file = "schedule.tsv"       # one row per trial
onset_column = "onset"      # absolute onsets, e.g. from optseq2
```

## How timing works

See [docs/TIMING.md](docs/TIMING.md). In short, rendering uses vsync, and Onset presents each frame through Metal itself so the display reports when that frame was really on screen. Those reports become the measured onsets, and a frame planner uses them to predict which refresh each new frame lands on, so CPU jitter can't skip frames or log drops that didn't happen. Each frame shows the event whose planned onset is nearest that frame's flip. The photodiode rig ([hardware/](hardware/)) measures the remaining display or projector lag.

## Status

v0.1, under active development. macOS only (by design). Not yet supported: audio, video, scripting/staircases, eye tracking, and a graphical editor.

## License

MIT OR Apache-2.0
