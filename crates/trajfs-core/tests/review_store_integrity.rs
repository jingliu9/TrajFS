use anyhow::Result;
use arrow::array::{
    ArrayRef, FixedSizeBinaryArray, Int64Array, MapBuilder, MapFieldNames, StringArray,
    StringBuilder, UInt16Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use trajfs_core::catalog;
use trajfs_core::events::{Event, EventsWriter, SegmentedEventsWriter};
use trajfs_core::hash::sha_of_bytes;
use trajfs_core::manifest::{AdapterInfo, RulesInfo};
use trajfs_core::pack::{IndexRow, Loc, PackWriter};
use trajfs_core::{Batch, FileRow, Kind, Manifest, Sha, Store};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    manifest: Manifest,
    known: HashSet<Sha>,
}

fn row(path: &str, bytes: &[u8], kind: Kind, mtime_ns: i64) -> (FileRow, Vec<u8>) {
    (
        FileRow {
            path: path.to_owned(),
            kind,
            mode: 0o644,
            size: bytes.len() as i64,
            sha: sha_of_bytes(bytes),
            mtime_ns,
            batch: 0,
            attrs: vec![],
        },
        bytes.to_vec(),
    )
}

impl Fixture {
    fn new(format: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        fs::create_dir(&root).unwrap();
        Self {
            _dir: dir,
            root,
            manifest: Manifest {
                format,
                store_id: "review".into(),
                source: "/fixture".into(),
                adapter: AdapterInfo {
                    name: "none".into(),
                    version: 1,
                },
                rules: RulesInfo {
                    name: "none".into(),
                    version: 1,
                },
                batches: vec![],
            },
            known: HashSet::new(),
        }
    }

    fn add(&mut self, input: Vec<(FileRow, Vec<u8>)>) {
        self.add_with_limit(input, 64 << 10);
    }

    fn add_with_limit(&mut self, mut input: Vec<(FileRow, Vec<u8>)>, max_bytes: u64) {
        let id = self.manifest.next_batch_id();
        let mut writer =
            PackWriter::new(&self.root.join("packs"), self.manifest.next_pack_id()).unwrap();
        input.sort_by(|a, b| a.0.path.cmp(&b.0.path));
        for (r, bytes) in &mut input {
            r.batch = id;
            if r.kind != Kind::Empty && self.known.insert(r.sha) {
                writer.add_bytes(r.sha, bytes).unwrap();
            }
        }
        let (index, packs, new_blob_bytes, packed_bytes) = writer.finish().unwrap();
        let files: Vec<FileRow> = input.into_iter().map(|(r, _)| r).collect();
        let dirs = catalog::dirs_from_files(files.iter(), id);
        let segments =
            catalog::write_batch_segments(&self.root, id, &files, &dirs, &[], &index, max_bytes)
                .unwrap();
        self.manifest.batches.push(Batch {
            id,
            created: "2026-09-05T00:00:00Z".into(),
            label: String::new(),
            paths: files.len() as u64,
            bytes: files.iter().map(|r| r.size as u64).sum(),
            new_blobs: index.len() as u64,
            new_blob_bytes,
            packed_bytes,
            packs,
            segments: if self.manifest.format == 1 && segments.len() == 1 {
                vec![]
            } else {
                segments
            },
            derived: vec![],
            excluded: 0,
            errors: vec![],
            elapsed_ms: 0,
        });
        self.manifest.save(&self.root).unwrap();
    }

    fn open(&self) -> Store {
        Store::open(&self.root).unwrap()
    }
}

fn write_record_batch(path: &Path, batch: RecordBatch) {
    let mut writer = catalog::arrow_writer(path, batch.schema()).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn replace_column(path: &Path, name: &str, replacement: ArrayRef) {
    let original = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let fields: Vec<_> = original
        .schema()
        .fields()
        .iter()
        .enumerate()
        .map(|(i, field)| {
            (
                field.name().clone(),
                if field.name() == name {
                    replacement.clone()
                } else {
                    original.column(i).clone()
                },
            )
        })
        .collect();
    write_record_batch(path, RecordBatch::try_from_iter(fields).unwrap());
}

fn assert_rejected_without_panic(f: impl FnOnce() -> Result<()>) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    assert!(outcome.is_ok(), "malformed catalog panicked");
    assert!(outcome.unwrap().is_err(), "malformed catalog was accepted");
}

