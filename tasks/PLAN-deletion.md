# Plan: delete one task-scoped experiment

**Status: proposed; no deletion command is implemented yet.**

Delete one task execution, including all of its agents, rounds, logs, reviews, and workspace snapshots. Do not
delete a round on its own. A run may contain several tasks, and an unfinished task execution is still a valid
deletion unit.

Change the current archive files and record that change in a normal Git commit. Keep earlier Git versions.
Deletion is rare: favor a verified rewrite over in-place pack surgery.

## 1. Define the boundary

| Term | Meaning in this plan |
|---|---|
| Task | A problem or objective given to the agents. |
| Experiment / task execution | One execution of one task, starting at an explicit task boundary. It can be solved, failed, stopped, or still partial. |
| `experiment_id` | A stable identifier for that execution, not its model, task name, round number, or directory basename. Retrying the same problem creates another ID. |
| Run | A container that may hold several task executions. It is not automatically a deletion unit. |
| Round | One iteration within a task execution. It is never independently deletable through this command. |
| Store | One `.trajstore` archive. Today, its scope is the source tree passed to `traj pack`; it need not equal a task. |
| Published snapshot | The manifest and every artifact it declares, including catalog history and derived tables. |
| Deletion record | Small persistent metadata saying that an execution was deliberately removed, so later ingestion cannot silently bring it back. |

For the user's `onesw-gen` example, delete the whole task execution, not one of its rounds. A task-level start and
completion decision establish the lifecycle; a round's `DONE` marker is only a checkpoint. Missing completion
does not prevent deleting an otherwise unambiguously identified partial execution.

Recommend **one task execution per store** for new archives. Support multiple tasks in one store when ownership
is explicit. Do not silently delete only part of an execution fragmented across stores: reject that case in the
first version and report the locations requiring consolidation. Copies outside the selected archive scope remain.
See [store granularity](granularity.md).

## 2. What deletion means

Remove the selected execution's file-catalog rows across **all published batches**, not just its latest paths.
Remove its owned exclusion records and derived events too. Keep every other execution's retained history and bytes.
Repack only content still referenced by surviving rows.

Keep Git history, raw source trees, and other copies untouched. Do not stop agent processes, rewrite commits,
force-push, or promise secure erasure. Shared content needed by another task remains. Shared run-level provenance
may still mention the deleted task; this is task removal, not a text-redaction operation.

The deletion record is not a substitute for deletion. The task's owned payloads and records leave the current
archive; the small record prevents accidental re-ingestion. If no tasks remain, keep a valid, empty store with that
record rather than removing the store directory and losing its identity.

## 3. Gaps in the current implementation

| Current behavior | Required change |
|---|---|
| [`Adapter::is_trajectory`](../crates/trajfs-core/src/adapter.rs) identifies parseable log files, not task executions. | Add a separate task-boundary and ownership contract. Do not reuse this predicate as the deletion boundary. |
| [`FileRow`](../crates/trajfs-core/src/lib.rs) has paths, hashes, batches, and free-form attributes. | Persist authoritative task ownership; an optional `round` or `role` attribute is not enough. |
| [`insert_latest`](../crates/trajfs-core/src/store.rs) retains absent historical paths. | Removing a source directory or changing retention rules is not deletion. |
| [`ingest`](../crates/trajfs-core/src/ingest.rs) adds immutable packs and batch tables. | Build replacement artifacts without overwriting the published ones. |
| [`traj derive`](../crates/traj/src/cmd/derive.rs) reparses latest retained trajectories. | Deletion must filter existing event history, not rebuild only the latest logs and lose other tasks' history. |
| [`mount::open_snapshot`](../crates/traj/src/mount/mod.rs) deliberately opens without the normal reader lock. | Add a maintenance lease before reclaiming artifacts a mount could still use. |
| [`traj commit`](../crates/traj/src/cmd/commit.rs) stages one store with `git add -A`. | Reuse its scoped commit behavior; account for deletion records and maintenance-aware messages. |

## 4. Make ownership explicit before allowing deletion

Add a manifest-declared task inventory and typed ownership on file, exclusion, and event records. An owner is one
`experiment_id`, explicitly shared infrastructure, or unclassified. **Unclassified does not mean shared.**

The inventory records each execution's identity, task label, source-run identity, owned roots/paths, start evidence,
and observed lifecycle state. Adapters describe these boundaries from the runner's actual layout and signals.
Normal agents continue writing files; no agent SDK change is required.

First-version rules:

- Require complete, reviewed ownership classification for the affected store, including historical records.
- Allow several disjoint owned roots for one execution, so auxiliary outputs are not accidentally omitted.
- Reject overlapping task roots, ambiguous reused roots, and cross-task file/directory shadowing.
- Reject inseparable mixed-task payloads unless an explicit, validated splitter exists. Do not drop a shared log
  wholesale or quietly call task payloads "shared" to bypass the check.
- Never infer task boundaries from round numbers, model names, modification times, or a terminal marker alone.
- Legacy stores require an explicit ownership-adoption step, based on archived evidence and a supplied adapter.
  Preserve the original store until that adoption is verified and committed.

Introduce a **deletion-capable format 3**. Readers continue supporting formats 1 and 2; adoption is explicit.
Format 3 must validate task metadata and deletion records, and old binaries must reject it rather than silently
discarding those records on a later write.

Keep the existing `catalog/`, `packs/`, and `derived/` layout. Publish task inventories and deletion records as
bounded, manifest-declared tables. Do not invent unlisted sidecars that SQL, verification, or the Git hook ignores.
Persist monotonic artifact-ID high-water marks so removing the last pack does not reset future allocation to one.

## 5. Proposed command

The following interface is **planned**, not available today. `task-a-0001` is an inventory ID for one execution.
`PLAN_DIGEST` is the digest printed by the read-only preview.

```bash
traj -S stores/run-42.trajstore delete-task task-a-0001 --dry-run
traj -S stores/run-42.trajstore delete-task task-a-0001 --apply "$PLAN_DIGEST"
traj commit -m "Delete task execution task-a-0001" stores/run-42.trajstore
```

Default to a read-only preview. Show the task identity, lifecycle state, owned roots, affected rounds and agents,
path/version counts, surviving tasks, shared-content retention, and the Git checkpoint. Report unreferenced logical
blob bytes separately from estimated compressed reclamation: shared frames make exact reclamation a rewrite result.

Bind the digest to the target ID, complete source-artifact fingerprints, ownership rules, manifest, Git base commit,
and relevant configuration. Recompute it under the apply locks. A stale preview requires a new preview.

Accept exactly one registered execution ID. No file paths, round selectors, globs, bulk deletion, or force flag
that bypasses ownership and integrity checks. An unknown ID is an error; an already deleted ID is an explicit,
idempotent "already deleted" result with no new batch or commit.

## 6. Preservation invariants

Let `C` be all published file-catalog rows, including historical versions; let `e` be the selected execution ID;
and let `owner(r)` be the declared owner of row `r`. Let `Empty` denote the existing zero-length-file kind.

```text
C_keep = all rows r in C for which owner(r) != e
H_keep = distinct r.sha values in C_keep, excluding rows whose kind is Empty
```

Preserve the original batch/path ordering of `C_keep`. For every hash in `H_keep`, preserve and verify the original
bytes. A hash shared with another execution or an explicitly shared artifact stays, even if the deleted execution
also referenced it.

Required postconditions:

- No owned file, exclusion, or event record for `e` remains in the published data tables.
- Every surviving file row keeps its path, kind, mode, size, hash, mtime, attributes, and logical batch identity.
- Other tasks' event rows retain their payloads, sequence numbers, adapter versions, multiplicity, and ordering.
- Other tasks' latest visible namespaces are unchanged. Reject a rewrite that would resurrect previously shadowed
  non-target paths through an ownership conflict.
- Directory summaries, retained batch counts, indexes, and the manifest agree with the rewritten data.
- All declared artifacts exist, fit the configured size limits, and pass deep verification; no obsolete finalized
  data artifacts remain when the command reports completion.

Keep original ingestion measurements as provenance. Do not present newly rewritten pack sizes as measurements of
an earlier ingest. Format 3 must distinguish historical ingest statistics from current retained counts and storage.

## 7. Apply: build, verify, publish, clean

1. **Establish the Git checkpoint.** Require the selected store to be fully tracked and unchanged from `HEAD`,
   with no staged, unstaged, untracked, or ignored extra payloads inside it. Unrelated repository changes are allowed.
   Check actual committed Git blob contents against the published artifacts, not just cached `git status` results.
   Refuse uncommitted data or external-filter/LFS pointers that do not themselves provide the required checkpoint.

2. **Acquire maintenance access.** Hold the exclusive maintenance lease, then the existing exclusive store lock.
   Revalidate the Git checkpoint, source fingerprints, task ownership, and preview digest. Deep-verify the source.
   A busy store, invalid configuration, unknown artifact, or corrupt input stops the operation.

3. **Write a durable intent.** Create an exclusive, no-follow transaction journal and a private staging directory
   under the store, on the same filesystem. Record the operation ID, Git checkpoint, old manifest digest, target,
   and owned staging paths. Sync the journal and its directory before creating replacement artifacts.

