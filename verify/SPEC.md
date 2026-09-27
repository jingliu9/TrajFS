# TrajFS: the high-level specification

This is the document to review first. It states, in prose, what the Verus files under `src/spec/`
say formally, and it names every assumption the proofs rest on. The formal text is authoritative;
this page exists so the choices can be discussed without reading Verus.

## 1. What a store is

A store is a **namespace**: a finite map from catalog paths to entries.

- A *path* is a byte string like `rounds/round-0007/reviewer/review.json`: non-empty, no leading
  or trailing slash. `a` is a *strict ancestor* of `b` when `b` starts with `a` followed by `/`.
  (`src/spec/paths.rs`)
- An *entry* is a kind (regular file, empty file, symlink), a canonical mode (0644/0755 for files,
  0777 for symlinks), and the content bytes (the file's bytes, or the symlink's target).
  (`src/spec/abstract_store.rs`, `Entry`)
- The store also remembers the set of *deleted* paths.

Everything a reader can observe is a function of this map: `traj ls`, `cat`, `find`, `grep`,
`stat`, the FUSE mount, `extract`, and the `files` view in SQL. Batches, packs, catalog
segments, hashes, Parquet, and the manifest are implementation and do not appear at this level.

## 2. Operations

**pack(batch).** A batch is a sequence of (path, entry) pairs: the retained files of one walk of
the source tree, in path order. The batch is applied by inserting each pair, in order, under the
*shadowing rule*:

> Inserting path `p` removes every entry at a strict ancestor of `p` (that ancestor used to be a
> file and is now a directory) and every entry at a strict descendant of `p` (that path used to be
> a directory and is now a file), then binds `p`.

Paths absent from the batch keep their previous entry: **history is append-only**; a batch never
removes a path. Pairs at or below a deleted path are skipped.

**delete(p).** Removes every entry at or below `p` and records `p`, so later packs skip it. This
is the only operation that removes anything.

**read(p).** The entry at `p`, or nothing.

## 3. Invariant

At every reachable state: no present path is a strict ancestor of another present path (a path is
never both a file and a directory), and every present path is well formed. Proved to hold
initially and to be preserved by both operations (`pack_inv`, `delete_inv`).

## 4. What the operations promise (proved)

- **Read-back.** After `pack(batch)`, for a batch whose paths are pairwise compatible (what a real
  walk produces), every pair not under a deleted path reads back with exactly its entry
  (`pack_reads_back`).
- **Deletion.** After `delete(p)`, nothing at or below `p` reads back, and every other path reads
  as before (`delete_removes_subtree`).

## 5. What is deliberately not in the specification

- **Attributes, mtimes, sizes.** Recorded by the catalog; the spec keeps kind, mode, and bytes.
  Adding `mtime` and adapter attributes to `Entry` is a mechanical extension.
- **Events and SQL.** Derived tables are a rebuildable function of the trajectory bytes, which are
  entries in the namespace. Their correctness is a separate specification of each adapter's parser.
- **Retention rules and the walk.** Which files of a source tree enter a batch is policy; the spec
  starts from the batch.
- **Concurrency.** One writer at a time (the exclusive lock); readers see published snapshots. The
  publish protocol is L2's subject.

## 6. Trusted assumptions

Each lower layer states what it assumes; the list is short and each item is a real-world fact,
not a proof obligation that was skipped:

| Assumption | Where it is used |
|---|---|
| SHA-256 is collision-free on the contents a store will hold: two rows with the same sha have the same bytes. | L1 `pack` requires it as a precondition (`blobs_extended`); the implementation deduplicates by sha alone. |
| `fsync` makes the named file or directory durable; `rename` within one directory is atomic and, after the directory fsync, durable. | L2 protocol steps. |
| The manifest names artifacts unambiguously and readers use only declared artifacts. | L2 `visible`. |
| Parquet, zstd, and the pack frame codec decode what they encoded. | Below L2 (not modelled yet). |

## 7. Open question for review

The L1 model of `delete` matches the implementation (drop the rows at or below `p` from every
batch). It refines the L0 `delete` above only under a side condition, `no_resurrection`: no file
row at a strict ancestor of `p` precedes, in fold order, a row at or below `p`. Without it, the
row filter can make an older, previously shadowed ancestor file visible again. Example:

1. batch 1 packs a file `a`;
2. batch 2 packs `a/b/c` (so `a` is now a directory and the old file row is shadowed);
3. `delete a/b` drops `a/b/c`; the fold now shows the batch-1 file `a` again.

The L0 `delete` says the namespace outside `a/b` is unchanged, so `a` stays absent. Two ways to
close the gap: make the rebuild also drop shadowed ancestor rows of `p` (a change to
`crates/trajfs-core/src/delete.rs`, keeping L0 as the intended semantics), or weaken L0 `delete`
to the re-fold semantics. The first is recommended; the second makes `delete` depend on history.
See REFINEMENT.md for status.
