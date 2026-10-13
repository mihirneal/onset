/*
 * onset_timing_probe.ino — photodiode + scanner-trigger timestamper for Onset
 * ==========================================================================
 *
 * What it does
 * ------------
 * Timestamps two things with the Arduino's microsecond clock and prints them
 * over USB serial (115200 baud), one event per line:
 *
 *   P,<us>,1     photodiode patch turned ON  (light onset = stimulus onset)
 *   P,<us>,0     photodiode patch turned OFF
 *   T,<us>       scanner trigger (TTL rising edge, or desk button press)
 *   # ...        comments: banner, settings, warnings (ignored by tools)
 *
 * <us> is a 64-bit microsecond counter. Arduino's micros() is only 32 bits and
 * wraps every ~71.6 minutes; this sketch counts the wraps so the number keeps
 * increasing. It starts near 0 when the board resets.
 *
 * tools/capture_probe.py records these lines to a TSV file; tools/analyze_timing.py
 * lines them up with Onset's own logs. See hardware/README.md and docs/VALIDATION.md.
 *
 * Boards: Arduino Uno R3, Uno R4 (Minima / WiFi), Leonardo. All of them have
 * external interrupts on pins 2 and 3, which is what we use.
 *
 * Wiring (details and a diagram in hardware/README.md)
 * ------
 *   Pin 2  <- photodiode comparator output (LM393, with a 10k pull-up to 5V)
 *             [or A0 <- analog sensor output if PHOTO_MODE_ANALOG is chosen]
 *   Pin 3  <- trigger: opto-isolator output (6N137 / H11L1 / PC817) or a button
 *   Pin 9  -> fake TR pulses (only when FAKE_TR is 1; jumper pin 9 -> pin 3)
 *   Pin 13    on-board LED mirrors the photodiode state (handy for tuning)
 *   GND       common ground with the comparator / opto-isolator OUTPUT side only
 *
 * Recommended parts
 * -----------------
 *   Light sensor (pick one):
 *     - TSL257 (light-to-voltage, op-amp built in; easiest; ~µs response)
 *     - OPT101 (photodiode + transimpedance amp in one chip; set gain resistor)
 *     - BPW34 photodiode + op-amp transimpedance stage (e.g. MCP6001, 1 MΩ)
 *   Comparator: LM393 (open-collector output -> needs a pull-up resistor),
 *     with a 10k trim pot for the threshold and ~1 MΩ positive feedback
 *     for a little hysteresis. Many "light sensor module" boards are
 *     exactly an LM393 + pot; they work if the sensor itself is fast.
 *     (Avoid modules built around an LDR / photoresistor: tens of ms slow.)
 *   Scanner trigger: opto-isolator (6N137 is fast, ~100 ns; PC817 is fine
 *     too, ~10 µs) so the scanner's TTL line is never electrically connected
 *     to this board / the stimulus PC.
 *
 * Threshold tuning (comparator mode)
 * ---------------------------------
 *   1. Set CALIBRATE to 1, upload, open the Serial Monitor at 115200.
 *   2. Tape the sensor over the photodiode patch and run `onset timing-test`
 *      (patch flips white/black every frame) or a run with --simulate-tr.
 *   3. Turn the comparator pot until the on-board LED / "#cal" lines show the
 *      level following the patch: HIGH when white, LOW when black. A good
 *      threshold is about halfway between the black and white levels; check
 *      with the room lights on and off, since stray light shifts "black".
 *   4. Set CALIBRATE back to 0 and upload again before measuring.
 *   Analog mode: CALIBRATE prints raw A0 readings for black and white; set
 *   ANALOG_THRESHOLD halfway between them, ANALOG_HYSTERESIS ~10% of the gap.
 *
 * Flicker rejection (why GAP_US exists)
 * -------------------------------------
 *   LCD backlights are often dimmed by PWM, and DLP projectors build every
 *   colour out of very short light pulses. So a "white" patch can actually be
 *   a burst of on/off pulses. We treat light as ON from the first rising
 *   edge until the light has been continuously OFF for at least GAP_US.
 *   Short dark gaps inside a white frame are ignored. The reported ON time is
 *   the very first rising edge; the reported OFF time is the start of the
 *   final dark period. GAP_US must be shorter than one frame (16.7 ms at
 *   60 Hz, 8.3 ms at 120 Hz). Turning the display to 100% brightness often
 *   removes backlight PWM entirely. Very short noise blips (a few µs) are
 *   reported and then removed by analyze_timing.py (--min-pulse-ms).
 */

// ============================== settings ===================================

