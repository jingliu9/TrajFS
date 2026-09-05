# trajfs — design and test plan

Status: draft 2.5, 2026-09-04 (2.1 adds §6.1, raw data outside the repo; 2.2 adds §12 usage procedure and §13 agent skill; 2.3 adds §2.1 language rationale; 2.4: Rust everywhere, no Python in build or test paths; 2.5: FUSE removed entirely, no shell or Python anywhere, git hook is the binary itself). Draft 1 is kept as `PLAN.v1.md` beside this file. Change in draft 2: the core is format-agnostic; everything
that knows about a particular agent runner (onesw-gen rounds, GitHub Copilot CLI `events.jsonl`, Claude Code session
logs, ...) lives in an **adapter**. The four fourth-grid onesw lanes remain the primary test dataset, not the design
target.

Derived from `idea.md` and `idea-review.md` (same directory) (Review 1). Numbers quoted are Review 1 measurements on the fourth-grid run
`claude-opus-4.8-678xazw/onesw-generation-20260902T033145Z` (2.17 M paths, 12.9 GB kept, 23,864 distinct blobs).

## 1. Problem and goals

Agent runs produce directory trees with millions of tiny, mostly duplicated files: per-step stdout/stderr, status
markers, JSON records, snapshots of a workspace taken again and again. In the measured corpus p50 file size is 40 bytes
and the same content appears under 91 different paths on average. Git is content-addressed and handles the bytes, but
not the paths: index insertion is quadratic, `status`/checkout/clone of millions of paths are unusable, and every reader
pays the duplication again.

trajfs is a store for **any** such tree. It knows nothing about what the files mean; adapters can add meaning.

Goals, in priority order:

1. **Commit/push/pull are cheap and additive.** A run is a few dozen immutable files; new data adds only new content.
   Target: store ≤ 5 % of kept bytes, ≤ 100 files per run, packing ≤ 2 min per 2 M paths.
2. **Namespace operations work without a mount.** `traj ls/find/du/stat/tree` answer from the catalog in milliseconds.
3. **Any file can be opened by a human.** `traj cat` / `traj extract` materialise a file or subtree byte-identically, in
   microseconds per file (pack offsets, never scans).
4. **Analysis, mostly by agents, is declarative.** `traj sql` over the catalog and over adapter-derived tables (e.g. an
   `events` table parsed from trajectories), with `blob(sha)` to reach bytes from SQL.
5. **Integrity is verifiable.** Every path carries a sha256; `traj verify` proves a store, or a clone of it, is intact.

Non-goals: a mounted filesystem (dropped: `extract`/`edit` cover human viewing); in-place modification of stored content; cross-store dedupe (a shared
pack directory is a V2 option); defining a universal trajectory schema (adapters own that, and the raw bytes are always
kept).

## 2. Decisions

| decision | choice | why |
|---|---|---|
| storage model | content-addressed: `path → sha`, `sha → bytes` | 91× path duplication, 4.2× byte duplication in the measured corpus; only dedupe removes both |
| bytes container | zstd-compressed **chunks** in append-only **pack** files with an offset index | Parquet blob lookups cost 0.1–1.3 s per file (Review 1); pack offsets cost µs |
| chunk size | small blobs grouped to ≤ 1 MiB uncompressed; larger blobs get their own frame(s) | tiny files compress badly alone; 1 MiB decompress ≈ 1 ms read amplification |
| catalog container | Parquet, read by DuckDB | `ls`/`find` over 2.2 M rows in ≈ 10 ms; portable to pyarrow/polars/Spark |
| analytics engine | DuckDB, embedded, stateless | the Parquet files are the artifact; DuckDB is only compute |
| runner-specific knowledge | **adapters** (§3.6): path attributes, trajectory detection, event parsing, rule profiles, ingestion triggers | keeps the core generic; adding a new agent format never changes the store format |
| raw bytes are always kept | trajectories are stored as blobs *and* optionally parsed into derived tables | derived tables are rebuildable when an adapter changes; raw is the truth |
| shard size | packs, Parquet segments, and serialized manifests capped at ≤ 60 MiB (or a lower configured hook limit) | stays safely below the 65 MiB hook and GitHub's 100 MB hard limit |
| mutability | append-only batches listed in `MANIFEST.json` | additive commits; a batch is the unit of retry |
| implementation | **Rust for everything**: store, catalog, adapters, CLI, hook logic, watcher, skill export, benchmarks, tests. No Python in the build or test path; the Review 1 Python scripts are historical artifacts under `review-bench/` | one static binary for any host; one toolchain; the owner's directive |
| Rust deps | `parquet`+`arrow`, `zstd`, `sha2`, `rayon`, `clap`, `serde_json`, `duckdb` (bundled) | |

### 2.1 Language: Rust, as much as possible

The decision table above says Rust; the reasons, and what was weighed against it:

| | Rust | Python | Go |
|---|---|---|---|
| CLI start-up (matters: agents call `cat`/`stat` thousands of times) | ~2 ms | 150–300 ms (`import duckdb, pyarrow`) | ~2 ms |
| deployment to the two hosts | one static binary, same toolchain as onesw-gen | needs a venv (system `pip` cannot create one here; `uv` was required for the prototype) | one binary |
| Parquet / zstd / sha / parallel walk | `parquet`+`arrow`, `zstd`, `sha2`, `rayon`: mature | `pyarrow`, `zstandard`, `multiprocessing`: mature (the prototype) | Parquet libraries are weaker |
| DuckDB embedding | `duckdb` crate, `bundled` feature: ~10 min first build, ~50 MB binary | first-class | via CGO only |
| in-place risk | none; the store format is language-neutral Parquet + zstd, so a rewrite in another language is always possible | | |

Python would be the quickest to a working V1 (the prototype covered pack, catalog and queries in ~150 lines) but
loses on start-up latency and deployment, which are exactly the two things agents and the grid hosts hit. Go loses on
Parquet and DuckDB. Decision: **everything is Rust**, including what is usually left to scripts:

- the git pre-commit hook is the `traj` binary itself (a symlink `.git/hooks/pre-commit -> traj`; the binary dispatches on `argv[0]`), no shell stub, no script of any kind;
- `traj watch` uses `notify` (inotify) in-process, no cron or shell loop;
- `traj bench` replaces `review-bench/*.py` and writes the same JSON shape to `review-bench/history/`;
- tests are Rust (`proptest` for T2, `assert_cmd` for CLI tests, `criterion` for latency targets);
- no shell and no Python anywhere: no `sh -c`, no scripts in the repo, hooks, tests or CI; independent checks spawn
  existing binaries directly with `std::process::Command` (`zstd`, `sha256sum`, the DuckDB CLI) and compare trees in
  Rust (`walkdir`) instead of `diff -r`;

Crate layout (Cargo workspace): `crates/trajfs-core` (pack, catalog, manifest, verify), `crates/trajfs-adapters`
(`none`, `jsonl`, `copilot-cli`, `claude-code`, `onesw`), `crates/trajfs-query` (DuckDB session, views, `blob()`/`text()`),
`crates/traj` (the CLI, hook, watch, skill export, bench). If the bundled DuckDB build
proves too heavy for the hot verbs, `ls/find/stat/cat` read the catalog with `arrow`/`parquet` directly and DuckDB stays
only behind `sql`; that split changes no format.

## 3. Store layout and formats

```
<name>.trajstore/
  MANIFEST.json                     format version, store id, source, adapter (name, version), batches[]
  catalog/
    files-0001[-0001].parquet       one or more physical segments per batch, sorted by path
    dirs-0001[-0001].parquet        derived per batch: dir, parent, name, counts, bytes
    excluded-0001[-0001].parquet    paths the rule profile left out (path, size, rule)
  packs/
    0001.pack ...                   opaque: magic + zstd frames, ≤ 60 MiB each
    index-0001[-0001].parquet       sha → (pack, chunk_offset, chunk_len, offset, size)
  derived/
    <adapter>/<table>-0001[-0001].parquet  adapter-produced tables, rebuildable from packs
    <adapter>/events-rebuild-0001[-0001].parquet  manifest-published full rebuild generation
  .gitattributes                    *.pack -diff -delta binary ; *.parquet -diff binary
```

`MANIFEST.json` is the publication boundary. Readers and DuckDB register only its declared files; missing artifacts
make the store invalid, while finalized but unlisted files from an interrupted writer remain invisible until orphan
cleanup. A table that fits in one physical file keeps the legacy `-BBBB.parquet` name. Larger tables use
`-BBBB-PPPP.parquet`; the four batch tables use synchronized part suffixes (with empty schema-correct companions where
necessary), and `batches[].segments` lists the `files-*` stems. Segment ordering parses numeric batch/part ids rather
than relying on lexical filename order.

### 3.1 `files` (Parquet, zstd, row groups of 65,536, sorted bytewise by `path`)

| column | type | notes |
|---|---|---|
| path | utf8 | relative to the store root, `/`-separated, bytes preserved |
| dir | utf8 | parent, `""` for root |
| name | utf8 | basename |
| kind | uint8 | 0 file, 1 symlink, 2 empty file (no blob) |
| mode | uint16 | 0o644 / 0o755 (only the exec bit, like git) |
| size | int64 | uncompressed bytes (symlink: target length) |
| sha | fixed_size_binary(32) | sha256 of content (symlink: of the target string) |
| mtime_ns | int64 | source mtime, informational |
| batch | uint32 | ingestion batch |
| attrs | map<utf8, utf8> | **adapter-provided path attributes**, empty when no adapter (onesw: `round=37`, `role=builder`) |

Adapters may not add core columns; anything structured beyond `attrs` goes to `derived/`.

### 3.2 `dirs`

`dir, parent, name, depth, n_files, n_dirs, bytes, batch`. `ls` = rows of `dirs` with `parent = X` ∪ rows of `files`
with `dir = X`; `du` reads only `dirs`.

### 3.3 Pack format

```
pack  := magic "TRAJPACK\x01" , frame*
frame := zstd frame (level 3, content size set) of concat(blob bytes ...)
```

The pack carries no directory; `packs/index-*.parquet` is authoritative:

| column | type |
|---|---|
| sha | fixed_size_binary(32) |
| pack | uint32 |
| chunk_offset | int64 (offset of the frame in the pack) |
| chunk_len | int32 (compressed frame length) |
| offset | int32 (offset of the blob inside the decompressed frame) |
| size | int64 |
| part | uint16 (0 for single-frame blobs; blobs > 1 MiB are split into parts, one row each) |

Read path for one blob: sha → rows (in-memory map built from the index on first use), `pread` each frame, decompress,
slice. Optional, measure first (§8): a zstd dictionary trained on the small blobs (`packs/dict.zstd`), adopted only if
it beats plain chunks by ≥ 20 %.

### 3.4 `MANIFEST.json`

```json
{"format": 2, "store_id": "...", "source": "/workspace/farm/onesw-gen-outputs/.../onesw-generation-...",
 "adapter": {"name": "onesw", "version": 1}, "rules": {"name": "onesw-archive", "version": 3},
 "batches": [{"id": 1, "created": "2026-09-04T07:12:00Z", "label": "rounds 1-38", "paths": 2173722,
              "bytes": 12928153598, "new_blobs": 23864, "new_blob_bytes": 3053283334,
              "packs": [1, 12], "segments": ["files-0001"], "derived": ["onesw/events-0001"],
              "source_tree_sha256": "...", "errors": []}]}
```

Format 2 makes every derived artifact manifest-authoritative. Readers remain compatible with format-1 stores,
including the legacy `derived/<adapter>/events-0000.parquet` full-rebuild convention. `adapter` may be
`{"name": "none"}`; every verb in §5 works without one.

### 3.5 Derived tables: the generic `events` envelope

Any adapter that recognises trajectory files emits `derived/<adapter>/events-B[-P].parquet` with this envelope. The
envelope is deliberately thin (Review 1 §3: stable envelope, flexible payload); agent-specific fields stay in
`payload_json` and are promoted to columns only inside the adapter's own extra tables.

