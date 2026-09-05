---
name: traj
description: Manage agent run outputs (trajectories, logs, round snapshots) with the traj store. Use when asked to archive, commit or push run results, list/find/read files of a run, analyse rounds, reviews or events across runs, or extract a run for a human. Never git-add raw run directories; use traj.
---

# traj — run output store

Raw run trees live in `{{data_root}}` (outside every git repo, never committed).
Stores live in `{{store_root}}` inside this repo and are the only committed form: `{{store_root}}/<run-id>.trajstore/`.
Both roots come from `trajfs.toml`; `traj doctor` shows them and checks the setup.

## Rules

- Never `git add` anything under `{{data_root}}` or any `rounds/round-*/` tree. The pre-commit hook rejects it; the fix is `traj pack` + `traj commit`.
- Never delete or move anything under `{{data_root}}` or a `.trajstore` unless the owner asked explicitly; drops are moves into a dated `dropped/` directory, never deletes.
- Never use `--no-verify` in scripts. Never write back into a store; `traj edit` is a temp-copy viewer and `traj mount` is read-only (the kernel enforces it).
- Read from the store, not from the raw tree, whenever the store exists (`traj ls` answers in ms; `find` on the raw tree takes minutes).

## Procedures

One-time setup of a repo (operator; `traj doctor` afterwards must print OK):
```
traj init --data-root <abs dir outside any git repo> --store-root stores --adapter <name> --rules <profile>
```

Archive a finished or paused run, then commit and push only the new batch:
```
traj pack {{data_root}}/<workload>/<run-id> --label "<what this batch is>"
traj verify -S {{store_root}}/<run-id>.trajstore
traj commit --push {{store_root}}/<run-id>.trajstore
```

Keep a running grid archived automatically (one per host):
```
traj watch {{data_root}} --store-root {{store_root}} --commit --push
```

Browse and read:
```
traj ls   -S <store> [-l] <dir>            traj tree -S <store> <dir> --depth 2
traj find -S <store> --name '<glob>' [--attr round=37] [--attr role=reviewer]
traj cat  -S <store> <path>                 traj stat -S <store> <path>
traj grep -S <store> -e '<regex>' --name '*.stdout' -l
traj du   -S <store> <dir> --depth 1
```

Materialise for a person (VS Code, diff tools). A mount shows every store under `{{store_root}}` as a read-only tree, one directory per run, and writes the VS Code settings (watcher exclude, read-only) for it. `extract` makes a writable copy:
```
traj mount <mountpoint outside every git work tree> --daemon --save   # first time; then: code -r <mountpoint>/<run-id>
traj umount <mountpoint>
traj extract -S <store> <dir-or-file> /tmp/<name>
traj edit    -S <store> <path>
```

Rebuild derived tables after an adapter upgrade, check a commit for raw paths in CI, or record a performance baseline:
```
traj derive -S <store>            traj hook check-tree HEAD            traj bench -S <store> --out review-bench/history/
```

Analyse with SQL (DuckDB; views: `files`, `dirs`, `blobs`, `excluded`, `events` and other `derived/*` tables; functions `blob(sha)`, `text(sha)`; several `-S` register a `store` column):
```
traj sql -S <store> "<query>"
```
`files`: path, dir, name, kind, mode, size, sha, mtime_ns, batch, attrs (map; e.g. attrs['round'], attrs['role']).
`events`: trajectory, seq, ts, type, id, parent_id, actor, tool_name, exit_code, payload_json, adapter_version.

## Worked examples

Each block is one command; `S` is a store path or id. They are executed against a fixture store by trajfs's tests,
so every one of them is known to run.

Reviews per round, with the verdict field of each review record:
```
traj sql -S S "select attrs['round'] r, json_extract_string(text(sha),'$.verdict') v from files where name='review.json' and attrs['role']='reviewer' order by r"
```
Tool calls that failed in one trajectory:
```
traj sql -S S "select seq, tool_name, exit_code, substr(payload_json,1,120) from events where trajectory='rounds/round-0001/builder/events.jsonl' and exit_code is not null and exit_code<>0 order by seq"
```
Paths, distinct blobs and bytes per round:
```
traj sql -S S "select attrs['round'] r, count(*) paths, count(distinct sha) blobs, sum(size) bytes from files group by 1 order by r"
```
Which stdout/stderr files mention a traceback:
```
traj grep -S S -l -e Traceback --name '*.std*'
```
One round's review directory as real files, for an editor or a diff:
```
traj extract -S S rounds/round-0001/reviewer /tmp/traj-r1
```

## Adapter (this repo's own; trajfs has no knowledge of any runner)

The adapter is a TOML file in this repository (default `trajfs/adapter.toml`, named by `adapter = ...` in `trajfs.toml`). It declares the run-tree layout: `[[attrs]]` regexes whose named groups become `files.attrs` keys; `[trajectories]` globs plus `format` (`copilot-cli`, `claude-code` or `jsonl`) for the events table; `[batch_ready]` `run_glob` and marker globs for `traj watch`; `[hook] raw_patterns` the pre-commit hook refuses; `rules` naming the rule profile (`trajfs/rules.toml`). If `traj init` scaffolded templates, complete them from the runner's actual output layout, run `traj doctor`, pack one run and check `traj ls -l` shows the expected attrs, then commit the two files. Changing `[[attrs]]` or `[trajectories]` later needs `traj derive` (events) or a new pack batch (attrs); packs and blobs never change.

## When something is refused

- Hook: "raw run paths in commit" → run the `traj pack` / `traj commit` lines above.
- `traj pack`: "store inside data_root" or "source inside store_root" → the roots are nested or swapped; run `traj doctor`.
- `traj doctor` warns "skill version differs" → run `traj skill export` and commit the updated skill.
