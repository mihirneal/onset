# Onset timing probe (photodiode + trigger timestamper)

A small Arduino box that records, with microsecond timestamps on its own clock,

* when light actually appears in Onset's **photodiode patch** (the corner square
  that is black normally and white for `flash_frames` frames at every stimulus
  onset), and
* when the **scanner trigger** pulses arrive.

`tools/analyze_timing.py` then lines this up with Onset's own logs and reports
how far each logged onset is from the real light onset. The full measurement
protocol is in [`docs/VALIDATION.md`](../docs/VALIDATION.md).

```
 display ──light──> sensor ─> comparator ─> D2 ┐
                                               ├─ Arduino ──USB──> capture_probe.py ──> probe.tsv
 scanner TTL ──> opto-isolator ──────────────> D3 ┘                                         │
                                                                                            v
 Onset ──> data/sub-XX/onset/*_eventlog.tsv, *_inputs.tsv, *_frames.tsv ──> analyze_timing.py
```

---

## Safety first — read this before going near the scanner

* **Nothing from this kit goes into the magnet room.** No Arduino, sensor,
  cable, tape roll or tool with metal in it. The magnet is always on.
* The trigger connection is made **in the control/equipment room**, at the
  output of the existing trigger interface (e.g. the BNC TTL output of the
  fORP / trigger converter box), never by opening scanner equipment and never
  by running new cables through the penetration panel or waveguide.
* The scanner's trigger line is connected **only through an opto-isolator**,
  so there is no electrical path between the scanner's electronics and the
  stimulus PC / USB ground. Ask your MR physicist or engineer which output you
  may use and get their OK before plugging anything into it.
* The photodiode goes on the **display side outside the magnet room**: the
  projector in the equipment room, or a control-room monitor.
  - If participants view a **projector** through the waveguide, tape the
    sensor over the patch in a spot of the projected image you can reach on
    the equipment-room side (e.g. a small screen or white card placed in the
    beam path before it enters the waveguide, during testing only), or
    measure with the same projector set up on the bench.
  - If the display is an **MR-compatible monitor inside the magnet room**
    (e.g. a BOLDscreen), do **not** tape electronics to it. Use the vendor's
    fiber-optic photodiode accessory, or measure the identical monitor model
    on the bench. A cloned control-room monitor is *not* a substitute — it
    has its own, different latency.

---

## Bill of materials

