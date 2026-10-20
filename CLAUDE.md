# Onset

General-purpose, open-source stimulus presentation software in Rust: lean, fast and modern. A real, production-quality tool for anyone running experiments (fMRI first), not a one-lab project. A BCM lab using the CAMRI scanner is the first user and test site; its rig and paradigms are examples, not the scope.

What that means for every change:
- **General, not site-specific.** Site details belong in rig files and examples, never hard-coded. No paradigm is "the" goal; each is one example of what Onset runs.
- **Lean.** One Rust program does everything, from running to checking timing to building schedules, with nothing else to install. Avoid extra languages and runtimes; Python helpers are being ported into `onset` subcommands.
- **Fast and honest timing.** Frame-accurate presentation with measured (not assumed) onsets in the logs.
- **Modern and pleasant.** Clear terminal output, good defaults, TOML/TSV instead of code, BIDS output.
- **Respectful.** Describe Onset on its own merits. Don't name or criticize other presentation software in docs, messages or marketing; naming a file format Onset can read (e.g. an export it imports) is fine.

**macOS only.** The stimulus computer at the scanner will be a Mac. Don't add Windows or Linux code, docs, or CI.

## How we work

Since 2026-10-13, Claude builds and fixes Onset directly. The owner is a Rust beginner who knows Python best and knows the fMRI side well (TRs, triggers, onsets, BIDS):

- When a change uses a Rust idea worth knowing, explain it briefly by comparing it to Python. Don't explain the fMRI side.
- If they ask for an exercise instead, use the "tests as spec" style: signatures with `todo!()` plus tests, checked in a scratch copy first (a correct solution passes, a common beginner mistake fails), then a review and `cargo clippy`. Exercise 1 (`src/timing.rs`) is theirs. Keep their solution as written.
- Work happens on more than one Mac, so keep this file current when decisions change. Pull before starting: on 2026-10-20 the scanner laptop hadn't pulled v0.1 and a whole session rebuilt pieces of it from the old exercise code (branch `scanner-day-2026-10-20`).
- Claude has full control over git for this repo: commit, create branches, push, open and merge PRs without asking.

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
- **Scanner sync:** keyboard trigger (the CAMRI fORP types "t" in its BYGRT mode, "5" in 12345 mode), serial trigger box, or a simulated scanner. The window must keep keyboard focus for keyboard triggers; Onset warns loudly if it loses focus.
- **Experiments** are TOML; trial lists are TSV files (`--schedule` swaps one in per run).
- **Output:** BIDS `events.tsv` with measured onsets, plus raw logs streamed from a background thread so crashes keep data.
- **Timing check:** photodiode on the corner patch, with an Arduino timestamping the photodiode and the trigger on one clock. The same check works for other presentation software on the same paradigm, for a like-for-like comparison.
- **NSD:** stimuli may not be redistributed. NSD images live outside the repo (`~/nsd/`), and `.gitignore` blocks `*_nsd*.png`. Never commit them.

## The scanner rig (CAMRI, confirmed 2026-10-20)

- **Scanner:** Siemens MAGNETOM Prisma at the Center for Advanced Magnetic Resonance Imaging (CAMRI), Baylor College of Medicine, Houston, TX. The README credits CAMRI as where Onset is developed and tested.
- **Display:** BOLDscreen32, 1920×1080 at 120 Hz. Connect it as a separate display, not mirrored: mirroring rendered at the laptop's 16:10 and scaled it onto the 16:9 panel at the laptop's 60 Hz. Video path: Mac → USB-C-to-DVI adapter → Gefen DVI KVM in the control room → BOLDscreen; the control-room "SCANNER VIEW" monitor shows what the participant sees. Rig file: `examples/rigs/scanner-forp.toml`.
- **Trigger:** Current Designs fORP 932 (USB 0x181b:0x0008, an HID keyboard + mouse + joystick, no serial port) in "HID KEY BYGRT" mode: one `t` per volume, buttons `b y g r`. Its USB-A cable is bundled with the Gefen KVM cable; a laptop needs a USB-C-to-A adapter. macOS shows the Keyboard Setup Assistant the first time; close it.
- **Trigger behaviour:** measured TR 2.8 s → 2.7997–2.8001 s, TR 1.5 s → 1.4991–1.4999 s, SD about 5 ms (keyboard and event-loop delay, not the scanner). Dummy volumes send no pulse, so the first pulse is the first saved volume. The localizer sends no pulses.
- **Trigger wiring:** the scanner's trigger arrives on an orange fiber with a blue connector, plugged into the fORP's fiber input. **The connector is loose and held with tape**: not fully seated, no pulses got through, and one run dropped 16 pulses. Check the fORP's trigger LED (bottom of the right-hand pair) blinks before every run until it's fixed.
- **The scanner laptop sleeps its displays after 2 idle minutes**, which blacks out the BOLDscreen. Onset now runs `caffeinate` for as long as it's open.

## Status and open questions

- v0.1 runs end to end on the dev Mac in a window with a simulated scanner and participant. Not yet run at the scanner: v0.1 itself. (On 2026-10-20 a separate checkerboard program on branch `scanner-day-2026-10-20` ran there with real triggers.) Not yet done: a full-length v0.1 run at the scanner, the button pads with a person pressing them, and the photodiode rig.
- Fullscreen is now a borderless window covering the chosen display (`macos::cover_screen`), because macOS fullscreen moved the window off the BOLDscreen at the scanner.
- **Frame timing at the BOLDscreen:** taking frame k's flip time as frame k+1's acquire time (`docs/TIMING.md`) is unreliable there. The compositor sometimes hands back buffers at uneven times (4.5 ms, 12 ms, ... pairs that add to two refreshes), so the "interval > 1.5 periods" rule logs drops that didn't happen and onsets jitter by up to a frame. The fix is proven on the `scanner-day-2026-10-20` branch: present through our own Metal command buffer and read each frame's real on-screen time from `MTLDrawable.presentedTime` (`src/present.rs`, `src/hostclock.rs`), with a frame planner that never lets CPU jitter skip frames (`src/planner.rs`, tested in `tests/planner_sim.rs`). Port it into `render.rs`/`run.rs`.
- **Run ends should follow the scanner:** a run that ends before the scan leaves the last volumes without a stimulus, and a scan that outlasts the run can start the next run with its last pulses (both happened at the scanner). The branch's `PulseWatch` (`src/scanner.rs`) holds the final screen until no pulse for max(5 s, 2.5 TR) and ignores pulses from a scan already under way. Port it into `run.rs`.
- **Unexplained drops:** about one real dropped frame every 208 frames (1.73 s) on the scanner laptop at 120 Hz, confirmed by Metal's own timestamps, with both timing methods. Cause unknown.
- Needed from the owner: the viewing distance to the BOLDscreen (paradigms given in degrees of visual angle need it to convert to pixels), and which finger each fORP button color is.
- Not supported yet: audio, video, scripting/staircases, eye tracking, pRF/fLoc-style animated or masked stimuli, editor UI.

## Setup on a new Mac

- Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- If links fail with a macOS SDK error: the installed linker in Apple's Command Line Tools can't read the macOS 27.0 SDK. Pin builds to an installed older SDK (26.5 worked) by setting `SDKROOT` under `[env]` in `~/.cargo/config.toml`. Keep that file out of the repo, because the SDK path is machine-specific. Remove the pin once a Command Line Tools update fixes the mismatch.
- NSD images: `aws s3 sync s3://natural-scenes-dataset/nsddata/stimuli/nsd/shared1000/ ~/nsd/shared1000/ --no-sign-request` (after accepting the NSD terms).