// Photodiode input: 1 = analog sensor on A0 with a software threshold,
//                   0 = comparator output on pin 2 with an interrupt (preferred:
//                       timestamps are ~4 µs accurate; analog is ~0.1–0.5 ms).
#define PHOTO_MODE_ANALOG 0

// Comparator output is HIGH when the patch is white? (Set 1 if your circuit
// goes LOW on light, e.g. sensor on the comparator's inverting input.)
#define PHOTO_ACTIVE_LOW 0

// Light counts as OFF only after this many µs of continuous darkness.
#define GAP_US 2000UL

// Analog mode only: A0 reading (0..1023) threshold and hysteresis band.
#define ANALOG_THRESHOLD 512
#define ANALOG_HYSTERESIS 40
// Analog mode only: is a brighter patch a HIGHER reading? (TSL257/OPT101: yes)
#define ANALOG_BRIGHT_IS_HIGH 1

// Trigger input. Opto-isolators with a pull-up usually INVERT the signal
// (output pulled LOW while the scanner pulse is HIGH): then set 1.
// A button wired from pin 3 to GND with INPUT_PULLUP is also active-low.
#define TRIG_ACTIVE_LOW 1
// Use the board's internal pull-up on the trigger pin (fine for open-collector
// opto outputs and buttons). Set 0 if your circuit drives the pin actively.
#define TRIG_INTERNAL_PULLUP 1
// Ignore further trigger edges for this long after one (button bounce,
// ringing on long cables). Must be shorter than your TR.
#define TRIG_HOLDOFF_US 50000UL

// Desk testing: generate fake scanner pulses on FAKE_TR_PIN every TR_MS.
// Jumper FAKE_TR_PIN -> TRIG_PIN so this board logs them as "T" lines, and
// also feed the same pin to whatever turns a pulse into Onset's trigger
// (your lab's trigger box / fORP input), or use FAKE_TR_KEYBOARD below.
#define FAKE_TR 0
#define TR_MS 2000UL
#define FAKE_TR_PULSE_US 5000UL  // pulse width
// Pulse polarity. By default it matches TRIG_ACTIVE_LOW so the D9 -> D3
// loopback jumper timestamps the START of each pulse. Set 0 for an
// active-high, scanner-like TTL pulse into a trigger box (and then, for the
// loopback, also set TRIG_ACTIVE_LOW 0).
#define FAKE_TR_ACTIVE_LOW TRIG_ACTIVE_LOW
// Leonardo / Uno R4 only (native USB): also type the key '5' on each fake TR,
// like an fORP does, so Onset (trigger.source = keyboard, key "5") sees it.
// Leave 0 on Uno R3 (no native USB; it will not compile).
#define FAKE_TR_KEYBOARD 0

// 1 = print the photodiode level ten times a second for threshold tuning.
#define CALIBRATE 0

// Pins. 2 and 3 are interrupt-capable on Uno R3, Uno R4 and Leonardo.
const uint8_t PHOTO_PIN = 2;
const uint8_t PHOTO_ANALOG_PIN = A0;
const uint8_t TRIG_PIN = 3;
const uint8_t FAKE_TR_PIN = 9;
const uint8_t LED_PIN = LED_BUILTIN;

// =========================== implementation ================================

#if FAKE_TR && FAKE_TR_KEYBOARD
#include <Keyboard.h>
#endif

// ---- 64-bit time ------------------------------------------------------------
// micros() wraps from 4294967295 back to 0 every ~71.6 minutes. We remember
// the last value we saw; if the new one is smaller, the counter wrapped.
// now64() must run with interrupts disabled (it is called from ISRs, where
// they already are, and from the main loop inside noInterrupts()), so the
// two variables are never updated half-way. loop() calls it constantly, so
// a wrap can never be missed.
static uint32_t lastMicros = 0;
static uint32_t wrapCount = 0;

static uint64_t now64() {
  uint32_t m = micros();
  if (m < lastMicros) wrapCount++;
  lastMicros = m;
  return ((uint64_t)wrapCount << 32) | m;
}

// ---- event ring buffer -------------------------------------------------------
// Interrupt handlers must be short, so they only push (kind, time, level)
// here; the main loop pops entries and does the slow Serial printing.
struct Event {
  uint64_t t;
  char kind;      // 'P' or 'T'
  uint8_t level;  // P: 1 = light on, 0 = off
};

const uint8_t RING_SIZE = 64;  // power of two; 64 * ~10 bytes fits an Uno's 2 KB
static volatile Event ring[RING_SIZE];
static volatile uint8_t ringHead = 0;  // written by producers
static volatile uint8_t ringTail = 0;  // written by the main loop
static volatile uint16_t ringOverflows = 0;

