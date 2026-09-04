# trajfs

A content-addressed store for agent run trees, with a Parquet catalog, DuckDB queries and a git-friendly on-disk
form. One binary, `traj`, written in Rust.

Agent runners leave behind directory trees of millions of tiny files: per-step stdout, status markers, JSON records,
and the same workspace snapshotted round after round. In the corpus this project was built against, the median file
is 40 bytes and each distinct piece of content appears under 91 paths. Git stores the bytes fine but chokes on the
paths; every reader pays the duplication again. trajfs keeps the tree browsable and queryable while git only ever
sees a few dozen immutable files per run.

```
2,140,904 paths, 12.0 GB kept   ->   23,685 distinct blobs, 454 MB of packs + 27 MB of catalog, 16 files
pack: 55 s      verify --deep: 21 s      ls / stat / cat: 10-20 ms      find by name: 0.4 s      SQL: < 0.2 s
```

## How it works

A store is a directory (`<run>.trajstore/`) of immutable files:

```
MANIFEST.json                 format, store id, source, adapter, rule profile, ingestion batches
catalog/files-NNNN.parquet    path, dir, name, kind, mode, size, sha256, mtime, batch, attrs (map)
catalog/dirs-NNNN.parquet     per-directory counts and bytes
catalog/excluded-NNNN.parquet what the rule profile left out, and why
packs/NNNN.pack               zstd frames of up to 1 MiB of concatenated blobs; sealed at 64 MiB
packs/index-NNNN.parquet      sha256 -> (pack, frame offset, frame length, offset in frame, size, part)
derived/<adapter>/events-NNNN.parquet   trajectories parsed into a stable event envelope (rebuildable)
```

- **Content-addressed.** A path maps to a sha256; a sha256 maps to bytes in a pack. Duplicate content is stored once,
  across rounds and across the runner's own snapshot copies.
- **Packs, not Parquet, for bytes.** Reading one file is a `pread` of one frame plus a decompress of at most 1 MiB,
  a few microseconds to a few milliseconds. Parquet holds only the catalog and the derived tables.
- **Append-only batches.** Each `traj pack` adds one batch: new paths, only the blobs not seen before, and its own
  catalog segments. Nothing already written is modified, so every commit is additive.
- **Every byte is verifiable.** `traj verify --deep` re-hashes the store; `cat` checks the sha on every read.

## Quick start

```
cargo build --release                       # ~4 min the first time (DuckDB is bundled); binary at target/release/traj

# pack a run tree into a store, browse it, query it
traj pack /data/runs/run-42 --out stores/run-42.trajstore
traj -S stores/run-42.trajstore ls -l rounds/round-0037/reviewer
traj -S stores/run-42.trajstore find --name review.json --attr role=reviewer
traj -S stores/run-42.trajstore cat rounds/round-0037/reviewer/review.json
traj -S stores/run-42.trajstore grep -l -e Traceback --name '*.stdout'
traj -S stores/run-42.trajstore sql "select attrs['round'] r, count(*) paths, count(distinct sha) blobs from files group by 1 order by r"
traj -S stores/run-42.trajstore extract rounds/round-0037 /tmp/r37     # real files, for an editor
traj -S stores/run-42.trajstore verify --deep
```

Verbs: `init doctor pack watch ls tree find du stat cat extract edit grep sql derive verify commit skill hook bench`.
`traj <verb> --help` documents each one.

## Setting up a repository

trajfs enforces one structural rule: **raw run trees live outside every git work tree; the repo holds only stores.**

```
cd <your repo>
traj init --data-root /data/runs --store-root stores --adapter trajfs/adapter.toml
traj doctor
```

`init` writes `trajfs.toml`, a `.gitattributes` for the store directory, a catch-all `.gitignore` in the data root,
installs the pre-commit hook (a symlink to the `traj` binary), and exports an agent skill to
`.claude/skills/traj/SKILL.md` and `AGENTS.md`. The hook refuses commits that stage raw run paths, more than ten
thousand new paths, or files over 64 MiB, and prints the `traj pack` / `traj commit` commands to use instead.
Deleting tracked raw paths stays allowed, so an existing repository can migrate.

