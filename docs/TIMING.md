# How Onset handles timing

## Clock and t = 0

All timestamps come from one monotonic clock (`std::time::Instant`: `mach_absolute_time` on macOS, QPC on Windows). The run starts at **t = 0, the time the first kept trigger arrived**. With `dummy_volumes = N`, the first N triggers are logged and ignored. BIDS onsets are relative to the first volume you keep, so set `dummy_volumes` to match the volumes you discard. Every trigger is logged in `inputs.tsv`, so you can re-align afterwards if needed.

## Absolute scheduling

The schedule compiler turns the experiment into a list of events with **absolute planned onsets** in seconds after t = 0. Back-to-back events are planned by adding up durations at compile time, which is exact arithmetic. At runtime nothing is ever added up: each frame asks "which event's onset is nearest this flip?" A dropped frame, slow disk, or busy CPU therefore delays at most the frame it hits; the next frame is back on schedule. E-Prime's "cumulative" mode does the same, but there it's opt-in; here it's the only mode.

## Frames

The display can only change at a refresh (every 16.67 ms at 60 Hz). Onset uses:

- `PresentMode::Fifo` (vsync, no tearing), with `desired_maximum_frame_latency = 1`
- so `get_current_texture()` blocks until the previous frame has reached the screen.

The frame loop:

1. **Acquire** the next frame. This blocks until the previous frame flips, so the timestamp taken right after is that previous frame's **flip time**.
2. The frame being drawn will flip about one refresh period later: `predicted_flip = acquired + period`.
3. Start every event with `planned_onset ≤ predicted_flip + period/2`, so each onset lands on the nearest refresh. End the current event the same way.
4. Draw, present, and send the frame record to the log thread.

The **measured onset** of an event is the flip time of its first frame, which is known one frame later. Events are logged with `onset` (measured), `planned_onset`, `onset_delay_ms` and `frames`. Because the display clock runs independently of the scanner, the delays fall anywhere in ±½ frame (±8.3 ms at 60 Hz); that's normal and is reported honestly. Durations are exact frame multiples: 0.8 s at 60 Hz is always 48 frames.

The **refresh period** is measured (median frame interval) during the instructions and waiting screens and locked when the run starts. It's recorded as `measured_refresh_hz` in `run.json`.

**Dropped frames:** an interval longer than 1.5 periods counts `round(interval / period) − 1` missed refreshes. These appear in `frames.tsv`, the status line, and `run.json`.

**Remaining error:** the flip time is when the GPU swapped buffers. The display or projector adds its own fixed lag on top, often 1–3 frames for MR projectors. Measure it with the photodiode rig (see `docs/VALIDATION.md`). A constant lag is harmless for fMRI; jitter is what matters.

## Inputs

- **Serial** (trigger boxes, button boxes, Arduino): a dedicated thread blocks on the port and timestamps each byte the moment `read()` returns, at sub-millisecond precision. For FTDI adapters on Windows, set the latency timer to 1 ms in Device Manager (the default is 16 ms).
- **Keyboard** (fORP in keyboard mode): winit delivers key events on the main thread between frames. They're timestamped as soon as they're received, so precision is about one frame, depending on when the OS delivers the event.
- **Simulated scanner**: a thread that sleeps, then spins, until each multiple of the TR.

Inputs go through a channel and are processed at the start of the next frame, but always with their arrival timestamps. Reaction time = response timestamp − the stimulus's measured onset.

## No I/O on the render thread

Disk writes (`frames.tsv`, `inputs.tsv`, `eventlog.tsv`, `run.json`) and terminal output happen on their own threads, fed by channels. The render thread only does in-memory work. Text is laid out and glyphs rasterized before t = 0 (`prewarm`), and images are decoded and uploaded before the run.

## Process priority

On macOS the render thread runs at the `USER_INTERACTIVE` QoS class. On Windows the process runs at `HIGH_PRIORITY_CLASS` with 1 ms timer resolution. Use **fullscreen** at the scanner: on macOS a fullscreen window bypasses most compositor work, and in testing, windowed mode on a busy desktop dropped about one frame every 10–20 s.

## Checking a setup

```bash
onset timing-test --rig scanner.toml --seconds 30
```

This alternates black/white every frame and reports the measured refresh, interval SD, and dropped frames. With the photodiode taped on the corner patch, the Arduino rig measures the real on-screen timing.