// Call with interrupts disabled.
static void push(char kind, uint64_t t, uint8_t level) {
  uint8_t next = (ringHead + 1) & (RING_SIZE - 1);
  if (next == ringTail) {  // full: drop the event, but count it
    ringOverflows++;
    return;
  }
  ring[ringHead].t = t;
  ring[ringHead].kind = kind;
  ring[ringHead].level = level;
  ringHead = next;
}

// ---- photodiode edge filter -----------------------------------------------
// photoOn : our filtered "patch is lit" state
// rawLow  : the raw sensor is currently dark
// lastFall: when the raw sensor last went dark
static volatile bool photoOn = false;
static volatile bool rawLow = true;
static volatile uint64_t lastFall = 0;

// A raw change of the sensor level at time t. Call with interrupts disabled.
static void photoRawChange(uint64_t t, bool lit) {
  if (lit) {
    rawLow = false;
    if (photoOn && (t - lastFall) >= GAP_US) {
      // We were dark long enough to count as OFF, but the main loop had not
      // noticed yet (it was busy printing). Emit the OFF, then a new ON.
      push('P', lastFall, 0);
      photoOn = false;
    }
    if (!photoOn) {
      photoOn = true;
      push('P', t, 1);  // first light after darkness = onset
    }
    // else: brief flicker inside a lit period; ignore.
  } else {
    rawLow = true;
    lastFall = t;
  }
}

// Called from loop(): has the sensor been dark for GAP_US? Then it's OFF.
static void photoCheckGap(uint64_t now) {
  if (photoOn && rawLow && (now - lastFall) >= GAP_US) {
    photoOn = false;
    push('P', lastFall, 0);  // OFF time = start of the dark period
  }
}

#if !PHOTO_MODE_ANALOG
static void photoISR() {
  uint64_t t = now64();  // timestamp first, before anything slow
  bool high = digitalRead(PHOTO_PIN) == HIGH;
  bool lit = PHOTO_ACTIVE_LOW ? !high : high;
  photoRawChange(t, lit);
}
#endif

// ---- trigger -------------------------------------------------------------------
static volatile uint32_t lastTrigMicros = 0;
static volatile bool anyTrig = false;

static void trigISR() {
  uint64_t t = now64();
  uint32_t lo = (uint32_t)t;
  // Unsigned subtraction handles the 32-bit wrap correctly.
  if (anyTrig && (uint32_t)(lo - lastTrigMicros) < TRIG_HOLDOFF_US) return;
  anyTrig = true;
  lastTrigMicros = lo;
  push('T', t, 1);
}

// ---- printing ------------------------------------------------------------------
// AVR's Serial.print can't print 64-bit integers, so do it by hand.
static void printU64(uint64_t v) {
  char buf[21];
  char *p = buf + sizeof(buf) - 1;
  *p = '\0';
  do {
    *--p = '0' + (char)(v % 10);
    v /= 10;
  } while (v != 0);
  Serial.print(p);
}

static void printEvent(const Event &e) {
  Serial.print(e.kind);
  Serial.print(',');
  printU64(e.t);
  if (e.kind == 'P') {
    Serial.print(',');
    Serial.print(e.level);
  }
  Serial.print('\n');
}

static void banner() {
  Serial.println(F("# onset_timing_probe v1"));
  Serial.print(F("# photo_mode="));
  Serial.print(PHOTO_MODE_ANALOG ? F("analog") : F("comparator"));
  Serial.print(F(" gap_us="));
  Serial.print(GAP_US);
  Serial.print(F(" trig_holdoff_us="));
  Serial.print(TRIG_HOLDOFF_US);
  Serial.print(F(" fake_tr="));
  Serial.print(FAKE_TR);
  Serial.print(F(" tr_ms="));
  Serial.println(TR_MS);
  Serial.println(F("# columns: P,<us>,<1=on|0=off>  T,<us>"));
  Serial.println(F("# ready"));
}

// ---- analog sampling (fallback mode) ---------------------------------------
#if PHOTO_MODE_ANALOG
static bool analogLit = false;

static void sampleAnalog() {
  int v = analogRead(PHOTO_ANALOG_PIN);  // ~110 µs on an Uno, faster on R4
  if (!ANALOG_BRIGHT_IS_HIGH) v = 1023 - v;
  bool lit = analogLit;
  // Hysteresis: need to cross threshold +/- half the band to change state,
  // so noise near the threshold does not chatter.
  if (!analogLit && v > ANALOG_THRESHOLD + ANALOG_HYSTERESIS / 2) lit = true;
  if (analogLit && v < ANALOG_THRESHOLD - ANALOG_HYSTERESIS / 2) lit = false;
  if (lit != analogLit) {
    analogLit = lit;
    noInterrupts();
    photoRawChange(now64(), lit);
    interrupts();
  }
}
#endif

