use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use trajfs_core::compact::{compact, storage_report};
use trajfs_core::hash::sha_of_file;
use trajfs_core::ingest::{ingest, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::{FileRow, NoAdapter, Store, CHUNK_BYTES};

fn pack(source: &Path, destination: &Path) {
    ingest(
        source,
        destination,
        IngestOptions {
            rules: Rules::resolve("none").unwrap(),
            rules_name: "none".into(),
            adapter: &NoAdapter,
            label: "retained history".into(),
            jobs: 2,
            derive: false,
            store_id: Some("compaction-test".into()),
        },
    )
    .unwrap();
}

fn noise() -> Vec<u8> {
    let mut state = 17u64;
    (0..CHUNK_BYTES)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn rows(store: &Store) -> Vec<FileRow> {
    let mut rows = Vec::new();
    store.for_each_file(true, |row| rows.push(row)).unwrap();
    rows
}

#[test]
fn repeated_paths_are_one_blob_despite_names_inodes_and_modes() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let payload = noise()[..4096].to_vec();
    for i in 0..1344 {
        let name = source.join(format!("copy-{i:04}"));
        if i > 0 && i % 2 == 0 {
            fs::hard_link(source.join("copy-0000"), name).unwrap();
        } else {
            fs::write(name, &payload).unwrap();
        }
    }
    fs::set_permissions(source.join("copy-0001"), fs::Permissions::from_mode(0o755)).unwrap();
    let destination = temp.path().join("store");
    pack(&source, &destination);
    let store = Store::open(&destination).unwrap();
    assert!(store.verify(true).unwrap().ok());
    let usage = storage_report(&store).unwrap();
    assert_eq!(usage.catalog_rows, 1344);
    assert_eq!(usage.logical_bytes, 1344 * payload.len() as u64);
    assert_eq!(usage.blobs, 1);
    assert_eq!(usage.unique_blob_bytes, payload.len() as u64);
    assert!(usage.compact());
    for name in ["copy-0000", "copy-0001", "copy-1343"] {
        assert_eq!(
            store
                .read_row(
                    &mut store.reader(),
                    &store.stat(name).unwrap().unwrap(),
                    true
                )
                .unwrap(),
            payload
        );
    }
    assert_eq!(store.stat("copy-0001").unwrap().unwrap().mode, 0o755);
}

#[test]
fn new_pack_reuses_shared_blocks_of_different_files() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let shared = noise();
    let first = [shared.as_slice(), b"A"].concat();
    let second = [shared.as_slice(), b"B"].concat();
    fs::write(source.join("a"), &first).unwrap();
    fs::write(source.join("b"), &second).unwrap();
    let destination = temp.path().join("store");
    pack(&source, &destination);
    let store = Store::open(&destination).unwrap();
    let usage = storage_report(&store).unwrap();
    assert!(store.verify(true).unwrap().ok());
    assert_eq!(usage.blobs, 2);
    assert!(usage.compact());
    assert!(usage.pack_bytes < CHUNK_BYTES as u64 + 4096);
    for (name, want) in [("a", &first), ("b", &second)] {
        assert_eq!(
            &store
                .read_row(
                    &mut store.reader(),
                    &store.stat(name).unwrap().unwrap(),
                    true
                )
                .unwrap(),
            want
        );
    }
}

#[test]
fn file_bytes_and_identical_symlink_target_share_one_valid_blob() {
    for file_first in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let (file, link) = if file_first {
            ("a-file", "z-link")
        } else {
            ("z-file", "a-link")
        };
        fs::write(source.join(file), b"target").unwrap();
        symlink("target", source.join(link)).unwrap();
        let destination = temp.path().join("store");
        pack(&source, &destination);
        let store = Store::open(&destination).unwrap();
        assert!(store.verify(true).unwrap().ok());
        let usage = storage_report(&store).unwrap();
        assert_eq!(usage.blobs, 1);
        assert_eq!(usage.unique_blob_bytes, 6);
        assert!(usage.compact());
        let extracted = temp.path().join("out");
        store.extract("", &extracted, false, true, true).unwrap();
        assert_eq!(fs::read(extracted.join(file)).unwrap(), b"target");
        assert_eq!(
            fs::read_link(extracted.join(link)).unwrap(),
            Path::new("target")
        );
    }
}