#[test]
fn malformed_file_columns_return_errors_instead_of_panicking() {
    for replacement in [
        Arc::new(StringArray::from(vec!["not a kind"])) as ArrayRef,
        Arc::new(UInt8Array::from(vec![None])),
        Arc::new(UInt8Array::from(vec![255])),
    ] {
        let mut fixture = Fixture::new(2);
        fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
        let path = fixture.root.join("catalog/files-0001.parquet");
        replace_column(&path, "kind", replacement);
        assert_rejected_without_panic(|| catalog::scan_files(&path, None, true, |_| {}));
    }

    let mut fixture = Fixture::new(1);
    fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
    let path = fixture.root.join("catalog/files-0001.parquet");
    let short_sha = FixedSizeBinaryArray::try_from_iter([&[0u8; 31][..]].into_iter()).unwrap();
    replace_column(&path, "sha", Arc::new(short_sha));
    assert_rejected_without_panic(|| catalog::scan_files(&path, None, false, |_| {}));
}

#[test]
fn malformed_empty_tables_are_not_accepted_by_scanners() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.parquet");
    write_record_batch(
        &path,
        RecordBatch::new_empty(Arc::new(Schema::new(vec![Field::new(
            "unrelated",
            DataType::Utf8,
            false,
        )]))),
    );
    assert_rejected_without_panic(|| catalog::scan_files(&path, None, true, |_| {}));
    assert_rejected_without_panic(|| catalog::scan_direct_children(&path, "", true, |_| {}));
    assert_rejected_without_panic(|| catalog::scan_dirs(&path, None, |_| {}));
    assert_rejected_without_panic(|| catalog::read_index(&path, |_, _| {}));
}

#[test]
fn malformed_catalog_paths_and_empty_metadata_are_rejected() {
    for path in [
        "../escape",
        "/absolute",
        "a/../escape",
        "a//b",
        "a/./b",
        "a\0b",
        "",
    ] {
        let mut fixture = Fixture::new(2);
        fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
        let segment = fixture.root.join("catalog/files-0001.parquet");
        replace_column(&segment, "path", Arc::new(StringArray::from(vec![path])));
        assert_rejected_without_panic(|| catalog::scan_files(&segment, None, true, |_| {}));
    }
    for format in [1, 2] {
        let mut fixture = Fixture::new(format);
        fixture.add(vec![row("empty", b"", Kind::Empty, 0)]);
        let path = fixture.root.join("catalog/files-0001.parquet");
        replace_column(&path, "size", Arc::new(Int64Array::from(vec![9])));
        assert_rejected_without_panic(|| catalog::scan_files(&path, None, false, |_| {}));
    }
}

#[test]
fn malformed_index_locations_are_rejected_without_reading_packs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.parquet");
    let valid = Loc {
        pack: 1,
        chunk_offset: 9,
        chunk_len: 20,
        offset: 0,
        size: 4,
        part: 0,
    };
    for invalid in [
        Loc { pack: 0, ..valid },
        Loc {
            chunk_offset: -1,
            ..valid
        },
        Loc {
            chunk_len: 0,
            ..valid
        },
        Loc {
            offset: -1,
            ..valid
        },
        Loc { size: -1, ..valid },
        Loc {
            size: i64::MAX,
            ..valid
        },
    ] {
        catalog::write_index(
            &path,
            &[IndexRow {
                sha: [7; 32],
                loc: invalid,
            }],
        )
        .unwrap();
        assert_rejected_without_panic(|| catalog::read_index(&path, |_, _| {}));
    }
}

#[test]
fn verify_reads_every_declared_parquet_artifact() {
    for format in [1, 2] {
        for corrupt in [
            "catalog/dirs-0001.parquet",
            "catalog/excluded-0001.parquet",
            "derived/none/events-0001.parquet",
        ] {
            let mut fixture = Fixture::new(format);
            fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
            if corrupt.starts_with("derived/") {
                fixture.manifest.batches[0].derived = vec!["none/events-0001".into()];
                fs::create_dir_all(fixture.root.join("derived/none")).unwrap();
                fixture.manifest.save(&fixture.root).unwrap();
            }
            fs::write(fixture.root.join(corrupt), b"not a parquet file").unwrap();
            for deep in [false, true] {
                let store = fixture.open();
                let result = store.verify(deep);
                assert!(
                    result.is_err() || !result.unwrap().ok(),
                    "format {format}: verify({deep}) ignored {corrupt}"
                );
            }
        }
    }
}

