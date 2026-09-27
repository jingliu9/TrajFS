//! L0: the high-level specification of a TrajFS store.
//!
//! A store *is* a namespace: a finite map from catalog paths to entries (a regular file's bytes and
//! mode, an empty file, or a symlink's target). Everything a reader can observe (`traj ls`, `cat`,
//! `find`, `grep`, the FUSE mount, `extract`) is a function of this map. Batches, packs, catalog
//! segments, hashes, and the manifest are implementation; they appear only in the lower layers.
//!
//! Operations:
//!
//! - `pack(batch)`: a batch is a sequence of (path, entry) pairs, the retained files of one source
//!   walk in path order. Applying it inserts each pair in order under the *shadowing rule*: a path
//!   that becomes a file removes every entry below it, and an entry removes every entry at an
//!   ancestor path (an ancestor that used to be a file is now a directory). Paths absent from the
//!   batch keep their previous entry: history is append-only, a batch never "removes" a path.
//!   Entries at or below a deleted path are skipped.
//! - `delete(p)`: removes every entry at or below `p` and records `p`, so later packs skip it.
//! - `read(p)`: the entry at `p`, when present.
//!
//! Invariant `inv`: no two present paths are ancestor and descendant (a path is never both a
//! file and a directory), and every present path is well formed.

use vstd::prelude::*;
use crate::spec::paths::*;

