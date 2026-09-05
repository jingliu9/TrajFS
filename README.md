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
catalog/files-NNNN[-PPPP].parquet    path, dir, name, kind, mode, size, sha256, mtime, batch, attrs
catalog/dirs-NNNN[-PPPP].parquet     per-directory counts and bytes
catalog/excluded-NNNN[-PPPP].parquet what the rule profile left out, and why
packs/NNNN.pack                       zstd frames of up to 1 MiB of concatenated blobs
packs/index-NNNN[-PPPP].parquet      sha256 -> (pack, frame offset, frame length, offset, size, part)
derived/<adapter>/events-NNNN[-PPPP].parquet   parsed event envelope (rebuildable)
derived/<adapter>/events-rebuild-GGGG[-PPPP].parquet   transactional full rebuild
```

- **Git-safe physical segments.** Packs, Parquet files, and the serialized manifest are capped at the lower of the
  configured hook limit and a conservative 60 MiB target. Oversized batch tables use a `-PPPP` part suffix. The
  `files`, `dirs`, `excluded`, and `index` tables use synchronized suffixes.
- **Manifest publication boundary.** Readers and SQL open only artifacts declared by `MANIFEST.json`; interrupted,
  unlisted output is invisible and removed by the next pack. Missing declarations fail store opening, and the hook
  rejects both missing and surplus finalized artifacts. Format-1 stores remain readable; new writes upgrade them to
  format 2 with explicit derived-file publication. Manifest replacement uses a unique, exclusive-created,
  no-follow temporary file that is flushed and synced before rename.
- **Content-addressed.** A path maps to a sha256; a sha256 maps to bytes in a pack. Duplicate content is stored once,
  across rounds and across the runner's own snapshot copies.
- **Consistent live prefixes.** Each regular file is bound to its captured inode and length. Hashing, packing,
  and derived events use that same prefix; later appends are left for the next batch. Replacement, truncation,
  or a changed prefix aborts publication rather than producing mismatched sizes/hashes. This is a per-file
  checkpoint, not an atomic snapshot of a whole actively changing tree.
- **Packs, not Parquet, for bytes.** Reading one file is a `pread` of one frame plus a decompress of at most 1 MiB,
  a few microseconds to a few milliseconds. Parquet holds only the catalog and the derived tables.
- **Append-only batches.** Each `traj pack` adds one batch: new paths, only the blobs not seen before, and its own
  catalog segments. Nothing already written is modified, so every commit is additive.
- **Transactional derivation.** `traj derive` writes a uniquely named replacement generation, atomically publishes it
  through the manifest, and only then removes the prior generation. Readers retain a shared store lock for their
  lifetime; pack and derive hold the exclusive lock through publication and cleanup. Readers open `.lock` read-only;
  legacy stores without it use a shared lock on `MANIFEST.json` while the first writer bootstraps `.lock`.
- **Safe derived paths.** Adapter names are one non-empty path component, and every resolved artifact must be a regular
  file inside the canonical store root.
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

Verbs: `init doctor pack watch ls tree find du stat cat extract edit grep sql derive verify commit skill hook bench mount umount`.
`traj <verb> --help` documents each one.

## Browsing in VS Code and other tools

A store can be mounted as an ordinary read-only directory tree (FUSE, Linux; no root needed where `/dev/fuse` and
`fusermount3` exist). VS Code, `grep -r`, `diff -r`, Python and every other tool then see normal files, served from
the catalog and the packs with nothing extracted: a round of the 2.1 M-path reference run lists in 0.2 s the first
time and in milliseconds after, a file opens in 2–3 ms, and a `diff -r` of a whole round against `traj extract` is
clean in 10 s. New batches and new stores appear without remounting. Design and numbers: `docs/PLAN-fuse.md`.

```
traj mount ~/traj-mnt --daemon --save                 # every store under stores/, one directory per run; --save records
                                                      # mount_root in trajfs.toml and writes the VS Code watcher exclude below
code -r ~/traj-mnt                                    # or a run, or a round: VS Code settings are written for you
traj -S stores/run-42.trajstore mount ~/mnt-42        # or one store, its tree at the mountpoint
traj umount ~/traj-mnt
```

VS Code needs two settings for a mount, and `traj mount` writes them for you: the mount is excluded from the file
watcher (which otherwise crawls every workspace folder to set inotify watches: minutes on a 2 M-path tree, and it
exhausts `max_user_watches`), and its files are marked read-only so editors show a lock instead of a failed save.
They go into VS Code's machine-level settings on the host on every mount (Remote-SSH: `~/.vscode-server/data/Machine/settings.json`),
so opening the mountpoint itself or any folder under it is fine; `--save` and `traj init --mount-root` also put
them into the repo's `.vscode/settings.json`, and `--no-vscode` skips all of it:

```json
{ "files.watcherExclude": { "/home/me/traj-mnt/**": true },
  "files.readonlyInclude": { "/home/me/traj-mnt/**": true },
  "search.followSymlinks": false }
