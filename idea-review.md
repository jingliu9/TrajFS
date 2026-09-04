# idea-review.md

Reviews are appended in order; earlier reviews are never edited.

---

## Review 1 — 2026-09-04 — DuckDB + Parquet + `traj` CLI  vs  tar + FUSE

Reviewer: Claude (session d57052fe), measured on the fourth-grid archive that was being committed to git at the time
(run `onesw-gen-outputs/suite-traj-log-summary-replicas/claude-opus-4.8-678xazw/onesw-generation-20260902T033145Z`,
host fwf2-n0, 32 cores, page cache warm). Scripts and raw numbers: `review-bench/bench.py`, `review-bench/fullrun.py`,
`review-bench/bench.json`, `review-bench/fullrun.json`.

### Verdict

**Neither "tar + FUSE" nor "Parquet as the blob store" is right on its own. The measured data says the storage layer must be
content-addressed (path → sha, sha → bytes) with an offset index for the bytes; put the catalog (and later the parsed
events) in Parquet queried by DuckDB; ship a thin `traj` CLI first; add FUSE later only as a projection over the same
backend.** That is closer to the DuckDB proposal than to tar+FUSE, but with one correction: Parquet is the right container
for metadata and for analytics, and the wrong container for random point reads of blobs.

The reason is not "small files" in the abstract. It is that the same small files are snapshotted over and over.

### 1. What the data actually looks like (measured)

Whole run, files kept by the archive filter (trajectories, review records, stdout/stderr/status, sources, tests, docs):

| quantity | value |
|---|---|
| paths | 2,173,703 |
| bytes | 12.93 GB (60.9 GB on disk before filtering) |
| distinct contents (sha256) | 23,864 |
| distinct bytes | 3.05 GB |
| duplication factor by count | 91× |
| duplication factor by bytes | 4.2× |
| `events.jsonl` (trajectories) | 161 files, 2.19 GB = 72 % of the distinct bytes |
| `session.md` | 77 files, 87 MB |

One round directory (`rounds/round-0037`, the benchmark sample): 107,449 paths, 474 MB, **7,418 distinct contents, 173 MB
distinct** (14.5× by count within a single round: `builder/workspace` and `selected/` are byte-identical copies, and the
per-call evidence files repeat).

Size distribution of kept files (rank 4 run, 699 K files): p50 = 40 bytes, p90 = 1.9 KB, p99 = 80 KB; 79 % of files are
≤ 1 KB and hold 1.8 % of the bytes; 100 % are ≤ 1 MB except the trajectories. Top names: `baseline-runtime.json` (29,613
copies), `runtime.json`, `summary.json`, `independent-check.json`, `NN.stdout`/`.stderr` per measurement call.

Consequences:

- Any format that is not content-addressed pays the 91× (tar per round, squashfs per round, parquet-with-content per
  round all store the same bytes 38 × 2 times across the run; only the within-round part is recovered by a compressor
  window or squashfs's own dedupe).
- Git is already content-addressed: rank 3's 1.87 M paths became 82,351 blobs / 2.11 GB. Git's problem is purely the
  number of *paths* (tree entries + index entries), not the bytes. Inserting 2.17 M paths into the 11 M-entry index took
  > 3.5 h and was still not done (quadratic memmove); grafting the pre-built subtree into HEAD's tree with `mktree`
  took seconds. So with git the pain is tooling (index, `git status`, checkout, clone of 5 M paths), not storage.
- The bulk of the *unique* bytes is the trajectories (`events.jsonl`, avg 13.6 MB). Those are not a small-files problem
  at all; they are row-structured logs that want to become an `events` table. That is where the OLAP argument is actually
  strong.

### 2. Benchmark

Sample = `rounds/round-0037` kept files (107,449 files, 474 MB). Whole run = 2.17 M files, 12.9 GB.

**Build cost and size**

| format | build time | size | notes |
|---|---|---|---|
| tar (python `tarfile`) | 18.0 s | 664 MB | GNU tar is faster; 512-byte headers on 40-byte files = 40 % overhead |
| tar + zstd -3 -T0 | +0.3 s | 38.3 MB | |
| tar + zstd -19 --long=27 | +44 s | 12.8 MB | long window dedupes within the round |
| parquet (path, dir, name, size, content), zstd, rg 4096 | 4.3 s | 33.3 MB | no dedupe |
| parquet dedup: `files` (path→sha) + `blobs` (sha→content) | 3.3 s | 1.6 MB + 17.5 MB | |
| squashfs, zstd, 128 K blocks | 2.5 s | 23.7 MB | dedupes identical files itself |
| **whole run**, content-addressed parquet (`files` + 12 blob shards) | 48 s (hash 22 s on 16 procs) | **23 MB + 449 MB** | 12.9 GB kept / 60.9 GB on disk → 472 MB in 13 immutable files |

For comparison, git for the same whole run: ~20 min to hash 2.17 M blobs, then hours for the index; 22 GB `.git/objects`
for the repo as a whole.

**Point read of one file (`cat`)**

| method | latency |
|---|---|
| tar.zst, no index (stream + `tar -xOf`) | 0.59 s |
| uncompressed tar + offset index (index build 6.9 s/round) | 7 µs |
| squashfs `unsquashfs -cat` | < 10 ms |
| DuckDB, single parquet, `where path = ?` (one round) | 8 ms |
| DuckDB, `files join blobs using(sha)` (one round) | 111 ms |
| DuckDB, same join, whole run (3 GB of blobs) | 1.26 s |
| DuckDB, direct `where sha = ?` on the blob shards | 0.12 s |

**Directory listing / find / grep**

| operation | native FS | tar.zst no index | DuckDB (one round) | DuckDB (whole run, 2.17 M rows) |
|---|---|---|---|---|
| `ls` one dir (64 entries) | 2.3 ms | 0.78 s | 2.4 ms | 10 ms |
| `find -name COMPLETE` | 0.36 s | — | 2 ms | 9 ms |
| grep `Traceback` in all `*.stdout` | 0.65 s | — | 0.18 s | — |
| grep `FAILED` in all `round*-final-test.stdout` | — | — | — | 5.4 s (join over blobs) |
| per-round file count / bytes | minutes of `du` | — | — | 0.18 s |

Reading of the numbers:

- Catalog operations (`ls`, `find`, `stat`, per-directory aggregates) are as fast in DuckDB over a 2 M-row `files` table
  as on a native filesystem, and orders of magnitude faster than any un-indexed tar. This settles "can `traj ls` replace
  `ls`": yes.
- Point reads are where Parquet loses. A Parquet blob column has no offset index; a lookup is a scan (row-group stats do
  not help much on a random sha), and a join hashes the blob side. 100 ms–1 s per `cat` is fine for a human, bad for an
  agent doing thousands of `cat`s, and terrible behind FUSE. An offset-indexed pack (git-pack, tar + index, squashfs) is
  7 µs–10 ms. So: **metadata in Parquet, bytes in offset-addressed packs** (or Parquet with a `(shard, row_group)`
  locator carried in `files` so one row group is read directly).
- Content-scan analytics over the whole run are single-digit seconds in DuckDB. Over FUSE the same grep would read
  91× redundant bytes unless the FUSE layer is dedupe-aware.

### 3. Against the stated requirements

Requirements from `idea.md`: (a) fast commit/push/pull, (b) `ls` etc. still work, (c) open individual files in VS Code
for review, (d) analysis, increasingly by agents.

**tar + FUSE (chunked tars committed to git, mounted for reading)**

- (a) Commit is fast only per round. Without content addressing each round's tar is a new blob containing 93 % old
  bytes: 38 rounds ≈ 1.4 GB pushed at zstd -3 (or 0.5 GB at zstd -19 with 44 s/round), versus 472 MB total for the whole
  run content-addressed. Git's delta compression across large binary blobs is unreliable, and blobs > 100 MB are rejected
  by GitHub, so chunking is mandatory anyway.
- (b, c) Good: a mount gives real POSIX paths, VS Code and `grep -r` just work. Cost: an index must be built and stored
  per tar (ratarmount: SQLite index, seconds per round-tar), the host needs FUSE (fusermount3 present here, ratarmount
  and squashfuse are not installed), and every access goes through a userspace round-trip.
- (d) Poor for analysis: every cross-run question becomes `find | xargs grep | jq` over 91× redundant bytes through FUSE.
- Note: squashfs is strictly better than tar for this role (built-in dedupe, directory index, kernel mount, 2.5 s to
  build a round, 23.7 MB) but mounting needs root or squashfuse.

**DuckDB + Parquet + `traj ls/cat/extract/sql`**

- (a) Excellent: per run ~13 immutable files (23 MB catalog + ~450 MB blob shards of 33–46 MB each, below GitHub's
  limit); later rounds add only new blobs. Pushes are additive.
- (b) `traj ls`, `traj find`, `traj du` are millisecond queries. Muscle memory changes, capability does not.
- (c) Weakest point. Humans get files via `traj cat` / `traj extract <subtree> /tmp/x`; extracting a full round (107 K
  files) is seconds. VS Code cannot open a Parquet blob; it opens the extracted tree. If human browsing is occasional,
  as `idea.md` states, this is acceptable. If it turns out to be daily, add FUSE over the same store (see hybrid).
- (d) Best of the options: `find`/`grep`/aggregates in ms–s, and, once `events.jsonl` is parsed into an `events` table,
  the task23/task24-style sweeps (review verdicts per round, roofline ratios, tool-call failures across runs) become
  single SQL statements instead of Python walkers. This is where the "AI agent consumer prefers SQL" argument is true;
  every analysis done on this grid so far was exactly such a sweep.
- Corrections to the proposal in `idea.md`: (1) do not put blob bytes in Parquet as the primary store (point-read cost
  above); (2) the "stable envelope + `payload_json`" schema advice is right, and `events.jsonl` already is that envelope,
  so the events table is a derived layer that can be rebuilt when the schema changes; (3) DuckDB's own `.db` file should
  never be the persistent artifact, Parquet + packs are.

### 4. Recommended design (hybrid, V1 without FUSE)

```
run.store/
  files.parquet        path, dir, name, size, mode, sha, round, role(builder|selected|reviewer)   ~23 MB / 2.2 M paths
  packs/NNNN.pack      zstd frames of distinct blobs, ≤ 64 MB each, append-only                    ~450 MB / run
  packs/index.parquet  sha → (pack, offset, length, uncompressed_size)                              small
  events/*.parquet     derived from events.jsonl: run, trajectory, seq, type, tool, exit, payload_json (rebuildable)
  MANIFEST.json        run id, source tree sha, rule version
```

- `selected/` becomes a list of shas (a manifest row per path), not a copy: the 2× within-round duplication and the
  38× across-round duplication vanish at write time, which is the same effect the hard-link + dedupe change in onesw-gen
  achieves on disk but at the archive layer.
- `traj ls|find|du|cat|grep|extract|sql`: `ls`/`find`/`du`/`sql` hit `files.parquet` only; `cat`/`extract` resolve sha →
  pack offset and `pread` one frame (microseconds); `grep` runs over distinct blobs once and maps hits back to paths.
- Integrity: per-file sha256 in the catalog doubles as the corruption check the in-place `selected/` protection needed.
- V2 (only if needed): a FUSE projection (`fuse-python`/`pyfuse3`) that serves `files.parquet` for the namespace and
  `pread`s the packs for data. Point reads at pack-offset speed, and VS Code sees a normal tree. This is the tar+FUSE
  idea done over a dedupe-aware store instead of over tar.
- V1 size estimate: ~300 lines of Python (hash + pack writer, catalog writer, six CLI verbs, DuckDB for `sql`). The
  whole-run build above took 48 s for 2.17 M files with a 16-process hasher.

### 5. Risks and open points

- Point reads via DuckDB alone stay at 0.1–1 s; the pack index is not optional.
- GitHub rejects files > 100 MB and warns > 50 MB: keep pack shards ≤ 64 MB (the 12 shards above are 33–46 MB).
- Trajectories (13.6 MB avg `events.jsonl`) go into packs as blobs *and* get parsed into `events/*.parquet`; if the
  parse is expensive, do it lazily per run.
- Keep the raw run tree on the host as today until the store has been round-tripped (extract → byte-identical) for at
  least one full run; only then treat the store as the archive copy.
- The archive-time filter (exclude regenerable CSV/binaries) still applies; content addressing does not remove the need
  for it because the excluded set is 90 % of the on-disk bytes.

### 6. Decision

Go with the content-addressed store + Parquet catalog + `traj` CLI (V1). Do not build tar+FUSE as the storage format.
Revisit FUSE as a read-only projection over the same store if extract-to-view turns out to be a daily friction.
