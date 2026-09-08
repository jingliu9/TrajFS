# Plan: delete one trajectory from a store

**Status: implemented as `traj delete` (2026-09-06). This document is the protocol it follows.**

Delete one trajectory from the current archive while keeping every earlier version in Git. A trajectory is
named by its catalog path: one file, or one directory subtree such as a round or a whole task execution. The
first principle is to be as simple as possible while being correct. Correctness here means:

1. At every instant, the store directory on disk is a complete, verifiable store: either the old one or the new one.
2. Nothing is removed from disk before the replacement has been deep-verified and made durable.
3. The state before the deletion is always recoverable from Git, so recovery never has to guess.
4. Once a trajectory is deleted, later `pack` runs do not silently bring it back.

Everything else in the earlier draft of this plan (task inventories, ownership columns, a maintenance lease on a
directory descriptor, a transaction journal with five recovery states, plan digests) was dropped. The store already
has the only ownership notion it needs: the path.

## 1. Granularity: what a store is and what a trajectory is

**A store is the source tree passed to `traj pack`, plus the batches appended to it later.** It is not
automatically one task, one round, or one campaign; `pack` archives whatever boundary it is given. For new archives,
prefer **one task execution per store**, keeping all of that execution's agents and rounds together. A run is then
a collection of task stores. The full discussion, including the effects on deduplication, ingestion, queries, Git,
and concurrency, is in [granularity.md](granularity.md).

| Unit | Meaning | How it appears in a store |
|---|---|---|
| Task | A problem or objective | Usually the store itself, or a directory such as `tasks/task-a-0001` |
| Task execution | One attempt at a task, finished or not | The same directory; retries get new directories or new stores |
| Round | One iteration of agent work inside an execution | A directory such as `rounds/round-0002` |
| Trajectory file | One parseable log, for example `events.jsonl` | One catalog path |
| Batch | One `pack` capture | Catalog rows carrying that batch id, across every batch |

Every one of these units is a **path prefix** in the catalog. Rows, directory summaries, exclusion records and event
rows are all keyed by path. Deletion therefore takes one path and removes everything at or below it, in every
batch. The user picks the unit by picking the path; the dry run shows exactly what the prefix covers.

Two consequences of the recommended layout:

- In a one-task-per-store layout, deleting the task is `git rm -r` of the store directory plus moving the raw tree
  aside. No command is needed.
- `traj delete` exists for the other cases: one round or one log inside a task store, or one task inside a grouped
  store. A grouped store rewrites more surviving data; that is the cost of grouping, not a correctness risk.

Not supported, deliberately: deleting by round number, model name, or attribute; bulk deletion; deleting an
execution that is split across several stores (delete from each store separately); secure erasure; rewriting Git
history. Content shared with a surviving path stays because it is still referenced; deletion is by reference, not by
content.

## 2. The on-disk model both protocols rely on

```text
task-a.trajstore/
  MANIFEST.json                  the only mutable file; names every published artifact
  catalog/{files,dirs,excluded}-B[-P].parquet
  packs/NNNN.pack, packs/index-B[-P].parquet
  derived/<adapter>/<table>-B[-P].parquet
  .lock                          flock target: shared for readers, exclusive for writers
  .gitattributes
```

Two invariants make the store transactional with nothing but rename:

- **Artifacts are immutable and never reused.** A pack or Parquet segment is written under a temporary name,
  synced, and then hard-linked into its final name; an existing final name is never replaced. Pack ids only grow.
- **The manifest is the commit point.** Readers open the manifest and then only the artifacts it declares. A file
  that exists but is not declared is invisible, and is removed as an orphan by the next writer. `MANIFEST.json` is
  replaced atomically (temp file, fsync, rename), so a reader sees either the old inventory or the new one.

Git holds the store's files as ordinary tracked content. `traj commit <store>` stages exactly the store's path and
commits. The pre-commit hook checks that the staged tree is a complete store: every declared artifact present, no
undeclared finalized artifact, no foreign file. A commit is therefore a verified checkpoint of one whole store.

## 3. Transaction protocol of `traj pack`

`pack` appends one batch. Its protocol, as implemented in `trajfs-core/src/ingest.rs`:

| Step | Action | State if interrupted here |
|---|---|---|
| P0 | Take the exclusive lock on `.lock`; refuse if a deletion of this store is pending (§4.6). | Unchanged. |
| P1 | Remove orphans: temporary files and finalized artifacts the manifest does not declare. | Unchanged; this is `pack`'s own recovery step. |
| P2 | Verify the existing store; load its rows and blob index. | Unchanged. |
| P3 | Walk the source, skipping deleted prefixes (§5), hash every candidate. | Unchanged. |
| P4 | Write new packs under fresh ids: each pack is synced, then hard-linked to its final name. | Old manifest still published; new packs are undeclared orphans. |
| P5 | Write catalog, index and derived segments the same way. | Same. |
| P6 | Sync the artifact directories. | Same. |
| P7 | Publish the manifest: write temp, fsync, rename over `MANIFEST.json`, fsync the store directory. **Commit point.** | Before the rename: old snapshot. After: new snapshot. |