```

Memory is a best-effort target (`--memory`, default 20 % of RAM): blobs and listings are evicted and inodes handed
back to the kernel when the estimate is over it. The mount is read-only and the kernel enforces it (editors offer "Save As"); every read is sha-verified, so a
corrupt pack shows up as an I/O error on that file, never as wrong bytes. The mountpoint must be outside every git
work tree (`traj mount` refuses otherwise, and `traj doctor` lists live mounts). `getfattr -d <file>` shows the
sha, the batch and the adapter attrs as `user.traj.*` xattrs. Ctrl+Shift+F is ripgrep over the mount: fine within a
round, slow over a run, where `traj grep` and `traj sql` remain the tools.

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
thousand new paths, files over 65 MiB, symlink/gitlink-backed store artifacts, or nested `store_root` results that are
not complete, readable TrajFS stores. It prints the `traj pack` / `traj commit` commands to use instead. Deleting tracked raw paths stays
allowed, so an existing repository can migrate. Policy and file sizes are read from the staged Git objects, not from
potentially different working-tree contents.

`traj commit` commits only its selected store and leaves unrelated staged and unstaged work intact.
`traj init` honors Git's hook location, including linked worktrees and `core.hooksPath`, and does not overwrite
an existing store-root attributes file. Malformed, unreadable, or missing explicitly selected configuration is an
error, not permission to fall back to different roots or retention rules. A watcher stops without committing when
packing returns an error.
Manual packing and watching use the same encoded data-root-relative store IDs for nested runs, so two tasks with
the same run-directory basename do not collide. An explicit `--out` or `--id` still takes precedence.

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
Content functions resolve hashes across all registered stores and verify returned bytes. An unknown or malformed
hash returns NULL; corrupted or unreadable stored content raises an error instead. Each SQL connection owns its
store bindings and releases their locks when it closes.

These SQL views expose the published batch history, including older versions of a path. Filesystem-style
`stat`, browse, extract, grep, and mount operations instead resolve the latest compatible file/directory namespace.
Adapter partial records preserve invalid UTF-8 as a JSON `bytes` array, rather than irreversibly replacing bytes.
The outer trajectory blob remains the authoritative original stream.

```
traj -S S sql "select tool_name, count(*) from events where type='tool.execution_complete' group by 1 order by 2 desc"
traj -S S sql "select path, json_extract_string(text(sha),'$.verdict') from files where name='review.json'"
```

## Testing

```
cargo test --release                                            # 59 tests, ~15 s: format, round trips (proptest),
                                                                # integrity, catalog verbs vs ls/find/du, grep vs grep,
                                                                # events, SIGKILL recovery, watch, hook, skill examples,
                                                                # the FUSE mount (T10, skipped without /dev/fuse)
TRAJ_SLOW_SRC=<run dir> TRAJ_SLOW_ADAPTER=<adapter.toml> \
  cargo test --release --features slow -p traj --test cli slow::   # a real round and a whole run, with size and latency bounds
TRAJ_SLOW_STORE=<store dir> \
  cargo test --release --features slow -p traj --test cli t10k     # the mount against a real store, with latency and RSS bounds
cargo bench -p traj                                              # criterion latency of the hot verbs
traj bench -S S --out review-bench/history/                 # regression baseline as JSON
```

No shell or Python anywhere in the build, hooks or tests; external checks spawn `zstd`, `grep`, `ls`, `find`, `du`
and the DuckDB CLI directly.

## Repository layout

```
crates/trajfs-core       walk + rules, hashing, packs, Parquet catalog, manifest, store reader, ingest
crates/trajfs-adapters   copilot-cli / claude-code / jsonl parsers; the TOML-declared adapter
crates/traj              the CLI (hook, watch, skill export, bench, the FUSE mount included)
rules/                   built-in rule profiles
skills/traj/SKILL.md     agent skill template, embedded in the binary
docs/PLAN.md             design and test plan, with measured status (§14, §15)
docs/idea-review.md      the evaluation that chose this design over tar+FUSE, with the benchmark
docs/idea.md             the original notes
docs/PLAN-fuse.md        the read-only FUSE mount: design, measurements (§16); docs/vscode-viewer.md records why a mount was chosen over an editor plugin
review-bench/            the prototype scripts and numbers from the review (historical)
```

## Status

Current writes use format 2 and readers remain compatible with format 1. The design, the CLI and the test plan in
`docs/PLAN.md` are implemented; measurements in §14 and §15 come from a 2.1 M-path, 12 GB run. Open items: DuckDB-backed full-catalog
scans (find by attribute is ~1 s on 2 M rows), `traj compact`, and the runner-side output-root guard described in
§6.1.
