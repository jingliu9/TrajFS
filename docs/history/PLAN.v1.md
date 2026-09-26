# trajfs — design and test plan

Status: draft 1, 2026-09-04. Derived from `idea.md` and `idea-review.md` (Review 1, same directory). Superseded by `../PLAN.md`. Numbers quoted below are the
Review 1 measurements on the reference run
(2.17 M paths, 12.9 GB kept, 23,864 distinct blobs, 3.05 GB distinct).

## 1. Problem and goals

Agent run trees are millions of tiny, mostly duplicated files (p50 = 40 bytes; the same workspace is snapshotted twice
per round and carried forward every round: 91× duplication by path count). Git handles the bytes fine (it is
content-addressed) but not the paths: index insertion is quadratic, `git status`/checkout/clone of 5 M paths are
unusable, and every consumer pays the duplication again on read.

Goals, in priority order:

1. **Commit/push/pull are cheap and additive.** A run is a few dozen immutable files; a new round adds only new
   content. Target: whole run ≤ 5 % of kept bytes, ≤ 100 files, packing ≤ 2 min for 2 M paths.
2. **Namespace operations keep working without a mount.** `traj ls/find/du/stat/tree` answer from the catalog in
   milliseconds.
3. **Any file can be opened by a human in an editor.** `traj cat` / `traj extract` materialise a file or a subtree,
   byte-identical, in microseconds per file (pack offsets, not scans).
4. **Analysis, mostly by agents, is declarative.** `traj sql` over the catalog and over a derived `events` table
   (parsed trajectories), with a `blob(sha)` function to reach bytes from SQL.
5. **Integrity is verifiable.** Every path carries a sha256; `traj verify` proves a store (or a clone of it) is intact.

Non-goals (V1): a mounted filesystem (V2, §9); writes into the store other than append; cross-run global dedupe
(each run is its own store; a shared pack directory is a V2 option); replacing the archive filter rules (they are
an input, §4.2).

## 2. Decisions

| decision | choice | why (see Review 1) |
|---|---|---|
| storage model | content-addressed: `path → sha`, `sha → bytes` | 91× path duplication, 4.2× byte duplication; only dedupe removes both |
| bytes container | zstd-compressed **chunks** inside append-only **pack** files, with an offset index | Parquet blob lookups cost 0.1–1.3 s per file; pack offsets cost µs |
| chunk size | small blobs grouped to ≤ 1 MiB uncompressed per chunk; blobs > 1 MiB get their own chunk(s) | tiny files compress badly alone; 1 MiB decompress ≈ 1 ms read amplification |
| catalog container | Parquet (`files`, `dirs`, `blobs` index), read by DuckDB | 2.2 M-row `ls`/`find` in ≈ 10 ms; standard, portable |
| analytics engine | DuckDB, embedded, no state of its own | the Parquet files are the persistent artifact; DuckDB is only compute |
| trajectories | stored as blobs **and** parsed into `events/*.parquet` (derived, rebuildable) | 72 % of distinct bytes are `events.jsonl`; they are row-structured |
| shard size | packs and Parquet segments capped at ≤ 60 MiB (or a lower configured hook limit) | GitHub rejects > 100 MB, warns > 50 MB |
| mutability | append-only; ingestion batches; a `MANIFEST.json` lists batches | additive commits; a batch is the unit of retry |
| implementation language | Rust crate `trajfs`, binary `traj` (repo already has a Cargo `.gitignore`; the rounds-style runner is Rust) | one static binary for the measurement hosts; Python scripts from Review 1 stay as the cross-implementation oracle in tests |
| Rust deps | `parquet` + `arrow` (write/read catalog), `zstd`, `sha2`, `rayon`, `clap`, `duckdb` (bundled) for `sql`/`ls`/`find` | |

## 3. Store layout

```
<run>.trajstore/
  MANIFEST.json                     format version, run id, source path, rules version, batches[]
  catalog/
    files-0001[-0001].parquet       one or more physical segments per ingestion batch (sorted by path)
    dirs-0001[-0001].parquet        derived per batch: dir, parent, name, n_files, n_dirs, bytes
  packs/
    0001.pack  0002.pack ...        opaque: magic + zstd frames (chunks), ≤ 60 MiB each
    index-0001[-0001].parquet       sha → (pack, chunk_offset, chunk_len, offset_in_chunk, size)
  events/
    events-0001[-0001].parquet      derived from every events.jsonl in the batch (optional, `traj index-events`)
  .gitattributes                    *.pack -diff -delta binary ; *.parquet -diff binary
```