#[test]
fn empty_content_can_be_read_by_sha_without_an_index_entry() {
    for format in [1, 2] {
        let mut fixture = Fixture::new(format);
        fixture.add(vec![row("empty", b"", Kind::Empty, 0)]);
        let store = fixture.open();
        let empty = sha_of_bytes(b"");
        assert!(store.index().unwrap().is_empty());
        assert_eq!(store.read_sha(&mut store.reader(), &empty).unwrap(), b"");
        assert_eq!(
            store.read_blob(&mut store.reader(), &empty, true).unwrap(),
            b""
        );
        assert!(store.verify(true).unwrap().ok());
    }
}

#[test]
fn incremental_namespace_transitions_keep_only_compatible_latest_paths() {
    for format in [1, 2] {
        let mut fixture = Fixture::new(format);
        fixture.add(vec![
            row("shape/old", b"old child", Kind::File, 1),
            row("mode", b"old file", Kind::File, 1),
            row("keep", b"retained history", Kind::File, 1),
        ]);
        fixture.add(vec![
            row("shape", b"elsewhere", Kind::Symlink, 2),
            row("mode/new", b"new child", Kind::File, 2),
        ]);
        {
            let store = fixture.open();
            assert!(store.stat("shape/old").unwrap().is_none());
            assert!(store.stat("mode").unwrap().is_none());
            assert!(store.files_under("shape", false).unwrap().is_empty());
        }
        fixture.add(vec![
            row("shape/new", b"restored directory", Kind::File, 3),
            row("mode", b"latest file", Kind::File, 3),
        ]);
        let store = fixture.open();
        assert_eq!(
            store
                .files_under("", false)
                .unwrap()
                .iter()
                .map(|r| r.path.as_str())
                .collect::<Vec<_>>(),
            ["keep", "mode", "shape/new"]
        );
        assert!(store.stat("shape").unwrap().is_none());
        assert!(store.stat("shape/old").unwrap().is_none());
        assert!(store.stat("mode/new").unwrap().is_none());
        let root = store.dir_info("").unwrap().unwrap();
        assert_eq!(root.n_files, 3);
        assert_eq!(root.n_dirs, 1);
        assert_eq!(
            root.bytes,
            (b"retained history".len() + b"latest file".len() + b"restored directory".len()) as i64
        );
        let (dirs, files) = store.children("").unwrap();
        assert_eq!(
            dirs.iter().map(|r| r.dir.as_str()).collect::<Vec<_>>(),
            ["shape"]
        );
        assert_eq!(
            files.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            ["keep", "mode"]
        );
        assert_eq!(store.children("shape").unwrap().1[0].path, "shape/new");
        assert!(store.verify(true).unwrap().ok());
    }
}

#[test]
fn latest_path_filter_and_sha_mapping_do_not_revive_shadowed_files() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![
        row("tree/old.txt", b"old descendant", Kind::File, 1),
        row("changed.txt", b"old content", Kind::File, 1),
    ]);
    fixture.add(vec![
        row("tree", b"target", Kind::Symlink, 2),
        row("changed.txt", b"new content", Kind::File, 2),
    ]);
    let store = fixture.open();
    let mut paths = vec![];
    store
        .scan_under("", false, |p| p.ends_with(".txt"), |r| paths.push(r.path))
        .unwrap();
    assert_eq!(paths, ["changed.txt"]);
    let by_sha = store.paths_by_sha(|r| r.kind == Kind::File).unwrap();
    assert_eq!(by_sha.len(), 1);
    assert_eq!(by_sha[&sha_of_bytes(b"new content")], ["changed.txt"]);
    assert_eq!(store.verify(true).unwrap().files, 4);
}

