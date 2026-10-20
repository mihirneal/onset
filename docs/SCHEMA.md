# Experiment and rig file reference

Onset reads two TOML files:

- **experiment** (`experiment.toml`): what to show and when. Portable between computers.
- **rig** (`rig.toml`): this computer's display, trigger box, buttons, and photodiode. Swap rigs to move between your desk and the scanner.

Run `onset validate experiment.toml --rig rig.toml` after every edit. Unknown keys are errors, so typos don't slip through silently.

## Templates

Any string marked *template* can use `{column}` to insert a value from the current trial's schedule row. `{trial}` is the trial number. `{outcome}` is the result of this trial's response window: `correct`, `incorrect`, or `miss`, or `response`/`miss` when no correct answer is defined. Use `{{` and `}}` for literal braces.

## Experiment file

### `[experiment]`

| key | default | |
|---|---|---|
| `name` | required | Human-readable name |
| `task` | required | BIDS task label; letters and digits only |
| `background` | `"black"` | Screen color |
| `color` | `"white"` | Default color for text and fixation |
| `font_size` | `48` | Default text size (logical px) |
| `font` | system sans-serif | Font family name |
| `instructions` | none | Text shown before the run; the operator presses **Space** to continue |
| `waiting_text` | `"Get ready…"` | Shown while waiting for the first trigger |
| `baseline` | `"blank"` | Stimulus shown whenever no event is running |

Colors can be names (`white black gray red green blue yellow`), hex values (`"#808080"`, `"#fff"`, or `"#ff000080"` with alpha), or `[r, g, b]` / `[r, g, b, a]` from 0 to 255. Values go to the display without gamma conversion.

### `[welcome]`

An optional animated title screen behind the instructions and the "waiting for the scanner" screen. Leave the table out and those screens are plain text on the experiment's `background`, as before. `[welcome]` with no keys turns it on with the defaults.

```toml
[welcome]
style = "grid"
```

| key | default | |
|---|---|---|
| `style` | `"grid"` | `"grid"`: a dark horizon with a slowly approaching perspective grid, a soft scan beam and drifting particles |
| `title` | `[experiment] name` | Study title at the top of the screen |
| `greeting` | `"Welcome, {name}"` | Line under the title, shown only when `onset run --name "First Last"` is given; `{name}` is replaced |
| `accent` | `"#40c8ff"` | Color of the grid, glow lines and greeting |
| `color` | `"white"` | Color of all text on these screens, including the instructions |
| `font` | `"Avenir Next"` | Font of the title and greeting. The instructions keep `[experiment] font` |

The instructions and waiting text sit on a dark glass panel so they stay readable, and move down if they would run into the title. Text too tall to fit under the title keeps its place, and the title is left out. The waiting screen adds a slowly pulsing standby beacon.

The welcome screen is gone from the first kept trigger (t = 0): from then on only the experiment's own screens are drawn, and the welcome background costs nothing (no draw call, no per-frame work). Its animation runs on the frame clock, like checkerboard reversals. The photodiode patch is drawn on top of it exactly as on any other screen.

The participant's name is display only. It is never written to `events.tsv`, `run.json`, the raw logs, or any other file; the subject label (`--sub`) stays the only identifier in the data.

The welcome screen is dark. If the experiment's background is bright, the switch at t = 0 is a step in luminance, so start the run with a few seconds of baseline, as the examples do.

### `[scanner]`

| key | default | |
|---|---|---|
| `tr` | none | Repetition time (s). Used to check the run length |
| `dummy_volumes` | `0` | Triggers to ignore before t = 0 |
| `volumes` | none | With `tr`, the run lasts at least `volumes × tr` seconds |

### `[stimuli.<name>]`

Each stimulus has a `type`. Positions `x` and `y` are logical pixels from the screen center, with +y up. `blank` is built in.

