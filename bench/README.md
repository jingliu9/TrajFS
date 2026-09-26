# bench/: reproducible Git-vs-TrajFS benchmark on synthetic trees

Everything here runs on generated data, so the numbers in
[`docs/benchmarks/`](../docs/benchmarks/README.md) can be reproduced by anyone without access to real
trajectories.

| File | Purpose |
|---|---|
| `synthetic_tree.py` | Generates a deterministic rounds-style trajectory tree (`--files`, `--rounds`, `--seed`) in which every round is a checkpoint of the whole task directory; `--only-rounds FIRST LAST` writes only those rounds of the same tree, which is how the runner appends round R+1 |
| `run.py` | Generates trees, times the git-native and the TrajFS workflows, writes `results/<UTC timestamp>.json` (standard library only) |
| `plot.py` | Renders the newest (or a given) results JSON into `docs/benchmarks/*.png` and `docs/benchmarks/README.md` (needs matplotlib) |
| `results/` | Result files; each one is self-contained and includes a `host` block |

## Run

```bash
cargo build --release -p traj                 # run.py uses target/release/traj by default

python3 bench/run.py --sizes 100000           # quick run: 100k files, 20 rounds, a few minutes
python3 bench/run.py                          # default: 100k, 300k, 1M files (20/30/40 rounds)
python3 bench/run.py --sizes 50000,200000 --rounds 10,20 --read-runs 3 --keep
```

Options: `--work DIR` (scratch directory, default `/workspace/trajfs-bench`; it must be outside every git
worktree and needs a few GB per million files), `--traj PATH`, `--out FILE`, `--read-runs N` (default 5),
`--read-path-all` (measure the read path at every size, not only the largest), `--git-timeout-min` (default 25;
a command over the limit is recorded as aborted and larger sizes are skipped), `--seed`, `--keep` (do not delete
the trees and repositories afterwards).

Progress goes to stderr, one line per timed command; the JSON is rewritten after every step so a killed run
still leaves partial results.

## What is measured

For each size the runner generates the tree, then:

- **TrajFS**: `traj pack <tree> --out <repo>/stores/synthetic.trajstore --adapter copilot-cli --label round-NNNN`
  (default retention rules), `traj commit <store>` in a plain git repository that holds only the store (no
  `traj init` is needed for `commit`), `git status`, `git push` to a local bare repository, `git clone` from it.
- **git-native**: `git add -A`, `git commit`, `git status`, `git push` to a local bare repository, `git clone`,
  with the tree as the worktree. The repository's `GIT_DIR` lives outside the tree, so `traj pack` never reads a
  `.git` directory. Git uses an isolated config (`core.fsmonitor=false`, `gc.auto=0`, bench user).
- One more round is appended to the tree; both sides commit it (second `traj pack --label` batch / second
  `git add -A` + `git commit`) and then `git checkout` the first commit and `main` again (timed separately).
- The `traj pack` summary line (paths, new blobs, raw and packed bytes) is parsed into `pack_summary` /
  `pack2_summary`, and `traj sql` counts paths, distinct contents and event rows after both batches.
- Sizes: `du -sb` of the raw tree, of the working `.git` (loose objects), of the bare repository after a final
  untimed push (packed objects), of the store and of the store repository's `.git`; `git count-objects -vH` output.
- **Read path** (largest size only, median of `--read-runs` runs after a warm-up, whole-process wall time):
  `ls` of one round's `builder/logs`, `find -name review.json`, `grep -rl Traceback --include='*.stderr'`, `cat`
  of one file, on the raw files, with `traj ls/find/grep/cat`, and through a `traj mount --daemon` FUSE mount
  (rows are skipped with the reason if mounting fails); plus `traj sql "select count(*) from files"` and a
  tool-call count over `events`.

Only the subprocess is timed; stdout is discarded so terminal rendering is not part of the measurement.
All measurements are warm-cache: before each measured side (TrajFS batch 1, git commit 1, git commit 2, TrajFS
batch 2) the runner syncs, waits for `Dirty:` in `/proc/meminfo` to drop below 50 MB (at most 60 s) and reads
every file once with `tar -cf /dev/null`; those passes are recorded under `warmups` and never counted.

## Plot

```bash
python3 -m venv .venv && .venv/bin/pip install matplotlib
.venv/bin/python bench/plot.py                          # newest bench/results/*.json
.venv/bin/python bench/plot.py bench/results/2026-09-26T141345Z.json --out-dir /tmp/plots
```

`plot.py` writes `git-commands.png`, `space.png`, `read-path.png` (2x DPI, light background) and regenerates
`docs/benchmarks/README.md` with the host block, the exact reproduction commands and the results tables.