#[test]
fn extraction_replaces_leaf_links_without_modifying_their_targets() {
    for kind in [Kind::File, Kind::Empty] {
        for hard_link in [false, true] {
            let mut fixture = Fixture::new(2);
            fixture.add(vec![row(
                "file",
                if kind == Kind::Empty { b"" } else { b"kept" },
                kind,
                0,
            )]);
            let destination = fixture._dir.path().join("out");
            fs::create_dir(&destination).unwrap();
            let outside = fixture._dir.path().join("sentinel");
            fs::write(&outside, b"do not modify").unwrap();
            if hard_link {
                fs::hard_link(&outside, destination.join("file")).unwrap();
            } else {
                std::os::unix::fs::symlink(&outside, destination.join("file")).unwrap();
            }
            fixture
                .open()
                .extract("", &destination, false, false, true)
                .unwrap();
            assert_eq!(fs::read(&outside).unwrap(), b"do not modify");
            assert_eq!(
                fs::read(destination.join("file")).unwrap(),
                if kind == Kind::Empty {
                    b"".as_slice()
                } else {
                    b"kept".as_slice()
                }
            );
        }
    }
}

#[test]
fn extraction_refuses_symlinked_destination_directories() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![row("sub/file", b"kept", Kind::File, 0)]);
    let destination = fixture._dir.path().join("out");
    let outside = fixture._dir.path().join("outside");
    fs::create_dir(&destination).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file"), b"do not modify").unwrap();
    std::os::unix::fs::symlink(&outside, destination.join("sub")).unwrap();
    assert!(fixture
        .open()
        .extract("", &destination, false, false, true)
        .is_err());
    assert_eq!(fs::read(outside.join("file")).unwrap(), b"do not modify");
}

#[test]
fn hardlink_dedupe_preserves_requested_distinct_mtimes() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![
        row("a", b"same", Kind::File, 1_000_000_000),
        row("b", b"same", Kind::File, 2_000_000_000),
        row("c", b"same", Kind::File, 1_000_000_000),
        row("before-epoch", b"older", Kind::File, -1_000_000_000),
    ]);
    let destination = fixture._dir.path().join("out");
    fixture
        .open()
        .extract("", &destination, true, true, true)
        .unwrap();
    let a = fs::metadata(destination.join("a")).unwrap();
    let b = fs::metadata(destination.join("b")).unwrap();
    let c = fs::metadata(destination.join("c")).unwrap();
    assert_eq!(a.mtime(), 1);
    assert_eq!(b.mtime(), 2);
    assert_eq!(c.ino(), a.ino());
    assert_ne!(b.ino(), a.ino());
    assert_eq!(
        fs::metadata(destination.join("before-epoch"))
            .unwrap()
            .mtime(),
        -1
    );
}

#[test]
fn empty_store_extracts_as_an_empty_directory() {
    for format in [1, 2] {
        let mut fixture = Fixture::new(format);
        fixture.add(vec![]);
        let destination = fixture._dir.path().join("out");
        let store = fixture.open();
        assert_eq!(
            store.extract("", &destination, false, false, true).unwrap(),
            0
        );
        assert!(destination.is_dir());
        assert!(store.verify(true).unwrap().ok());
    }
}

#[test]
fn invalid_manifest_publication_does_not_replace_the_readable_generation() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![]);
    let published = fs::read(Manifest::path(&fixture.root)).unwrap();
    let mut duplicate = fixture.manifest.batches[0].clone();
    duplicate.segments = vec!["files-other".into()];
    fixture.manifest.batches.push(duplicate);
    assert!(fixture.manifest.artifacts().is_err());
    assert!(fixture.manifest.save(&fixture.root).is_err());
    assert_eq!(fs::read(Manifest::path(&fixture.root)).unwrap(), published);
}

#[test]
fn range_pruning_uses_the_top_level_path_not_an_attribute_leaf() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
    let path = fixture.root.join("catalog/files-0001.parquet");
    let original = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&path).unwrap())
        .unwrap()
        .build()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let mut attrs = MapBuilder::new(
        Some(MapFieldNames {
            entry: "entries".into(),
            key: "path".into(),
            value: "value".into(),
        }),
        StringBuilder::new(),
        StringBuilder::new(),
    );
    attrs.keys().append_value("nested/attribute");
    attrs.values().append_value("value");
    attrs.append(true).unwrap();
    let mut columns = vec![("attrs".to_owned(), Arc::new(attrs.finish()) as ArrayRef)];
    for (field, values) in original.schema().fields().iter().zip(original.columns()) {
        if field.name() != "attrs" {
            columns.push((field.name().clone(), values.clone()));
        }
    }
    write_record_batch(&path, RecordBatch::try_from_iter(columns).unwrap());

    let mut selected = vec![];
    catalog::scan_files(&path, Some((b"file", b"file\0")), true, |r| {
        selected.push(r.path)
    })
    .unwrap();
    assert_eq!(selected, ["file"]);
    selected.clear();
    catalog::scan_direct_children(&path, "", true, |r| selected.push(r.path)).unwrap();
    assert_eq!(selected, ["file"]);
}

