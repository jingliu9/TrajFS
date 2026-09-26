//! Property test: the latest namespace of a multi-batch store equals a model
//! that applies every batch in order (last write wins; the store is
//! append-only, so a path absent from a later source tree stays visible).

use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use trajfs_core::hash::sha_of_bytes;
use trajfs_core::ingest::{ingest_with_max_artifact_bytes, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::{Kind, NoAdapter, Store};

/// A fixed universe of file paths: three top-level directories, nested up to
/// three levels, so batches overlap on directories and files. No path is ever
/// both a file and a directory, which keeps the model a plain map.
const UNIVERSE: &[&str] = &[
    "README",
    "notes.txt",
    "rounds/round-0001/builder/events.jsonl",
    "rounds/round-0001/builder/stdout.log",
    "rounds/round-0001/reviewer/review.json",
    "rounds/round-0002/builder/events.jsonl",
    "rounds/round-0002/builder/stdout.log",
    "rounds/round-0002/reviewer/review.json",
    "rounds/round-0003/builder/events.jsonl",
    "rounds/round-0003/DONE",
    "workspace/src/main.rs",
    "workspace/src/lib/util.rs",
    "workspace/src/lib/deep/inner.rs",
    "workspace/Cargo.toml",
    "workspace/target/out.bin",
    "logs/a.log",
    "logs/b.log",
];

/// Content pool: a handful of distinct blobs so batches share content across
/// paths (deduplication) and rewrite files with the same bytes (unchanged).
fn content(id: u8) -> Vec<u8> {
    match id {
        0 => Vec::new(),
        1 => b"alpha\n".to_vec(),
        2 => b"beta beta\n".to_vec(),
        3 => (0..200u32).map(|i| (i % 251) as u8).collect(),
        4 => b"{\"type\":\"session.start\"}\n".to_vec(),
        5 => vec![b'x'; 3000],
        _ => format!("content-{id}\n").into_bytes(),
    }
}

/// One batch: a subset of the universe with a content id per file.
fn batch_strategy() -> impl Strategy<Value = BTreeMap<usize, u8>> {
    prop::collection::btree_map(0..UNIVERSE.len(), 0u8..8, 1..=UNIVERSE.len())
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
        // small artifact limit so several batches split into more than one pack
        16 << 10,
    )
    .unwrap();
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DirSummary {
    n_files: i64,
    n_dirs: i64,
    bytes: i64,
}

/// Directory summaries computed from the model: recursive file count and
/// bytes, direct subdirectory count.
fn model_dirs(model: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, DirSummary> {
    let mut dirs: BTreeMap<String, DirSummary> = BTreeMap::new();
    let mut seen_child: BTreeSet<(String, String)> = BTreeSet::new();
    for (path, bytes) in model {
        let mut cur = trajfs_core::parent_of(path).to_string();
        loop {
            let entry = dirs.entry(cur.clone()).or_default();
            entry.n_files += 1;
            entry.bytes += bytes.len() as i64;
            if cur.is_empty() {
                break;
            }
            let parent = trajfs_core::parent_of(&cur).to_string();
            if seen_child.insert((parent.clone(), cur.clone())) {
                dirs.entry(parent.clone()).or_default().n_dirs += 1;
            }
            cur = parent;
        }
    }
    dirs
}

fn check_store_against_model(store: &Store, model: &BTreeMap<String, Vec<u8>>) {
    // 1. the visible namespace is exactly the model
    let rows = store.files_under("", true).unwrap();
    let visible: BTreeMap<String, Vec<u8>> = {
        let mut reader = store.reader();
        rows.iter()
            .map(|r| {
                let bytes = store.read_row(&mut reader, r, true).unwrap();
                (r.path.clone(), bytes)
            })
            .collect()
    };
    assert_eq!(&visible, model, "latest namespace differs from the model");

    // 2. every path resolves through `stat` to the right hash and kind
    for (path, bytes) in model {
        let row = store
            .stat(path)
            .unwrap()
            .unwrap_or_else(|| panic!("{path} is missing"));
        assert_eq!(row.sha, sha_of_bytes(bytes), "{path}: wrong content hash");
        assert_eq!(row.size as usize, bytes.len(), "{path}: wrong size");
        let expected_kind = if bytes.is_empty() {
            Kind::Empty
        } else {
            Kind::File
        };
        assert_eq!(row.kind, expected_kind, "{path}: wrong kind");
    }
    for unused in UNIVERSE.iter().filter(|p| !model.contains_key(**p)) {
        assert!(
            store.stat(unused).unwrap().is_none(),
            "{unused} should not exist"
        );
    }

    // 3. directory summaries match the model, both per directory and listed
    let expected_dirs = model_dirs(model);
    let mut actual_dirs: BTreeMap<String, DirSummary> = BTreeMap::new();
    let root = store.dir_info("").unwrap().expect("root directory");
    actual_dirs.insert(
        String::new(),
        DirSummary {
            n_files: root.n_files,
            n_dirs: root.n_dirs,
            bytes: root.bytes,
        },
    );
    for dir in store.dirs_under("").unwrap() {
        let info = store.dir_info(&dir.dir).unwrap().expect("dir_info");
        assert_eq!(
            (info.n_files, info.n_dirs, info.bytes),
            (dir.n_files, dir.n_dirs, dir.bytes),
            "{}: dirs_under and dir_info disagree",
            dir.dir
        );
        actual_dirs.insert(
            dir.dir.clone(),
            DirSummary {
                n_files: dir.n_files,
                n_dirs: dir.n_dirs,
                bytes: dir.bytes,
            },
        );
    }
    assert_eq!(actual_dirs, expected_dirs, "directory summaries differ");

    // 4. `children` agrees with the model for every directory
    for (dir, summary) in &expected_dirs {
        let (subdirs, files) = store.children(dir).unwrap();
        assert_eq!(
            subdirs.len() as i64,
            summary.n_dirs,
            "{dir:?}: subdir count"
        );
        let expected_files: BTreeSet<&str> = model
            .keys()
            .filter(|p| trajfs_core::parent_of(p) == dir)
            .map(String::as_str)
            .collect();
        let actual_files: BTreeSet<&str> = files.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(actual_files, expected_files, "{dir:?}: direct files");
        for sub in &subdirs {
            let expected = &expected_dirs[&sub.dir];
            assert_eq!(
                (sub.n_files, sub.n_dirs, sub.bytes),
                (expected.n_files, expected.n_dirs, expected.bytes),
                "{}: child summary",
                sub.dir
            );
        }
    }

    // 5. the store verifies, deeply
    let report = store.verify(true).unwrap();
    assert!(report.ok(), "verify failed: {report:?}");
    assert_eq!(
        report.files as usize,
        store
            .manifest
            .batches
            .iter()
            .map(|b| b.paths as usize)
            .sum::<usize>()
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 8,
        max_shrink_iters: 40,
        .. ProptestConfig::default()
    })]

    #[test]
    fn latest_namespace_matches_a_last_write_wins_model(
        batches in prop::collection::vec(batch_strategy(), 2..=6),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let store = tmp.path().join("store");
        let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (i, batch) in batches.iter().enumerate() {
            write_tree(&src, batch);
            for (index, id) in batch {
                model.insert(UNIVERSE[*index].to_string(), content(*id));
            }
            pack(&src, &store, &format!("b{i}"));
            let st = Store::open(&store).unwrap();
            prop_assert_eq!(st.manifest.batches.len(), i + 1);
            check_store_against_model(&st, &model);
        }
    }

    #[test]
    fn every_batch_has_the_rows_its_manifest_entry_declares(
        batches in prop::collection::vec(batch_strategy(), 2..=4),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let store = tmp.path().join("store");
        for (i, batch) in batches.iter().enumerate() {
            write_tree(&src, batch);
            pack(&src, &store, &format!("b{i}"));
        }
        let st = Store::open(&store).unwrap();
        let mut per_batch: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
        st.for_each_file(false, |r| {
            let e = per_batch.entry(r.batch).or_default();
            e.0 += 1;
            e.1 += r.size as u64;
        }).unwrap();
        for b in &st.manifest.batches {
            let (paths, bytes) = per_batch.get(&b.id).copied().unwrap_or((0, 0));
            prop_assert_eq!((paths, bytes), (b.paths, b.bytes), "batch {}", b.id);
        }
        // pack ids never repeat across batches
        let mut packs: Vec<u32> = st.manifest.batches.iter().flat_map(|b| b.packs.iter().copied()).collect();
        let n = packs.len();
        packs.sort_unstable();
        packs.dedup();
        prop_assert_eq!(packs.len(), n, "pack ids are reused");
        prop_assert!(st.verify(true).unwrap().ok());
    }
}