`traj derive` rebuilds events as a uniquely named generation while the old manifest-listed generation remains active.
After every new segment is closed and size-checked, one atomic manifest replacement publishes the generation; old
files are removed afterward. Readers hold a shared store lock for their lifetime, while pack/derive retain the
exclusive lock through cleanup, so an already-open reader cannot lose its generation. Interrupted generations are
therefore invisible and later cleaned as orphans. Readers open an existing `.lock` read-only. For a legacy store
without `.lock`, they lock `MANIFEST.json` shared; writers lock that manifest exclusively before creating `.lock`,
which preserves coordination without requiring readers to mutate a read-only store.

| column | type | meaning |
|---|---|---|
| trajectory | utf8 | path of the source file within the store |
| seq | int32 | event order within the trajectory |
| ts | timestamp(µs) nullable | event time when the format has one |
| type | utf8 | event kind as named by the source format |
| id, parent_id | utf8 nullable | when the format has event ids / threading |
| actor | utf8 nullable | `user` / `assistant` / `tool` / `system` when derivable |
| tool_name | utf8 nullable | when the event is a tool call/result |
| exit_code | int32 nullable | when present |
| payload_json | utf8 | the event body verbatim |
| adapter_version | uint16 | parser version that produced the row |

Known source formats and how they map (each is an adapter or an adapter option):

| format | trajectory detection | event split | envelope mapping |
|---|---|---|---|
| generic JSONL | `--trajectories '<glob>'` | one line = one event | `type` ← first of `type/event/kind`; `ts` ← first of `timestamp/ts/time`; rest to payload |
| GitHub Copilot CLI `events.jsonl` (`{type,timestamp,id,parentId,data}`) | `**/events.jsonl` | line | direct; `tool_name`/`exit_code` from `data` for `tool.*` types |
| Claude Code session `~/.claude/projects/**/<session>.jsonl` (`{type,uuid,parentUuid,timestamp,message}`) | by path | line | `id` ← `uuid`, `parent_id` ← `parentUuid`, `actor` ← `message.role` |
| OpenAI/Anthropic message arrays (single JSON) | glob | array element | `actor` ← `role`, `type` ← content block type |
| plain text logs | glob | none (no events) | trajectory still recorded in `attrs`, no derived rows |

### 3.6 Adapters: declared by the target repo, never by trajfs

trajfs ships **no runner-specific code**. It knows three trajectory *formats* (`copilot-cli`, `claude-code`, generic
`jsonl`) and two built-in rule profiles (`none`, `no-build-products`). Everything about a particular runner's layout
is a TOML file that lives in that runner's repository and is written there, by its maintainers or by the agent
working in that repo:

```toml
# <repo>/trajfs/adapter.toml
name = "my-runner"            # recorded in MANIFEST.json
version = 1
rules = "rules.toml"          # built-in name or path relative to this file
[[attrs]]                     # named groups -> files.attrs keys
pattern = '^rounds/round-(?P<round>\d+)/(?P<role>[^/]+)(?:/|$)'
strip_leading_zeros = ["round"]
[trajectories]
globs = ["**/events.jsonl"]
format = "copilot-cli"
[batch_ready]                 # for `traj watch`
run_glob = "*/*/run-*"
markers = ["rounds/round-*/DONE"]
[hook]                        # copied into trajfs.toml [hook] by `traj init`
raw_patterns = ['(^|/)rounds/round-\d+/']
```

Adapter `name` is also a path-safety boundary: it must be exactly one non-empty normal path component. Absolute names,
slashes, `.` and `..` are rejected before creating or resolving derived paths.

`traj init` takes `--adapter <path|builtin>`, or `--scaffold-adapter` to write `trajfs/adapter.toml` and
`trajfs/rules.toml` templates; when neither is given on a terminal it asks whether to scaffold, and the exported
skill tells the repo's agent how to complete the file. The onesw adapter therefore lives at
`farm/onesw/trajfs/{adapter,rules}.toml`, and trajfs's own tests use a generic "rounds layout" fixture.

The trait behind this (`trajfs_core::Adapter`) stays available for compiled adapters if a format ever needs code
that TOML cannot express; the `Declared` implementation covers attrs, trajectories, readiness, rule profile and
hook patterns.

## 4. Write path: `traj pack`

`traj pack <src-dir> --store <dir> [--adapter A] [--rules PROFILE|none|<file.toml>] [--trajectories GLOB]... [--jobs N] [--label L]`

1. **Walk** the source tree in parallel; apply the rule profile (§4.2). Symlinks are recorded, never followed. Hard
   links are ordinary files (dedupe makes them free).
2. **Skip already-ingested paths**: same `(size, mtime_ns)` as an existing catalog row → dropped from the batch;
   different → re-hashed; if the sha changed the path is recorded again (newest segment wins on read; `verify` reports
   shadowing).
3. **Hash** every candidate (sha256, `--jobs` workers; 22 s for 2.17 M files warm in the prototype).
4. **Pack** blobs whose sha is not in any existing index, sorted by `(dir, name)` for chunk locality; frames are at
   most 1 MiB uncompressed, and packs are sealed before the next frame would exceed the artifact target; each is
   written as `NNNN.pack.tmp` and renamed when sealed.
5. **Write** size-bounded `catalog/files-B[-P]`, `dirs-B[-P]`, `excluded-B[-P]`, `packs/index-B[-P]`, then
   size-bounded adapter-derived tables, then append the
   batch to `MANIFEST.json`. The serialized manifest is size-checked, written through a unique create-new/no-follow
   same-directory temporary handle, flushed and synced, then atomically renamed. Until that rename, readers and SQL
   ignore every new artifact; a crash leaves unreferenced orphans that the next `pack` safely removes.
6. Summary line; non-zero exit if any path was unreadable (batch is still written; failures listed in the manifest).

`<store>/.lock` (flock) is held for the duration of a pack; readers never lock.

### 4.1 Incremental use

