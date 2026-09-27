# Refinement plan and status

Three layers, each a state machine with an invariant, connected by abstraction functions. A
transition of a lower layer must map to a transition (or a stutter) of the layer above, so every
guarantee proved at L0 holds of the real store. Verified executable code sits under L1/L2 and is
proved against the layer it implements.

```
L0  abstract_store   namespace: Map<Path, Entry>, deleted: Set<Path>          spec + invariant + promises
        ^ interp
L1  catalog          batches: Seq<Seq<Row>>, blobs: Map<Sha, bytes>, deleted   pack / delete refine L0
        ^ (planned) decode
L2  durable          artifacts + manifest, durable vs volatile, crash          publish protocol is crash-safe
        ^
exec namespace       Vec<(Vec<u8>, Slot)> shadowing insert                     == L0 insert_shadowing
```

## Status

| Obligation | File | Status |
|---|---|---|
| L0 invariant holds initially and is preserved by `pack` and `delete` | `spec/abstract_store.rs` | proved |
| L0 read-back after `pack`; subtree removal after `delete` | `spec/abstract_store.rs` | proved |
| L1 `pack` refines L0 `pack` and preserves the L1 invariant | `spec/catalog.rs::pack_refines` | proved |
| L1 `delete` preserves the L1 invariant | `spec/catalog.rs::delete_inv` | proved |
| L1 `delete` refines L0 `delete` under `no_resurrection` | `spec/catalog.rs::delete_refines` | **stated, admitted** (`admit()`); see SPEC.md §7 |
| L2 publish protocol: invariant preserved by every step including crash | `spec/durable.rs::step_inv` | proved |
| L2 crash safety: after a crash the visible snapshot is old or new, and complete | `spec/durable.rs::crash_safe` | proved |
| L2 durability once the directory fsync follows the rename | `spec/durable.rs::publish_durable` | proved |
| exec shadowing insert equals L0 `insert_shadowing` pointwise | `exec/namespace.rs::insert` | proved |
| exec byte-string ancestor test equals `is_strict_ancestor` | `exec/namespace.rs::is_strict_ancestor_exec` | proved |

Run `verify/run.sh`; the last line reports the count. `admit()` appears exactly once, in
`delete_refines`, and `grep -n admit verify/src` is the audit.

## Abstraction functions

- **L1 to L0** (`catalog::interp`): fold the batches in order; each row becomes `(path, Entry {
  kind, mode, content: blobs[sha] })`; the fold step is L0's `apply_batch` with no deleted set
  (L1 keeps no rows under deleted paths, an L1 invariant). The deleted set is the manifest's list.
- **L2 to L1** (planned, `decode`): the artifacts the manifest declares, decoded: files segments
  in batch order give `batches`; index segments plus pack frames give `blobs` (sha to bytes);
  the manifest gives `deleted`. Its proof obligations are the codec round trips, which are where
  the trusted Parquet and zstd libraries enter. The plan is to keep those libraries trusted and
  verify TrajFS's own framing: pack magic and frame table, part assembly (`Loc`), segment
  splitting and the size bound.

## Refinement obligations, in the order they are worth doing next

1. **`delete_refines`.** Decide SPEC.md §7 first. If the implementation is changed to drop
   shadowed ancestor rows, the L1 `delete` gains a second filter and the lemma loses its side
   condition; the proof is an induction over batches with a per-batch commutation of the row
   filter and the shadowing fold.
2. **Namespace fold as executable code.** `exec/namespace.rs::insert` covers one insertion;
   `files_under` in `store.rs` is that insertion folded over the scanned rows. Verify the fold
   loop against `apply_batch`, then the `children` and `stat` queries against `read`.
3. **Pack index and part assembly.** `PackReader::blob(parts)` concatenates frame slices in part
   order; specify `Loc` validity (parts contiguous, offsets inside frames) and prove the
   assembled bytes equal the blob when the index rows are consistent. The sha check on read then
   gives the integrity promise: what `cat` returns hashes to the catalog's sha.
4. **L2 decode.** Connect the manifest and artifact model to L1 through the codec round trips,
   with Parquet and zstd as trusted external bodies.
5. **Delete's swap protocol.** Extend `durable.rs` with `renameat2(RENAME_EXCHANGE)` and the
   candidate directory, and prove the same crash-safety shape for `traj delete`.
6. **Ingest's change check.** The per-file snapshot (inode, size, prefix hash) and the
   "abort the batch if a source changed" rule, as a refinement of `pack`'s precondition that the
   batch's entries are the source's bytes.

## Method

- Specs are `open spec fn`s over `Seq`, `Map`, and `Set` from vstd; transitions are predicates
  `step(pre, post, args)`; invariants are `spec fn`s with `proof fn` preservation lemmas.
- Refinement lemmas have the shape `requires inv(pre), step_L1(pre, post, ...) ensures
  step_L0(interp(pre), interp(post), ...)`.
- Executable code is verified in Verus's exec mode with loop invariants; its postconditions are
  stated pointwise against the L0 spec functions so no finite-map machinery is needed at the
  boundary. I/O, hashing, compression, and Parquet stay outside Verus behind trusted
  `#[verifier::external_body]` signatures, listed in SPEC.md §6.
- Nothing is `assume`d in exec code; `admit()` is used only to state an obligation whose proof
  is pending, and every such use is listed in the status table above.