`MANIFEST.json` is the publication boundary: readers and SQL use only declared artifacts, reject missing declarations,
and ignore unlisted output from interrupted writers until orphan cleanup. Physical parts are ordered by parsed numeric
batch and part ids, not lexically. Segments are otherwise immutable.

### 3.1 `files` schema (Parquet, zstd, row groups of 65,536, sorted by `path` bytewise)

| column | type | notes |
|---|---|---|
| path | utf8 | relative to the run root, `/`-separated, NFC not applied (bytes preserved) |
| dir | utf8 | parent path, `""` for root |
| name | utf8 | basename |
| kind | uint8 | 0 file, 1 symlink, 2 empty file (no blob) |
| mode | uint16 | 0o644 or 0o755 (only the exec bit is kept, like git) |
| size | int64 | uncompressed bytes (symlink: target length) |
| sha | fixed_size_binary(32) | sha256 of content (symlink: of the target string) |
| mtime_ns | int64 | source mtime, informational |
| round | int32 nullable | parsed from `rounds/round-NNNN/` when present |
| role | utf8 nullable | `builder` / `selected` / `reviewer` / other top-level segment |
| batch | uint32 | ingestion batch id |

### 3.2 `dirs` schema

`dir, parent, name, depth, n_files, n_dirs, bytes, batch`. Enables `ls` (children of a dir = rows in `dirs` with
`parent = X` ∪ rows in `files` with `dir = X`) and `du` without scanning `files`.

### 3.3 Pack format

```
pack      := magic "TRAJPACK\x01" (9 bytes) , chunk*
chunk     := zstd frame (level 3; frame content size set) of concat(blob bytes...)
```

The pack itself carries no directory; `packs/index-*.parquet` is authoritative:

| column | type |
|---|---|
| sha | fixed_size_binary(32) |
| pack | uint32 |
| chunk_offset | int64 (byte offset of the zstd frame in the pack) |
| chunk_len | int32 (compressed frame length) |
| offset | int32 (offset of the blob inside the decompressed chunk) |
| size | int64 |

Read path for one blob: look up sha (DuckDB, or an in-memory hash map built from the index on first use, ~24 K
entries per run), `pread(chunk_offset, chunk_len)`, zstd-decompress ≤ 1 MiB, slice. Blobs > 1 MiB are stored as a
sequence of chunks (`offset = 0`, one row per chunk with a `part` column, V1 keeps it simple: a large blob is a single
frame; frames are streamed on read).

Optional (measure first, §8): a zstd dictionary trained on the run's small blobs, stored as `packs/dict.zstd`, applied
to small-blob chunks. Skip if it does not beat plain chunks by ≥ 20 %.

### 3.4 `events` schema (derived)

Envelope only; agent-specific content stays in `payload_json` (Review 1 §3, "stable envelope + flexible payload").
The observed envelope of the reference run's `events.jsonl` is `{type, timestamp, id, parentId, ephemeral, data}`.

| column | type | source |
|---|---|---|
| trajectory | utf8 | path of the `events.jsonl` |
| round, role | as in `files` | |
| seq | int32 | line number |
| id, parent_id | utf8 | `id`, `parentId` |
| ts | timestamp(µs) | `timestamp` |
| type | utf8 | `type` |
| ephemeral | bool | |
| tool_name | utf8 nullable | promoted from `data` when `type` starts with `tool.` or `model.tool_execution` |
| exit_code | int32 nullable | promoted when present |
| payload_json | utf8 | `data` verbatim |
| schema_version | uint16 | parser version |

Promotion rules live in one small module; changing them means re-running `traj index-events`, never re-packing.

### 3.5 `MANIFEST.json`

```json
{"format": 1, "run_id": "...", "source": "/data/experiments/task-1/run-42",
 "rules": {"name": "my-runner-archive", "version": 3},
 "batches": [{"id": 1, "created": "2026-09-04T07:12:00Z", "paths": 2173722, "bytes": 12928153598,
              "new_blobs": 23864, "new_blob_bytes": 3053283334, "packs": [1, 12], "segments": ["files-0001"],
              "source_tree_sha256": "..."}]}
```