Runners that append (new round, new episode, new session) call `traj pack` again; each batch contains the new paths
and only the blobs not seen before. Adapters that implement `batch_ready` allow `traj watch <src> --store S` to pack
automatically.

### 4.2 Rules

A rule profile is TOML: `exclude_dirs`, `exclude_ext`, `exclude_under = [{dirs, ext, max_bytes}]`, `max_bytes`,
`always_keep` globs, `elf_min_bytes`. Profiles ship in the crate (`rules/onesw-archive.toml` ports `archfilter.py`;
`rules/none.toml` keeps everything; `rules/no-build-products.toml` is the generic default). The profile name and
version go to the manifest, the excluded list to `catalog/excluded-B[-P].parquet`.

## 5. Read path: the CLI

All verbs take `--store <dir>` (or `TRAJ_STORE`) and a path relative to the store root.

| verb | semantics | data touched |
|---|---|---|
| `traj ls [-l] [-R] <dir>` | children, dirs first; `-l` adds kind/mode/size and attrs | catalog |
| `traj tree <dir> [--depth d]` | | catalog |
| `traj find <dir> [--name g] [--path g] [--kind f/l] [--size ±N] [--attr k=v]...` | paths | catalog |
| `traj du <dir> [--depth d]` | raw and deduplicated bytes/counts | catalog |
| `traj stat <path>` | kind, mode, size, sha, mtime, attrs, batch, pack location | catalog + index |
| `traj cat <path>...` | bytes to stdout, sha verified unless `--no-verify` | index + pack |
| `traj extract <path-or-dir> <dst> [--hardlink-dedupe] [--mtime]` | recreates files/symlinks/modes | index + packs |
| `traj grep [-e re]... [--name g] [--path prefix] [-l] [-c] [-a]` | regex over each distinct blob once, hits mapped to every path | index + packs |
| `traj sql "<query>"` | DuckDB with views `files`, `dirs`, `blobs` (index), `excluded`, every `derived/*` table, and functions `blob(sha) → BLOB`, `text(sha) → VARCHAR` | all |
| `traj derive [--adapter A] [--force]` | (re)build derived tables from packs | packs → derived |
| `traj verify [--deep]` | catalog ↔ index ↔ packs consistency; `--deep` re-hashes every blob | all |
| `traj edit <path>` | extract to a temp file, run `$EDITOR`, print a diff; never written back | |
| `traj watch <src> --store S` | pack whenever the adapter reports a batch ready | |

Latency targets (warm, 2 M-path store): `ls`/`find` ≤ 50 ms; `stat` ≤ 20 ms; `cat` ≤ 5 ms after a ≤ 100 ms one-time
index load; `extract` of a 100 K-file subtree ≤ 30 s; `grep` over all distinct text blobs ≤ 10 s.

## 6. Git integration

- The store is committed as ordinary files; ≤ 60 MiB shards by default, no LFS. `.gitattributes` marks packs and Parquet binary
  and `-delta`.
- A batch is a commit touching only its new files, so `add`/`commit`/`push` cost is proportional to the batch.
- A clone is directly readable by `traj`; nothing is extracted to browse.
- Existing raw-tree archive commits (5.3 M paths on `explore/onesw-storage-dedupe`) stay; M4 re-packs those runs
  from disk into stores committed beside them.

### 6.1 Raw data lives outside the repo; the repo holds only stores

The four fourth-grid trees were committed raw because the runner writes into the repo by default: onesw-gen's
`output_root` defaults to `onesw-gen-outputs` relative to the working directory (`onesw-gen/src/config.rs:33`), and the
grid TOML does not override it, and the farm repo's `.gitignore` does not exclude it. Nothing prevented `git add` of a
2 M-path tree. The fix is structural, not a convention:

**Two roots, both mandatory, never nested.**

| root | what | where | git |
|---|---|---|---|
| `data_root` | raw run trees as the agent writes them; the runner's `output_root` | outside every git work tree, e.g. `/workspace/runs/<workload>/<run-id>/` | never a repo; carries a `.gitignore` with `*` as a belt-and-braces |
| `store_root` | `<run-id>.trajstore/` directories produced by `traj pack` | inside the repo, e.g. `<repo>/stores/` | the only thing committed |

Configuration is one file, `trajfs.toml`, found by walking up from the current directory (or `TRAJ_CONFIG`):

```toml
data_root  = "/workspace/runs"        # absolute; required, no default
store_root = "stores"                 # relative to the repo root that contains this file
adapter    = "onesw"                  # default adapter for pack/watch
rules      = "onesw-archive"
```

**Enforcement, in order of where a mistake would be caught:**

1. **Runner side.** onesw-gen's `output_root` becomes required in the grid TOML (no default) and the dispatcher refuses
   to start when it resolves inside a git work tree (`git rev-parse --show-toplevel` succeeds from that path) unless
   `allow_output_in_repo = true` is set explicitly. Same check for any other runner that gains a trajfs hook.
2. **Pack side.** `traj pack` refuses a `--store` path under `data_root` and refuses a source under `store_root`; it
   warns (and with `--strict` refuses) when the source is inside a git work tree.
3. **Repo side.** `traj init` installs the pre-commit hook (a symlink to the `traj` binary, §2.1) and a `.gitattributes`. The hook rejects a commit that adds
   more than 10,000 paths, any path matching a raw-run pattern (`rounds/round-*/`, `*/call*/events.jsonl`, `selected/`),
   any file over the configured 65 MiB default, any symlink/gitlink-backed store artifact, or a store whose staged
   artifact inventory differs in either direction from its manifest. Policy, sizes, and manifests are parsed from the
   staged Git objects, not through working-tree paths. Hooks are local, so committed trees receive the same inventory and mode checks through
   `traj hook check-tree <rev>` in CI.
4. **Commit side.** `traj commit [--push] <store>` is the intended path: it packs if needed, stages only the new batch
   files under `store_root`, commits with a generated message (store id, batch label, paths, new bytes) and pushes.
   People and agents are told to use it; the hook makes the raw path fail loudly if they do not.
