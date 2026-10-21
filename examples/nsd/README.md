# Image paradigm (NSD-style)

Modeled on the Natural Scenes Dataset core run (Allen et al. 2022): 3 s images + 1 s gap, continuous old/new recognition (button 1 = new, 2 = old), 7T TR 1.6 s × 188 volumes. Parameters come from NSD's original Psychtoolbox code.

No images ship with this example: the NSD images may not be redistributed. Accept the [NSD terms](https://cvnlab.slite.page/p/IB6BSeW_7o/Terms-and-Conditions), download them outside this repo, and `onset make-schedule` builds each run's schedule from that folder, following the design in `[schedule.generate]` (see [docs/SCHEMA.md](../../docs/SCHEMA.md)). `.gitignore` blocks `*_nsd*.png` in case they end up here.

```bash
aws s3 sync s3://natural-scenes-dataset/nsddata/stimuli/nsd/shared1000/ ~/stimuli/shared1000/ --no-sign-request
```

To try it with a fake scanner and participant:

```bash
onset make-schedule examples/nsd/experiment.toml --run 1 --images ~/stimuli/shared1000
onset run examples/nsd/experiment.toml --rig examples/rigs/dev.toml --simulate --robot --sub 01
```

For real runs, make one schedule per run; `--history` tracks old/new across runs for this participant:

```bash
onset make-schedule examples/nsd/experiment.toml --run 1 --images ~/stimuli/shared1000 \
    --history ~/stimuli/sub-01_seen.txt --out ~/stimuli/sub-01/run01.tsv
onset run examples/nsd/experiment.toml --schedule ~/stimuli/sub-01/run01.tsv --rig my-rig.toml --sub 01 --run 1
```
