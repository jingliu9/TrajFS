//! Catalog paths: byte strings such as `rounds/round-0001/reviewer/review.json`, never empty,
//! never starting or ending with `/`, never containing `//`, `.`, or `..` components.
//! The store's namespace rule (docs/PLAN.md, `insert_latest`) is stated in terms of one relation:
//! `a` is a strict ancestor of `b` when `b` starts with `a` followed by a slash.

use vstd::prelude::*;

verus! {

pub type Path = Seq<u8>;

pub const SLASH: u8 = 47; // b'/'

/// `a` is a directory on the way to `b`: `b == a + "/" + something`.
pub open spec fn is_strict_ancestor(a: Path, b: Path) -> bool {
    a.len() < b.len()
        && b.subrange(0, a.len() as int) == a
        && b[a.len() as int] == SLASH
}

/// The paths at or below `p`: `p` itself and its descendants.
pub open spec fn at_or_below(p: Path, q: Path) -> bool {
    q == p || is_strict_ancestor(p, q)
}

/// Two catalog paths can coexist in one namespace only when neither is an ancestor of the other
/// (a path is a file or a directory, never both).
pub open spec fn compatible(a: Path, b: Path) -> bool {
    !is_strict_ancestor(a, b) && !is_strict_ancestor(b, a)
}

/// A well-formed catalog path: non-empty and with no slash at either end (so the ancestor
/// relation behaves like directory containment).
pub open spec fn well_formed(p: Path) -> bool {
    p.len() > 0 && p[0] != SLASH && p[p.len() - 1] != SLASH
}

/// Ancestry is transitive.
pub proof fn ancestor_transitive(a: Path, b: Path, c: Path)
    requires
        is_strict_ancestor(a, b),
        is_strict_ancestor(b, c),
    ensures
        is_strict_ancestor(a, c),
{
    assert(c.subrange(0, a.len() as int) =~= b.subrange(0, b.len() as int).subrange(0, a.len() as int));
    assert(b.subrange(0, b.len() as int) =~= b);
    assert(c.subrange(0, a.len() as int) =~= a);
    assert(c[a.len() as int] == b[a.len() as int]);
}

/// Nothing is its own strict ancestor.
pub proof fn ancestor_irreflexive(a: Path)
    ensures
        !is_strict_ancestor(a, a),
{
}

/// Ancestry is antisymmetric.
pub proof fn ancestor_antisymmetric(a: Path, b: Path)
    requires
        is_strict_ancestor(a, b),
    ensures
        !is_strict_ancestor(b, a),
{
}

} // verus!