| type | keys |
|---|---|
| `blank` | none |
| `fixation` | `size` (40), `thickness` (4), `color`, `x`, `y` |
| `text` | `text` (*template*), `size`, `color`, `x`, `y`. Multi-line text is centered. |
| `rect` | `width`, `height`, `color`, `x`, `y` |
| `circle` | `radius`, `color`, `border_width` (0), `border_color` (black), `x`, `y`. The border is inside the radius |
| `image` | `path` (*template*, relative to the experiment file), `width`, `height` (default: the image's pixel size; give one to keep the aspect ratio), `x`, `y` |
| `checkerboard` | Polar checkerboard: `radius` (500), `inner_radius` (12), `rings` (8, log-spaced), `wedges` (24), `reversal_hz` (8), `color_a`, `color_b`, `x`, `y` |
| `group` | `items = ["a", "b", …]`, drawn in order with later items on top. Text is always drawn above shapes and images. |

### `[trial]` and `[[trial.events]]`

A trial is a fixed sequence of events, each starting when the previous one ends. Every schedule row runs this sequence once.

| key | default | |
|---|---|---|
| `trial.trial_type` | the event name | BIDS `trial_type` (*template*) |
| `name` | required | Event name (the `event` column in logs) |
| `stimulus` | required | Stimulus name (*template*, may use `{outcome}`) |
| `duration` | required | Seconds, number or *template* (e.g. `"{iti}"`) |
| `trial_type` | trial's | Per-event override (*template*) |
| `log` | `true` | Include in BIDS events.tsv (always in the raw event log) |
| `response` | none | See below |

`response = { … }`:

| key | default | |
|---|---|---|
| `buttons` | required | Button labels (from the rig's `[buttons]`) that count |
| `correct` | none | Correct button (*template*, e.g. `"{answer}"`) |
| `window` | event duration | Seconds after onset during which responses count. Can extend into later events |
| `end_on_response` | `false` | Swap the stimulus for `after_response` once answered. Later events keep their scheduled times |
| `after_response` | baseline | What to show after a response when `end_on_response = true` |

Only the first response in a window counts. Every press is still recorded in `inputs.tsv`. Reaction time is measured from the event's **measured** onset (its first flip).

### `[schedule]`

| key | default | |
|---|---|---|
| `file` | none | TSV (or `.csv`) with a header row; one row per trial |
| `rows` | none | Rows written inline instead: `rows = [{ word = "red" }, …]` |
| `onset_column` | none | Column with each trial's absolute onset (seconds after t = 0). Without it, trials run back to back |
| `start_delay` | `0` | Back-to-back mode: seconds before the first trial |
| `order` | `"sequential"` | or `"shuffle"` |
| `repeat` | `1` | Repeat the rows this many times (before shuffling) |
| `seed` | random | Shuffle seed. A random seed is recorded in `run.json`; replay it with `--seed` |

With no `file` or `rows`, the trial runs once. `onset run --schedule other.tsv` replaces the schedule for one run, which is handy when every run has its own trial list. Shuffling or repeating can't be combined with `onset_column`; generate a shuffled schedule file instead.

### `[schedule.generate]`

A design for `onset make-schedule`, which writes the schedule file from a folder of images. Running the experiment ignores this table. Trials sit in fixed slots, `start + k × slot` seconds after t = 0; some slots stay blank (the baseline shows). With `repeat_prob`, some trials show an image seen before ("old") and the rest show unseen ones ("new"), for continuous recognition.

| key | default | |
|---|---|---|
| `images` | none | Folder of PNG/JPEG images, relative to the experiment file (`--images` overrides it) |
| `start` | `0` | Onset of the first slot (s) |
| `slot` | required | Seconds from one slot's onset to the next |
| `slots` | required | Slots per run |
| `trials` | `slots` | Image trials per run; the other slots are blank. A list such as `[63, 62]` cycles through runs (run 1 takes the first entry). Blanks are placed for the largest entry; runs with fewer trials leave their last image slots blank |
| `stretch` | blanks anywhere | `[min, max]`: blank slots split the image trials into stretches of min–max trials |
| `repeat_prob` | `0` | Chance that a trial shows an image seen before (`--repeat-prob` overrides it) |
| `new_answer`, `old_answer` | `"1"`, `"2"` | The `answer` column for unseen and seen images |
| `name_id` | none | `{ column = "id", prefix = "img" }` copies the number after `prefix` in each file name into its own column, e.g. an image database's ID |

```bash
onset make-schedule experiment.toml --run 2 --images ~/stimuli \
    --history ~/study/sub-01_seen.txt --out ~/study/sub-01/run02.tsv
```

The schedule gets the columns `onset`, `stim_file`, `memory` (`new`/`old`), `answer` and `image` (the file name without its extension), plus the `name_id` column. Images inside the experiment's folder are listed relative to it, others by absolute path. Without `--out` it overwrites the experiment's `[schedule] file`.

`--history` keeps old and new honest across runs and sessions: images listed in it count as seen, and each run's new images are appended to it. Use one history file per participant. `--seed` (default 1) and `--run` together fix the draw; the generator draws exactly like Python's `random.Random(seed × 1000 + run)`, so schedules made that way in Python come out identical. [`examples/nsd`](../examples/nsd) uses this.

## Rig file

```toml
[display]
monitor = 1            # index or part of the name, from `onset displays`. Default: a non-primary monitor if present
fullscreen = true
window_size = [1280, 800]   # when not fullscreen
expected_hz = 60       # warn if the measured refresh differs by > 1%
hide_cursor = true

[trigger]
source = "keyboard"    # "keyboard" | "hid" | "serial" | "simulated"
key = "5"              # keyboard / hid: the key the trigger box types
device = "181b:0008"   # hid: USB "vendor:product" of a keyboard-style trigger box, read
                       # directly whatever window has focus (`onset ports` lists them;
                       # macOS asks once for Input Monitoring). Its buttons come from it too.
port = "/dev/tty.usbserial-1410"   # serial: from `onset ports`
baud = 115200
serial_char = "5"      # serial: the character that means trigger; other characters are buttons
simulated_tr = 2.0     # simulated: seconds between fake triggers (the experiment's TR wins)

[buttons]              # raw key or serial character = label used in experiments
"1" = "index"
"2" = "middle"

[photodiode]
enabled = true
corner = "bottom-left" # bottom-right, top-left, top-right
size = 40              # logical px
flash_frames = 3       # white for this many frames at every onset, black otherwise

[output]
dir = "data"
```

Operator keys always work, whatever the rig: **Space** or **Enter** continues past the instructions, and **Esc** aborts and saves.
