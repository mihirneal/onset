# Onset

Open-source stimulus presentation software in Rust, meant to replace E-Prime for fMRI experiments at a BCM lab. The goal is a real, production-quality tool that is faster, nicer looking and more fun to run than E-Prime. The lab does not have to adopt it.

**macOS only.** The stimulus computer at the scanner will be a Mac. Don't add Windows or Linux code, docs, or CI.

## How we work

Since 2026-10-13, Claude builds and fixes Onset directly. The owner is a Rust beginner who knows Python best and knows the fMRI side well (TRs, triggers, onsets, BIDS):

- When a change uses a Rust idea worth knowing, explain it briefly by comparing it to Python. Don't explain the fMRI side.
- If they ask for an exercise instead, use the "tests as spec" style: signatures with `todo!()` plus tests, checked in a scratch copy first (a correct solution passes, a common beginner mistake fails), then a review and `cargo clippy`. Exercise 1 (`src/timing.rs`) is theirs. Keep their solution as written.
- Work happens on more than one Mac, so keep this file current when decisions change.

## Layout

| Path | What it is |
|---|---|
| `src/experiment.rs`, `src/rig.rs` | TOML formats: experiment (portable) and rig (display, trigger, buttons, photodiode) |
| `src/schedule.rs` | Compiles an experiment + schedule TSV into absolute planned onsets; validation |
| `src/run.rs` | The run state machine: picks what each frame shows, handles triggers/responses, produces records. No GPU code, unit-tested with synthetic frame times |
| `src/render.rs`, `src/shaders/` | wgpu renderer: shapes, circles, radial checkerboard, images, text (glyphon), photodiode patch, offscreen snapshots |
| `src/output.rs` | Writer thread: BIDS events.tsv/json, raw frames/inputs/eventlog TSVs, run.json |
| `src/input.rs` | Serial and simulated-scanner threads; every input timestamped on arrival |
| `src/main.rs`, `src/console.rs`, `src/macos.rs` | CLI, window and frame loop, operator status line, macOS priority and window placement |
| `examples/` | flanker, checkerboard localizer, NSD core run, NSD resting state, rig files |
| `hardware/`, `tools/`, `docs/VALIDATION.md` | Photodiode/Arduino timing kit and analysis scripts (not yet tested on hardware) |
| `docs/SCHEMA.md`, `docs/TIMING.md` | Format reference and timing model; update them when behavior changes |

## Checks

- `cargo test`, `cargo clippy -- -D warnings`, `onset validate` on every example, and `python -m pytest tools` (in a venv). CI on GitHub runs the same on macOS.
- End-to-end without hardware: `onset run <exp> --rig examples/rigs/dev.toml --simulate --robot --sub 99 --out <scratch dir>`. For a quick check, copy the experiment with the `instructions` removed and fewer volumes. `ONSET_SNAPSHOTS=<dir>` saves each event's first frame as a PNG; it disturbs timing, so use it only for checking visuals.
- Test runs open windows on the owner's screen. Keep them short, and ask before anything fullscreen.

## Design decisions

- **Rendering:** winit + wgpu, Fifo vsync with one frame in flight. The flip time of frame k is taken as the moment frame k+1 is acquired. Every frame and any dropped frames are logged.
- **Scheduling:** onsets are absolute times after the first kept trigger (`dummy_volumes` are skipped), snapped to the nearest refresh. Durations are never added up at runtime.
- **Scanner sync:** keyboard trigger (a fORP types "5"), serial trigger box, or a simulated scanner. The window must keep keyboard focus for keyboard triggers; Onset warns loudly if it loses focus.
- **Experiments** are TOML; trial lists are TSV files (`--schedule` swaps one in per run).
- **Output:** BIDS `events.tsv` with measured onsets, plus raw logs streamed from a background thread so crashes keep data.
- **Timing check:** photodiode on the corner patch, with an Arduino timestamping the photodiode and the trigger on one clock. Compare against E-Prime on the same paradigm.
- **NSD:** stimuli may not be redistributed. NSD images live outside the repo (`~/nsd/`), and `.gitignore` blocks `*_nsd*.png`. Never commit them.

## Status and open questions

- v0.1 runs end to end on the dev Mac in a window with a simulated scanner and participant. Not yet done: fullscreen, a second display or projector, a full-length run, real fORP/trigger/serial hardware, and the photodiode rig.
- Needed from the owner: the scanner display (model, resolution, refresh rate, viewing distance) to convert NSD's degrees to pixels; the trigger and button hardware and what keys/bytes they send.
- Not supported yet: audio, video, scripting/staircases, eye tracking, pRF/fLoc-style animated or masked stimuli, editor UI.

## Setup on a new Mac

- Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- If links fail with a macOS SDK error: the installed linker in Apple's Command Line Tools can't read the macOS 27.0 SDK. Pin builds to an installed older SDK (26.5 worked) by setting `SDKROOT` under `[env]` in `~/.cargo/config.toml`. Keep that file out of the repo, because the SDK path is machine-specific. Remove the pin once a Command Line Tools update fixes the mismatch.
- NSD images: `aws s3 sync s3://natural-scenes-dataset/nsddata/stimuli/nsd/shared1000/ ~/nsd/shared1000/ --no-sign-request` (after accepting the NSD terms).
