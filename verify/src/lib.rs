//! Verified TrajFS: specification, refinement, and verified executable modules, checked by Verus.
//!
//! Layers (see verify/SPEC.md and verify/REFINEMENT.md):
//!
//! - `spec::paths`          catalog paths and the ancestor relation
//! - `spec::abstract_store` L0, the high-level specification: a store is a namespace of paths to
//!                          entries; `pack`, `delete`, and `read` are its operations
//! - `spec::catalog`        L1, the content-addressed model: rows per batch and a blob map, with the
//!                          abstraction function to L0 and the refinement lemmas
//! - `spec::durable`        L2, the publish protocol under crashes: artifacts and the manifest, with
//!                          the theorem that a crash leaves either the old or the new snapshot
//! - `exec::namespace`      verified executable namespace merge, proven equal to the L0 fold
#![allow(unused_imports)]

use vstd::prelude::*;

pub mod spec {
    pub mod paths;
    pub mod abstract_store;
    pub mod catalog;
    pub mod durable;
}

pub mod exec {
    pub mod namespace;
}