#[test]
fn schema_correct_catalog_row_loss_is_detected_by_verify() {
    for format in [1, 2] {
        let mut fixture = Fixture::new(format);
        fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
        catalog::write_files(&fixture.root.join("catalog/files-0001.parquet"), &[]).unwrap();
        let store = fixture.open();
        for deep in [false, true] {
            let report = store.verify(deep).unwrap();
            assert!(
                !report.ok(),
                "format {format}: lost catalog rows passed verify"
            );
        }
    }
}

#[test]
fn immutable_catalog_and_event_segments_are_not_replaced() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
    let path = fixture.root.join("catalog/files-0001.parquet");
    let original = fs::read(&path).unwrap();
    let result = catalog::write_batch_segments(&fixture.root, 1, &[], &[], &[], &[], 64 << 10);
    assert!(result.is_err());
    assert_eq!(fs::read(&path).unwrap(), original);

    let dir = fixture.root.join("derived/none");
    let writer = SegmentedEventsWriter::create(&dir, "events-0001", 1, 64 << 10).unwrap();
    let (_, paths) = writer.finish().unwrap();
    let original = fs::read(&paths[0]).unwrap();
    let mut writer = SegmentedEventsWriter::create(&dir, "events-0001", 1, 64 << 10).unwrap();
    writer
        .push(
            "file",
            vec![Event {
                r#type: "new".into(),
                ..Default::default()
            }],
        )
        .unwrap();
    assert!(writer.finish().is_err());
    assert_eq!(fs::read(&paths[0]).unwrap(), original);
}

#[test]
fn segmented_events_reject_unsafe_names_and_zero_limits_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    for stem in ["../escaped", "/absolute", "", ".", "name/", "name\0"] {
        assert!(SegmentedEventsWriter::create(&output, stem, 1, 64 << 10).is_err());
    }
    assert!(SegmentedEventsWriter::create(&output, "events-0001", 1, 0).is_err());
    assert!(!output.exists());
}

#[test]
fn deep_verify_checks_derived_pages_beyond_the_footer() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
    let dir = fixture.root.join("derived/none");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events-0001.parquet");
    let mut writer = EventsWriter::create(&path, 1).unwrap();
    writer
        .push(
            "file",
            &[Event {
                r#type: "example".into(),
                ..Default::default()
            }],
        )
        .unwrap();
    writer.finish().unwrap();
    fixture.manifest.batches[0].derived = vec!["none/events-0001".into()];
    fixture.manifest.save(&fixture.root).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&path).unwrap()).unwrap();
    let page = builder.metadata().row_group(0).column(0).data_page_offset() as usize;
    let mut bytes = fs::read(&path).unwrap();
    bytes[page] = 0xff;
    fs::write(&path, bytes).unwrap();
    let store = fixture.open();
    assert!(store.verify(false).unwrap().ok());
    let result = store.verify(true);
    assert!(result.is_err() || !result.unwrap().ok());
}

#[test]
fn failed_extraction_preserves_existing_files_and_removes_unpublished_entries() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
    let pack = fixture.root.join("packs/0001.pack");
    fs::OpenOptions::new()
        .write(true)
        .open(pack)
        .unwrap()
        .set_len(9)
        .unwrap();
    let destination = fixture._dir.path().join("out");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("file"), b"keep original output").unwrap();
    let store = fixture.open();
    for verify in [false, true] {
        assert!(store
            .extract("", &destination, false, false, verify)
            .is_err());
        assert_eq!(
            fs::read(destination.join("file")).unwrap(),
            b"keep original output"
        );
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
    }
}