verus! {

#[derive(PartialEq, Eq)]
pub enum Kind {
    File,
    Symlink,
    Empty,
}

/// What a path maps to. `content` is the file's bytes, the symlink's target, or empty.
pub struct Entry {
    pub kind: Kind,
    /// Canonical mode: 0o644 or 0o755 for files, 0o777 for symlinks.
    pub mode: u16,
    pub content: Seq<u8>,
}

pub struct AbstractStore {
    /// The visible namespace.
    pub view: Map<Path, Entry>,
    /// Paths removed by `delete`; `pack` skips source entries at or below them.
    pub deleted: Set<Path>,
}

/// The paths that inserting `p` evicts: its strict ancestors and its strict descendants.
pub open spec fn shadowed_by(p: Path, q: Path) -> bool {
    is_strict_ancestor(q, p) || is_strict_ancestor(p, q)
}

/// One insertion under the shadowing rule (the specification of `insert_latest` in
/// crates/trajfs-core/src/store.rs).
pub open spec fn insert_shadowing(view: Map<Path, Entry>, p: Path, e: Entry) -> Map<Path, Entry> {
    view.restrict(view.dom().filter(|q: Path| !shadowed_by(p, q))).insert(p, e)
}

/// `p` is at or below some deleted path.
pub open spec fn is_deleted(deleted: Set<Path>, p: Path) -> bool {
    exists|d: Path| #[trigger] deleted.contains(d) && at_or_below(d, p)
}

/// Apply a batch: insert its pairs in order, skipping deleted paths.
pub open spec fn apply_batch(view: Map<Path, Entry>, deleted: Set<Path>, batch: Seq<(Path, Entry)>) -> Map<Path, Entry>
    decreases batch.len(),
{
    if batch.len() == 0 {
        view
    } else {
        let (p, e) = batch[0];
        let next = if is_deleted(deleted, p) { view } else { insert_shadowing(view, p, e) };
        apply_batch(next, deleted, batch.subrange(1, batch.len() as int))
    }
}

/// Every path in a batch is well formed.
pub open spec fn batch_well_formed(batch: Seq<(Path, Entry)>) -> bool {
    forall|i: int| 0 <= i < batch.len() ==> well_formed(#[trigger] batch[i].0)
}

// ------------------------------------------------------------------ transitions

pub open spec fn pack(pre: AbstractStore, post: AbstractStore, batch: Seq<(Path, Entry)>) -> bool {
    &&& batch_well_formed(batch)
    &&& post.view == apply_batch(pre.view, pre.deleted, batch)
    &&& post.deleted == pre.deleted
}

pub open spec fn delete(pre: AbstractStore, post: AbstractStore, p: Path) -> bool {
    &&& well_formed(p)
    &&& post.view == pre.view.restrict(pre.view.dom().filter(|q: Path| !at_or_below(p, q)))
    &&& post.deleted == pre.deleted.insert(p)
}

/// `read` observes without changing the store.
pub open spec fn read(s: AbstractStore, p: Path) -> Option<Entry> {
    if s.view.contains_key(p) { Some(s.view[p]) } else { None }
}

pub open spec fn init(s: AbstractStore) -> bool {
    s.view == Map::<Path, Entry>::empty() && s.deleted == Set::<Path>::empty()
}

// ------------------------------------------------------------------ invariant

/// No present path is an ancestor of another present path; every present path is well formed.
pub open spec fn inv(s: AbstractStore) -> bool {
    &&& forall|a: Path, b: Path| s.view.contains_key(a) && s.view.contains_key(b) ==> #[trigger] compatible(a, b) || a == b
    &&& forall|a: Path| s.view.contains_key(a) ==> #[trigger] well_formed(a)
}

pub proof fn init_inv(s: AbstractStore)
    requires
        init(s),
    ensures
        inv(s),
{
}

/// One shadowing insertion keeps the namespace consistent.
pub proof fn insert_shadowing_keeps_compatible(view: Map<Path, Entry>, p: Path, e: Entry)
    requires
        forall|a: Path, b: Path| view.contains_key(a) && view.contains_key(b) ==> #[trigger] compatible(a, b) || a == b,
    ensures
        forall|a: Path, b: Path| insert_shadowing(view, p, e).contains_key(a) && insert_shadowing(view, p, e).contains_key(b)
            ==> #[trigger] compatible(a, b) || a == b,
{
    let post = insert_shadowing(view, p, e);
    assert forall|a: Path, b: Path| post.contains_key(a) && post.contains_key(b) implies #[trigger] compatible(a, b) || a == b by {
        if a == p || b == p {
            // the other one survived the insertion, so it is not an ancestor or descendant of p
        } else {
            // both survived and were compatible before
        }
    }
}

/// Applying a batch preserves the invariant.
pub proof fn apply_batch_inv(view: Map<Path, Entry>, deleted: Set<Path>, batch: Seq<(Path, Entry)>)
    requires
        forall|a: Path, b: Path| view.contains_key(a) && view.contains_key(b) ==> #[trigger] compatible(a, b) || a == b,
        forall|a: Path| view.contains_key(a) ==> #[trigger] well_formed(a),
        batch_well_formed(batch),
    ensures
        forall|a: Path, b: Path| apply_batch(view, deleted, batch).contains_key(a) && apply_batch(view, deleted, batch).contains_key(b)
            ==> #[trigger] compatible(a, b) || a == b,
        forall|a: Path| apply_batch(view, deleted, batch).contains_key(a) ==> #[trigger] well_formed(a),
    decreases batch.len(),
{
    if batch.len() == 0 {
    } else {
        let (p, e) = batch[0];
        let rest = batch.subrange(1, batch.len() as int);
        assert(batch_well_formed(rest)) by {
            assert forall|i: int| 0 <= i < rest.len() implies well_formed(#[trigger] rest[i].0) by {
                assert(rest[i] == batch[i + 1]);
            }
        }
        if is_deleted(deleted, p) {
            apply_batch_inv(view, deleted, rest);
        } else {
            let next = insert_shadowing(view, p, e);
            insert_shadowing_keeps_compatible(view, p, e);
            assert(well_formed(batch[0].0));
            assert forall|a: Path| next.contains_key(a) implies #[trigger] well_formed(a) by {
                if a != p {
                    assert(view.contains_key(a));
                }
            }
            apply_batch_inv(next, deleted, rest);
        }
    }
}

pub proof fn pack_inv(pre: AbstractStore, post: AbstractStore, batch: Seq<(Path, Entry)>)
    requires
        inv(pre),
        pack(pre, post, batch),
    ensures
        inv(post),
{
    apply_batch_inv(pre.view, pre.deleted, batch);
}

pub proof fn delete_inv(pre: AbstractStore, post: AbstractStore, p: Path)
    requires
        inv(pre),
        delete(pre, post, p),
    ensures
        inv(post),
{
}

