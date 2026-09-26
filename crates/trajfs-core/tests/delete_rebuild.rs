//! Property test for `trajfs_core::delete`: after packing random batches and
//! deleting a random file or subtree, the target is gone from every batch,
//! every other row is byte-identical, only the blobs referenced solely by the
//! deleted rows leave the packs, the store deep-verifies, and a later pack of
//! the same source skips the deleted path (docs/PLAN-deletion.md §5, §7).

use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use trajfs_core::catalog::read_excluded;
use trajfs_core::delete::{apply, plan, rebuild, referenced_shas};
use trajfs_core::ingest::{ingest_with_max_artifact_bytes, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::store::lock_store_exclusive;
use trajfs_core::walk::{is_under, DELETED_RULE};
use trajfs_core::{FileRow, Kind, NoAdapter, Sha, Store};

const UNIVERSE: &[&str] = &[
    "README",
    "rounds/round-0001/builder/events.jsonl",
    "rounds/round-0001/builder/stdout.log",
    "rounds/round-0001/reviewer/review.json",
    "rounds/round-0002/builder/events.jsonl",
    "rounds/round-0002/builder/stdout.log",
    "rounds/round-0002/reviewer/review.json",
    "rounds/round-0003/DONE",
    "workspace/src/main.rs",
    "workspace/src/lib/util.rs",
    "workspace/Cargo.toml",
    "logs/a.log",
];

/// Directory prefixes a deletion may target (every proper ancestor in the universe).
fn directories() -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for path in UNIVERSE {
        let mut cur = trajfs_core::parent_of(path);
        while !cur.is_empty() {
            dirs.insert(cur.to_string());
            cur = trajfs_core::parent_of(cur);
        }
    }
    dirs.into_iter().collect()
}

fn content(id: u8) -> Vec<u8> {
    match id {
        0 => Vec::new(),
        1 => b"shared\n".to_vec(),
        2 => b"also shared\n".to_vec(),
        3 => (0..300u32).map(|i| (i % 253) as u8).collect(),
        _ => format!("content-{id}\n").into_bytes(),
    }
}

fn batch_strategy() -> impl Strategy<Value = BTreeMap<usize, u8>> {
    // few content ids so blobs are shared across paths and batches
    prop::collection::btree_map(0..UNIVERSE.len(), 0u8..6, 1..=UNIVERSE.len())
}

fn write_tree(src: &Path, batch: &BTreeMap<usize, u8>) {
    if src.exists() {
        fs::remove_dir_all(src).unwrap();
    }
    for (index, id) in batch {
        let path = src.join(UNIVERSE[*index]);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content(*id)).unwrap();
    }
}

fn pack(src: &Path, store: &Path, label: &str) {
    ingest_with_max_artifact_bytes(
        src,
        store,
        IngestOptions {
            rules: Rules::resolve("none").unwrap(),
            rules_name: "none".into(),
            adapter: &NoAdapter,
            label: label.into(),
            jobs: 2,
            derive: false,
            store_id: Some("prop".into()),
        },
        16 << 10,
    )
    .unwrap();
}

fn all_rows(store: &Store) -> Vec<FileRow> {
    let mut rows = Vec::new();
    store.for_each_file(true, |r| rows.push(r)).unwrap();
    rows
}

fn delete_path(store_dir: &Path, target: &str) -> trajfs_core::delete::Plan {
    let lock = lock_store_exclusive(store_dir).unwrap();
    let st = Store::open_unlocked(store_dir).unwrap();
    let p = plan(&st, target).unwrap();
    let work = rebuild(&st, &p, 16 << 10).unwrap();
    drop(st);
    apply(store_dir, &work).unwrap();
    drop(lock);
    assert!(!work.exists(), "the sibling must be removed after the swap");
    p
}

