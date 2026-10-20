# Onset

Open-source stimulus presentation software in Rust, meant to replace E-Prime for fMRI experiments at a BCM lab. The goal is a real, production-quality tool that is faster, nicer looking and more fun to run than E-Prime. The lab does not have to adopt it.

## How we work

**Claude writes Onset's code.** (Until 2026-10-20 the project was set up as Rust exercises for the owner to fill in. The owner switched to this on 2026-10-20, so don't hand out exercises or `todo!()` stubs. Older code still has a tutorial tone in places.)

- Explain plainly what each change does and why, including the scanner and hardware side when it comes up. The owner knows Python best, so compare to Python when explaining Rust.
- Run `cargo test` and `cargo clippy --all-targets` before calling anything done.
- Agents and workflows may be used to build Onset, including writing its code.
- Claude has full control over git for this repo: commit, create branches and push without asking. Commit finished work so the other Mac gets it.

## Roadmap

| # | Piece | Status |
|---|---|---|
| 1 | Seconds ↔ frames conversion (`src/timing.rs`) | done |
| 2 | Rejecting bad inputs (negative onsets, 0 Hz) (`src/timing.rs`) | done |
| 3 | Reading the experiment's TOML file | |
| 4 | Trial schedule from a TSV | |
| 5 | Writing BIDS events.tsv | |
| 6 | Fake scanner and real trigger input | **done**: fake scanner and TR stats in `src/scanner.rs`, `trigger-log` tool in `src/bin/`. Real pulses confirmed with a phantom on 2026-10-20 (see The scanner rig) |
| 7 | First window, photodiode square | **done**: `src/render.rs`, `src/present.rs` (Metal present path, exact per-frame on-screen times) |
| 8 | Frame loop: a checkerboard running at the desk | **done**: `src/bin/checkerboard.rs`, `src/planner.rs`. Ran at the scanner with real triggers on 2026-10-20; holds the last rest screen until the scanner stops, ignores pulses from a scan already under way. Open: about 1 real dropped frame every 208 frames (1.73 s) on the scanner laptop, cause unknown; the planner catches up |
| 9 | The lab's real paradigm | |

Update the Status column as pieces are finished, so it stays in sync across machines.

## The scanner rig

- **Scanner:** Siemens MAGNETOM Prisma at BCM's Core for Advanced MRI (CAMRI).
- **Display:** BOLDscreen32, 1920×1080 at 120 Hz (one frame = 8.33 ms). The Mac sees it as a separate screen, not mirrored. Checked from the Mac at the scanner on 2026-10-20. The control-room sticky notes say "32 BOLD 120 Hz" and "3D BOLD 60 Hz".
- **Video path:** Mac → white USB-C video adapter → Gefen DVI KVM switch in the control room → BOLDscreen. A control-room monitor labeled "SCANNER VIEW" is meant to show what the participant sees. The E-Prime stim PC runs Windows.
- **Trigger:** Current Designs fORP 932 interface (silver box on the control-room desk, 2×5 green button LEDs, small display and one knob). Confirmed from the Mac on 2026-10-20: USB vendor 0x181b, product 0x0008, shows up as an HID keyboard + mouse + joystick, no serial port. Its USB-A cable comes bundled with the Gefen KVM cable; a laptop needs a USB-C to USB-A adapter. Its display read "HH-UNKNOWN" / "HID KEY BYGRT". In that mode buttons send `b y g r` and the trigger sends `t`. macOS shows the Keyboard Setup Assistant the first time; just close it.
- **Trigger, confirmed with a phantom on 2026-10-20** (`logs/trigger-dummy-scan-1.tsv`): one `t` per volume. TR 2.8 s measured 2.8001 s, TR 1.5 s measured 1.4991 s, SD about 4 ms (keyboard and terminal delay, not the scanner). 22 measurements gave exactly 22 pulses, so the scanner's dummy volumes send no pulse and the first pulse is the first saved volume. The localizer sends no pulses.
- **Trigger wiring:** the scanner's trigger arrives as light on an orange fiber with a blue connector, plugged into the fORP's fiber trigger input on the back. The gold SMB jack above it is the electrical (TTL) trigger input and is empty. The bottom LED of the right-hand pair blinks on each pulse; the top one is power. **The blue connector is loose and held with tape**: with it not fully seated, no pulses got through, and one run dropped 16 pulses in the middle. Press it all the way in and check that the LED blinks before every run until it gets a proper connector. The fORP has no trigger settings in its menus; MANUAL CONFIG only picks the button pad.

## Design decisions

- **Rendering:** winit + wgpu, vsync on, log every frame's present time, detect dropped frames.
- **Scanner sync:** wait for the MRI trigger (serial through the `serialport` crate, or a keypress if the trigger box acts as a keyboard). Timestamp every event relative to the first TR.
- **Experiments** are defined in TOML, not code.
- **Output:** BIDS `events.tsv` plus a raw timing log.
- **Timing check:** a photodiode on a flashing square in a screen corner, with an Arduino timestamping the photodiode and the trigger on one clock. Compare against E-Prime on the same paradigm.
- **MVP:** port one real lab paradigm end to end before building any editor UI.

## Open questions

- Which paradigm to port first (an E-Prime `.es3`/`.ebs3` file or a written description would be ideal). Placeholder examples until then: a checkerboard block localizer and a flanker task with jittered timing.
- The rig config should support the fORP in keyboard mode (what this scanner uses), a serial trigger box, and a simulated scanner.

## Setup on a new Mac

Work happens on more than one Mac, and only this repo is shared between them.

- Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- If links fail with a macOS SDK error: the installed linker in Apple's Command Line Tools can't read the macOS 27.0 SDK. Pin builds to an installed older SDK (26.5 worked) by setting `SDKROOT` under `[env]` in `~/.cargo/config.toml`. Keep that file out of the repo, because the SDK path is machine-specific. Remove the pin once a Command Line Tools update fixes the mismatch.