5. **Doctor.** `traj doctor` prints both roots, checks nesting, git status of each, hook presence, and any raw-run
   pattern already tracked in the repo.

**Migration of the farm repo** (owner's call, per the no-deletion rule): move the raw trees already on disk from
`onesw-gen-outputs/` to `data_root` (a move, then a pointer file), set `output_root` in the grid TOML, run `traj pack`
for each run into `stores/`, and commit the stores. The raw-tree commits already in history stay in history; the
working tree simply stops tracking them (`git rm --cached`, no data touched).

**Why not just `.gitignore` the output directory?** It would stop accidental `git add`, but the runner would still
fill the repo's disk under a path that looks committed, `git status` would still scan millions of files, and one
`git add -f` undoes it. Keeping the data physically outside the repo is what makes the mistake impossible rather than
discouraged.

## 7. Integration with runners

Generic: `traj pack` after a run, or `traj watch` during it. The runner does not need to know trajfs exists.

onesw-gen (Phase A): the only runner change is the `output_root` guard of §6.1 (required, outside any git work tree). The `onesw` adapter's `batch_ready` fires on a new
`rounds/round-N/selected-storage.json`; `onesw/tools/archive-run.sh` calls `traj pack` per lane at grid stop.

onesw-gen (Phase B, later, adapter-specific): the `selected/` snapshot becomes a manifest of shas and
`seed_builder_workspace` extracts from the store, removing the on-disk 2× copy and reusing the store's sha check as the
seed integrity check. Out of scope for V1.

## 8. Performance and size targets (acceptance)

Prototype numbers on the reference run; the Rust build must meet or beat them.

| metric | target | prototype |
|---|---|---|
| pack 2.17 M paths / 12.9 GB, warm | ≤ 120 s | 48 s |
| store size for that run | ≤ 600 MB | 472 MB (23 MB catalog + 449 MB blobs) |
| files per store | ≤ 100 | 13 |
| `ls` one dir | ≤ 50 ms | 10 ms |
| `find --name COMPLETE` (133,921 hits) | ≤ 50 ms | 9 ms |
| `cat` one small file after index load | ≤ 5 ms | 7 µs (offset read) |
| per-attr aggregate over `files` | ≤ 0.5 s | 0.18 s |
| `grep` one pattern over all distinct stdout | ≤ 10 s | 5.4 s (Parquet blobs; packs should be faster) |

## 9. Reserved

(Section number kept so cross-references in `docs/idea-review.md` stay valid; FUSE was removed from the plan in draft 2.5.)

## 10. Milestones

| id | deliverable | exit criterion |
|---|---|---|
| M0 | this plan; Cargo workspace skeleton (§2.1 crates) building an empty `traj --help`; fixtures (§11.0) | `cargo build --release` on both hosts; fixtures committed |
| M1 | pack writer/reader, `cat`, `extract`, `verify`, adapter `none` | T1–T3, T6 green; `round-0037` round-trips byte-identical |
| M2 | catalog + `ls/tree/find/du/stat` (DuckDB) | T4 green; §8 latency targets met |
| M3 | `grep`, `sql` with `blob()/text()`, `derive`, adapters `jsonl`, `copilot-cli`, `claude-code` | T5 green on both real formats |
| M4 | rule profiles, incremental batches, manifest, `traj init/commit/doctor` + hook (§6.1), declared adapters + scaffold, onesw-gen `output_root` guard; four fourth-grid stores committed | T7–T9, T9b, T9c green; §8 size/file targets met |
| M5 | `traj watch`, `traj init/skill export`, onesw Phase A hook; `README.md`; `OPERATIONS.md` §7 | one live round packed automatically; T11 green; skill installed in the farm repo |

## 11. Test plan

Test code is Rust only: unit tests in each crate, integration tests in `crates/traj/tests/` (`assert_cmd`), property
tests with `proptest`, latency checks with `criterion` (`cargo bench -p traj`). `cargo test --release` runs the
synthetic-fixture tests in < 10 s; `cargo test --release --features slow -p traj --test cli slow::` runs the
real-dataset tests and needs `TRAJ_SLOW_SRC` (a run directory) and `TRAJ_SLOW_ADAPTER` (its adapter TOML); their
expected counts are recorded on first run under `crates/traj/tests/expected/` and asserted afterwards.

**11.0 Fixtures.**
- `synthetic/`: generated 200-file tree covering every kind/mode, unicode names, duplicates, an empty dir, a
  symlink chain; deterministic seed.
- `onesw` dataset: the four fourth-grid lanes on disk (rank 1 `claude-opus-4.8-678xazw`, rank 2 `gpt-5.5-p3jmfrc`,
  rank 3 `claude-opus-4.6-1m-kVtKs9a`, rank 4 `gpt-5.5-GLihDpe`, all `onesw-generation-20260902T033145Z`) and the
  `round-0037` sample list. Expected counts are recorded in `tests/expected/onesw.json`.
- `claude-code` dataset: session JSONL files from `~/.claude/projects/` on the dev host (copied under a fixture dir;
  contents are not asserted on, only envelope parsing).
- `generic` dataset: a synthetic tree of plain logs plus JSONL with heterogeneous keys, no adapter.

**T1 Pack format (unit).** Frame cut at exactly 1 MiB; 0-byte blob (kind 2, no index row); 1 MiB, 1 MiB + 1 and
100 MiB blobs (multi-part); packs remain below the 60 MiB target; magic and frame boundaries validated by the `zstd` CLI; every index
row points inside its pack.

**T2 Round trip (property-based).** Random trees (proptest): depth ≤ 8, unicode/space/dot names, kinds file/symlink/
empty, modes 644/755, sizes from the measured distribution (p50 40 B, p99 80 KB, a few multi-MB), 30 % duplicate
contents. `pack → extract` is byte-identical (`diff -r --no-dereference`), modes and symlink targets equal;
`--hardlink-dedupe` yields `nlink > 1` only for identical blobs. Runs with adapter `none` and with `onesw` (attrs must
not change bytes).

**T3 Integrity.** Flip one byte in a pack → `verify --deep` names the frame and every affected path; `cat` of an
affected path fails unless `--no-verify`. Truncated pack → `verify` fails, other packs readable. Missing
manifest-declared catalog, index, pack, or derived artifacts → quick and deep verification refuse the store. Missing
`MANIFEST.json` → every verb refuses clearly.

**T4 Catalog semantics.** On the synthetic and generic fixtures, `ls`, `tree`, `find`, `du`, `stat` equal the same
operations on the extracted tree (`ls -A`, `find`, `du -b --apparent-size`), including empty directories (present only
in `dirs`). With adapter `none`, `attrs` is empty and `find --attr` matches nothing; with `onesw`, `find --attr
round=37 --attr role=reviewer` returns exactly the paths under `rounds/round-0037/reviewer/`.

**T5 grep, sql, derived tables.** `traj grep -l P` equals `grep -rl P` on the extracted tree for 20 random patterns,
one of which matches only a duplicated blob (hit-to-all-paths mapping); binary blobs skipped unless `-a`. `sql` views
exist; `blob(sha)` equals `cat`. Derived `events`: (a) `copilot-cli` on the onesw dataset: row count = total lines of
all `events.jsonl`; `type`/`ts`/`id`/`parent_id` round-trip; `payload_json` re-parses to the original `data`;
(b) `claude-code` on the session fixture: rows = lines, `actor` ∈ {user, assistant, system, tool}, `parent_id` chains
resolve; (c) `jsonl` on the generic fixture with heterogeneous keys: no row lost, unknown keys land in `payload_json`;
(d) re-running `derive` with a bumped adapter version replaces the derived segment and leaves packs and catalog
unchanged (byte-compare).

**T6 Reference round (slow).** Pack `rounds/round-0037` of rank 1: 107,449 paths, 7,418 distinct blobs; extract
byte-identical; store ≤ 30 MB; `cat` p50 ≤ 5 ms over 1,000 random paths after warm-up.

**T7 Whole runs (slow).** Pack all four lanes. Rank 1: 2,173,722 paths (2,173,703 files + 19 symlinks), 23,864 distinct
blobs, 3.05 GB distinct, all §8 targets, `verify --deep` clean, one full round extracted and `diff -r` clean. The other
three lanes: counts recorded on first run into `tests/expected/onesw.json` and asserted thereafter. Compatibility:
every Parquet file readable by the DuckDB CLI with no options; one
frame cut from a pack by offset decodes with `zstd -dc`; every sha in the catalog equals `sha256sum` of the extracted
file.

**T8 Incremental batches.** Pack rounds 1–10 then 1–11 of one lane: batch 2 holds only round-11 paths and only blobs
absent from batch 1; manifest lists two batches; a path modified between batches is recorded again and `stat` shows the
newest; SIGKILL during pack leaves the store readable and `verify` clean; the next `pack` removes orphans and completes
the batch. `traj watch` on a copy of a lane packs when a `selected-storage.json` is added.

**T9 Git.** Commit a store to a scratch repo: every file ≤ 60 MiB by default; adding batch 2 stages only batch-2 files; packs are
binary per `.gitattributes`; a clone passes T4/T5 unchanged. Push the four stores to a side branch and record push and
clone times next to the raw-tree commits on `explore/onesw-storage-dedupe`.

**T9b Separation.** `traj pack --store <data_root>/x` and `traj pack <store_root>/y` are refused; `traj init` installs the hook; a commit adding `rounds/round-0001/…` or a 65 MiB file is rejected by the hook with the suggested `traj pack` command; `traj commit` stages exactly the batch files; `traj doctor` flags a nested root and a tracked raw-run path; onesw-gen refuses to start with `output_root` inside the repo and starts with it outside.

**Regression baseline.** `traj bench --store S --source <run>` is run at each milestone; results go to
`review-bench/history/<date>.json` (same keys as the Review 1 JSON) for comparison with §8.

## 12. Usage procedure

Five situations cover everything; each is a short, fixed sequence. `traj` prints the next step on success and the
correct command on refusal, so the procedure is also discoverable from the tool itself.

**12.1 One-time setup of a repo (operator).**

```
traj init --data-root /workspace/runs --store-root stores --adapter onesw --rules onesw-archive
#  writes trajfs.toml, stores/.gitkeep, .gitattributes, the pre-commit hook, /workspace/runs/.gitignore
#  and installs the agent skill (§13) under .claude/skills/traj/ and an AGENTS.md section
traj doctor            # both roots, nesting, hook, skill version, tracked raw-run paths: all must be OK
git add trajfs.toml stores/.gitkeep .gitattributes .claude/skills/traj AGENTS.md && git commit -m "trajfs: init"
```

Runner config: point the runner's output root at `data_root` (onesw-gen: `output_root = "/workspace/runs"` in the
grid TOML; the dispatcher refuses to start otherwise, §6.1).