/// Pick the target from what the store actually contains: `choice` selects a
/// file (even) or a directory (odd) among those with at least one row.
fn choose_target(rows: &[FileRow], choice: usize) -> String {
    let files: Vec<&str> = {
        let mut v: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let dirs: Vec<String> = directories()
        .into_iter()
        .filter(|d| rows.iter().any(|r| is_under(&r.path, d)))
        .collect();
    if choice.is_multiple_of(2) || dirs.is_empty() {
        files[(choice / 2) % files.len()].to_string()
    } else {
        dirs[(choice / 2) % dirs.len()].clone()
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 10,
        max_shrink_iters: 30,
        .. ProptestConfig::default()
    })]

    #[test]
    fn deleting_a_path_removes_only_that_subtree_and_its_private_blobs(
        batches in prop::collection::vec(batch_strategy(), 2..=4),
        choice in 0usize..1000,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let store = tmp.path().join("store");
        for (i, batch) in batches.iter().enumerate() {
            write_tree(&src, batch);
            pack(&src, &store, &format!("b{i}"));
        }

        let before = Store::open(&store).unwrap();
        let rows_before = all_rows(&before);
        let target = choose_target(&rows_before, choice);
        let referenced_before = referenced_shas(&before).unwrap();
        let index_before: HashSet<Sha> = before.index().unwrap().keys().copied().collect();
        // the index holds every non-empty referenced blob and nothing else
        let expected_index: HashSet<Sha> = referenced_before.keys().copied().collect();
        prop_assert_eq!(&index_before, &expected_index);

        let expected_rows: Vec<FileRow> = rows_before
            .iter()
            .filter(|r| !is_under(&r.path, &target))
            .cloned()
            .collect();
        let removed_rows: Vec<&FileRow> = rows_before
            .iter()
            .filter(|r| is_under(&r.path, &target))
            .collect();
        prop_assert!(!removed_rows.is_empty(), "target {target} matches nothing");
        let kept_shas: HashSet<Sha> = expected_rows
            .iter()
            .filter(|r| r.kind != Kind::Empty)
            .map(|r| r.sha)
            .collect();
        let removed_shas: HashSet<Sha> = removed_rows
            .iter()
            .filter(|r| r.kind != Kind::Empty)
            .map(|r| r.sha)
            .collect();
        let private_shas: HashSet<Sha> = removed_shas.difference(&kept_shas).copied().collect();
        let batches_touched: BTreeSet<u32> = removed_rows.iter().map(|r| r.batch).collect();
        let visible_removed = before.stat(&target).unwrap().is_some() as usize
            + before.files_under(&target, false).unwrap().len();
        drop(before);

        let p = delete_path(&store, &target);
        prop_assert_eq!(p.paths as usize, removed_rows.len(), "plan.paths");
        prop_assert_eq!(p.survivors as usize, expected_rows.len(), "plan.survivors");
        prop_assert_eq!(p.blobs as usize, private_shas.len(), "plan.blobs");
        prop_assert_eq!(p.batches.iter().copied().collect::<BTreeSet<u32>>(), batches_touched);
        prop_assert_eq!(p.latest_paths as usize, visible_removed, "plan.latest_paths");

        let after = Store::open(&store).unwrap();
        // gone from every batch, everything else byte-identical (including batch ids and attrs)
        let rows_after = all_rows(&after);
        prop_assert!(rows_after.iter().all(|r| !is_under(&r.path, &target)));
        prop_assert_eq!(&rows_after, &expected_rows);
        prop_assert_eq!(after.manifest.batches.len(), batches.len(), "batch history is stable");
        prop_assert_eq!(after.manifest.deleted.len(), 1);
        prop_assert_eq!(after.manifest.deleted[0].path.as_str(), target.as_str());
        prop_assert_eq!(after.manifest.deleted[0].paths as usize, removed_rows.len());
        prop_assert_eq!(after.manifest.deleted[0].blobs as usize, private_shas.len());
        for b in &after.manifest.batches {
            let paths = rows_after.iter().filter(|r| r.batch == b.id).count();
            prop_assert_eq!(paths as u64, b.paths, "batch {} count", b.id);
        }
        // blobs: shared content survives, private content is no longer reachable
        let index_after: HashSet<Sha> = after.index().unwrap().keys().copied().collect();
        prop_assert_eq!(&index_after, &kept_shas, "index after deletion");
        for sha in &private_shas {
            prop_assert!(after.parts(sha).is_err(), "private blob still readable");
        }
        // surviving content is still readable and hash-checked
        let mut reader = after.reader();
        for r in &expected_rows {
            let bytes = after.read_row(&mut reader, r, true).unwrap();
            prop_assert_eq!(bytes.len(), r.size as usize);
        }
        drop(reader);
        prop_assert!(after.stat(&target).unwrap().is_none());
        prop_assert!(after.files_under(&target, false).unwrap().is_empty());
        let report = after.verify(true).unwrap();
        prop_assert!(report.ok(), "verify --deep after delete: {report:?}");
        drop(after);

        // a later pack of the same source skips the deleted path, whatever the source holds
        let last = batches.last().unwrap();
        write_tree(&src, last);
        let source_has_target = src.join(&target).exists();
        pack(&src, &store, "after-delete");
        let again = Store::open(&store).unwrap();
        prop_assert_eq!(again.manifest.batches.len(), batches.len() + 1);
        prop_assert!(again.stat(&target).unwrap().is_none(), "the deleted path came back");
        prop_assert!(again.files_under(&target, false).unwrap().is_empty());
        prop_assert!(all_rows(&again).iter().all(|r| !is_under(&r.path, &target)));
        let mut deleted_records = 0;
        for seg in again.excluded_segments() {
            read_excluded(seg, |e| {
                if e.rule == DELETED_RULE {
                    deleted_records += 1;
                    assert!(is_under(&e.rel, &target), "{} recorded as deleted", e.rel);
                }
            })
            .unwrap();
        }
        if source_has_target {
            prop_assert!(deleted_records >= 1, "no `deleted` exclusion record");
        } else {
            prop_assert_eq!(deleted_records, 0);
        }
        prop_assert!(again.verify(true).unwrap().ok());
        // deleting inside the deleted subtree again is refused as already deleted
        let err = plan(&again, &target).unwrap_err().to_string();
        prop_assert!(err.contains("already deleted"), "{err}");
    }

    #[test]
    fn two_successive_deletions_compose(
        batches in prop::collection::vec(batch_strategy(), 2..=3),
        first in 0usize..1000,
        second in 0usize..1000,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let store = tmp.path().join("store");
        for (i, batch) in batches.iter().enumerate() {
            write_tree(&src, batch);
            pack(&src, &store, &format!("b{i}"));
        }
        let rows = all_rows(&Store::open(&store).unwrap());
        let t1 = choose_target(&rows, first);
        delete_path(&store, &t1);
        let remaining = all_rows(&Store::open(&store).unwrap());
        if remaining.is_empty() {
            return Ok(());
        }
        let t2 = choose_target(&remaining, second);
        if is_under(&t2, &t1) {
            return Ok(());
        }
        delete_path(&store, &t2);
        let after = Store::open(&store).unwrap();
        let expected: Vec<FileRow> = rows
            .into_iter()
            .filter(|r| !is_under(&r.path, &t1) && !is_under(&r.path, &t2))
            .collect();
        prop_assert_eq!(all_rows(&after), expected);
        prop_assert_eq!(after.manifest.deleted.len(), 2);
        let kept: HashSet<Sha> = referenced_shas(&after).unwrap().keys().copied().collect();
        let index: HashSet<Sha> = after.index().unwrap().keys().copied().collect();
        prop_assert_eq!(index, kept);
        prop_assert!(after.verify(true).unwrap().ok());
    }
}
