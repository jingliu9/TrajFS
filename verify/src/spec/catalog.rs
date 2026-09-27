//! L1: the content-addressed model, one level below the namespace.
//!
//! A store is a sequence of batches, each a sequence of catalog rows (path, kind, mode, size,
//! sha), plus a blob map from sha to bytes, plus the list of deleted paths. This is exactly what
//! `catalog/files-*.parquet`, `packs/*.pack` with `packs/index-*.parquet`, and the manifest's
//! `deleted` list represent, with the physical encoding (Parquet, zstd frames, segment splitting)
//! left to L2.
//!
//! Abstraction to L0 (`interp`): fold every batch's rows in batch order through the shadowing
//! insert of L0; a row's entry is its kind and mode with the blob its sha names.
//!
//! Transitions:
//! - `pack(rows)`: append one batch. New shas bring their bytes; a sha already in the blob map is
//!   reused *only if* its bytes equal the row's bytes. That equality is what the implementation
//!   assumes when it deduplicates by SHA-256 alone; it is the one place the model relies on hash
//!   collision resistance, and it is stated as a precondition rather than proved.
//! - `delete(p)`: drop the rows at or below `p` from every batch and record `p`.
//!
//! Refinement: `pack` at L1 is `pack` at L0 on the interpreted states (proved). `delete` at L1 is
//! `delete` at L0 *provided* no file row at a strict ancestor of `p` precedes a row at or below
//! `p` (`no_resurrection`); without it the row filter can make an older, shadowed ancestor file
//! visible again, which the L0 `delete` (a plain restriction of the namespace) never does.
//! That gap is a finding of this work, recorded in REFINEMENT.md.

use vstd::prelude::*;
use crate::spec::paths::*;
use crate::spec::abstract_store::{self, AbstractStore, Entry, Kind};

