# Benchmarks on synthetic trajectory trees

Every number here comes from [`bench/run.py`](../../bench/run.py) on generated data, so anyone can reproduce it without access to private trajectories. The source file for this page is [`bench/results/2026-09-26T161044Z.json`](../../bench/results/2026-09-26T161044Z.json); [`bench/plot.py`](../../bench/plot.py) renders the charts and this table from it.

## Host

| | |
|---|---|
| CPU | Intel(R) Xeon(R) Silver 4114 CPU @ 2.20GHz (40 logical cores) |
| Memory | 187.6 GB |
| Kernel | 6.8.0-138-generic |
| Filesystem | ext4 |
| traj | traj 0.1.0 (release build) |
| git | git version 2.43.0 |
| Run | started 2026-09-26T161044Z, total 18 min |

## Reproduce

```bash
cargo build --release -p traj
python3 bench/run.py --sizes 100000,300000,1000000 --rounds 20,30,40 --read-runs 5
python3 bench/plot.py            # newest bench/results/*.json -> docs/benchmarks/
```

`run.py` needs only the Python standard library; `plot.py` needs matplotlib. Git runs with an isolated configuration (`user.name=trajfs-bench, user.email=bench@example.invalid, core.fsmonitor=false, gc.auto=0, init.defaultBranch=main, advice.detachedHead=false`), a local bare repository as the remote, and `git clone` from that bare repository. The raw tree's repository keeps its `GIT_DIR` outside the tree so `traj pack` never sees a `.git` directory. TrajFS packs with `--adapter copilot-cli` and the default retention rules, commits with `traj commit <store>` inside a plain git repository that holds only the store, and adds the extra round as a second batch (`--label`). Any single command over 25 minutes is recorded as aborted and larger sizes are skipped.

All measurements are **warm-cache**: before each measured side the runner syncs, waits for Dirty: in /proc/meminfo to drop below 50 MB (max 60 s) and reads every file once (tar -cf /dev/null); that pass is recorded per size under warmups but never counted. Both sides therefore start from an identical page-cache state.

## Git commands

![git commands](git-commands.png)

Whole-process wall time. *add + commit* is `git add -A` + `git commit` versus `traj pack` + `traj commit`; the ingest comparison is apples to apples because both read every byte of the tree (`traj pack` includes a full re-read of unchanged files as its change check, which is also why the second, one-round batch is not free). *checkout* is the round trip from the second commit to the first and back.

| Files | Rounds | Command | git on raw tree | TrajFS | Speedup |
|---:|---:|---|---:|---:|---:|
| 99,830 | 20+1 | add + commit | 6.05 s | 2.45 s | 2.5x |
| 99,830 | 20+1 | status | 0.37 s | 9 ms | 41x |
| 99,830 | 20+1 | push | 3.20 s | 0.74 s | 4.3x |
| 99,830 | 20+1 | clone | 5.55 s | 65 ms | 85x |
| 99,830 | 20+1 | checkout, round trip | 1.76 s | 26 ms | 68x |
| 300,180 | 30+1 | add + commit | 16.2 s | 6.57 s | 2.5x |
| 300,180 | 30+1 | status | 0.85 s | 9 ms | 95x |
| 300,180 | 30+1 | push | 6.79 s | 2.10 s | 3.2x |
| 300,180 | 30+1 | clone | 17.2 s | 94 ms | 184x |
| 300,180 | 30+1 | checkout, round trip | 4.16 s | 26 ms | 160x |
| 999,580 | 40+1 | add + commit | 50.9 s | 18.9 s | 2.7x |
| 999,580 | 40+1 | status | 2.27 s | 9 ms | 252x |
| 999,580 | 40+1 | push | 17.0 s | 7.00 s | 2.4x |
| 999,580 | 40+1 | clone | 59.5 s | 0.23 s | 263x |
| 999,580 | 40+1 | checkout, round trip | 12.0 s | 28 ms | 430x |

Individual steps:

| Files | git add | git commit | git add (2nd) | git commit (2nd) | traj pack | traj commit | traj pack (2nd) | traj commit (2nd) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 99,830 | 4.72 s | 1.33 s | 0.90 s | 0.34 s | 1.72 s | 0.73 s | 1.42 s | 0.45 s |
| 300,180 | 13.5 s | 2.77 s | 2.24 s | 0.86 s | 4.39 s | 2.17 s | 3.71 s | 1.18 s |
| 999,580 | 43.0 s | 7.93 s | 5.90 s | 2.50 s | 11.5 s | 7.38 s | 12.2 s | 3.88 s |