Guarantees: a crash at any step leaves the old snapshot valid and readable; the next `pack`, `derive` or `delete`
removes the undeclared leftovers. There is no partial batch: either the manifest declares the batch and all of
its artifacts, or none of them are visible. Readers holding the shared lock are blocked for the duration; a mount,
which reads without the lock, keeps its current snapshot until it notices the new manifest and reopens.

`traj derive` follows the same shape: it writes a fresh `events-rebuild-N` generation, publishes it in the manifest,
and only then removes the previous generation.

Git is a separate, later step. `traj commit` stages the store, and the hook refuses an incomplete inventory. Until
that commit, the new batch exists only on disk; that is acceptable because `pack` is repeatable from the raw tree.

## 4. Transaction protocol of `traj delete`

Let `S` be the store directory and `W = S` with `.deleting` appended, a sibling in the same directory and hence on
the same filesystem. `traj delete` never modifies anything inside `S`. It builds a complete replacement store in
`W`, verifies it, and swaps the two directories in one atomic system call.

### 4.1 Preconditions (all checked before anything is written)

| Check | Why | On failure |
|---|---|---|
| `S` is inside a Git worktree, tracked, and identical to `HEAD` (no modified, untracked, or ignored files under it, except the lock file). | Git is the recovery anchor. The state before deletion must be a commit. | Refuse: "commit the store first" (or clean it). |
| No `W` exists. | A pending deletion must be resolved first. | Refuse; run `traj delete --recover`. |
| No FUSE mount serves `S`: no `fuse.traj` mount whose label is `S` or a directory containing `S`. | A mount reads pack files by name; after the swap the same names hold different bytes. | Refuse, listing the mountpoints: "please unmount: `traj umount <mountpoint>`". |
| Exclusive `.lock` acquired. | No reader or writer may hold the old artifacts open. | Refuse: another command holds the store. |
| The store passes verification. | Never rewrite a broken source. | Refuse; repair first. |
| The path exists in at least one batch and is not already in the deleted list. | Nothing to do otherwise. | Refuse, or report "already deleted". |

A mount started during the seconds of an apply is the only window these checks leave open. Its effect is benign:
that mount sees hash-verification failures until its next manifest refresh reopens the new store. Store contents
are never affected.

### 4.2 Build the candidate in `W`

For each batch, in id order:

1. Read the batch's file rows (with attributes). Keep the rows whose path is not at or below the target.
2. Rebuild the directory table from the kept rows.
3. Read the batch's exclusion records; keep the ones not at or below the target.
4. Repack content: every kept row's hash that has not yet been written to `W` is read from `S` with its hash
   checked, and written to `W`'s packs. Pack ids continue from one; the batch's index lists the hashes it wrote.
   Empty files have no blob, as before.
5. Write the four batch tables with the same segmented writer `pack` uses, under the same names.
6. Copy each of the batch's derived tables, dropping the rows whose `trajectory` column is at or below the target.
   Tables without that column are copied unchanged. Names are preserved, so the manifest's derived lists carry over.
7. Record the batch with its original id, timestamp, label, errors, and elapsed time, and with its retained counts
   (paths, bytes, new blobs, packed bytes, exclusions) recomputed from what was written. Verification compares those
   counts to the tables, so they must describe the rewritten data, not the original ingest.

Then write the manifest with a new entry in the `deleted` list (§5), the `.gitattributes`, and an empty `.lock`.
A batch whose rows are all deleted stays in the manifest with empty tables, so batch ids and history order are
stable for the survivors.

Memory: the rebuild streams rows batch by batch and blobs one at a time. A single blob is materialized whole, which is
what `verify --deep` and `cat` already do.

### 4.3 Verify the candidate independently

Open `W` as a normal store and run deep verification: every declared artifact present, every hash re-computed from
the packed bytes, per-batch counts matching, directory tables matching file rows. Then scan `W` again and check:

- no row, exclusion record, or event row at or below the target remains;
- every kept row of every batch is present with identical path, kind, mode, size, hash, mtime, batch, and attributes;
- the survivor row count equals the plan's count.

Then fsync every file and directory under `W`. Only after this may `W` replace `S`.

### 4.4 Swap and finish

| Step | Action | State if interrupted here |
|---|---|---|
| D1 | Lock `W/.lock` exclusively (so the new `S/.lock` is held until the end). | `S` old, `W` candidate. Not applied. |
| D2 | `renameat2(S, W, RENAME_EXCHANGE)`: `S` and `W` exchange names atomically. **Commit point.** Then fsync the parent directory. | Before: as D1. After: `S` new, `W` old. Applied. |
| D3 | Remove `W` (now the old store). | `S` new; leftover `W`. Applied. |
| D4 | Release the locks and report. Print the `traj commit` command. | `S` new. Applied, uncommitted. |