**12.2 During a run (automatic).**

```
traj watch /workspace/runs --store-root stores          # one per host; systemd unit or started with the dispatcher
```

Each time the adapter reports a batch ready (onesw: a new `rounds/round-N/selected-storage.json`), `watch` packs that
run's new paths into its store and, with `--commit --push`, commits and pushes the batch. Without `--commit` the store
grows locally and 12.3 commits it. `watch` never reads a round that is still being written and never touches the raw
tree.

**12.3 After a run, or when catching up (operator or agent).**

```
traj pack /workspace/runs/<workload>/<run-id> --label "rounds 1-38"     # store path derived: stores/<run-id>.trajstore
traj verify --store stores/<run-id>.trajstore
traj commit --push stores/<run-id>.trajstore                              # stages only the new batch, commits, pushes
```

Re-running `pack` on an unchanged run is a no-op batch (0 paths) and exits 0.

**12.4 Analysis (agent, or a person with DuckDB).**

```
traj ls   -S stores/<run-id>.trajstore rounds/round-0037/reviewer
traj find -S ... --name review.json
traj cat  -S ... rounds/round-0037/reviewer/review.json | jq .verdict
traj grep -S ... -e 'Traceback' --name '*.stdout' -l
traj sql  -S ... "select attrs['round'] r, count(*) from files group by 1 order by 1"
traj sql  -S ... "select trajectory, count(*) from events where type like 'tool.%' group by 1"
traj sql  -S ... "select path, text(sha) from files where name='review.json' and text(sha) like '%not done%'"
```