4. **Rewrite retained data.** Stream `C_keep` and its referenced blobs into new packs, checking SHA-256 while copying.
   Rebuild all indexes and directory summaries. Preserve logical batch IDs, including empty batches where necessary;
   produce schema-correct empty tables when the last task is removed. Filter existing derived rows by ownership
   without reparsing logs or deduplicating legitimate historical event rows.

5. **Use fresh physical names.** Allocate pack IDs and synchronized catalog/index suffixes that have never been
   published before. Extend the segmented-writer helpers rather than overwriting `files-0001.parquet` or reusing an
   old pack number. Keep the existing lower-of-hook-limit-and-60-MiB artifact bound, including new metadata tables.

6. **Verify the candidate independently.** Compare surviving rows and event records against the source projection,
   then deeply verify the candidate's content and metadata. Check task absence, survivor visibility, byte equality,
   schema, part ordering, counts, and size bounds. Persist a prepared journal containing the exact old/new artifact
   inventories and expected new manifest digest. Sync the candidate files and relevant directories.

7. **Publish once.** Install the new immutable artifacts without replacement, using the existing exclusive
   publication pattern. Sync their parent directories. Atomically replace `MANIFEST.json`, then sync the store
   directory. This manifest replacement is the logical commit point; the directory sync supplies its durability
   barrier on the supported filesystem.

8. **Reclaim and finish.** Remove only the journal's verified obsolete inventory: old published data paths no longer
   referenced by the new manifest. Verify root containment and file identity before unlinking; never glob-delete
   directories or remove the store root. Re-verify the live store, sync affected directories, remove staging and the
   journal, sync the store root, and release locks. Only then report deletion complete.

Use bounded streaming readers and writers; never materialize the entire archive. Extend blob verification to stream
multipart content where the current whole-blob API would require excessive memory. Preflight conservative staging
space and still handle disk/quota exhaustion at every write. Insufficient space must not trigger in-place fallback.

## 8. Readers, mounts, and recovery

The first version is an **offline maintenance operation for the affected store**. A store containing several tasks
will temporarily block all of them; separate stores remain independent.

Add a shared/exclusive maintenance lease on an open store-directory descriptor on supported local Linux
filesystems. Ordinary readers, writers, and mounts hold it shared; deletion and recovery need it exclusively.
The directory inode stays stable because publication replaces the manifest, not the whole store directory.
Verify cross-process lock behavior and directory syncing on the target filesystem; refuse deletion where those
guarantees are unavailable. Do not fall back to the existing `.lock` alone.

Acquire locks in one order: maintenance lease, then ordinary store lock. Mounts retain their maintenance lease but
still bypass the ordinary reader lock, so normal append ingestion can continue while mounted. Keep unlocked
snapshot construction behind a retained lease or writer guard; no bypass may outlive its guard.
Public openers reject private transaction staging directories; candidate readers use the operation's internal guard.

Deletion refuses an active mount with an actionable unmount message. It does not kill readers, force-unmount,
or reclaim packs behind open handles. Initial adoption is also offline: old binaries and external readers that do
not participate in this protocol must be stopped first. This is advisory coordination, not protection against
arbitrary filesystem edits.

Every opener and writer must inspect a pending deletion journal **before ordinary orphan cleanup**. After a crash,
block normal operations until explicit recovery. Recovery acquires the same locks and compares the actual manifest
digest with the journal, rather than trusting a possibly stale phase label.

| Observed state | Recovery action |
|---|---|
| Intent or partial candidate; old manifest still published | Verify the old snapshot; discard only transaction-owned candidates. Report that deletion was not applied. |
| Prepared candidate installed; old manifest still published | Same safe abort path. Do not publish a deletion merely because candidate files exist. |
| New manifest published; old artifacts remain | Verify the new snapshot and finish the exact obsolete-inventory cleanup. |
| Cleanup interrupted | Resume idempotently; a previously removed authorized obsolete path is already complete. |
| Expected published artifact missing/corrupt, or manifest matches neither digest | Stop, preserve evidence, and require repair. Never guess which files are safe to delete. |

Once the new manifest is published, recovery moves forward; it does not attempt a partial rollback after old files
may have been removed. A cleanup or sync failure is an explicit incomplete operation, not success with a warning.
Keep the journal for recovery. Never restore the entire Git worktree as an automatic error handler.

## 9. Prevent resurrection and preserve Git history

`pack` and `watch` must honor deletion records before hashing or deriving task-owned inputs. Deletion suppression
overrides retention rules, including `--rules none` and `always_keep`. Keep raw inputs intact, but report that their
deleted execution is intentionally excluded.