verus! {

pub type Sha = Seq<u8>;

pub struct Row {
    pub path: Path,
    pub kind: Kind,
    pub mode: u16,
    pub sha: Sha,
}

pub struct Catalog {
    pub batches: Seq<Seq<Row>>,
    pub blobs: Map<Sha, Seq<u8>>,
    pub deleted: Seq<Path>,
}

// ------------------------------------------------------------------ abstraction to L0

pub open spec fn entry_of(blobs: Map<Sha, Seq<u8>>, r: Row) -> Entry {
    Entry {
        kind: r.kind,
        mode: r.mode,
        content: if r.kind == Kind::Empty { Seq::<u8>::empty() } else { blobs[r.sha] },
    }
}

pub open spec fn pairs_of(blobs: Map<Sha, Seq<u8>>, rows: Seq<Row>) -> Seq<(Path, Entry)> {
    rows.map(|i: int, r: Row| (r.path, entry_of(blobs, r)))
}

/// The namespace after folding the first `n` batches.
pub open spec fn fold_batches(blobs: Map<Sha, Seq<u8>>, batches: Seq<Seq<Row>>, n: int) -> Map<Path, Entry>
    decreases n,
{
    if n <= 0 {
        Map::<Path, Entry>::empty()
    } else {
        abstract_store::apply_batch(
            fold_batches(blobs, batches, n - 1),
            Set::<Path>::empty(),
            pairs_of(blobs, batches[n - 1]),
        )
    }
}

pub open spec fn deleted_set(deleted: Seq<Path>) -> Set<Path> {
    deleted.to_set()
}

pub open spec fn interp(c: Catalog) -> AbstractStore {
    AbstractStore {
        view: fold_batches(c.blobs, c.batches, c.batches.len() as int),
        deleted: deleted_set(c.deleted),
    }
}

// ------------------------------------------------------------------ invariant

/// Every non-empty row's sha is in the blob map; no row lies at or below a deleted path; paths are
/// well formed.
pub open spec fn inv(c: Catalog) -> bool {
    &&& forall|i: int, j: int| 0 <= i < c.batches.len() && 0 <= j < c.batches[i].len()
            ==> (#[trigger] c.batches[i][j]).kind == Kind::Empty || c.blobs.contains_key(c.batches[i][j].sha)
    &&& forall|i: int, j: int| 0 <= i < c.batches.len() && 0 <= j < c.batches[i].len()
            ==> !abstract_store::is_deleted(deleted_set(c.deleted), (#[trigger] c.batches[i][j]).path)
    &&& forall|i: int, j: int| 0 <= i < c.batches.len() && 0 <= j < c.batches[i].len()
            ==> well_formed((#[trigger] c.batches[i][j]).path)
}

// ------------------------------------------------------------------ transitions

/// The blob map after a batch: new shas bring their bytes, known shas must agree (the
/// collision-resistance assumption, made explicit).
pub open spec fn blobs_extended(pre: Map<Sha, Seq<u8>>, post: Map<Sha, Seq<u8>>, rows: Seq<Row>, contents: Seq<Seq<u8>>) -> bool {
    &&& contents.len() == rows.len()
    &&& forall|j: int| 0 <= j < rows.len() && (#[trigger] rows[j]).kind != Kind::Empty
            ==> post.contains_key(rows[j].sha) && post[rows[j].sha] == contents[j]
    &&& forall|s: Sha| #[trigger] pre.contains_key(s) ==> post.contains_key(s) && post[s] == pre[s]
    &&& forall|s: Sha| #[trigger] post.contains_key(s) ==> pre.contains_key(s) || exists|j: int| 0 <= j < rows.len() && (#[trigger] rows[j]).sha == s
}

pub open spec fn pack(pre: Catalog, post: Catalog, rows: Seq<Row>, contents: Seq<Seq<u8>>) -> bool {
    &&& forall|j: int| 0 <= j < rows.len() ==> well_formed((#[trigger] rows[j]).path)
    &&& forall|j: int| 0 <= j < rows.len() ==> !abstract_store::is_deleted(deleted_set(pre.deleted), (#[trigger] rows[j]).path)
    &&& blobs_extended(pre.blobs, post.blobs, rows, contents)
    &&& post.batches == pre.batches.push(rows)
    &&& post.deleted == pre.deleted
}

/// The rows `delete(p)` keeps: those not at or below `p`. A named predicate, so the filter term is
/// the same in the definition and in the lemmas about it.
pub open spec fn keep_row(p: Path) -> spec_fn(Row) -> bool {
    |r: Row| !at_or_below(p, r.path)
}

pub open spec fn filter_rows(rows: Seq<Row>, p: Path) -> Seq<Row> {
    rows.filter(keep_row(p))
}

pub open spec fn delete(pre: Catalog, post: Catalog, p: Path) -> bool {
    &&& well_formed(p)
    &&& post.batches.len() == pre.batches.len()
    &&& forall|i: int| 0 <= i < pre.batches.len() ==> #[trigger] post.batches[i] == filter_rows(pre.batches[i], p)
    &&& post.deleted == pre.deleted.push(p)
    &&& forall|s: Sha| #[trigger] post.blobs.contains_key(s) ==> pre.blobs.contains_key(s) && post.blobs[s] == pre.blobs[s]
    &&& forall|i: int, j: int| 0 <= i < post.batches.len() && 0 <= j < post.batches[i].len()
            ==> (#[trigger] post.batches[i][j]).kind == Kind::Empty || post.blobs.contains_key(post.batches[i][j].sha)
}

// ------------------------------------------------------------------ refinement: pack

/// Extending the blob map does not change the entries of rows whose shas it already held.
proof fn pairs_stable(pre: Map<Sha, Seq<u8>>, post: Map<Sha, Seq<u8>>, rows: Seq<Row>)
    requires
        forall|s: Sha| #[trigger] pre.contains_key(s) ==> post.contains_key(s) && post[s] == pre[s],
        forall|j: int| 0 <= j < rows.len() ==> (#[trigger] rows[j]).kind == Kind::Empty || pre.contains_key(rows[j].sha),
    ensures
        pairs_of(pre, rows) == pairs_of(post, rows),
{
    assert(pairs_of(pre, rows) =~= pairs_of(post, rows));
}

/// The fold over the first `n` batches is the same under an extended blob map.
proof fn fold_stable(pre: Map<Sha, Seq<u8>>, post: Map<Sha, Seq<u8>>, batches: Seq<Seq<Row>>, n: int)
    requires
        forall|s: Sha| #[trigger] pre.contains_key(s) ==> post.contains_key(s) && post[s] == pre[s],
        forall|i: int, j: int| 0 <= i < batches.len() && 0 <= j < batches[i].len()
            ==> (#[trigger] batches[i][j]).kind == Kind::Empty || pre.contains_key(batches[i][j].sha),
        n <= batches.len(),
    ensures
        fold_batches(pre, batches, n) == fold_batches(post, batches, n),
    decreases n,
{
    if n > 0 {
        fold_stable(pre, post, batches, n - 1);
        assert forall|j: int| 0 <= j < batches[n - 1].len() implies (#[trigger] batches[n - 1][j]).kind == Kind::Empty || pre.contains_key(batches[n - 1][j].sha) by {
            assert(batches[n - 1][j] == batches[n - 1][j]);
        }
        pairs_stable(pre, post, batches[n - 1]);
    }
}

/// The fold of `batches.push(b)` is the fold of `batches` with `b` applied.
proof fn fold_push(blobs: Map<Sha, Seq<u8>>, batches: Seq<Seq<Row>>, b: Seq<Row>, n: int)
    requires
        0 <= n <= batches.len(),
    ensures
        fold_batches(blobs, batches.push(b), n) == fold_batches(blobs, batches, n),
    decreases n,
{
    if n > 0 {
        fold_push(blobs, batches, b, n - 1);
        assert(batches.push(b)[n - 1] == batches[n - 1]);
    }
}

/// Skipping deleted paths is a no-op on a batch that has none.
proof fn apply_batch_no_deleted(view: Map<Path, Entry>, deleted: Set<Path>, batch: Seq<(Path, Entry)>)
    requires
        forall|j: int| 0 <= j < batch.len() ==> !abstract_store::is_deleted(deleted, (#[trigger] batch[j]).0),
    ensures
        abstract_store::apply_batch(view, deleted, batch) == abstract_store::apply_batch(view, Set::<Path>::empty(), batch),
    decreases batch.len(),
{
    if batch.len() > 0 {
        let (p, e) = batch[0];
        let rest = batch.subrange(1, batch.len() as int);
        assert(!abstract_store::is_deleted(deleted, batch[0].0));
        assert(!abstract_store::is_deleted(Set::<Path>::empty(), p));
        assert forall|j: int| 0 <= j < rest.len() implies !abstract_store::is_deleted(deleted, (#[trigger] rest[j]).0) by {
            assert(rest[j] == batch[j + 1]);
        }
        apply_batch_no_deleted(abstract_store::insert_shadowing(view, p, e), deleted, rest);
    }
}

/// L1 `pack` refines L0 `pack` with the batch's interpreted pairs.
pub proof fn pack_refines(pre: Catalog, post: Catalog, rows: Seq<Row>, contents: Seq<Seq<u8>>)
    requires
        inv(pre),
        pack(pre, post, rows, contents),
    ensures
        abstract_store::pack(interp(pre), interp(post), pairs_of(post.blobs, rows)),
        inv(post),
{
    let n = pre.batches.len() as int;
    let pairs = pairs_of(post.blobs, rows);
    // batch paths are well formed
    assert(abstract_store::batch_well_formed(pairs)) by {
        assert forall|i: int| 0 <= i < pairs.len() implies well_formed(#[trigger] pairs[i].0) by {
            assert(pairs[i].0 == rows[i].path);
        }
    }
    // the view: fold of pre's batches under post's blobs, then the new batch
    fold_stable(pre.blobs, post.blobs, pre.batches, n);
    fold_push(post.blobs, pre.batches, rows, n);
    assert(post.batches[n] == rows);
    assert(fold_batches(post.blobs, post.batches, n + 1)
        == abstract_store::apply_batch(fold_batches(post.blobs, post.batches, n), Set::<Path>::empty(), pairs));
    assert forall|j: int| 0 <= j < pairs.len() implies !abstract_store::is_deleted(deleted_set(pre.deleted), (#[trigger] pairs[j]).0) by {
        assert(pairs[j].0 == rows[j].path);
    }
    apply_batch_no_deleted(fold_batches(pre.blobs, pre.batches, n), deleted_set(pre.deleted), pairs);
    // the invariant of the new state, conjunct by conjunct
    assert forall|i: int, j: int| 0 <= i < post.batches.len() && 0 <= j < post.batches[i].len()
        implies (#[trigger] post.batches[i][j]).kind == Kind::Empty || post.blobs.contains_key(post.batches[i][j].sha) by {
        if i < n {
            assert(post.batches[i] == pre.batches[i]);
            assert(pre.batches[i][j].kind == Kind::Empty || pre.blobs.contains_key(pre.batches[i][j].sha));
        } else {
            assert(post.batches[i] == rows);
            assert(rows[j].kind == Kind::Empty || post.blobs.contains_key(rows[j].sha));
        }
    }
    assert forall|i: int, j: int| 0 <= i < post.batches.len() && 0 <= j < post.batches[i].len()
        implies !abstract_store::is_deleted(deleted_set(post.deleted), (#[trigger] post.batches[i][j]).path) by {
        if i < n {
            assert(post.batches[i] == pre.batches[i]);
            assert(!abstract_store::is_deleted(deleted_set(pre.deleted), pre.batches[i][j].path));
        } else {
            assert(post.batches[i] == rows);
            assert(!abstract_store::is_deleted(deleted_set(pre.deleted), rows[j].path));
        }
    }
    assert forall|i: int, j: int| 0 <= i < post.batches.len() && 0 <= j < post.batches[i].len()
        implies well_formed((#[trigger] post.batches[i][j]).path) by {
        if i < n {
            assert(post.batches[i] == pre.batches[i]);
            assert(well_formed(pre.batches[i][j].path));
        } else {
            assert(post.batches[i] == rows);
            assert(well_formed(rows[j].path));
        }
    }
}

// ------------------------------------------------------------------ refinement: delete

/// No file row at a strict ancestor of `p` precedes (in fold order) a row at or below `p`. When this
/// holds, dropping the rows at or below `p` cannot make a shadowed ancestor visible again.
pub open spec fn no_resurrection(c: Catalog, p: Path) -> bool {
    forall|i: int, j: int| 0 <= i < c.batches.len() && 0 <= j < c.batches[i].len()
        && is_strict_ancestor((#[trigger] c.batches[i][j]).path, p)
        ==> !(exists|i2: int, j2: int| 0 <= i2 < c.batches.len() && 0 <= j2 < c.batches[i2].len()
                && (i2 > i || (i2 == i && j2 > j))
                && at_or_below(p, (#[trigger] c.batches[i2][j2]).path))
}

/// L1 `delete` refines L0 `delete`, under `no_resurrection`.
///
/// Status: stated, not yet proved (see REFINEMENT.md). The proof is an induction over batches with
/// a per-batch commutation of the row filter and the shadowing fold; the side condition is exactly
/// what makes the commutation hold for ancestors of `p`.
pub proof fn delete_refines(pre: Catalog, post: Catalog, p: Path)
    requires
        inv(pre),
        delete(pre, post, p),
        no_resurrection(pre, p),
    ensures
        abstract_store::delete(interp(pre), interp(post), p),
{
    admit();
}

/// A row of the filtered batch is a row of the original batch, and is not at or below `p`.
proof fn filter_rows_origin(rows: Seq<Row>, p: Path, j: int)
    requires
        0 <= j < filter_rows(rows, p).len(),
    ensures
        exists|k: int| 0 <= k < rows.len() && rows[k] == filter_rows(rows, p)[j],
        !at_or_below(p, filter_rows(rows, p)[j].path),
{
    let x = filter_rows(rows, p)[j];
    rows.lemma_filter_pred(keep_row(p), j);
    assert(rows.filter(keep_row(p)).contains(x));
    rows.lemma_filter_contains_rev(keep_row(p), x);
}

/// L1 `delete` keeps the invariant (no side condition needed).
pub proof fn delete_inv(pre: Catalog, post: Catalog, p: Path)
    requires
        inv(pre),
        delete(pre, post, p),
    ensures
        inv(post),
{
    assert forall|i: int, j: int| 0 <= i < post.batches.len() && 0 <= j < post.batches[i].len()
        implies !abstract_store::is_deleted(deleted_set(post.deleted), (#[trigger] post.batches[i][j]).path)
            && well_formed(post.batches[i][j].path) by {
        assert(post.batches[i] == filter_rows(pre.batches[i], p));
        let r = post.batches[i][j];
        // a filtered row comes from the original batch and is not at or below p
        let rows = pre.batches[i];
        filter_rows_origin(rows, p, j);
        let k = choose|k: int| 0 <= k < rows.len() && rows[k] == filter_rows(rows, p)[j];
        assert(pre.batches[i][k] == r);
        assert(!abstract_store::is_deleted(deleted_set(pre.deleted), pre.batches[i][k].path));
        assert(well_formed(pre.batches[i][k].path));
        assert(post.deleted.to_set() =~= pre.deleted.to_set().insert(p)) by {
            assert forall|q: Path| post.deleted.to_set().contains(q) <==> pre.deleted.to_set().insert(p).contains(q) by {
                if post.deleted.contains(q) {
                    let m = choose|m: int| 0 <= m < post.deleted.len() && post.deleted[m] == q;
                    if m < pre.deleted.len() { assert(pre.deleted[m] == q); }
                }
                if pre.deleted.contains(q) {
                    let m = choose|m: int| 0 <= m < pre.deleted.len() && pre.deleted[m] == q;
                    assert(post.deleted[m] == q);
                }
                if q == p { assert(post.deleted[pre.deleted.len() as int] == p); }
            }
        }
        assert(!abstract_store::is_deleted(deleted_set(post.deleted), r.path)) by {
            if abstract_store::is_deleted(deleted_set(post.deleted), r.path) {
                let d = choose|d: Path| #[trigger] deleted_set(post.deleted).contains(d) && at_or_below(d, r.path);
                if d == p {
                    assert(!at_or_below(p, r.path));
                } else {
                    assert(deleted_set(pre.deleted).contains(d));
                }
            }
        }
    }
}

} // verus!