// ---- fake TR generator ------------------------------------------------------------
#if FAKE_TR
static uint64_t nextTR = 0;
static uint64_t fakePulseEnd = 0;
static bool fakePulseActive = false;  // true while a pulse is in progress

static void fakeTR(uint64_t now) {
  if (!fakePulseActive && now >= nextTR) {
    digitalWrite(FAKE_TR_PIN, FAKE_TR_ACTIVE_LOW ? LOW : HIGH);  // pulse starts
    fakePulseActive = true;
    fakePulseEnd = now + FAKE_TR_PULSE_US;
    nextTR += (uint64_t)TR_MS * 1000UL;  // schedule from the ideal time: no drift
#if FAKE_TR_KEYBOARD
    Keyboard.write('5');
#endif
  }
  if (fakePulseActive && now >= fakePulseEnd) {
    digitalWrite(FAKE_TR_PIN, FAKE_TR_ACTIVE_LOW ? HIGH : LOW);  // back to idle
    fakePulseActive = false;
  }
}
#endif

// ---- setup / loop -----------------------------------------------------------------
void setup() {
  Serial.begin(115200);
  // Leonardo / R4 have native USB: wait (up to 3 s) for the host to open the port
  // so the banner isn't lost. On an Uno R3, Serial is always "ready".
  unsigned long start = millis();
  while (!Serial && millis() - start < 3000) {
  }

  pinMode(LED_PIN, OUTPUT);

#if PHOTO_MODE_ANALOG
  pinMode(PHOTO_ANALOG_PIN, INPUT);
#else
  pinMode(PHOTO_PIN, INPUT);  // the LM393 board provides the pull-up
  attachInterrupt(digitalPinToInterrupt(PHOTO_PIN), photoISR, CHANGE);
#endif

  pinMode(TRIG_PIN, TRIG_INTERNAL_PULLUP ? INPUT_PULLUP : INPUT);
  attachInterrupt(digitalPinToInterrupt(TRIG_PIN), trigISR, TRIG_ACTIVE_LOW ? FALLING : RISING);

#if FAKE_TR
  pinMode(FAKE_TR_PIN, OUTPUT);
  digitalWrite(FAKE_TR_PIN, FAKE_TR_ACTIVE_LOW ? HIGH : LOW);  // idle level
#if FAKE_TR_KEYBOARD
  Keyboard.begin();
#endif
  noInterrupts();
  nextTR = now64() + 1000000UL;  // first fake pulse 1 s after boot
  interrupts();
#endif

  banner();
}

void loop() {
#if PHOTO_MODE_ANALOG
  sampleAnalog();
#endif

  // Read the clock (keeps the wrap counter current) and close OFF periods.
  noInterrupts();
  uint64_t now = now64();
  photoCheckGap(now);
  bool lit = photoOn;
  interrupts();

  digitalWrite(LED_PIN, lit ? HIGH : LOW);

#if FAKE_TR
  fakeTR(now);
#endif

  // Print at most one event per pass, so analog sampling stays frequent.
  noInterrupts();
  bool have = ringTail != ringHead;
  Event e;
  if (have) {
    e.t = ring[ringTail].t;
    e.kind = ring[ringTail].kind;
    e.level = ring[ringTail].level;
    ringTail = (ringTail + 1) & (RING_SIZE - 1);
  }
  uint16_t overflows = ringOverflows;
  ringOverflows = 0;
  interrupts();

  if (have) printEvent(e);
  if (overflows) {
    Serial.print(F("# WARNING ring buffer overflow, events lost: "));
    Serial.println(overflows);
  }

#if CALIBRATE
  static unsigned long lastCal = 0;
  if (millis() - lastCal >= 100) {
    lastCal = millis();
    Serial.print(F("#cal "));
#if PHOTO_MODE_ANALOG
    Serial.print(F("A0="));
    Serial.print(analogRead(PHOTO_ANALOG_PIN));
    Serial.print(F(" threshold="));
    Serial.print(ANALOG_THRESHOLD);
#else
    Serial.print(F("pin2="));
    Serial.print(digitalRead(PHOTO_PIN));
#endif
    Serial.print(F(" lit="));
    Serial.println(lit ? 1 : 0);
  }
#endif
}