## 4. Write path: `traj pack`

`traj pack <run-dir> --store <dir> [--rules <profile>] [--jobs N] [--batch-label L]`

1. **Walk** the run tree (parallel, `rayon`), apply the rules (§4.2), producing the candidate path list. Symlinks are
   recorded, never followed. Hard links are ordinary files (dedupe makes them free).
2. **Skip already-ingested paths**: a path present in an existing segment with the same `(size, mtime_ns)` is assumed
   unchanged and dropped from the batch; a path with a different `(size, mtime_ns)` is re-hashed and, if its sha
   changed, recorded again in the new segment (the newest segment wins on read; `traj verify` reports the shadowing).
3. **Hash** every candidate (sha256, 16 workers: 22 s for 2.17 M files / 12.9 GB warm).
4. **Pack** the blobs whose sha is not in any existing index: sorted by `(dir, name)` so neighbours share a chunk;
   chunk cut at 1 MiB uncompressed; pack capped at 60 MiB compressed; each pack written to `NNNN.pack.tmp` and renamed
   when sealed.
5. **Write** `catalog/files-B[-P].parquet`, `catalog/dirs-B[-P].parquet`,
   `packs/index-B[-P].parquet`, then append the batch to
   `MANIFEST.json` (written to a temp file and renamed). Nothing outside the new batch is touched; a crash before the
   manifest update leaves orphan files that the next `traj pack` removes after checking they are unreferenced.
6. Print a summary (paths, bytes, new blobs, new bytes, packs, elapsed) and exit non-zero if any file could not be read
   (the batch is still written; unreadable paths are listed in `MANIFEST.json.batches[].errors`).

Lock: `<store>/.lock` (flock) for the duration of a pack; readers do not lock.

### 4.1 Round-incremental use

Rounds are append-only, so running `traj pack` after every round is the intended mode: batch N contains the new
round directory's paths, and its new blobs only (a round adds ≈ 7–12 MB of new distinct bytes in the measured run).
Snapshots that the runner still writes (`selected/` as a copy) cost nothing but catalog rows.

### 4.2 Rules

The archive rules from the reference run's Python archive filter (exclude `.cache`, `node_modules`, `target`, `build`, binaries, bulk measurement
CSV/JSONL under `logs/`/`results/`/`experiments/`, files > 20 MB, ...) become a TOML rule set shipped in the crate
(`rules/<profile>.toml`) and recorded by name+version in the manifest. `traj pack --rules none` stores
everything. Excluded paths are written to `catalog/excluded-B[-P].parquet` (path, size, rule) so the manifest of what was
left out travels with the store, as `ARCHIVE-EXCLUDED.tsv` does today.

## 5. Read path: the CLI

All verbs take `--store <dir>` (or `TRAJ_STORE`), and a path argument relative to the run root.

| verb | semantics | data touched |
|---|---|---|
| `traj ls [-l] [-R] <dir>` | children (dirs first), `-l` adds kind/mode/size/round | catalog |
| `traj tree <dir> [--depth d]` | | catalog |
| `traj find <dir> [--name glob] [--path glob] [--kind f/l] [--size ±N] [--round N]` | prints paths | catalog |
| `traj du <dir> [--depth d]` | bytes and counts, deduplicated and raw | catalog |
| `traj stat <path>` | kind, mode, size, sha, mtime, batch, pack location | catalog + index |
| `traj cat <path>...` | bytes to stdout; verifies sha unless `--no-verify` | index + pack |
| `traj extract <path-or-dir> <dst> [--hardlink-dedupe] [--mtime]` | recreates files/symlinks/modes; identical blobs may be hard-linked | index + packs |
| `traj grep [-e re]... [--name glob] [--path prefix] [-l] [-c]` | regex over distinct blobs once, results mapped back to every path; skips binary blobs unless `-a` | index + packs |
| `traj sql "<query>"` | DuckDB with views `files`, `dirs`, `blobs` (index), `events`, and scalar functions `blob(sha) → BLOB`, `text(sha) → VARCHAR` | all |
| `traj index-events [--force]` | build/rebuild `events/*.parquet` from every `events.jsonl` | packs → events |
| `traj verify [--deep]` | catalog ↔ index ↔ packs consistency; `--deep` re-hashes every chunk's blobs | all |
| `traj edit <path>` | extract to a temp file, run `$EDITOR`, print a diff (never written back) | |