The exchange is a single system call, so there is no state in which `S` is missing or half-populated. If the
filesystem does not support `RENAME_EXCHANGE`, the command refuses before building anything and says so.

### 4.5 States and recovery

The on-disk state is fully described by two facts: does `W` exist, and does `S`'s manifest carry the new deletion
record? Recovery does not need a journal because both answers are readable from disk.

| `W` exists | `S` manifest has the record | Meaning | Recovery |
|---|---|---|---|
| no | no | Nothing pending. | None. |
| yes | no | Interrupted before the exchange. `S` is untouched. | Remove `W`. Report "not applied". |
| yes | yes | Interrupted after the exchange. `S` is the verified new store. | Remove `W`. Report "applied, run `traj commit`". |
| no | yes | Applied, not yet committed. | `traj commit`, or discard with Git (below). |

`traj delete --recover` performs the removal after checking that `S` opens and verifies. `pack` and `derive`
refuse to touch a store while `W` exists, so a leftover cannot be built upon by accident. Readers are unaffected:
`S` is always a valid store.

**Undo after apply, before commit.** The pre-deletion store is `HEAD`:

```bash
git checkout HEAD -- stores/task-a.trajstore
git clean -fdx -- stores/task-a.trajstore
```

**Undo after commit.** Earlier versions stay in Git history; restore the store's path from the earlier commit in a
separate worktree or with the same two commands against that commit. Deletion never rewrites history.

### 4.6 Git

`traj delete` does not run Git. After D4, `traj commit <store>` stages the store's path, which now contains
modified, added, and removed files, and commits with a message naming the deleted path. The hook validates the
staged inventory exactly as it does for a new batch. Repacking changes pack contents, so a deletion commit can add
Git objects even though the working tree shrinks; the old objects are kept on purpose.

## 5. Preventing resurrection

The manifest gains a `deleted` list (format 3):

```json
"deleted": [
  {"path": "rounds/round-0002", "created": "2026-09-06T10:00:00Z",
   "paths": 17, "bytes": 1048576, "events": 240, "excluded": 0, "blobs": 5, "blob_bytes": 900000}
]
```

`pack` and `watch` skip every source entry at or below a deleted path, regardless of the rule profile, and record
it in the batch's exclusion table with rule `deleted`. The raw tree is not touched; the dry run reports when the
source still contains the path. Deleting a path a second time is refused as already deleted. A retry of the same
task must use a new directory, or a new store; the deleted list is permanent for the store.

Format 3 is format 2 plus this list. Readers accept formats 1 through 3; a binary that does not know format 3
refuses the store instead of dropping the list on its next write.

## 6. Command

```bash
traj -S stores/run-42.trajstore delete tasks/task-a-0001          # dry run: what would be removed
traj -S stores/run-42.trajstore delete tasks/task-a-0001 --yes    # apply (§4)
traj commit -m "Delete task-a-0001" stores/run-42.trajstore       # checkpoint in Git
traj -S stores/run-42.trajstore delete --recover                  # after an interruption
```

The dry run prints the batches touched, rows and bytes removed, event and exclusion rows removed, blobs that become
unreferenced, survivors, the Git and mount checks, and whether the raw source still contains the path.
`--yes` is the only switch; there is no force flag that bypasses a check.

## 7. Tests

| Case | Expected |
|---|---|
| Two rounds in one store, three batches; delete one round | Its rows, events and exclusions are gone from every batch; every other row is byte-identical; `verify --deep` passes; SQL and `ls` agree. |
| Delete one file | Same, for a single path; a shared blob survives because another path references it. |
| Unknown path, path already deleted, store not committed, store dirty, store mounted (simulated leftover), pending `W` | Refused before any write; `S` unchanged. |
| Interrupted before the exchange (leftover candidate) | `--recover` removes it, `S` unchanged, `pack` refused until then. |
| Interrupted after the exchange (leftover old copy) | `--recover` removes it, `S` is the new store. |
| `pack` the same source again after deletion | The deleted path is skipped and recorded as excluded; other changes are archived. |
| `traj commit` after deletion | Message names the path; the hook accepts the staged store; `check-tree HEAD` passes; the pre-deletion store is readable from the previous commit. |
| Last path removed | A valid store with empty tables, zero counts, and the deletion record. |

## 8. Implementation map

| Location | Change |
|---|---|
| `crates/trajfs-core/src/manifest.rs` | Format 3: `deleted` list; the manifest save syncs the store directory. |
| `crates/trajfs-core/src/walk.rs`, `ingest.rs` | Deleted prefixes are skipped and recorded as excluded; `pack` refuses when a deletion is pending; artifact directories are synced before the manifest is published. |
| `crates/trajfs-core/src/delete.rs` | Plan, rebuild into `W`, verification, atomic exchange, recovery. |
| `crates/traj/src/cmd/delete.rs` | The command: Git and mount preconditions, dry run, apply, recover. |
| `crates/traj/src/cmd/commit.rs`, `mount.rs` | Deletion commit messages; mounts ignore `*.deleting` siblings and report which mounts serve a store. |