#[test]
fn legacy_readers_are_read_only_and_relative_writer_locks_coordinate() {
    use std::os::unix::fs::PermissionsExt;
    use trajfs_core::store::lock_store_exclusive;

    for format in [1, 2] {
        let mut fixture = Fixture::new(format);
        fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
        fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o555)).unwrap();
        let reader = fixture.open();
        assert!(reader.verify(true).unwrap().ok());
        assert_eq!(
            reader
                .read_sha(&mut reader.reader(), &sha_of_bytes(b"kept"))
                .unwrap(),
            b"kept"
        );
        assert!(!fixture.root.join(".lock").exists());
        assert!(lock_store_exclusive(&fixture.root).is_err());
        drop(reader);
        fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let mut common = cwd.as_path();
        let mut relative = PathBuf::new();
        while !fixture.root.starts_with(common) {
            relative.push("..");
            common = common.parent().unwrap();
        }
        relative.push(fixture.root.strip_prefix(common).unwrap());
        let writer = lock_store_exclusive(&relative).unwrap();
        assert!(Store::open(&fixture.root).is_err());
        drop(writer);
        assert!(fixture.open().verify(true).unwrap().ok());
    }
}

#[test]
fn special_permission_bits_in_corrupt_catalogs_are_rejected() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
    let path = fixture.root.join("catalog/files-0001.parquet");
    replace_column(&path, "mode", Arc::new(UInt16Array::from(vec![0o4755])));
    assert_rejected_without_panic(|| catalog::scan_files(&path, None, false, |_| {}));
}

#[test]
fn malformed_blob_part_sequences_fail_without_hash_verification() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
    let path = fixture.root.join("packs/index-0001.parquet");
    let mut index = vec![];
    catalog::read_index(&path, |sha, loc| index.push(IndexRow { sha, loc })).unwrap();
    let original = index[0].clone();
    for broken in [
        vec![IndexRow {
            loc: Loc {
                part: 1,
                ..original.loc
            },
            ..original.clone()
        }],
        vec![original.clone(), original.clone()],
    ] {
        catalog::write_index(&path, &broken).unwrap();
        let store = fixture.open();
        let row = store.stat("file").unwrap().unwrap();
        assert!(store.read_row(&mut store.reader(), &row, false).is_err());
        assert!(!store.verify(false).unwrap().ok());
    }
}

#[test]
fn schema_correct_directory_loss_and_false_aggregates_are_detected() {
    for format in [1, 2] {
        for replacement in [
            vec![],
            vec![catalog::DirRow {
                dir: "".into(),
                batch: 1,
                n_files: 1,
                bytes: 99,
                ..Default::default()
            }],
        ] {
            let mut fixture = Fixture::new(format);
            fixture.add(vec![row("file", b"kept", Kind::File, 0)]);
            catalog::write_dirs(
                &fixture.root.join("catalog/dirs-0001.parquet"),
                &replacement,
            )
            .unwrap();
            assert!(!fixture.open().verify(false).unwrap().ok());
        }
    }
}

#[test]
fn declared_unindexed_packs_still_require_valid_headers() {
    let mut fixture = Fixture::new(2);
    fixture.add(vec![]);
    fixture.manifest.batches[0].packs = vec![1];
    fs::write(fixture.root.join("packs/0001.pack"), b"invalid").unwrap();
    fixture.manifest.save(&fixture.root).unwrap();
    let store = fixture.open();
    assert!(store.index().unwrap().is_empty());
    assert!(!store.verify(false).unwrap().ok());
}

#[test]
fn segmented_catalogs_remain_readable_and_verify_across_formats_and_batches() {
    for format in [1, 2] {
        let mut fixture = Fixture::new(format);
        let input: Vec<_> = (0..400)
            .map(|i| {
                let content = format!("blob {i:04}");
                let name = format!(
                    "dir-{}/file-{i}",
                    hex::encode(sha_of_bytes(content.as_bytes()))
                );
                row(&name, content.as_bytes(), Kind::File, 1)
            })
            .collect();
        let changed_path = input[0].0.path.clone();
        fixture.add_with_limit(input, 8 << 10);
        assert!(fixture.manifest.batches[0].segments.len() > 1);
        assert_eq!(fixture.open().files_under("", false).unwrap().len(), 400);
        assert!(fixture.open().verify(true).unwrap().ok());
        fixture.add(vec![row(&changed_path, b"changed content", Kind::File, 2)]);
        let store = fixture.open();
        assert_eq!(store.files_under("", false).unwrap().len(), 400);
        assert_eq!(store.dir_info("").unwrap().unwrap().n_files, 400);
        assert_eq!(store.children("").unwrap().0.len(), 400);
        assert!(store.verify(true).unwrap().ok());
    }
}