Cross-run questions use one DuckDB session over several stores: `traj sql -S stores/a -S stores/b "..."` registers
each store's tables with a `store` column.

**12.5 Viewing and editing by a person.**

```
traj extract -S ... rounds/round-0037/reviewer /tmp/r37        # then open /tmp/r37 in VS Code
traj edit    -S ... rounds/round-0037/reviewer/review.json     # temp copy in $EDITOR, prints a diff, never writes back
```

Nothing in 12.4–12.5 needs the raw tree, so a fresh clone of the repo is enough on any machine.

**12.6 What is deliberately impossible.** Committing raw run trees (hook + roots, §6.1); modifying stored content
(append-only; `edit` never writes back); deleting a store batch by accident (`compact` is the only deleting verb and
requires `--yes`); nesting the roots.

## 13. Agent skill

Future agents will operate the runs and analyse them; they must find and use `traj` instead of walking raw trees or
running `git add` on outputs. So the binary ships the skill and installs it:

- `traj skill export [--format claude-code|agents-md|markdown] [--out DIR]` renders the skill from a template
  embedded in the binary, filled with the real roots from `trajfs.toml` and the exact verb list from the CLI's own
  `--help`, so the skill can never describe a verb the installed binary lacks. `traj init` runs it; `traj doctor`
  warns when the installed skill's version differs from the binary's.
- Claude Code form: `.claude/skills/traj/SKILL.md` (frontmatter `name: traj`, `description` naming the trigger words:
  trajectories, run outputs, archive, commit run results, analyse rounds/events). Generic form: a section appended to
  `AGENTS.md` (or `CLAUDE.md`/`.cursorrules`) with the same content.
- Content of the skill (draft in `skills/traj/SKILL.md`, the template for the export): when to use it; the roots and
  the rule that raw data is never committed; the 12.2–12.5 procedures as copy-paste commands; the SQL views and their
  columns; five worked examples (find failed rounds, diff two rounds' reviews, list all tool errors in a trajectory,
  size by round, extract a round for a human); the guardrails (drops are moves, never delete `data_root`, never
  `--no-verify` in scripts, never commit outside `traj commit`); how to read `traj doctor` output; and the error
  messages the hook prints with what to do instead.
- The skill is data, not policy: it repeats what the hook and the roots already enforce, so an agent that ignores it
  still cannot do damage; it only wastes time.

**T11 Skill.** `traj skill export` renders without placeholders; every command in the skill's examples is executed
against the synthetic fixture store in tests and must exit 0 with the documented output shape; every verb named in the
skill exists in `traj --help` and vice versa; `doctor` warns on a version mismatch; `init` on a repo with an existing
`.claude/skills/traj/SKILL.md` updates it in place and leaves other skills untouched.

## 14. Implementation status — 2026-09-04 (first build)

Built and tested in this repo: Cargo workspace `crates/trajfs-core`, `crates/trajfs-adapters`, `crates/traj`
(binary 55 MB stripped, DuckDB bundled, ~4 min clean release build). `cargo test --release`: 13 tests green
(core unit tests, adapter tests, CLI tests T1–T5, T6 when `TRAJ_SLOW_SRC` is set, T8, T9b, T11).

Implemented verbs: `init`, `doctor`, `pack`, `watch` (polling), `ls`, `tree`, `find`, `du`, `stat`, `cat`,
`extract`, `edit`, `grep`, `sql` (with `text(sha)` / `blob(sha)`), `derive`, `verify`, `commit`, `skill export|verbs`,
`hook pre-commit|check-tree` (the hook is a symlink to the binary). Built-in adapters: `none`, `jsonl`, `copilot-cli`,
`claude-code`; runner layouts are TOML files in the runner's repo (§3.6); the onesw one is
`farm/onesw/trajfs/adapter.toml` + `rules.toml`. Built-in rule profiles: `none`, `no-build-products`.

Measured on fwf2-n0 against the fourth-grid lanes (stores under `/workspace/trajstores-test/`, outside any repo;
rank 2 is on the other host and was not packed):

| lane | paths | kept bytes | distinct blobs | packs | catalog | derived events | pack time | verify --deep |
|---|---|---|---|---|---|---|---|---|
| rank 1 (claude-opus-4.8) | 2,140,904 | 12.0 GB | 23,685 | 454 MB (8 files) | 27 MB | 359 MB | 118 s | 21 s |
| rank 3 (claude-opus-4.6-1m) | 1,868,148 | 5.9 GB | 82,339 | 406 MB (7 files) | 45 MB | 251 MB | 75 s | 16 s |
| rank 4 (gpt-5.5-GLihDpe) | 699,040 | 4.7 GB | 15,937 | 347 MB (6 files) | 15 MB | 269 MB | 55 s | 13 s |
| round-0037 of rank 1 alone | 105,940 | 450 MB | 7,257 | 17 MB | 1.6 MB | — | 3.1 s | — |

Round trip of `round-0037`: extract of 105,940 entries in 10 s, byte-identical to the source for every kept path
(the 1,510 paths the old `archfilter.py` kept and `onesw-archive` drops are `*.bin` benchmark binaries).

Verb latency on the rank 1 store (2.14 M rows, warm cache, whole process):

| verb | time | RSS |
|---|---|---|
| `ls <dir>` | 0.07 s | 35 MB |
| `stat`, `cat` (one file) | 0.04 s | 31 MB |
| `find --name COMPLETE` (133 K hits) | 0.43 s | 37 MB |
| `find --attr round=37 --attr role=reviewer` | 1.4 s | 57 MB |
| `du --depth 1` | 0.8 s | 177 MB |
| `tree --depth 1` | 0.6 s | 50 MB |
| `grep -l FAILED --name 'round*-final-test.stdout'` | 1.1 s | |
| `sql` per-round aggregate over `files` | 0.18 s | |
| `sql` over `events` (tool-call counts, 3.9 M rows) | 0.07 s | |
| `sql` with `text(sha)` on 76 review records | 0.8 s | |

