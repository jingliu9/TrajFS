//! L2: publishing a batch under crashes.
//!
//! What is on disk is a set of artifacts (packs, catalog segments, index segments, derived
//! tables, each identified by name) and one manifest that *declares* which artifacts form the
//! current snapshot. Readers trust only declared artifacts, so the visible store is a function of
//! the manifest alone, given that every declared artifact exists.
//!
//! The protocol of `traj pack` (docs/PLAN-deletion.md §3, `ingest.rs`):
//!
//!   write new artifacts  ->  fsync them  ->  write the new manifest to a temp file and fsync it
//!   ->  rename it over MANIFEST.json (commit point)  ->  fsync the directory
//!
//! Model: the disk has a *durable* state and a *volatile* state (page cache). Writes change the
//! volatile state; an fsync makes the named things durable; a crash discards the volatile state.
//! `rename` is atomic on the volatile state and becomes durable with the directory fsync; until
//! then the durable manifest is the old one.
//!
//! Theorem (`crash_safe`): at every point of the protocol, the durable manifest is either the old
//! one or the new one, and every artifact it declares is durable. So after a crash, readers see
//! the old snapshot or the new snapshot, complete either way; undeclared artifacts are orphans
//! that the next writer removes.
//!
//! Trusted here: the filesystem guarantees assumed of `fsync` and atomic `rename` (ext4/xfs with
//! ordered data), and that the manifest names artifacts unambiguously.

use vstd::prelude::*;

verus! {

pub type ArtifactId = nat;

/// A manifest is the sequence of artifacts it declares (the batches' segments and packs in order)
/// plus a version, so old and new manifests are distinguishable.
pub struct Manifest {
    pub version: nat,
    pub declared: Set<ArtifactId>,
}

pub struct Disk {
    pub artifacts: Set<ArtifactId>,
    pub manifest: Manifest,
}

pub struct State {
    pub durable: Disk,
    pub volatile: Disk,
    /// The manifest being published, once written to its temporary file.
    pub staged: Option<Manifest>,
    /// The manifest that was current when the protocol started.
    pub old: Manifest,
    /// The manifest the protocol is publishing.
    pub new: Manifest,
}

pub enum Step {
    /// Write one new artifact (into the page cache).
    WriteArtifact(ArtifactId),
    /// fsync every written artifact.
    FsyncArtifacts,
    /// Write the new manifest to a temporary file and fsync it.
    StageManifest,
    /// rename(temp, MANIFEST.json): atomic in the volatile state.
    Rename,
    /// fsync the store directory: the rename is durable.
    FsyncDir,
    /// Power loss: the volatile state is gone.
    Crash,
}

pub open spec fn init(s: State) -> bool {
    &&& s.durable == s.volatile
    &&& s.durable.manifest == s.old
    &&& s.staged == None::<Manifest>
    &&& s.old.declared.subset_of(s.durable.artifacts)
    &&& s.new.version != s.old.version
    // the new manifest declares everything the old one did (batches are append-only) plus new artifacts
    &&& s.old.declared.subset_of(s.new.declared)
}

pub open spec fn step(pre: State, post: State, st: Step) -> bool {
    &&& post.old == pre.old
    &&& post.new == pre.new
    &&& match st {
        Step::WriteArtifact(a) => {
            &&& pre.new.declared.contains(a)
            &&& !pre.old.declared.contains(a)
            &&& post.volatile == Disk { artifacts: pre.volatile.artifacts.insert(a), manifest: pre.volatile.manifest }
            &&& post.durable == pre.durable
            &&& post.staged == pre.staged
        },
        Step::FsyncArtifacts => {
            &&& post.volatile == pre.volatile
            &&& post.durable == Disk { artifacts: pre.volatile.artifacts, manifest: pre.durable.manifest }
            &&& post.staged == pre.staged
        },
        Step::StageManifest => {
            // the protocol stages only after the artifacts it declares are durable
            &&& pre.new.declared.subset_of(pre.durable.artifacts)
            &&& post.volatile == pre.volatile
            &&& post.durable == pre.durable
            &&& post.staged == Some(pre.new)
        },
        Step::Rename => {
            &&& pre.staged == Some(pre.new)
            &&& post.volatile == Disk { artifacts: pre.volatile.artifacts, manifest: pre.new }
            &&& post.durable == pre.durable
            &&& post.staged == pre.staged
        },
        Step::FsyncDir => {
            &&& post.volatile == pre.volatile
            &&& post.durable == Disk { artifacts: pre.durable.artifacts, manifest: pre.volatile.manifest }
            &&& post.staged == pre.staged
        },
        Step::Crash => {
            &&& post.volatile == pre.durable
            &&& post.durable == pre.durable
            &&& post.staged == None::<Manifest>
        },
    }
}

/// What a reader sees after a crash: the durable snapshot.
pub open spec fn visible(d: Disk) -> Manifest {
    d.manifest
}

/// The protocol invariant.
pub open spec fn inv(s: State) -> bool {
    // the durable manifest is the old or the new one
    &&& s.durable.manifest == s.old || s.durable.manifest == s.new
    // whichever it is, every artifact it declares is durable
    &&& s.durable.manifest.declared.subset_of(s.durable.artifacts)
    // the volatile manifest is the old or the new one, and the new one only after staging
    &&& s.volatile.manifest == s.old || s.volatile.manifest == s.new
    &&& s.volatile.manifest == s.new && s.durable.manifest != s.new ==> s.staged == Some(s.new)
    // the new manifest's artifacts are durable once it is staged
    &&& s.staged == Some(s.new) ==> s.new.declared.subset_of(s.durable.artifacts)
    // durable artifacts are never lost, and the volatile disk holds at least the durable artifacts
    &&& s.durable.artifacts.subset_of(s.volatile.artifacts)
    &&& s.old.declared.subset_of(s.durable.artifacts)
    &&& s.new.version != s.old.version
    &&& s.old.declared.subset_of(s.new.declared)
}

pub proof fn init_inv(s: State)
    requires
        init(s),
    ensures
        inv(s),
{
}

pub proof fn step_inv(pre: State, post: State, st: Step)
    requires
        inv(pre),
        step(pre, post, st),
    ensures
        inv(post),
{
    match st {
        Step::WriteArtifact(a) => {
            assert(post.volatile.artifacts =~= pre.volatile.artifacts.insert(a));
        },
        Step::FsyncArtifacts => {},
        Step::StageManifest => {},
        Step::Rename => {},
        Step::FsyncDir => {},
        Step::Crash => {},
    }
}

/// Crash safety: after any crash, readers see a complete old or new snapshot.
pub proof fn crash_safe(pre: State, post: State)
    requires
        inv(pre),
        step(pre, post, Step::Crash),
    ensures
        visible(post.volatile) == pre.old || visible(post.volatile) == pre.new,
        visible(post.volatile).declared.subset_of(post.volatile.artifacts),
{
}

/// Once the directory fsync has happened after the rename, the new snapshot is durable.
pub proof fn publish_durable(pre: State, post: State)
    requires
        inv(pre),
        pre.volatile.manifest == pre.new,
        step(pre, post, Step::FsyncDir),
    ensures
        post.durable.manifest == pre.new,
        pre.new.declared.subset_of(post.durable.artifacts),
{
}

} // verus!
