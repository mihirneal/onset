# Onset

General-purpose, open-source stimulus presentation software in Rust: lean, fast and modern. A real, production-quality tool for anyone running experiments (fMRI first). Site rigs and paradigms are examples, not the scope.

What that means for every change:
- **General, not site-specific.** Site details belong in rig files and examples, never hard-coded. No paradigm is "the" goal; each is one example of what Onset runs.
- **Lean.** One Rust program does everything, from running to checking timing to building schedules, with nothing else to install. Avoid extra languages and runtimes: the v0.1 Python helpers are now `onset probe`, `onset check-timing` and `onset make-schedule`.
- **Fast and honest timing.** Frame-accurate presentation with measured (not assumed) onsets in the logs.
- **Modern and pleasant.** Clear terminal output, good defaults, TOML/TSV instead of code, BIDS output.
- **Respectful.** Describe Onset on its own merits. Don't name or criticize other presentation software in docs, messages or marketing; naming a file format Onset can read (e.g. an export it imports) is fine.

**macOS only.** Onset targets Mac stimulus computers. Don't add Windows or Linux code, docs, or CI.

## Notes stay out of the repo

This repo is public, general software. Site details, session logs, checklists, scan protocols, plans and anything about particular people go in a git-ignored `CLAUDE.local.md` (Claude Code reads it automatically), never in tracked files, commit messages or PR descriptions.

## Layout

| Path | What it is |
|---|---|
| `src/experiment.rs`, `src/rig.rs` | TOML formats: experiment (portable) and rig (display, trigger, buttons, photodiode) |
| `src/schedule.rs` | Compiles an experiment + schedule TSV into absolute planned onsets; validation |
| `src/run.rs` | The run state machine: picks what each frame shows, handles triggers/responses, produces records. No GPU code, unit-tested with synthetic frame times |
| `src/render.rs`, `src/shaders/` | wgpu renderer: shapes, circles, radial checkerboard, images, text (glyphon), photodiode patch, welcome screen, offscreen snapshots (also headless, no window) |
| `src/output.rs` | Writer thread: BIDS events.tsv/json, raw frames/inputs/eventlog TSVs, run.json |
| `src/input.rs`, `src/keyfilter.rs` | Input threads (USB HID trigger boxes, serial, simulated scanner), every input timestamped on arrival; filtering a shared USB trigger box's keystrokes out of other apps |
| `src/main.rs`, `src/console.rs`, `src/macos.rs` | CLI, window and frame loop, operator status line, macOS priority and window placement |
| `src/probe.rs` | `onset probe`: records the Arduino timing probe's serial lines to a TSV |
| `src/check_timing/` | `onset check-timing`: clock fit on shared triggers, light-edge matching and statistics for Onset runs, `timing-test` and E-Prime exports. Tested in `tests/check_timing.rs` |
| `src/make_schedule.rs` | `onset make-schedule`: an experiment's schedule TSV from a folder of images, per its `[schedule.generate]` design |
| `examples/` | flanker, checkerboard localizer, NSD core run, NSD resting state, rig files |
| `hardware/`, `docs/VALIDATION.md` | Photodiode/Arduino timing kit and the measurement protocol (not yet tested on hardware) |
| `docs/SCHEMA.md`, `docs/TIMING.md` | Format reference and timing model; update them when behavior changes |

## Checks

- `cargo test`, `cargo clippy -- -D warnings`, and `onset validate` on every example. CI on GitHub runs the same on macOS.
- End-to-end without hardware: `onset run <exp> --rig examples/rigs/dev.toml --simulate --robot --sub 99 --out <scratch dir>`. For a quick check, copy the experiment with the `instructions` removed and fewer volumes. `ONSET_SNAPSHOTS=<dir>` saves each event's first frame as a PNG; it disturbs timing, so use it only for checking visuals.
- Test runs open windows on the developer's screen. Keep them short, and ask before anything fullscreen.
- Visual checks without a window: `ONSET_WELCOME_SNAPSHOTS=<dir> cargo test --test welcome` renders the welcome screens of every example offscreen (`Renderer::headless`) and saves PNGs.

## Design decisions

- **Rendering:** winit + wgpu, Fifo vsync with one frame in flight. The flip time of frame k is taken as the moment frame k+1 is acquired. Every frame and any dropped frames are logged.
- **Scheduling:** onsets are absolute times after the first kept trigger (`dummy_volumes` are skipped), snapped to the nearest refresh. Durations are never added up at runtime.
- **Scanner sync:** a USB keyboard-style trigger box read directly (`source = "hid"`: works whatever window has focus, timestamped on arrival on its own thread, opened exclusively so its keys don't leak into other apps; macOS Input Monitoring permission once), the focused window's keyboard (`"keyboard"`; Onset warns loudly if it loses focus), a serial trigger box, or a simulated scanner. A fORP 932 types "t" in its BYGRT mode, "5" in 12345 mode. `onset inputs --rig <rig>` prints every trigger and press as it arrives.
- **Experiments** are TOML; trial lists are TSV files (`--schedule` swaps one in per run).
- **Output:** BIDS `events.tsv` with measured onsets, plus raw logs streamed from a background thread so crashes keep data.
- **Timing check (optional):** photodiode on the corner patch, with an Arduino timestamping the photodiode and the trigger on one clock (`onset probe` records it, `onset check-timing` analyses it). The same check works for other presentation software on the same paradigm, for a like-for-like comparison. The analysis was ported from Python and reproduced its summaries on the same fixtures; keep its statistics and output names stable.
- **Schedules from images:** `onset make-schedule` draws with `rng::PyRandom`, a Mersenne Twister that matches Python's `random.Random`, so a `--seed`/`--run` gives the same schedule the old Python script made (`examples/nsd/schedule.tsv` is seed 1, run 1, and a test pins it). Run-time shuffling uses the separate `rng::Rng`.
- **Welcome screen:** an optional `[welcome]` table gives the screens before t0 an animated, white-on-black look: the participant's name large (`onset run --name`), spec-sheet labels, and a dot-matrix brain. Nothing of it runs after t0. The name is display-only by design: never in any log, events.tsv, run.json or other file (`tests/welcome.rs` checks); `--sub` stays the only identifier. The logo comes from the rig's `[site] logo`. Its fonts are bundled (`src/fonts`, SIL Open Font License).
- **Licensed stimuli** (e.g. the image set behind `examples/nsd`) may not be redistributed. Keep them outside the repo; `.gitignore` blocks the usual names and folders. Never commit them, or any participant data.

## Setup on a new Mac

- Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- If links fail with a macOS SDK error: the installed linker in Apple's Command Line Tools can't read the macOS 27.0 SDK. Pin builds to an installed older SDK (26.5 worked) by setting `SDKROOT` under `[env]` in `~/.cargo/config.toml`. Keep that file out of the repo, because the SDK path is machine-specific. Remove the pin once a Command Line Tools update fixes the mismatch.