Then, per run or continuously:

```
traj pack /data/runs/run-42 --label "rounds 1-38"     # store path comes from trajfs.toml
traj commit --push stores/run-42.trajstore            # stages only the new batch files
traj watch --commit --push                            # packs whenever the adapter reports a batch ready
```

## Adapters: the target repository describes its own layout

trajfs knows nothing about any particular runner. It ships three trajectory formats (`copilot-cli`, `claude-code`,
generic `jsonl`) and two rule profiles (`none`, `no-build-products`). Everything about a runner's tree is a TOML file
kept in that runner's repository:

```toml
# trajfs/adapter.toml
name = "my-runner"
rules = "rules.toml"                          # what to leave out (build products, bulk measurement dumps, ...)
[[attrs]]                                     # named groups become files.attrs keys
pattern = '^rounds/round-(?P<round>\d+)/(?P<role>[^/]+)(?:/|$)'
strip_leading_zeros = ["round"]
[trajectories]
globs = ["**/events.jsonl"]
format = "copilot-cli"
[batch_ready]                                 # for `traj watch`
run_glob = "*/*/run-*"
markers = ["rounds/round-*/DONE"]
[hook]                                        # copied into trajfs.toml by `traj init`
raw_patterns = ['(^|/)rounds/round-\d+/']
```

`traj init --scaffold-adapter` writes the templates for the repository's maintainers or its agent to complete; on a
terminal `init` asks whether to do so. The exported skill explains the file to agents working in that repo.

## SQL

`traj sql` opens an embedded DuckDB over the store's Parquet files: views `files`, `dirs`, `excluded`, `blobs` (the
pack index) and `events` (when derived), plus `text(sha)` and `blob(sha)` to reach content from a query. Several
`-S` stores register together with a `store` column. The Parquet files are plain and readable by any other tool.

```
traj -S S sql "select tool_name, count(*) from events where type='tool.execution_complete' group by 1 order by 2 desc"
traj -S S sql "select path, json_extract_string(text(sha),'$.verdict') from files where name='review.json'"
```

## Testing

```
cargo test --release                                            # 19 tests, ~8 s: format, round trips (proptest),
                                                                # integrity, catalog verbs vs ls/find/du, grep vs grep,
                                                                # events, SIGKILL recovery, watch, hook, skill examples
TRAJ_SLOW_SRC=<run dir> TRAJ_SLOW_ADAPTER=<adapter.toml> \
  cargo test --release --features slow -p traj --test cli slow::   # a real round and a whole run, with size and latency bounds
cargo bench -p traj                                              # criterion latency of the hot verbs
traj bench --store S --out review-bench/history/                 # regression baseline as JSON
```

No shell or Python anywhere in the build, hooks or tests; external checks spawn `zstd`, `grep`, `ls`, `find`, `du`
and the DuckDB CLI directly.

## Repository layout

```
crates/trajfs-core       walk + rules, hashing, packs, Parquet catalog, manifest, store reader, ingest
crates/trajfs-adapters   copilot-cli / claude-code / jsonl parsers; the TOML-declared adapter
crates/traj              the CLI (hook, watch, skill export, bench included)
rules/                   built-in rule profiles
skills/traj/SKILL.md     agent skill template, embedded in the binary
docs/PLAN.md             design and test plan, with measured status (§14, §15)
docs/idea-review.md      the evaluation that chose this design over tar+FUSE, with the benchmark
docs/idea.md             the original notes
review-bench/            the prototype scripts and numbers from the review (historical)
```

## Status

First release of the format (`"format": 1`). The design, the CLI and the test plan in `docs/PLAN.md` are
implemented; measurements in §14 and §15 come from a 2.1 M-path, 12 GB run. Open items: DuckDB-backed full-catalog
scans (find by attribute is ~1 s on 2 M rows), `traj compact`, and the runner-side output-root guard described in
§6.1.
