# How Onset handles timing

## Clock and t = 0

All timestamps come from one monotonic clock (`std::time::Instant`, which is `mach_absolute_time` on macOS; the display's reports of when frames were on screen use the same clock). The run starts at **t = 0, the time the first kept trigger arrived**. With `dummy_volumes = N`, the first N triggers are logged and ignored. BIDS onsets are relative to the first volume you keep, so set `dummy_volumes` to match the volumes you discard. Every trigger is logged in `inputs.tsv`, so you can re-align afterwards if needed.

## Absolute scheduling

The schedule compiler turns the experiment into a list of events with **absolute planned onsets** in seconds after t = 0. Back-to-back events are planned by adding up durations at compile time, which is exact arithmetic. At runtime nothing is ever added up: each frame asks "which event's onset is nearest this flip?" A dropped frame, slow disk, or busy CPU therefore delays at most the frame it hits; the next frame is back on schedule. This is the only scheduling mode.

## Frames

The display can only change at a refresh (every 8.33 ms at 120 Hz, 16.67 ms at 60 Hz). Onset draws with vsync on (`PresentMode::Fifo`, no tearing) and one frame in flight, so waiting for the next frame buffer paces the loop at the refresh rate.

When that wait returns is only a rough hint of where the display is. Under the macOS compositor, frame buffers can come back at uneven times (say 4.5 ms and then 12 ms apart at 120 Hz, a pair that adds up to two refreshes) even though the display shows every refresh. Timing frames from the CPU would read the long gaps as dropped frames and put onsets up to a frame off. So Onset asks the display itself.

**Presented times.** Onset renders each frame with wgpu but presents it through its own Metal command buffer, with a "presented handler" on the drawable (`src/present.rs`). Metal calls it once the frame is on screen and reports `presentedTime`, the host time it appeared. Host time is `mach_absolute_time`, the same clock as Onset's `Instant`, so one pair of readings converts between them for the whole session (`src/hostclock.rs`). Reports come back a few milliseconds later, in any order, on Metal's own thread.

**Predicting the flip.** A frame planner (`src/planner.rs`) turns the reports into the display's vsync grid: when each refresh happens and the exact refresh period (a least-squares fit over the last couple of thousand reports). For each new frame it predicts which refresh it will land on: the refresh after the previous frame's, unless we started drawing it so late (more than a quarter of a refresh) that that refresh had already passed. CPU jitter smaller than that never moves a frame, and after a frame that really was late the next one catches up instead of running late. Each new report re-anchors the grid, so predictions can't drift. The planner is tested against a simulated display in `tests/planner_sim.rs`.

The frame loop:

1. **Acquire** the next frame buffer and note the time.
2. Hand the display's reports to the planner, and ask it when this frame will be on screen (`predicted_flip`). Until it has enough reports to trust its grid (about half a second at 120 Hz, normally long before the first pulse), `predicted_flip = acquired + period`.
3. Start every event with `planned_onset ≤ predicted_flip + period/2`, so each onset lands on the nearest refresh. End the current event the same way.
4. Draw, present, and later file the frame's report.

The **measured onset** of an event is when its first frame was on screen, as Metal reported it, and its end is when the first frame that no longer showed it was on screen. Events are logged with `onset` (measured), `planned_onset`, `onset_delay_ms` and `frames`. Because the display clock runs independently of the scanner, the delays fall anywhere in ±½ frame (±4.2 ms at 120 Hz); that's normal and is reported honestly. Durations are exact frame multiples: 0.8 s at 60 Hz is always 48 frames.

**Without a report:** if Metal says a frame was never shown, or no report comes within 0.5 s, its flip is estimated the old way: as the moment the next frame buffer came back. `run.json` counts these as `flip_estimates`. If the Metal path isn't available at all, every flip is estimated like this, the planner is off, and `run.json` says so in `frame_timing`.

**Logs wait for reports.** Each frame's record is held until its report is in (or given up on), so `frames.tsv` has the presented time of every frame (`presented`, `presented_t`, and `report`: `shown`, `not_shown`, `no_report`, or `n/a` without reports). When the run ends, Onset keeps showing the final screen for up to 0.5 s until the last frames have been reported, then writes the logs.

The **refresh period** used by the run is locked when it starts: the planner's measured period if it has one, otherwise the median gap between frames during the instructions and waiting screens. It's recorded as `measured_refresh_hz` in `run.json`.

**Dropped frames** are counted from the presented times while the run is going: a gap of n refreshes between frames on screen means n − 1 refreshes showed the previous frame again (a frame without a report is assumed to have used a refresh, so it can't make a drop appear). Without reports, an acquire interval longer than 1.5 periods counts `round(interval / period) − 1`. Drops appear in `frames.tsv` (`missed`), the status line, and `run.json`.

**Remaining error:** `presentedTime` is when the frame went out to the display. The display or projector adds its own fixed lag on top, often 1–3 frames for MR projectors. Measure it with the photodiode rig (see `docs/VALIDATION.md`). A constant lag is harmless for fMRI; jitter is what matters.

## Following the scanner

A run's stimulus and its scan don't always end together, so Onset watches the pulses. The scanner counts as stopped after no pulse for max(5 s, 2.5 × the median TR), so a single missed pulse doesn't look like the end of a scan.

- **The scan outlasts the stimulus:** when the timeline ends but pulses are still coming, Onset holds the final screen (the baseline) and keeps logging pulses until the scanner stops, then saves the run. The status line says it's holding; Esc stops waiting (the run still counts as completed).
- **The scan stops before the stimulus:** Onset warns as soon as the pulses have stopped, carries on to the end, and warns again in the summary if the last pulse plus one TR came more than half a TR before the stimulus ended. `run.json` records `scan_until_s` and `scan_stopped_early`.
- **The tail of an earlier scan:** before the run's first pulse, a pulse that comes while a scan is already under way when Onset gets ready (within that quiet time of Onset being ready, or of the pulse before) is ignored, so the last pulses of the previous scan can't start the next run. Onset says so, and says again once that scan has stopped. Ignored pulses stay in `inputs.tsv` as triggers labelled `previous-scan`, and `run.json` counts them as `ignored_triggers`. Once the first pulse of a new scan is taken, the rest of that scan (dummy volumes included) counts as usual. A simulated scanner is Onset's own, so it skips this check, and it stops when the stimulus ends.

The first kept pulse starts the run from the instructions screen too, so nobody has to touch the stimulus computer.

## Inputs

- **Serial** (trigger boxes, button boxes, Arduino): a dedicated thread blocks on the port and timestamps each byte the moment `read()` returns, at sub-millisecond precision.
- **Keyboard** (fORP in keyboard mode): winit delivers key events on the main thread between frames. They're timestamped as soon as they're received, so precision is about one frame, depending on when the OS delivers the event.
- **Simulated scanner**: a thread that sleeps, then spins, until each multiple of the TR.

Inputs go through a channel and are processed at the start of the next frame, but always with their arrival timestamps. Reaction time = response timestamp − the stimulus's measured onset.

## No I/O on the render thread

Disk writes (`frames.tsv`, `inputs.tsv`, `eventlog.tsv`, `run.json`) and terminal output happen on their own threads, fed by channels. The render thread only does in-memory work. Text is laid out and glyphs rasterized before t = 0 (`prewarm`), and images are decoded and uploaded before the run.

## Process priority

The render thread runs at macOS's `USER_INTERACTIVE` QoS class. Use **fullscreen** at the scanner: on macOS a fullscreen window bypasses most compositor work, and in testing, windowed mode on a busy desktop dropped about one frame every 10–20 s.

## Checking a setup

```bash
onset timing-test --rig scanner.toml --seconds 30
```

This alternates black/white every frame and reports the measured refresh, interval SD, and dropped frames, from the display's own reports of when each frame was on screen (or from the frame loop's timing if there are none). With the photodiode taped on the corner patch, the Arduino rig measures the real on-screen timing.