## Space

![space](space.png)

| Files | Raw tree | `.git` working repo (loose) | git objects, packed (bare) | TrajFS store | of which events tables | Store repo `.git` | Distinct contents |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 99,830 | 260 MB | 22 MB | 12 MB | 7 MB | 135 kB | 7 MB | 8,605 of 107,310 paths |
| 300,180 | 844 MB | 58 MB | 26 MB | 22 MB | 201 kB | 22 MB | 18,576 of 316,169 paths |
| 999,580 | 2.82 GB | 178 MB | 68 MB | 74 MB | 260 kB | 74 MB | 48,572 of 1,041,236 paths |

Sizes are `du -sb` after both commits. The store size includes the parsed `events` tables (`derived/`), which `--no-derive` would omit.

Deduplication, as reported by the `traj pack` summary line (first batch; sizes as traj prints them) and by `traj sql` after both batches:

| Files | Batch-1 paths | New blobs | Blob bytes raw | Blob bytes packed | Distinct contents after both batches |
|---:|---:|---:|---:|---:|---:|
| 99,830 | 99,770 (228.7 MB) | 8,315 | 21.7 MB | 4.0 MB | 8,605 of 107,310 (8.0%) |
| 300,180 | 300,090 (760.2 MB) | 18,099 | 49.0 MB | 9.1 MB | 18,576 of 316,169 (5.9%) |
| 999,580 | 999,460 (2.51 GB) | 47,570 | 125.3 MB | 23.6 MB | 48,572 of 1,041,236 (4.7%) |

## Read path

![read path](read-path.png)

On the 1,041,359-file tree; median of 5 whole-process runs after one warm-up, warm page cache, except that whole-tree walks through the FUSE mount are measured once with no warm-up (marked). `ls` lists `rounds/round-0001/builder/logs/round-0001`; `cat` reads `rounds/round-0001/reviewer/review.json`; one round is `rounds/round-0040`, the largest checkpoint. A recursive walk through the mount visits every duplicate path, which is why `traj grep`/`traj find` (one pass over distinct contents and the catalog) are the recommended whole-store tools.

| Operation | Raw files | `traj` command | FUSE mount |
|---|---:|---:|---:|
| ls one directory | 8 ms | 82 ms | 9 ms |
| cat one file | 4 ms | 79 ms | 5 ms |
| find -name review.json, one round | 80 ms | 580 ms | 492 ms |
| grep -rl Traceback --include='*.stderr', one round | 190 ms | 1.05 s | 892 ms |
| find -name review.json, whole tree | 2.36 s | 9.12 s | 84.7 s (1 run) |
| grep -rl Traceback --include='*.stderr', whole tree | 4.49 s | 9.92 s | 84.0 s (1 run) |
| sql: `select count(*) from files` | - | 131 ms | - |
| sql: tool-call counts over events | - | 130 ms | - |

The events query groups 18,528 parsed event rows by tool name.

## What synthetic data does and does not capture

The generator (`bench/synthetic_tree.py`) models what makes real trajectory trees hard for Git: hundreds of thousands to millions of small files in a rounds layout where every round is a checkpoint of the whole task directory, so round *r* carries the workspace again (about 2% of its files edited that round) plus the logs of every round up to *r*, together with Copilot-CLI-style `events.jsonl` streams and a few build products for the retention rules to exclude. Most paths are therefore byte-for-byte copies of files in earlier rounds, and later rounds are larger than earlier ones. In the largest tree here, 48,572 of 1,041,236 recorded paths (5%) have distinct contents; the real Task A recorded in the main README had 23,685 distinct contents among 2.1 million paths (about 1%), so the model is conservative and real trees deduplicate better still. Contents are log-like and code-like text drawn from a small vocabulary, which compresses in the normal range for text but not exactly like real tool output, and the size distribution (log-normal, median near 1 KB, long tail to a few hundred KB) is a guess rather than a measurement. The tree has no binary data, no very large files and one task with three roles, and every command ran on a local ext4 disk with a warm page cache, so treat these results as a controlled comparison of the two workflows on the same tree, not as a prediction of any particular dataset's numbers.