Against §8: pack time (118 s ≤ 120 s), store size (≈ 480 MB without derived ≤ 600 MB), file count (≤ 20),
`ls`/`stat`/`cat` all meet the targets. `find` over the full catalog is 0.4–1.4 s instead of ≤ 50 ms: the Rust
scan materialises rows; routing full-catalog verbs through DuckDB (already bundled) would get the 10 ms of the
prototype and is the first optimisation to do (M3.1). Derived `events` tables are large (250–360 MB per lane,
3.9 M rows for rank 1) because `payload_json` is stored verbatim; they are rebuildable, so they can be left out of git
with `pack --no-derive` and built on the reading side with `traj derive`.

Deviations from the plan text above, all deliberate:
- `pack` writes to `--out` (not `--store`), because `-S/--store` is the global "which store to read" flag.
- `watch` polls (default 60 s) instead of using inotify; the adapter's `batch_ready` is the readiness signal either way.
- `dirs.n_files` and `dirs.bytes` are recursive; `n_dirs` is direct children.
- A directory excluded by `exclude_dirs` is recorded once as a directory row in `excluded` (its subtree is not walked).
- The onesw `rules.toml` excludes `*.bin` everywhere (the Python filter only excluded it under `logs/`).
- 2026-09-04, later: the compiled `onesw` adapter and the `onesw-archive` built-in were removed after review; the
  same behaviour now comes from `farm/onesw/trajfs/adapter.toml` (attrs, trajectories, readiness, hook patterns) and
  `rules.toml`, loaded at run time. Hook patterns are no longer hard-coded: they come from `trajfs.toml [hook]`.

Not yet done: onesw-gen `output_root` guard (§6.1 step 1, lives in the onesw repo); `traj bench`; `traj compact`;
DuckDB-backed full-catalog verbs (M3.1); property-based T2 with `proptest` (the current T2 uses a fixed tree);
T7 as an automated test (numbers above were taken by hand); T9 push/clone timing against the raw-tree commits.

## 15. Test-plan status — 2026-09-04 (second pass)

After review the §11 plan was implemented in full rather than sampled. `cargo test --release`: 19 tests
(core 4, adapters 1, CLI 14) in about 8 s; `--features slow`: T6 and T7 against the rank 1 run.

| §11 item | test(s) | notes |
|---|---|---|
| T1 pack format | `pack::tests::{roundtrip_small_and_large, t1_pack_seals_at_64mib_and_large_blobs_split_into_parts}` | 100 MiB noise blob across two packs; a frame cut by offset decodes with the `zstd` CLI |
| T2 property round trip | `t2_property::pack_then_extract_is_byte_identical` (proptest, 24 cases) + `t2_round_trip_is_byte_identical` | random trees: unicode/space/dot names, symlinks, empty files, exec bits, 30 % shared content, multi-MB files; plain and `--hardlink-dedupe --mtime` |
| T3 integrity | `t3_integrity_detects_corruption` | bit flip (deep), truncated pack, missing index segment, missing pack, missing manifest |
| T4 catalog semantics | `t4_catalog_verbs_match_the_tree` | `ls`/`find` equal to `ls -A`/`find` on the extracted tree; `du` against `du -sb`; attrs filters; `tree` |
| T5 grep/sql/events | `t5_grep_cat_sql_events`, `t5b_grep_matches_grep_rl_for_random_patterns` (20 patterns vs `grep -rlE`), `t5c_other_formats_and_rederive` | Claude Code and heterogeneous-JSONL fixtures; re-derive with a bumped adapter version leaves packs and catalog byte-identical |
| T6 reference round | `slow::t6_reference_round` | counts recorded/asserted; deep verify; byte-identical extract; `cat` p50 ≤ 5 ms; `ls` < 0.5 s |
| T7 whole run | `slow::t7_whole_run` | counts recorded/asserted; pack ≤ 120 s per 2 M paths; store ≤ 5 % of kept bytes and ≤ 100 files; verb latency bounds; one round extracted byte-identical; DuckDB CLI reads the catalog when installed |
| T8 incremental | `t8_incremental_batches_add_only_new_content`, `t8b_sigkill_mid_pack_leaves_a_readable_store` (real SIGKILL), `t8c_watch_packs_when_the_adapter_reports_a_batch_ready` | |
| T9 git | `t9b_separation_and_hook` | hook refusal, migration commit allowed, `traj commit`, clone verifies, push of store vs raw tree against a local bare remote (path counts compared, times printed) |
| T9b/T9c separation, scaffold | `t9b_…`, `t9c_init_scaffolds_an_adapter_for_the_target_repo` | |
| T11 skill | `t11_skill_mentions_every_verb_and_carries_the_version` | every worked example in the skill is executed against a fixture store |
| criterion / bench | `crates/traj/benches/verbs.rs`, `traj bench` | |

Defects the full plan caught that the sampled tests had not:
- `extract --hardlink-dedupe` linked files with identical content but different exec bits (hard links share the mode);
  now keyed by (sha, mode).
- `grep` anchored `^`/`$` at blob boundaries, not line boundaries, and counted a trailing newline as an empty line.
- `grep` searched symlink targets as content; `grep -r` skips symlinks, so does `traj grep` now.
- `cat`/`stat` p50 was 8 ms on the reference round (row groups of 65,536 rows were decoded whole for a point lookup);
  row groups are now 16,384 and `stat` materialises only the matching row: 1–2 ms per lookup, whole process 10 ms.
- `ls <dir>` on the whole run decoded every path below the directory to find its direct files (0.65 s for
  `ls rounds`); row groups whose min and max paths lie under the same subdirectory are now skipped, so a listing
  touches only the groups that can hold direct children (10–20 ms).
