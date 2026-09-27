# TrajFS demo recording

`trajfs-demo.gif` is the animated quick-start shown in the top-level README: a scripted terminal
session that shows a million-file agent run, what Git does with it, what `traj pack` / `traj commit`
do instead, and how agents (`traj grep/sql/cat`) and humans (`traj mount`) read the store back.

Every number in the GIF is measured on the machine that ran `measure.py`; nothing is typed in by hand.

## Regenerate

Requirements: Linux, `git`, `tree`, `python3` (stdlib only), the release `traj` binary, FUSE for the
mount capture (the recording falls back to the captured output if the mount fails), and
[agg](https://github.com/asciinema/agg) (`cargo install --git https://github.com/asciinema/agg`)
plus DejaVu Sans Mono (`fonts-dejavu-core`). `asciinema` is not needed: `record.py` writes the
asciicast v2 file itself. Pillow is optional, for the `.png` poster frame.

```bash
cargo build --release -p traj

# 1. Generate the tree and measure git and traj on it (~1,000,000 files, 40 rounds, ~5-15 GB of
#    scratch under /workspace/trajfs-demo; takes 30-90 minutes, dominated by git). Writes
#    demo/measurements.json. Re-running is a no-op unless --force.
python3 demo/measure.py

# 2. Compose the asciicast (seconds; re-runs traj ls/cat/grep/sql live against the measured store).
python3 demo/record.py

# 3. Render the GIF with agg (about a minute). record.py writes three casts: the body and the
#    title and end cards, which render.sh draws at a larger font and joins with Pillow
#    (POSTER_PY=/path/to/python-with-pillow); without Pillow only the body is produced.
POSTER_PY=venv/bin/python demo/render.sh
```

The cards draw the logo's "TrajFS" wordmark with quadrant block cells from `demo/wordmark.txt`, which
`demo/make_wordmark.py` (Pillow) cuts from `docs/trajfs-logo.png`; rerun it only if the logo changes.

`measure.py --keep-git` keeps the multi-GB raw-tree git repositories; by default they are deleted
after their sizes are recorded. The synthetic tree (`bench/synthetic_tree.py`) and the packed store
`/workspace/trajfs-demo/exp/stores/task-a.trajstore` are kept.

For a fast end-to-end check use a small tree and separate output files:

```bash
python3 demo/measure.py --files 20000 --rounds 10 --out demo/measurements.small.json
python3 demo/record.py --measurements demo/measurements.small.json --out demo/trajfs-demo.small.cast
demo/render.sh demo/trajfs-demo.small.cast demo/trajfs-demo.small.gif
```

Knobs: `render.sh` reads `FONT_SIZE` (16), `FPS` (20), `SPEED` (1), `AGG_THEME` (default: the
GitHub-dark palette embedded in the cast header) and `POSTER_PY` (a python with Pillow).
`record.py --offline` uses the outputs captured by `measure.py` instead of running `traj`.

## What is measured

`measure.py` times, with wall clocks around whole processes:

- git on the raw tree (git dir outside the tree, `gc.auto=0` so a background gc cannot skew later
  timings): `add -A`, `commit`, `status`, `push` to a local bare remote, `clone --no-checkout` over
  `file://` (a real pack transfer, no hardlinks), `checkout main` (writing every file), and the
  `.git` size after one commit.
- TrajFS: `traj pack ... --adapter copilot-cli` (its summary line is shown verbatim), the store
  size on disk, `traj commit`, then `git status`, `git push` and `git clone` of the experiment repo
  that holds the store.
- the outputs of the read commands used in section 4 (`traj ls/grep/sql/cat`, and `traj mount`
  followed by `ls` and `cat` through the mount).

Sections of the GIF: title, 1 the tree, 2 git, 3 TrajFS and the comparison table, 4 split
agent/human view, 5 end card. `record.py` prints the timing of each section; the whole thing plays
in about 70 seconds. agg caps idle gaps at 3 s and the GIF loops.
