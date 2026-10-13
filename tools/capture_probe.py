#!/usr/bin/env python3
"""Record the onset_timing_probe Arduino's serial output to a TSV file.

The Arduino sketch (hardware/onset_timing_probe) prints one line per event:

    P,<us>,1      photodiode light ON  (stimulus onset)
    P,<us>,0      photodiode light OFF
    T,<us>        scanner trigger
    # ...         comments (banner, warnings) — echoed to the terminal

This script writes them to a tab-separated file with the columns

    kind  arduino_us  level  host_time

(`level` is n/a for triggers; `host_time` is this computer's Unix time when the
line arrived — only good to a few ms, for bookkeeping, never for timing.)
It runs until you press Ctrl-C. Start it BEFORE starting Onset so every
trigger Onset sees is also recorded here.

Usage:
    python3 tools/capture_probe.py                      # auto-detect the port
    python3 tools/capture_probe.py --port /dev/cu.usbmodem1101 --out probe.tsv
    python3 tools/capture_probe.py --list               # show serial ports

Needs pyserial:  python3 -m pip install pyserial
"""

from __future__ import annotations

import argparse
import datetime as dt
import sys
import time
from pathlib import Path

# Arduino's USB vendor IDs (official boards and the older .org ones).
ARDUINO_VIDS = {0x2341, 0x2A03}


def import_serial():
    """Import pyserial, or exit with instructions if it is missing."""
    try:
        import serial  # noqa: F401
        import serial.tools.list_ports  # noqa: F401
    except ImportError:
        sys.exit(
            "capture_probe.py needs the 'pyserial' package.\n"
            "Install it with:   python3 -m pip install pyserial\n"
            "(or inside a virtual environment: python3 -m venv .venv && "
            ".venv/bin/pip install pyserial)"
        )
    return serial


def find_port(serial) -> str:
    """Pick the Arduino's serial port, or explain how to choose one."""
    ports = list(serial.tools.list_ports.comports())
    arduinos = [p for p in ports if p.vid in ARDUINO_VIDS]
    if not arduinos:
        # Clones use other USB chips (CH340, FTDI); fall back to name patterns.
        arduinos = [
            p for p in ports if any(s in p.device for s in ("usbmodem", "usbserial", "ttyACM", "ttyUSB"))
        ]
    if len(arduinos) == 1:
        return arduinos[0].device
    listing = "\n".join(f"  {p.device}  {p.description}" for p in ports) or "  (none)"
    if not arduinos:
        sys.exit(f"No Arduino found. Is it plugged in? Serial ports:\n{listing}")
    sys.exit(f"Several candidate ports; pick one with --port:\n{listing}")


def parse_line(line: str) -> tuple[str, int, str] | None:
    """Turn 'P,123,1' / 'T,456' into (kind, us, level). None for anything else."""
    parts = line.strip().split(",")
    try:
        if parts[0] == "P" and len(parts) == 3 and parts[2] in ("0", "1"):
            return "P", int(parts[1]), parts[2]
        if parts[0] == "T" and len(parts) == 2:
            return "T", int(parts[1]), "n/a"
    except ValueError:
        pass
    return None


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", help="serial port (default: auto-detect the Arduino)")
    ap.add_argument("--baud", type=int, default=115200)
    ap.add_argument("--out", type=Path, help="output TSV (default: probe_<date>_<time>.tsv)")
    ap.add_argument("--list", action="store_true", help="list serial ports and exit")
    ap.add_argument("--quiet", action="store_true", help="don't print every event")
    args = ap.parse_args(argv)

    serial = import_serial()
    if args.list:
        for p in serial.tools.list_ports.comports():
            print(f"{p.device}\t{p.description}\tvid={p.vid}")
        return 0

    port = args.port or find_port(serial)
    out = args.out or Path(f"probe_{dt.datetime.now():%Y%m%d_%H%M%S}.tsv")
    out.parent.mkdir(parents=True, exist_ok=True)

    # Opening the port resets an Uno R3 (it reboots and prints its banner again
    # after ~2 s). Leonardo / Uno R4 do not reset.
    ser = serial.Serial(port, args.baud, timeout=0.5)
    print(f"reading {port} @ {args.baud} -> {out}   (Ctrl-C to stop)", file=sys.stderr)

    counts = {"P": 0, "T": 0}
    bad = 0
    with open(out, "w", newline="") as f:
        f.write(f"# capture_probe.py started {dt.datetime.now().isoformat()} port={port}\n")
        f.write("kind\tarduino_us\tlevel\thost_time\n")
        f.flush()
        try:
            while True:
                raw = ser.readline()
                if not raw:
                    continue  # timeout: nothing arrived; keep waiting
                line = raw.decode("ascii", errors="replace").strip()
                if not line:
                    continue
                if line.startswith("#"):
                    # Keep comments in the file too (pandas skips them).
                    print(line, file=sys.stderr)
                    f.write(line + "\n")
                    f.flush()
                    continue
                ev = parse_line(line)
                if ev is None:
                    bad += 1  # e.g. a half line right after the port opened
                    continue
                kind, us, level = ev
                counts[kind] += 1
                f.write(f"{kind}\t{us}\t{level}\t{time.time():.3f}\n")
                f.flush()  # so a crash or yanked cable loses nothing
                if not args.quiet:
                    label = {"T": "trigger", "P": "light on" if level == "1" else "light off"}[kind]
                    print(
                        f"{us / 1e6:12.6f} s  {label:9s}  (triggers {counts['T']}, edges {counts['P']})",
                        file=sys.stderr,
                    )
        except KeyboardInterrupt:
            pass
        finally:
            ser.close()
    print(
        f"\nsaved {out}: {counts['T']} triggers, {counts['P']} photodiode edges"
        + (f", {bad} unparseable lines skipped" if bad else ""),
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
