# Scanner session checklist

Steps for a session with Onset at the scanner. Written for CAMRI (Siemens
Prisma, BOLDscreen32, fORP 932) but the order works anywhere; the rig file
holds the site details.

## Before you leave your desk

- [ ] `git pull` on the stimulus Mac, then `cargo build --release`. Onset is
      `./target/release/onset` below.
- [ ] Any licensed stimuli are on the Mac, **outside the repo** (e.g.
      `~/stimuli/`). They never go in git (`.gitignore` blocks the usual
      places).
- [ ] Per-run schedules are made (see "Image runs" below), or you know which
      experiment files you'll run.

## Setting up at the scanner

1. **Display:** connect the stimulus display as a **separate display, not
   mirrored** (System Settings → Displays). It should show 1920×1080 at
   120 Hz: `./target/release/onset displays`.
2. **Trigger box:** plug the fORP's USB-A cable into the Mac (USB-C
   adapter). Close the Keyboard Setup Assistant if it appears. Press the
   blue fiber connector on the back of the fORP all the way in (it's loose).
3. **Inputs check:** `./target/release/onset inputs --rig examples/rigs/scanner-forp.toml`,
   then press each pad button (right pad = 1, 2; left pad = 3, 4) and,
   if you can, run a few volumes of any EPI: each should print as it
   arrives. It stops by itself when the scan ends, or with Ctrl-C.
   First time on a Mac, or in a new terminal app: allow **Input Monitoring**
   and **Accessibility** for that app when asked (Onset says if one is missing).
4. **Timing check:** `./target/release/onset timing-test --rig examples/rigs/scanner-forp.toml --seconds 10`
   (fullscreen on the BOLDscreen for 10 s). It should report about 120 Hz.

## Each run

1. Load the sequence on the console. Its length must cover the stimulus run
   (Onset prints the run length; volumes × TR ≥ that).
2. Start Onset:
   ```bash
   ./target/release/onset run <experiment.toml> --rig examples/rigs/scanner-forp.toml --sub <ID> --ses <N> --run <N>
   ```
   Add `--schedule <file.tsv>` for a per-run trial list, and `--name "First"`
   for a greeting on experiments with a welcome screen (shown only, never
   saved).
3. Check SCANNER VIEW shows the instructions, then **start the scan. Don't
   touch the Mac**: the first pulse starts the run, and triggers and buttons
   are read directly, whatever window is in front.
4. To stop early: Ctrl-C in Onset's terminal (data so far is saved).
5. Afterwards Onset prints the summary: triggers, frames, onsets. Each run's
   data is in `data/sub-<ID>/` (BIDS `func/` and raw logs in `onset/`).

## Image runs

Example: the image paradigm in `examples/nsd` (3 s images + 1 s gaps,
continuous old/new recognition, 300 s per run). Real images come from the
dataset's shared set, after accepting its terms
(see `examples/nsd/README.md`):

```bash
aws s3 sync s3://natural-scenes-dataset/nsddata/stimuli/nsd/shared1000/ ~/stimuli/shared1000/ --no-sign-request
```

One schedule per run; `--history` keeps old/new right across runs for this
participant:

```bash
./target/release/onset make-schedule examples/nsd/experiment.toml --run 1 \
    --images ~/stimuli/shared1000 --history ~/stimuli/sub-01_seen.txt --out ~/stimuli/sub-01/run01.tsv
./target/release/onset run examples/nsd/experiment.toml --schedule ~/stimuli/sub-01/run01.tsv \
    --rig examples/rigs/scanner-forp.toml --sub 01 --ses 1 --run 1
```

Scanner length for one run: 300.8 s. At TR 1.6 s that's 188 volumes; at
TR 1.5 s, 201.

## Things to measure or check once per site

- [ ] **Viewing distance** from the participant's eyes to the screen (via
      the head-coil mirror), and the visible screen width. Image sizes in
      degrees depend on it.
- [ ] **The real protocol** (see "Proposed 3T protocol" in CLAUDE.md): is the
      sequence installed, which head coil, does the TR fit, and does it send
      one trigger per volume (`onset inputs`).
- [ ] Which fORP pad goes in which hand (keys belong to the pad: keep the
      r/g pad left, y/b pad right). Labelling them L/R helps.

## Afterwards

- [ ] Copy `data/` to the lab's storage. It never goes in git.
- [ ] Note anything odd (missed triggers, dropped frames, focus or
      permission prompts) in CLAUDE.md or an issue.
