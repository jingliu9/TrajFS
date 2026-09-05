# What is one TrajFS store?

**Today, a store contains the tree passed to `traj pack`, plus later batches appended to that store.**
It is not automatically one task, one agent, one round, or an entire experiment campaign.

For new archives, prefer **one task execution per store**. Keep all of that execution's agents and rounds together.
A larger run can then be a collection of task stores.

## The units are different

| Unit | Meaning |
|---|---|
| Task | A problem or objective. The same task can be attempted more than once. |
| Task execution / experiment | One attempt at that task, with a start and an eventual outcome or stop. An unfinished attempt is still one execution. |
| Run | A container that may include several task executions. |
| Round | One iteration of agent work within an execution. It is not independently disposable under the proposed deletion policy. |
| Store | A `.trajstore` directory holding the archived paths, content, metadata, and batch history for a chosen source tree. |
| Batch | One capture appended by `pack`. A batch can contain changes from several rounds or tasks if the source tree contains them. |

A store has one manifest and its declared packs, catalogs, and optional event tables. File paths are logical entries
in that catalog. Identical file contents share a content hash and stored bytes **within the store**.

A physical `.pack` file is not a task or a round: its compressed frames can contain data from many logical files.
The default 60 MiB artifact target limits individual generated files, not the total store or its source tree.

## Choose the source root deliberately

Suppose one run contains two task executions:

```text
run-42/
  tasks/
    task-a-0001/
      rounds/round-0001/
      rounds/round-0002/
    task-b-0001/
      rounds/round-0001/
```

Packing `run-42/` creates one store containing both tasks. Packing each task directory creates two independent
stores. The command works at either boundary; it does not infer which boundary you intended.

For task-sized stores, a manual example is:

```bash
traj pack /data/runs/run-42/tasks/task-a-0001 \
  --out stores/run-42-task-a-0001.trajstore \
  --id run-42-task-a-0001 --rules none

traj pack /data/runs/run-42/tasks/task-b-0001 \
  --out stores/run-42-task-b-0001.trajstore \
  --id run-42-task-b-0001 --rules none
```

`--out` chooses the destination, and `--id` names the store. Neither changes the source boundary. Use distinct IDs
for distinct executions, including retries. Choose an adapter before the first pack if parsed events are needed.

In a configured repository, the adapter's `run_glob` selects the directories that `watch` passes to `pack`.
Despite its name, it can select task directories:

```toml
[batch_ready]
run_glob = "run-*/tasks/*"
markers = ["rounds/round-*/DONE"]
```

Here, `data_root` is the directory containing `run-42/`. Each matching task directory becomes a store, and a round
marker triggers another capture. That marker does **not** define the task's completion or deletion boundary.

Manual packing without `--out` and watching use the configured, data-root-relative destination naming. Explicit
`--out` names are independent of that convention; do not accidentally create a second archive by mixing naming
schemes. The physical destination and the manifest's display ID are also distinct concepts.

## How the boundary affects behavior

| Concern | Effect of store granularity |
|---|---|
| Deduplication | Repeated files across rounds share bytes when those rounds are in one store. Separate stores do not share a blob pool. Grouping tasks can save cross-task duplicates, but couples their storage. |
| Ingestion | Each pack traverses and checks the selected source tree. Incremental storage avoids rewriting known blobs; it does not make source scanning free. A whole-run source also revisits the other tasks. |
| Browsing and search | Native commands operate on the selected store. Smaller stores narrow the catalog and distinct-content search scope. Mounted recursive tools still visit logical paths. |
| Cross-task analysis | Separate stores can be queried together with repeated `-S` arguments to SQL. A `store` column identifies their records; unique store IDs keep those labels useful. |
| Concurrency | Ordinary readers and writers coordinate per store. Grouped tasks share that contention boundary. Separate stores can ingest independently, although Git commits still share one repository index. |
| Git | `traj commit` commits the selected store's physical artifacts, not individual source paths. Larger stores can produce broader catalog or pack changes during maintenance. |
| Sharing and retention | A task store is easy to copy or share without bundling unrelated tasks. Separate lifetimes and retention needs favor separate stores. |
| Deletion, as planned | The deletion unit is the task execution, regardless of storage layout. A grouped store must preserve and rewrite the surviving tasks correctly; a task-sized store has much less surviving data to rewrite. |

Store granularity is not a hard latency or memory guarantee. Query shape, catalog history, distinct content,
cache state, and concurrent use still matter.

## Recommended default and alternatives

| Layout | When it makes sense | Main cost |
|---|---|---|
| One task execution per store | Default for independently completed, shared, or discarded experiments. Keep all agents and rounds together. | Identical content in different task stores is stored separately. |
| Several tasks per store | Tasks intentionally share a storage and maintenance lifetime, or cross-task duplication is worth the coupling. | Task deletion needs explicit ownership and can require rewriting the larger archive; maintenance affects every task in it. |
| One round, agent, or log per store | A specialized layout requiring an explicit higher-level grouping mechanism. | Fragments a task, loses cross-round deduplication, and makes task-atomic deletion a multi-store problem. Not the recommended default. |

This recommendation is a convention, **not a boundary the current CLI enforces**. The current adapter identifies
event-log formats and path attributes, not authoritative task ownership.

Appending a smaller subtree to an existing grouped store does not split it or remove the other tasks. Source
disappearance and changed exclusion rules also do not purge earlier captures. Changing granularity requires an
explicit migration into new stores, with verification and a Git checkpoint.

## Relationship to deletion

The [deletion plan](PLAN-deletion.md) adds a stable task-execution identity and a complete ownership map.
It must not mistake "one store" for "one task," or "one round" for an independently deletable experiment.

For a single-task store, deleting its payloads can leave a tiny valid store envelope and a deletion record. For a
grouped store, deletion removes that task's records across all batches and retains every other task's history and
shared content. The first version refuses an execution fragmented across stores rather than deleting only one part.

Git history remains in either layout. Deleting or repacking current artifacts does not shrink earlier Git history;
rewriting a large grouped store can add new Git objects while old versions remain.

## Implementation references

Current behavior is defined by [pack/watch destination selection](../crates/traj/src/cmd/pack.rs),
[ingestion](../crates/trajfs-core/src/ingest.rs), the [manifest](../crates/trajfs-core/src/manifest.rs),
[store readers and locks](../crates/trajfs-core/src/store.rs),
[multi-store SQL](../crates/traj/src/cmd/sql.rs), and [scoped Git commits](../crates/traj/src/cmd/commit.rs).