Latency targets (warm cache, 2 M-path store): `ls`/`find` ≤ 50 ms; `stat` ≤ 20 ms; `cat` ≤ 5 ms for a small file
after a 100 ms one-time index load; `extract` of a 100 K-file round ≤ 30 s; `grep` over all distinct text blobs of a
run ≤ 10 s.

## 6. Git integration

- The store directory is committed as ordinary files. With ≤ 60 MiB shards no LFS is needed. `.gitattributes` marks
  packs and Parquet as binary and `-delta` so git does not waste time trying to delta them.
- A batch is a commit: `git add <store>/catalog/*-B.parquet <store>/packs/* <store>/MANIFEST.json`. Only new files are
  added, so `git add`, `commit` and `push` cost is proportional to the batch (tens of files), not to the run.
- A fresh clone contains every store; `traj` reads directly from the clone. Nothing needs to be extracted to browse.
- The existing archive commits of raw trees (5.3 M paths on the experiment repository's archive branch) stay as they are; the
  first `traj` milestone re-packs those four runs from disk and the stores are committed beside them.

## 7. Integration with the rounds-style runner

Phase A (archive time, no runner change): the runner's archive script calls `traj pack` per run at grid stop, or a
cron/`Monitor` hook calls it after every `rounds/round-N/selected-storage.json` appears (the file the runner writes when
a round is finalised).

Phase B (write time, later): the runner's `selected/` snapshot becomes a manifest of shas produced by `traj pack`, and
`seed_builder_workspace` extracts from the store. This removes the on-disk 2× copy entirely and reuses the store's
sha check as the seed integrity check. Out of scope for V1.

## 8. Performance and size targets (acceptance)

Measured on the reference run with the Python prototype; the Rust build must meet or beat them.

| metric | target | prototype |
|---|---|---|
| pack 2.17 M paths / 12.9 GB, warm | ≤ 120 s | 48 s (hash 22 s + write 26 s) |
| store size for that run | ≤ 600 MB | 472 MB (23 MB catalog + 449 MB blobs) |
| files per store | ≤ 100 | 13 |
| `ls` one dir | ≤ 50 ms | 10 ms |
| `find --name COMPLETE` (133,921 hits) | ≤ 50 ms | 9 ms |
| `cat` one small file (after index load) | ≤ 5 ms | 7 µs (tar offset), 8 ms (parquet single table) |
| per-round aggregate over `files` | ≤ 0.5 s | 0.18 s |
| `grep` one pattern over all distinct stdout | ≤ 10 s | 5.4 s (join over Parquet blobs; packs should be faster) |

## 9. V2: FUSE projection (optional)

A read-only FUSE filesystem (`traj mount <store> <mnt>`, `fuser` crate) that serves `getattr`/`readdir` from an
in-memory tree built from `files`+`dirs` (≈ 2 M entries, ~200 MB) and `read` by pack offset. It gives VS Code and
`grep -r` a normal tree with no per-file cost beyond one chunk decompress. Build it only if `traj extract`-to-view is
found to be a daily friction; the store format does not change.

## 10. Milestones

| id | deliverable | exit criterion |
|---|---|---|
| M0 | this plan; fixtures: a 200-file synthetic tree and the `round-0037` sample list; Python prototype kept as an oracle (later dropped; its raw results are in `bench/`) | fixtures committed |
| M1 | pack writer/reader, `cat`, `extract`, `verify` | T1–T3, T6 green; round-trip of `round-0037` byte-identical |
| M2 | catalog + `ls/tree/find/du/stat` with DuckDB | T4 green; latency targets in §8 met on the reference run |
| M3 | `grep`, `sql` with `blob()`/`text()`, `index-events` | T5, T7 green |
| M4 | rules file, incremental batches, manifest, `.gitattributes`, archive of the reference run and its three sibling runs as stores | T8–T9 green; §8 size/file targets met; stores pushed |
| M5 | the runner's Phase A hook; docs (`README.md`, `OPERATIONS.md` §7) | one live round packed automatically |
| M6 | FUSE (only if triggered by §9) | T10 |

## 11. Test plan

Test code lives in `tests/` (Rust integration tests) and `tests/oracle/` (Python, the prototype's venv).
`cargo test` runs T1–T7 on the synthetic fixture in < 60 s; `cargo test --features slow` adds the reference-run tests.

**T1 Pack format, unit.** Chunk cutting at exactly 1 MiB; a 0-byte blob (kind = empty, no index row); a blob of
exactly 1 MiB, 1 MiB + 1, and 100 MiB (multi-frame path); pack sealing below 60 MiB; magic and frame boundaries validated
by an independent zstd decoder; index rows point inside their pack.

**T2 Round trip, property-based.** Random trees (proptest): depth ≤ 8, names with spaces/unicode/leading dots/`\n`
excluded, kinds file/symlink/empty, modes 644/755, sizes drawn from the measured distribution (p50 40 B, p99 80 KB, a
few multi-MB), 30 % duplicate contents. `pack → extract` must give a byte-identical tree (`diff -r --no-dereference`),
identical modes and symlink targets; `--hardlink-dedupe` output has `nlink > 1` only for identical blobs.

**T3 Integrity.** After packing: flip one byte in a pack → `verify --deep` names the chunk and every affected path;
`cat` of an affected path fails with a sha mismatch unless `--no-verify`. Truncate a pack → `verify` fails, other packs
still readable. Delete a manifest-declared artifact → quick and deep verification refuse the store. Delete
`MANIFEST.json` → every verb refuses with a clear error.

**T4 Catalog semantics.** For the synthetic tree, `ls`, `tree`, `find`, `du`, `stat` output equals the output of the
same operations on the extracted tree (golden comparison via `ls -A`, `find`, `du -b --apparent-size`), including
empty directories represented only through `dirs`, and `find --round/--role` on paths that match `rounds/round-N/`.

**T5 grep and sql.** `traj grep -l PATTERN` equals `grep -rl PATTERN` on the extracted tree for 20 random patterns
(including one that matches only a duplicated blob, verifying hit-to-all-paths mapping); binary blobs skipped unless
`-a`. `traj sql` views exist; `blob(sha)` returns the same bytes as `cat`; a join `files ⨝ blobs` runs; `events`
row count equals the line count of all `events.jsonl`; `type`/`ts`/`parent_id` round-trip; `payload_json` re-parses to
the original `data` object.

**T6 Reference round (slow).** Pack `rounds/round-0037` from the reference run: exactly 107,449 paths, 7,418 distinct
blobs, extract byte-identical (`diff -r`), store ≤ 30 MB; `cat` p50 ≤ 5 ms over 1,000 random paths (after warm-up).

**T7 Whole run (slow).** Pack the full reference run: 2,173,722 paths (2,173,703 files + 19 symlinks), 23,864
distinct blobs, 3.05 GB distinct; all §8 targets; `verify --deep` passes; `extract` of one full round then
`diff -r` against the source.

**T8 Incremental batches.** Pack rounds 1–10, then 1–11: batch 2 contains only round 11 paths and only blobs not in
batch 1; `MANIFEST.json` lists two batches; a path modified between batches is recorded again and `stat` shows the
newest; killing `traj pack` (SIGKILL) mid-pack leaves the store readable and `verify` clean, and the next `pack`
removes the orphan `.tmp` files and completes the batch.

**T9 Git.** Commit a store to a scratch repo: every file ≤ 60 MiB by default; `git add` of batch 2 touches only batch-2 files;
`.gitattributes` marks packs binary; clone the scratch repo and run T4/T5 against the clone unchanged. Push the four
stores to a side branch and time push/clone against the raw-tree commits.

**T10 FUSE (V2 only).** Mount the reference round; `diff -r` against the source tree; `grep -r` result equality;
`ls -R` time ≤ 3× native; concurrent readers; unmount under open files.

**Compatibility checks (part of T7).** `catalog/*.parquet` and `packs/index-*.parquet` are readable by DuckDB CLI,
Python `pyarrow`, and `polars` without options; packs are decodable by the `zstd` CLI (`zstd -dc` of one frame
extracted by offset).

**Regression baseline.** The prototype scripts are re-run on each milestone; their JSON is committed under a
history directory (`<date>.json`) so the §8 table can be compared over time.