| Qty | Part | Notes |
|---|---|---|
| 1 | Arduino **Uno R4 Minima/WiFi**, **Leonardo**, or **Uno R3** | R4 / Leonardo have native USB (and can act as a keyboard for desk tests). Uno R3 uses a ceramic resonator (clock off by up to ~0.3%) — fine, the analysis fits it out. |
| 1 | Light sensor — **TSL257** (recommended), or **OPT101**, or **BPW34** photodiode + op-amp (e.g. MCP6001, 1 MΩ feedback) | Needs to be fast (µs). **Not** an LDR/photoresistor (tens of ms). |
| 1 | **LM393** dual comparator (DIP-8) | Or a ready-made "LM393 light sensor module" *if* you replace its LDR with one of the sensors above. |
| 1 | 10 kΩ trim potentiometer | Comparator threshold. |
| 1 | 10 kΩ resistor | Comparator output pull-up. |
| 1 | 10 kΩ resistor + 1 MΩ resistor | Hysteresis (optional but recommended). |
| 1 | **6N137** opto-isolator (or H11L1; PC817 works but is slower, ~10 µs) | Scanner trigger isolation. |
| 1 | 330–470 Ω resistor | 6N137 LED current limit for a 5 V TTL input. |
| 1 | 2.2 kΩ resistor | 6N137 output pull-up (faster than the Arduino's internal one). |
| 2 | 100 nF ceramic capacitors | Decoupling, one at each chip's supply pins. |
| 1 | BNC panel socket or BNC-to-wire lead | Trigger input from the trigger box. |
| 1 | Small momentary push button | Desk trigger (optional). |
| — | Breadboard or perfboard, hookup wire, a USB cable | |
| — | **Black** foam (e.g. 5–10 mm craft foam) and black gaffer / electrical tape | Light shield for the sensor. |

---

## Wiring

All grounds on the Arduino side are common (Arduino GND, comparator GND,
opto-isolator **output** side GND). The opto-isolator **input** side is a
separate circuit powered by the trigger signal itself.

### Photodiode → comparator → D2

```
  +5V ───┬───────────────┬────────────────┬──────────────────┐
         │               │                │                  │
     ┌───┴────┐        ┌─┴─┐         ┌────┴─────┐          [10k] pull-up
     │ TSL257 │        │10k│ pot     │  LM393   │            │
     │  VDD   │        │   │<─┐      │ VCC (8)  │            │
     │  OUT ──┼─[10k]──┼───┼──┼──┬───┤ IN+ (3)  │            │
     │  GND   │        │   │  └──┼───┤ IN− (2)  │            │
     └───┬────┘        └─┬─┘     │   │ OUT (1) ─┼────────────┴──────> Arduino D2
         │               │       │   │ GND (4)  │      │
         │               │       │   └────┬─────┘      │
         │               │       └───────[1M]──────────┘  (hysteresis: OUT -> IN+)
  GND ───┴───────────────┴────────────────┴──────────────────────────  Arduino GND
```

Connection list (same circuit):

| From | To |
|---|---|
| TSL257 VDD, pot end A, LM393 pin 8, pull-up 10k (top) | +5V |
| TSL257 GND, pot end B, LM393 pin 4 | GND |
| TSL257 OUT → 10k → | LM393 pin 3 (IN+) |
| pot wiper | LM393 pin 2 (IN−) — the threshold |
| LM393 pin 1 (OUT) | pull-up 10k (bottom) and Arduino **D2** |
| 1 MΩ | between LM393 pin 1 and pin 3 |
| 100 nF | between LM393 pins 8 and 4 |

* TSL257 pins (flat side facing you, leads down): 1 = GND, 2 = VDD, 3 = OUT.
  Check your datasheet — some packages differ.
* Brighter → TSL257 OUT higher → above the pot voltage → LM393 output released
  → pulled HIGH. So **white patch = HIGH on D2** (`PHOTO_ACTIVE_LOW 0`).
* The 1 MΩ from OUT back to IN+ gives a few tens of mV of hysteresis so the
  output doesn't chatter while the screen's light ramps through the threshold.
* **OPT101**: pin 1 V+, pin 8 GND (and pin 3 −V to GND), output pin 5 → same
  place as the TSL257 OUT; bridge pins 4–5 for the built-in 1 MΩ feedback.
* **BPW34 + op-amp**: photodiode reverse-biased into the op-amp's inverting
  input, 1 MΩ feedback, output → same place.
* **Analog fallback** (no comparator): connect the sensor output straight to
  **A0** and set `PHOTO_MODE_ANALOG 1` in the sketch. Simpler, but timestamps
  are only good to ~0.1–0.5 ms.

### Scanner trigger → opto-isolator → D3

```
   SCANNER SIDE (isolated)        │            ARDUINO SIDE
                                  │
  trigger box BNC                 │  6N137
  (TTL, control room)       ┌─────┴─────────────┐
  centre ──[390 Ω]──── 2 ───┤ anode       VCC 8 ├──── +5V ──┬── 100 nF ── GND
                            │              EN 7 ├──── +5V   │
  shield ───────────── 3 ───┤ cathode     OUT 6 ├───┬───────┼──────────> Arduino D3
                            │             GND 5 ├─┐ └─[2.2k]┘ (pull-up to +5V)
                            └─────┬─────────────┘ └──────────────────── Arduino GND
                                  │
   the BNC shield is NOT connected to Arduino GND — that's the isolation
```

* When the scanner pulse is HIGH, the 6N137's output pulls D3 **LOW**, so the
  sketch triggers on the falling edge (`TRIG_ACTIVE_LOW 1`, the default).
* Desk alternative: a push button from **D3 to GND** (uses the internal
  pull-up; also active-low). `TRIG_HOLDOFF_US` (50 ms) debounces it.

### Fake TR for desk tests (optional)

Set `FAKE_TR 1` and jumper **D9 → D3**: the board generates a pulse every
`TR_MS` and logs it as a trigger. To make Onset see the same pulse:

* **Leonardo / Uno R4**: also set `FAKE_TR_KEYBOARD 1`; the board types `5` like
  an fORP (Onset rig: `trigger.source = keyboard`, key `"5"`). Keep the Onset
  window focused.
* Any board: feed D9 into your lab's trigger box input that produces the `5`
  keypress / serial character. Trigger boxes usually expect an active-high
  pulse: set `FAKE_TR_ACTIVE_LOW 0` and `TRIG_ACTIVE_LOW 0` (the jumper then
  still works, and D3's input matches).

Keyboard triggers add a few ms of USB latency and jitter. That's realistic (it's
what the fORP does) and the analysis reports it separately
(`trigger_residual_sd_ms`).

---

## Mounting the sensor over the patch

1. In the rig file make sure the patch is enabled and note its corner and size
   (default 40 logical px, bottom-left). Bigger (60–80 px) is easier to cover.
2. Cut a ~3 × 3 cm square of black foam with a hole the size of the sensor's
   lens. Push the sensor into the hole so its face is flush with the foam.
3. Press the foam flat against the screen, centred on the patch, and tape it
   down with black tape on all four sides. The foam blocks room light and
   light from the rest of the image; nothing but the patch should reach the
   sensor.
4. Route the cable away and tape it down so the sensor can't tilt.
5. Run `onset timing-test` and tune the threshold (below). Then check the
   LED on the Arduino stays dark while the patch is black with the room
   lights on.
6. For a projector, mount on a stiff white card in the beam (outside the
   magnet room) and shield the same way.

## Tuning the threshold

1. In `onset_timing_probe.ino` set `CALIBRATE 1`, upload, open the Arduino IDE
   Serial Monitor at 115200 baud.
2. Show a white patch, then a black one (`onset timing-test` flashes it every
   frame, which is too fast to see; a run with `--simulate-tr` shows 3-frame
   flashes; or temporarily make a test with a long white stimulus).
3. Turn the pot until `lit=1` exactly when white and `lit=0` when black. Aim
   for the middle of the range where it works: find the pot position where it
   stops seeing white and the one where it starts seeing black as white, and
   set it halfway.
4. With an LCD, set the display to **100% brightness**: many backlights only
   flicker (PWM dimming) below full brightness. The sketch's `GAP_US` filter
   (2 ms) ignores remaining flicker; reduce it for displays faster than
   240 Hz (it must be shorter than one frame).
5. Set `CALIBRATE 0` and upload again before recording.

## Uploading and recording

1. Arduino IDE → open `hardware/onset_timing_probe/onset_timing_probe.ino` →
   select the board and port → Upload. (The sketch has not been compiled in CI;
   if your board's core complains, the settings block at the top is the only
   part you should need to touch.)
2. `python3 -m pip install pyserial` (once), then
   `python3 tools/capture_probe.py --out probe_run01.tsv` — **start it before
   starting Onset** and stop it with Ctrl-C after the run ends.
3. `python3 tools/analyze_timing.py --probe probe_run01.tsv --run data/sub-XX/onset`
   (see `--help` and `docs/VALIDATION.md`).
