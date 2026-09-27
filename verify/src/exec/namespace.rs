//! Verified executable namespace merge: the shadowing insert of L0 on a concrete representation.
//!
//! The namespace is a vector of (path, slot) pairs with distinct paths. `insert` implements
//! `insert_latest` from crates/trajfs-core/src/store.rs: drop every entry at a strict ancestor or
//! strict descendant of the new path, drop the old entry at the path, append the new one. It is
//! proved to produce exactly `abstract_store::insert_shadowing` pointwise: the surviving paths
//! are the old ones not shadowed by the new path, plus the new path; their slots are unchanged;
//! the new path carries the new slot.
//!
//! The slot is what the merge carries per path (kind, mode, and a row handle); the bytes are not
//! part of the merge and stay with the read path.

use vstd::prelude::*;
use crate::spec::paths::*;
use crate::spec::abstract_store::shadowed_by;

verus! {

#[derive(Clone, Copy)]
pub struct Slot {
    pub kind: u8,
    pub mode: u16,
    pub row: u64,
}

pub type Items = Seq<(Vec<u8>, Slot)>;

/// Some item has path `q`.
pub open spec fn has(items: Items, q: Seq<u8>) -> bool {
    exists|i: int| 0 <= i < items.len() && (#[trigger] items[i]).0@ == q
}

pub open spec fn index_of(items: Items, q: Seq<u8>) -> int {
    choose|i: int| 0 <= i < items.len() && (#[trigger] items[i]).0@ == q
}

/// The slot at path `q` (meaningful when `has(items, q)` and paths are distinct).
pub open spec fn slot_at(items: Items, q: Seq<u8>) -> Slot {
    items[index_of(items, q)].1
}

/// Paths are distinct.
pub open spec fn wf(items: Items) -> bool {
    forall|i: int, j: int| 0 <= i < items.len() && 0 <= j < items.len() && i != j
        ==> (#[trigger] items[i]).0@ != (#[trigger] items[j]).0@
}

/// With distinct paths, the index of a path is the index of the item that carries it.
pub proof fn index_unique(items: Items, i: int)
    requires
        wf(items),
        0 <= i < items.len(),
    ensures
        has(items, items[i].0@),
        index_of(items, items[i].0@) == i,
        slot_at(items, items[i].0@) == items[i].1,
{
    let q = items[i].0@;
    assert(has(items, q));
    let k = index_of(items, q);
    assert(0 <= k < items.len() && items[k].0@ == q);
    if k != i {
        assert(items[k].0@ != items[i].0@);
    }
}

pub struct Namespace {
    pub items: Vec<(Vec<u8>, Slot)>,
}

/// `is_strict_ancestor(a@, b@)`, executable.
pub fn is_strict_ancestor_exec(a: &Vec<u8>, b: &Vec<u8>) -> (r: bool)
    ensures
        r == is_strict_ancestor(a@, b@),
{
    if a.len() >= b.len() {
        return false;
    }
    if b[a.len()] != SLASH {
        return false;
    }
    let mut i: usize = 0;
    while i < a.len()
        invariant
            i <= a.len(),
            a.len() < b.len(),
            forall|k: int| 0 <= k < i ==> a@[k] == b@[k],
        decreases a.len() - i,
    {
        if a[i] != b[i] {
            assert(b@.subrange(0, a@.len() as int)[i as int] == b@[i as int]);
            return false;
        }
        i += 1;
    }
    assert(b@.subrange(0, a@.len() as int) =~= a@);
    true
}

fn eq_bytes(a: &Vec<u8>, b: &Vec<u8>) -> (r: bool)
    ensures
        r == (a@ == b@),
{
    if a.len() != b.len() {
        assert(a@.len() != b@.len());
        return false;
    }
    let mut i: usize = 0;
    while i < a.len()
        invariant
            i <= a.len(),
            a.len() == b.len(),
            forall|k: int| 0 <= k < i ==> a@[k] == b@[k],
        decreases a.len() - i,
    {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    assert(a@ =~= b@);
    true
}

fn clone_bytes(a: &Vec<u8>) -> (r: Vec<u8>)
    ensures
        r@ == a@,
{
    let mut r: Vec<u8> = Vec::new();
    let mut i: usize = 0;
    while i < a.len()
        invariant
            i <= a.len(),
            r@ == a@.subrange(0, i as int),
        decreases a.len() - i,
    {
        r.push(a[i]);
        assert(r@ =~= a@.subrange(0, i as int + 1));
        i += 1;
    }
    assert(a@.subrange(0, a@.len() as int) =~= a@);
    r
}

/// Shadowing insert on the concrete namespace: exactly `insert_shadowing(old, p, s)`, pointwise.
pub fn insert(ns: &mut Namespace, p: Vec<u8>, s: Slot)
    requires
        wf(old(ns).items@),
    ensures
        wf(final(ns).items@),
        forall|q: Seq<u8>| has(final(ns).items@, q) <==> ((has(old(ns).items@, q) && !shadowed_by(p@, q) && q != p@) || q == p@),
        forall|q: Seq<u8>| has(final(ns).items@, q) && q != p@ ==> slot_at(final(ns).items@, q) == slot_at(old(ns).items@, q),
        slot_at(final(ns).items@, p@) == s,
{
    let ghost old_items = ns.items@;
    let mut kept: Vec<(Vec<u8>, Slot)> = Vec::new();
    let mut i: usize = 0;
    while i < ns.items.len()
        invariant
            i <= ns.items@.len(),
            ns.items@ == old_items,
            wf(old_items),
            wf(kept@),
            // every kept item is an unshadowed old item from the first i, unchanged
            forall|k: int| 0 <= k < kept@.len() ==> exists|j: int| 0 <= j < i && (#[trigger] kept@[k]).0@ == old_items[j].0@ && kept@[k].1 == old_items[j].1,
            // every unshadowed old item among the first i is kept
            forall|j: int| 0 <= j < i && !shadowed_by(p@, (#[trigger] old_items[j]).0@) && old_items[j].0@ != p@
                ==> has(kept@, old_items[j].0@),
            // nothing shadowed or at p is kept
            forall|k: int| 0 <= k < kept@.len() ==> !shadowed_by(p@, (#[trigger] kept@[k]).0@) && kept@[k].0@ != p@,
        decreases ns.items@.len() - i,
    {
        let it = &ns.items[i];
        let drop = is_strict_ancestor_exec(&it.0, &p) || is_strict_ancestor_exec(&p, &it.0) || eq_bytes(&it.0, &p);
        if !drop {
            let ghost before = kept@;
            kept.push((clone_bytes(&it.0), it.1));
            assert(kept@[kept@.len() - 1].0@ == old_items[i as int].0@);
            assert(kept@[kept@.len() - 1].1 == old_items[i as int].1);
            // the new item's path was not in `before`: its source index would have to be i
            assert(wf(kept@)) by {
                assert forall|a: int, b: int| 0 <= a < kept@.len() && 0 <= b < kept@.len() && a != b
                    implies (#[trigger] kept@[a]).0@ != (#[trigger] kept@[b]).0@ by {
                    if a < before.len() && b < before.len() {
                        assert(kept@[a] == before[a]);
                        assert(kept@[b] == before[b]);
                    } else if a < before.len() {
                        assert(kept@[a] == before[a]);
                        let j = choose|j: int| 0 <= j < i && (#[trigger] before[a]).0@ == old_items[j].0@ && before[a].1 == old_items[j].1;
                        assert(old_items[j].0@ != old_items[i as int].0@);
                    } else {
                        assert(kept@[b] == before[b]);
                        let j = choose|j: int| 0 <= j < i && (#[trigger] before[b]).0@ == old_items[j].0@ && before[b].1 == old_items[j].1;
                        assert(old_items[j].0@ != old_items[i as int].0@);
                    }
                }
            }
            assert forall|k: int| 0 <= k < kept@.len() implies exists|j: int| 0 <= j < i + 1 && (#[trigger] kept@[k]).0@ == old_items[j].0@ && kept@[k].1 == old_items[j].1 by {
                if k < before.len() {
                    assert(kept@[k] == before[k]);
                    let j = choose|j: int| 0 <= j < i && (#[trigger] before[k]).0@ == old_items[j].0@ && before[k].1 == old_items[j].1;
                    assert(kept@[k].0@ == old_items[j].0@ && kept@[k].1 == old_items[j].1);
                } else {
                    assert(kept@[k].0@ == old_items[i as int].0@ && kept@[k].1 == old_items[i as int].1);
                }
            }
            assert forall|j: int| 0 <= j < i + 1 && !shadowed_by(p@, (#[trigger] old_items[j]).0@) && old_items[j].0@ != p@
                implies has(kept@, old_items[j].0@) by {
                if j < i {
                    let k = choose|k: int| 0 <= k < before.len() && (#[trigger] before[k]).0@ == old_items[j].0@;
                    assert(kept@[k] == before[k]);
                } else {
                    assert(kept@[kept@.len() - 1].0@ == old_items[j].0@);
                }
            }
            assert forall|k: int| 0 <= k < kept@.len() implies !shadowed_by(p@, (#[trigger] kept@[k]).0@) && kept@[k].0@ != p@ by {
                if k < before.len() {
                    assert(kept@[k] == before[k]);
                }
            }
        } else {
            // the dropped item is shadowed or at p: nothing to keep
            assert(shadowed_by(p@, old_items[i as int].0@) || old_items[i as int].0@ == p@);
            assert forall|j: int| 0 <= j < i + 1 && !shadowed_by(p@, (#[trigger] old_items[j]).0@) && old_items[j].0@ != p@
                implies has(kept@, old_items[j].0@) by {
                assert(j < i);
            }
        }
        i += 1;
    }
    let ghost filtered = kept@;
    kept.push((p, s));
    ns.items = kept;
    proof {
        let new = ns.items@;
        let n = old_items.len() as int;
        assert(new[new.len() - 1].0@ == p@ && new[new.len() - 1].1 == s);
        // distinct paths: p is not among the filtered items
        assert(wf(new)) by {
            assert forall|a: int, b: int| 0 <= a < new.len() && 0 <= b < new.len() && a != b
                implies (#[trigger] new[a]).0@ != (#[trigger] new[b]).0@ by {
                if a < filtered.len() && b < filtered.len() {
                    assert(new[a] == filtered[a]);
                    assert(new[b] == filtered[b]);
                } else if a < filtered.len() {
                    assert(new[a] == filtered[a]);
                } else {
                    assert(new[b] == filtered[b]);
                }
            }
        }
        // domain
        assert forall|q: Seq<u8>| has(new, q) <==> ((has(old_items, q) && !shadowed_by(p@, q) && q != p@) || q == p@) by {
            if has(new, q) {
                let k = choose|k: int| 0 <= k < new.len() && (#[trigger] new[k]).0@ == q;
                if k < filtered.len() {
                    assert(new[k] == filtered[k]);
                    let j = choose|j: int| 0 <= j < n && (#[trigger] filtered[k]).0@ == old_items[j].0@ && filtered[k].1 == old_items[j].1;
                    assert(old_items[j].0@ == q);
                }
            }
            if has(old_items, q) && !shadowed_by(p@, q) && q != p@ {
                let j = choose|j: int| 0 <= j < old_items.len() && (#[trigger] old_items[j]).0@ == q;
                assert(has(filtered, old_items[j].0@));
                let k = choose|k: int| 0 <= k < filtered.len() && (#[trigger] filtered[k]).0@ == q;
                assert(new[k] == filtered[k]);
            }
            if q == p@ {
                assert(new[new.len() - 1].0@ == q);
            }
        }
        // values
        assert forall|q: Seq<u8>| has(new, q) && q != p@ implies slot_at(new, q) == slot_at(old_items, q) by {
            let k = index_of(new, q);
            assert(0 <= k < new.len() && new[k].0@ == q);
            assert(k < filtered.len());
            assert(new[k] == filtered[k]);
            let j = choose|j: int| 0 <= j < n && (#[trigger] filtered[k]).0@ == old_items[j].0@ && filtered[k].1 == old_items[j].1;
            index_unique(old_items, j);
        }
        index_unique(new, new.len() - 1);
    }
}

} // verus!