#[test]
fn compaction_shares_blocks_across_batches_and_preserves_every_version() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let shared = noise();
    fs::write(
        source.join("versioned"),
        [shared.as_slice(), b"old"].concat(),
    )
    .unwrap();
    fs::write(source.join("empty"), []).unwrap();
    symlink("versioned", source.join("link")).unwrap();
    let original = temp.path().join("original.trajstore");
    pack(&source, &original);
    fs::write(
        source.join("versioned"),
        [shared.as_slice(), b"new"].concat(),
    )
    .unwrap();
    fs::set_permissions(source.join("versioned"), fs::Permissions::from_mode(0o755)).unwrap();
    pack(&source, &original);
    let store = Store::open(&original).unwrap();
    let before = storage_report(&store).unwrap();
    assert!(before.duplicate_frames > 0);
    let original_artifacts: Vec<_> = store
        .manifest
        .artifacts()
        .unwrap()
        .all()
        .map(|path| (path.clone(), sha_of_file(&original.join(path)).unwrap()))
        .collect();
    let manifest_before = fs::read(original.join("MANIFEST.json")).unwrap();
    let output = temp.path().join("compacted.trajstore");
    let report = compact(&store, &output, Some(3)).unwrap();
    assert!(report.after.compact());
    assert!(report.saved_bytes > (CHUNK_BYTES / 2) as i128);
    let compacted = Store::open(&output).unwrap();
    assert_eq!(rows(&store), rows(&compacted));
    assert_eq!(compacted.manifest.batches.len(), 2);
    for row in rows(&store) {
        assert_eq!(
            store.read_row(&mut store.reader(), &row, true).unwrap(),
            compacted
                .read_row(&mut compacted.reader(), &row, true)
                .unwrap()
        );
    }
    assert_eq!(
        fs::read(original.join("MANIFEST.json")).unwrap(),
        manifest_before
    );
    for (path, digest) in original_artifacts {
        assert_eq!(sha_of_file(&original.join(path)).unwrap(), digest);
    }
    // A second compaction has no redundant frames to reclaim and remains readable.
    let again = temp.path().join("again.trajstore");
    let second = compact(&compacted, &again, None).unwrap();
    assert_eq!(second.before.pack_bytes, second.after.pack_bytes);
    assert_eq!(rows(&Store::open(&again).unwrap()), rows(&store));
    drop(compacted);
    // The rewritten store remains a normal appendable TrajFS store.
    fs::write(source.join("new-log"), "later observation\n").unwrap();
    pack(&source, &output);
    assert!(Store::open(&output).unwrap().verify(true).unwrap().ok());
}

#[test]
fn compaction_rejects_existing_or_nested_destination_and_corrupt_source() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("x"), b"retained bytes").unwrap();
    let original = temp.path().join("original");
    pack(&source, &original);
    let store = Store::open(&original).unwrap();
    assert!(compact(&store, &original, None).is_err());
    assert!(compact(&store, &original.join("nested"), None).is_err());
    assert!(!original.join("nested").exists());
    fs::write(
        original.join("unindexed-evidence.txt"),
        "preserve uncertain content",
    )
    .unwrap();
    let unexpected = temp.path().join("unexpected-output");
    assert!(compact(&store, &unexpected, None).is_err());
    assert!(!unexpected.exists());
    // Remove only this test's intentionally added, unpublished fixture file.
    fs::remove_file(original.join("unindexed-evidence.txt")).unwrap();
    let pack = original.join("packs/0001.pack");
    let mut bytes = fs::read(&pack).unwrap();
    *bytes.last_mut().unwrap() ^= 0x80;
    fs::write(&pack, bytes).unwrap();
    let output = temp.path().join("invalid-output");
    assert!(compact(&store, &output, None).is_err());
    assert!(!output.exists());
}
