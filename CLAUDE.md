# Onset

Open-source stimulus presentation software in Rust, meant to replace E-Prime for fMRI experiments at a BCM lab. The goal is a real, production-quality tool that is faster, nicer looking and more fun to run than E-Prime. The lab does not have to adopt it.

## The main point is learning Rust

The person working on this is learning Rust through this project. **They write the code.** Claude explains concepts, writes exercises and reviews their solutions.

- Exercises use tests as the spec: Claude writes function signatures with `todo!()` bodies plus tests, and the learner fills in the bodies. Don't fill in `todo!()` bodies or write feature code unless they ask for it directly.
- Before handing out an exercise, check it in a scratch copy: a correct solution should pass and a common beginner mistake should fail.
- After each exercise, review the code, show what more idiomatic Rust would look like, and have them run `cargo clippy`.
- The learner is new to Rust and knows Python best, so explain Rust ideas by comparing them to Python. They know the fMRI side well (TRs, triggers, onsets, BIDS), so there's no need to explain that.
- Don't use agents or workflows to build Onset itself.

## Curriculum

| # | Onset piece | Rust taught | Status |
|---|---|---|---|
| 1 | Seconds ↔ frames conversion (`src/timing.rs`) | functions, number types, `as` casts, tests | **in progress** |
| 2 | Rejecting bad inputs (negative onsets, 0 Hz) | `Result`/`Option` instead of exceptions | |
| 3 | Reading the experiment's TOML file | structs, enums, serde | |
| 4 | Trial schedule from a TSV | `Vec`, iterators, ownership and borrowing | |
| 5 | Writing BIDS events.tsv | traits, file I/O | |
| 6 | Fake scanner and serial trigger | threads, channels | |
| 7 | First window, photodiode square | winit and wgpu | |
| 8 | Frame loop: a checkerboard running at the desk | putting it all together | |
| 9 | The lab's real paradigm | — | |

Update the Status column as exercises are finished, so it stays in sync across machines.

Exercise 1 still to do: `cargo init --lib`, put `pub mod timing;` in `src/lib.rs`, replace the three `todo!()`s until `cargo test` passes, then `cargo clippy` and a review. The key lesson is the round-trip test: `as u64` truncates, so the answer has to round.

## Design decisions

- **Rendering:** winit + wgpu, vsync on, log every frame's present time, detect dropped frames.
- **Scanner sync:** wait for the MRI trigger (serial through the `serialport` crate, or a keypress if the trigger box acts as a keyboard). Timestamp every event relative to the first TR.
- **Experiments** are defined in TOML, not code.
- **Output:** BIDS `events.tsv` plus a raw timing log.
- **Timing check:** a photodiode on a flashing square in a screen corner, with an Arduino timestamping the photodiode and the trigger on one clock. Compare against E-Prime on the same paradigm.
- **MVP:** port one real lab paradigm end to end before building any editor UI.

## Open questions

- Which paradigm to port first (an E-Prime `.es3`/`.ebs3` file or a written description would be ideal). Placeholder examples until then: a checkerboard block localizer and a flanker task with jittered timing.
- Which trigger hardware the scanner uses (fORP / Current Designs / Arduino / other). The rig config should support fORP in keyboard mode, a serial trigger box, and a simulated scanner.

## Setup on a new Mac

Work happens on more than one Mac, and only this repo is shared between them.

- Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- If links fail with a macOS SDK error: the installed linker in Apple's Command Line Tools can't read the macOS 27.0 SDK. Pin builds to an installed older SDK (26.5 worked) by setting `SDKROOT` under `[env]` in `~/.cargo/config.toml`. Keep that file out of the repo, because the SDK path is machine-specific. Remove the pin once a Command Line Tools update fixes the mismatch.