Use task-aware readiness identities. Deleted-task markers must not consume another task's readiness label, produce
endless empty batches, or block archival of surviving tasks. A new execution of the same problem needs a new ID and
an unambiguous boundary; changing an adapter or reusing a directory must not clear a deletion record.

Filesystem publication and Git commit are separate steps. The existing checkpoint protects history before apply;
after apply, `traj commit` creates a forward commit containing only the selected store's replacements and removals.
Do not invoke it while still holding exclusive deletion locks: both commit and its hook open the store as readers.
If committing later fails, leave a valid deleted working tree and report the Git failure, rather than undoing the
deletion or modifying unrelated staging. Do not push automatically.

Extend the Git hook to recognize format-3 published tables and validate the complete staged inventory. Transaction
journals, staging directories, missing segments, and unreferenced finalized artifacts are not valid committed
stores. Teach commit summaries to describe maintenance instead of pretending another ingest batch occurred.

Earlier versions remain available through Git. Inspect or recover them in a separate worktree. Restoring a deleted
execution to the live archive is an explicit, quiesced operation, not an automatic retry or history rewrite.
Repacking can increase Git repository size even while reducing the current archive: old objects are deliberately kept.

## 10. Implementation order

Keep implementation in Rust and split the non-trivial deletion feature into focused modules.

| Phase | Work |
|---|---|
| Ownership | Add task identity, inventory, and ownership types; extend declared adapters; provide explicit legacy adoption and validation. |
| Format and readers | Add format 3, declared task/deletion tables, persistent allocation counters, and maintenance leases. Update manifest opening, verification, SQL, mounts, and the hook together. |
| Planner | Add `crates/trajfs-core/src/deletion/plan.rs` and a thin CLI `delete-task` command. Implement complete preview, Git checkpoint checks, and stale-plan rejection first. |
| Rewriter | Add focused `rewrite.rs` and `verify.rs` modules. Reuse pack/catalog primitives, but add fresh-name allocation, bounded streaming, and exact event filtering. |
| Transaction | Add `journal.rs` and `recover.rs`. Establish durable ordering and failure outcomes before enabling application. |
| Integration | Wire deletion suppression into ingestion/readiness, expose pending operations in `doctor`, update commit messages, command help, the exported skill, and documentation. |

Do not expose a destructive command before ownership, recovery, and reader coordination are complete.
Do not broaden this work into generic file deletion, background garbage collection, multi-store transactions,
secure erasure, or a new agent runtime.

## 11. Required evidence before release

Use the existing Rust test infrastructure and synthetic fixtures. Add task-aware fixtures rather than depending on
one private runner's data.

| Case | Required outcome |
|---|---|
| Two tasks, several agents and rounds | Removing one execution removes all its rounds and no part of the other. |
| Partial task without a terminal marker | The whole identified execution can be removed. |
| Round/log/path selector, unknown ID | Refused without modifying the store. |
| Repeated task names with different execution IDs | Only the selected execution changes. |
| Unclassified, overlapping, reused, or fragmented ownership | Refused; no guessed boundary or partial-task deletion. |
| Shared blobs and historical survivor versions | Required bytes remain readable; all surviving history is preserved. |
| File/directory type changes across batches | No non-target namespace resurrection or disappearance. |
| Incremental and rebuilt events | Preserve all non-target event rows and payloads, without reparsing or collapsing history. |
| Empty files, symlinks, large multipart blobs, segmented tables | Preserve content semantics and all physical size bounds. |
| Source corruption, stale plan, changed adapter/configuration | Refused before publication. |
| Active SQL/readers/mounts or another writer | Busy refusal; no forced eviction and no partial output. |
| Crash, write error, disk full, or sync failure at each transaction boundary | Old or new validated snapshot after recovery, never a mixture or false success. |
| Symlink/path replacement or unexpected files during cleanup | No unlink outside the verified obsolete inventory. |
| Last task removed | Valid empty store, browsable virtual root, zero root size/count summaries, empty schema-bearing views, and durable suppression metadata. |
| Subsequent pack/watch, including identical round labels in another task | Deleted execution stays absent; other tasks continue archiving normally. |
| Git checkpoint and deletion commit | Old task data remains retrievable from Git; current tracked artifacts are correct; unrelated staging/worktree changes survive. |
| Repeated apply/recovery | Explicit idempotent result, with no duplicate deletion records or spurious batches. |

The release criterion is not just "the task disappeared." It is: **the whole selected task disappeared from the
current archive, every other task remained correct, Git kept the earlier version, and every interrupted operation
has a deterministic recovery path.**