// ------------------------------------------------------------------ what the operations promise

/// After `pack`, every batch entry that was not shadowed by a later entry of the same batch, and is
/// not deleted, is readable with exactly the packed bytes. (Stated for the common case of a batch
/// without internal shadowing, which is what a walk of a real tree produces.)
pub open spec fn no_internal_shadowing(batch: Seq<(Path, Entry)>) -> bool {
    forall|i: int, j: int| 0 <= i < batch.len() && 0 <= j < batch.len() && i != j
        ==> #[trigger] compatible(batch[i].0, batch[j].0) && batch[i].0 != batch[j].0
}

pub proof fn pack_reads_back(pre: AbstractStore, post: AbstractStore, batch: Seq<(Path, Entry)>, i: int)
    requires
        pack(pre, post, batch),
        no_internal_shadowing(batch),
        0 <= i < batch.len(),
        !is_deleted(pre.deleted, batch[i].0),
    ensures
        read(post, batch[i].0) == Some(batch[i].1),
{
    apply_batch_keeps_entry(pre.view, pre.deleted, batch, i);
}

proof fn apply_batch_keeps_entry(view: Map<Path, Entry>, deleted: Set<Path>, batch: Seq<(Path, Entry)>, i: int)
    requires
        no_internal_shadowing(batch),
        0 <= i < batch.len(),
        !is_deleted(deleted, batch[i].0),
    ensures
        apply_batch(view, deleted, batch).contains_key(batch[i].0),
        apply_batch(view, deleted, batch)[batch[i].0] == batch[i].1,
    decreases batch.len(),
{
    let (p, e) = batch[0];
    let rest = batch.subrange(1, batch.len() as int);
    let next = if is_deleted(deleted, p) { view } else { insert_shadowing(view, p, e) };
    assert(no_internal_shadowing(rest)) by {
        assert forall|a: int, b: int| 0 <= a < rest.len() && 0 <= b < rest.len() && a != b
            implies #[trigger] compatible(rest[a].0, rest[b].0) && rest[a].0 != rest[b].0 by {
            assert(rest[a] == batch[a + 1]);
            assert(rest[b] == batch[b + 1]);
        }
    }
    if i == 0 {
        // inserted now; no later entry of the batch touches it
        apply_batch_leaves_untouched(next, deleted, rest, p);
        assert(next.contains_key(p) && next[p] == e);
    } else {
        assert(rest[i - 1] == batch[i]);
        apply_batch_keeps_entry(next, deleted, rest, i - 1);
    }
}

/// A path compatible with (and different from) every path of a batch is untouched by the batch.
proof fn apply_batch_leaves_untouched(view: Map<Path, Entry>, deleted: Set<Path>, batch: Seq<(Path, Entry)>, q: Path)
    requires
        forall|j: int| 0 <= j < batch.len() ==> #[trigger] compatible(batch[j].0, q) && batch[j].0 != q,
    ensures
        apply_batch(view, deleted, batch).contains_key(q) == view.contains_key(q),
        view.contains_key(q) ==> apply_batch(view, deleted, batch)[q] == view[q],
    decreases batch.len(),
{
    if batch.len() == 0 {
    } else {
        let (p, e) = batch[0];
        let rest = batch.subrange(1, batch.len() as int);
        let next = if is_deleted(deleted, p) { view } else { insert_shadowing(view, p, e) };
        assert(compatible(batch[0].0, q) && batch[0].0 != q);
        assert forall|j: int| 0 <= j < rest.len() implies #[trigger] compatible(rest[j].0, q) && rest[j].0 != q by {
            assert(rest[j] == batch[j + 1]);
        }
        apply_batch_leaves_untouched(next, deleted, rest, q);
    }
}

/// After `delete(p)`, nothing at or below `p` is readable, and everything else reads as before.
pub proof fn delete_removes_subtree(pre: AbstractStore, post: AbstractStore, p: Path, q: Path)
    requires
        delete(pre, post, p),
    ensures
        at_or_below(p, q) ==> read(post, q) == None::<Entry>,
        !at_or_below(p, q) ==> read(post, q) == read(pre, q),
{
}

} // verus!
