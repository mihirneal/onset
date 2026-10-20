# Image paradigm (NSD-style)

Modeled on the Natural Scenes Dataset core run (Allen et al. 2022): 3 s images + 1 s gap, continuous old/new recognition (button 1 = new, 2 = old), 7T TR 1.6 s × 188 volumes. Parameters come from NSD's original Psychtoolbox code.

The repo ships placeholder images. `onset make-schedule` builds a run's schedule from a folder of images, following the design in `[schedule.generate]` (see [docs/SCHEMA.md](../../docs/SCHEMA.md)). To regenerate the schedule and try it with a fake scanner:

```bash
onset make-schedule examples/nsd/experiment.toml --run 1
onset run examples/nsd/experiment.toml --rig examples/rigs/dev.toml --simulate --robot --sub 01
```

## Real NSD images

Accept the [NSD terms](https://cvnlab.slite.page/p/IB6BSeW_7o/Terms-and-Conditions) first. The images may not be redistributed, so keep them outside this repo (`.gitignore` blocks `*_nsd*.png`).

```bash
aws s3 sync s3://natural-scenes-dataset/nsddata/stimuli/nsd/shared1000/ ~/nsd/shared1000/ --no-sign-request
# one schedule per run; --history tracks old/new across runs for this participant
onset make-schedule examples/nsd/experiment.toml --run 1 --images ~/nsd/shared1000 \
    --history ~/nsd/sub-01_seen.txt --out ~/nsd/sub-01/run01.tsv
onset run examples/nsd/experiment.toml --schedule ~/nsd/sub-01/run01.tsv --rig my-rig.toml --sub 01 --run 1
```
