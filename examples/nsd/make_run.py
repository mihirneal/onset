#!/usr/bin/env python3
"""Build an NSD-style run (Allen et al. 2022, Nature Neuroscience) for Onset.

Run structure (from the NSD presentation code, runnsd.m / showmulticlass.m):
  12 s blank, then 68 four-second slots (3 s image + 1 s gap), then 16 s blank = 300 s.
  Odd runs: 63 image trials + 5 blank slots. Even runs: the 63rd image becomes a
  blank (62 images + 6 blanks). Blanks split the images into stretches of 9-14
  trials. Task: continuous recognition, button 1 = NEW, button 2 = OLD.

This writes schedule.tsv (one row per image trial) next to experiment.toml.
Images come from --images (any folder of PNG/JPEG files, e.g. NSD's shared1000
after you've accepted the NSD terms). Without --images it uses
images/placeholder_*.png, generating them first if missing (abstract color
patterns, NOT NSD images: NSD's terms forbid redistributing its stimuli).

    python3 examples/nsd/make_run.py --run 1 --seed 1
    python3 examples/nsd/make_run.py --run 2 --images ~/nsd/shared1000 \
        --history ~/nsd/sub-01_seen.txt --out ~/nsd/sub-01/run02.tsv

--history keeps "old/new" honest across runs and sessions: images listed in
it count as seen before, and this run's new images are appended to it. Use
one history file per participant.

Python standard library only.
"""

import argparse
import random
import re
import struct
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent

PRE_BLANK = 12.0
SLOT = 4.0
SLOTS = 68
IMAGES_ODD_RUN = 63


def write_png(path: Path, width: int, height: int, rows: list) -> None:
    """Write an 8-bit RGB PNG from a list of `bytes` rows (width*3 each)."""
    raw = b"".join(b"\x00" + row for row in rows)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


def placeholder(path: Path, seed: int, size: int = 425) -> None:
    """An abstract, colorful 'scene': sky/ground split plus a few blocks and a sun."""
    rng = random.Random(seed)
    col = lambda: bytes(rng.randrange(30, 230) for _ in range(3))
    sky, ground = col(), col()
    horizon = rng.randrange(size // 3, 2 * size // 3)
    rows = [bytearray(sky * size) if y < horizon else bytearray(ground * size) for y in range(size)]
    for _ in range(rng.randrange(3, 7)):
        c = col()
        x0, y0 = rng.randrange(0, size - 40), rng.randrange(size // 4, size - 40)
        w, h = rng.randrange(30, 160), rng.randrange(30, 180)
        for y in range(y0, min(size, y0 + h)):
            x1 = min(size, x0 + w)
            rows[y][x0 * 3 : x1 * 3] = c * (x1 - x0)
    cx, cy, r = rng.randrange(60, size - 60), rng.randrange(40, horizon), rng.randrange(15, 45)
    sun = col()
    for y in range(max(0, cy - r), min(size, cy + r)):
        dx = int((r * r - (y - cy) ** 2) ** 0.5)
        x0, x1 = max(0, cx - dx), min(size, cx + dx)
        rows[y][x0 * 3 : x1 * 3] = sun * (x1 - x0)
    write_png(path, size, size, [bytes(r) for r in rows])


def blank_slots(rng: random.Random, n_images: int = IMAGES_ODD_RUN, n_blanks: int = 5) -> set:
    """Pick blank slot positions so every stretch of images is 9-14 long."""
    stretches = n_blanks + 1
    while True:
        lengths = [rng.randint(9, 14) for _ in range(stretches)]
        if sum(lengths) == n_images:
            break
    blanks, pos = set(), 0
    for length in lengths[:-1]:
        pos += length
        blanks.add(pos)
        pos += 1
    return blanks


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--run", type=int, default=1, help="run number within the session (odd/even changes blanks)")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--images", type=Path, help="folder of images to draw from (default: placeholders)")
    ap.add_argument("--repeat-prob", type=float, default=0.3, help="chance a trial repeats an earlier image")
    ap.add_argument("--out", type=Path, default=HERE / "schedule.tsv")
    ap.add_argument("--history", type=Path, help="per-participant list of images already shown (read + appended)")
    args = ap.parse_args()
    rng = random.Random(args.seed * 1000 + args.run)

    if args.images:
        pool = sorted(p for p in args.images.iterdir() if p.suffix.lower() in {".png", ".jpg", ".jpeg"})
        if not pool:
            raise SystemExit(f"no PNG/JPEG images in {args.images}")
    else:
        folder = HERE / "images"
        folder.mkdir(exist_ok=True)
        pool = [folder / f"placeholder_{i:03d}.png" for i in range(80)]
        for i, p in enumerate(pool):
            if not p.exists():
                placeholder(p, seed=i)

    # Slot layout: odd runs 63 images + 5 blanks; even runs drop image 63.
    blanks = blank_slots(rng)
    layout = []  # True = image slot
    image_count = 0
    for slot in range(SLOTS):
        is_image = slot not in blanks
        if is_image:
            image_count += 1
            if args.run % 2 == 0 and image_count == IMAGES_ODD_RUN:
                is_image = False
        layout.append(is_image)

    # Continuous recognition: some trials repeat an image seen earlier
    # (in this run, or in earlier runs listed in --history).
    history = []
    if args.history and args.history.exists():
        history = [line.strip() for line in args.history.read_text().splitlines() if line.strip()]
    by_stem = {p.stem: p for p in pool}
    seen = [by_stem[h] for h in history if h in by_stem]
    unused = [p for p in pool if p.stem not in set(history)]
    rng.shuffle(unused)
    first_new = len(seen)
    rows = []
    for slot, is_image in enumerate(layout):
        if not is_image:
            continue
        onset = PRE_BLANK + slot * SLOT
        if not seen and not unused:
            raise SystemExit("ran out of images: every image in the pool is already in --history")
        if seen and (rng.random() < args.repeat_prob or not unused):
            img, memory = rng.choice(seen), "old"
        else:
            img, memory = unused.pop(), "new"
            seen.append(img)
        try:
            rel = img.relative_to(HERE)
        except ValueError:
            rel = img
        rows.append((onset, rel.as_posix(), memory, "1" if memory == "new" else "2", img.stem))

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        f.write("onset\tstim_file\tmemory\tanswer\timage\tnsd_id\n")
        for onset, stim, memory, answer, name in rows:
            # NSD file names look like shared0001_nsd02951.png -> 73k ID 2951.
            m = re.search(r"nsd(\d+)", name)
            nsd_id = str(int(m.group(1))) if m else "n/a"
            f.write(f"{onset:.1f}\t{stim}\t{memory}\t{answer}\t{name}\t{nsd_id}\n")
    if args.history:
        args.history.parent.mkdir(parents=True, exist_ok=True)
        with open(args.history, "a") as f:
            for img in seen[first_new:]:
                f.write(img.stem + "\n")
    n_old = sum(r[2] == "old" for r in rows)
    print(f"wrote {args.out}: {len(rows)} image trials ({n_old} old), {SLOTS - len(rows)} blank slots, run {args.run}")


if __name__ == "__main__":
    main()
