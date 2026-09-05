use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

use trajfs_core::events::Event;
use trajfs_core::ingest::{ingest, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::{Adapter, Kind, Store};

struct MutatingAdapter {
    path: PathBuf,
    action: &'static str,
    fired: AtomicBool,
    parsed: Mutex<Vec<u8>>,
    attribute: &'static str,
}

impl MutatingAdapter {
    fn new(path: PathBuf, action: &'static str) -> Self {
        Self {
            path,
            action,
            fired: AtomicBool::new(false),
            parsed: Mutex::new(Vec::new()),
            attribute: "one",
        }
    }
}

impl Adapter for MutatingAdapter {
    fn name(&self) -> &str {
        "fixture"
    }
    fn attrs(&self, path: &str) -> Vec<(String, String)> {
        if self.action != "truncate-after-pack"
            && path == "events.jsonl"
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            match self.action {
                "append" => OpenOptions::new()
                    .append(true)
                    .open(&self.path)
                    .unwrap()
                    .write_all(b"appended after hashing\n")
                    .unwrap(),
                "overwrite" => {
                    let size = fs::metadata(&self.path).unwrap().len() as usize;
                    fs::write(&self.path, vec![b'z'; size]).unwrap();
                }
                "truncate" => fs::write(&self.path, b"x").unwrap(),
                "replace" => {
                    fs::rename(&self.path, self.path.with_extension("retained")).unwrap();
                    fs::write(&self.path, b"replacement").unwrap();
                }
                "none" => {}
                _ => unreachable!(),
            }
        }
        vec![("revision".into(), self.attribute.into())]
    }
    fn is_trajectory(&self, path: &str) -> bool {
        if path == "events.jsonl"
            && self.action == "truncate-after-pack"
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            fs::write(&self.path, b"x").unwrap();
        }
        path == "events.jsonl"
    }
    fn parse_events(&self, _: &str, bytes: &[u8]) -> Vec<Event> {
        *self.parsed.lock().unwrap() = bytes.to_vec();
        vec![Event {
            r#type: "captured".into(),
            payload_json: "{}".into(),
            ..Event::default()
        }]
    }
}

fn pack(
    src: &Path,
    store: &Path,
    adapter: &dyn Adapter,
    derive: bool,
) -> anyhow::Result<trajfs_core::ingest::IngestSummary> {
    ingest(
        src,
        store,
        IngestOptions {
            rules: Rules::resolve("none")?,
            rules_name: "none".into(),
            adapter,
            label: "test".into(),
            jobs: 2,
            derive,
            store_id: None,
        },
    )
}

#[test]
fn append_between_hash_and_pack_keeps_catalog_blob_and_derived_bytes_equal() {
    for size in [0, 19, trajfs_core::CHUNK_BYTES * 2 + 17] {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("raw");
        let store = temp.path().join("snapshot.trajstore");
        fs::create_dir(&src).unwrap();
        let file = src.join("events.jsonl");
        let bytes = vec![b'a'; size];
        fs::write(&file, &bytes).unwrap();
        let adapter = MutatingAdapter::new(file, "append");
        let summary = pack(&src, &store, &adapter, true).unwrap();
        assert_eq!(summary.batch.bytes, size as u64);
        assert!(summary.batch.errors.is_empty());
        let saved = Store::open(&store).unwrap();
        let row = saved.stat("events.jsonl").unwrap().unwrap();
        assert_eq!(row.size, size as i64);
        if size == 0 {
            assert_eq!(row.kind, Kind::Empty);
        } else {
            assert_eq!(
                saved.read_sha(&mut saved.reader(), &row.sha).unwrap(),
                bytes
            );
            assert_eq!(*adapter.parsed.lock().unwrap(), bytes);
        }
        assert!(saved.verify(true).unwrap().ok());
    }
}

#[test]
fn source_mutations_abort_without_publishing_a_new_manifest() {
    for action in ["overwrite", "truncate", "replace", "truncate-after-pack"] {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("raw");
        let store = temp.path().join("snapshot.trajstore");
        fs::create_dir(&src).unwrap();
        let file = src.join("events.jsonl");
        fs::write(&file, b"old contents").unwrap();
        pack(
            &src,
            &store,
            &MutatingAdapter::new(file.clone(), "none"),
            false,
        )
        .unwrap();
        let manifest = fs::read(store.join("MANIFEST.json")).unwrap();
        fs::write(&file, vec![b'b'; trajfs_core::CHUNK_BYTES * 2 + 29]).unwrap();
        let result = pack(
            &src,
            &store,
            &MutatingAdapter::new(file.clone(), action),
            true,
        );
        assert!(result.is_err(), "{action}");
        assert_eq!(fs::read(store.join("MANIFEST.json")).unwrap(), manifest);
        assert!(Store::open(&store).unwrap().verify(true).unwrap().ok());
        pack(&src, &store, &MutatingAdapter::new(file, "none"), false).unwrap();
        assert!(Store::open(&store).unwrap().verify(true).unwrap().ok());
    }
}

#[test]
fn unchanged_metadata_cannot_hide_content_exec_bit_or_attribute_updates() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("raw");
    let store = temp.path().join("snapshot.trajstore");
    fs::create_dir(&src).unwrap();
    let file = src.join("events.jsonl");
    fs::write(&file, b"old").unwrap();
    let modified = fs::metadata(&file).unwrap().modified().unwrap();
    pack(
        &src,
        &store,
        &MutatingAdapter::new(file.clone(), "none"),
        false,
    )
    .unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    let mut adapter = MutatingAdapter::new(file.clone(), "none");
    let mode_update = pack(&src, &store, &adapter, false).unwrap();
    assert_eq!(mode_update.batch.paths, 1);
    assert_eq!(mode_update.batch.new_blobs, 0);
    assert_eq!(
        Store::open(&store)
            .unwrap()
            .stat("events.jsonl")
            .unwrap()
            .unwrap()
            .mode,
        0o755
    );
    adapter.attribute = "two";
    let attrs_update = pack(&src, &store, &adapter, false).unwrap();
    assert_eq!(attrs_update.batch.paths, 1);
    assert_eq!(attrs_update.batch.new_blobs, 0);
    assert_eq!(
        Store::open(&store)
            .unwrap()
            .stat("events.jsonl")
            .unwrap()
            .unwrap()
            .attrs,
        vec![("revision".into(), "two".into())]
    );
    fs::write(&file, b"new").unwrap();
    OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let update = pack(&src, &store, &adapter, false).unwrap();
    assert_eq!(update.skipped_unchanged, 0);
    {
        let saved = Store::open(&store).unwrap();
        let row = saved.stat("events.jsonl").unwrap().unwrap();
        assert_eq!(row.mode, 0o755);
        assert_eq!(row.attrs, vec![("revision".into(), "two".into())]);
        assert_eq!(
            saved.read_sha(&mut saved.reader(), &row.sha).unwrap(),
            b"new"
        );
    }
    let unchanged = pack(&src, &store, &adapter, false).unwrap();
    assert_eq!(unchanged.skipped_unchanged, 1);
    assert!(Store::open(&store).unwrap().verify(true).unwrap().ok());
}

#[test]
fn explicit_keeps_survive_directory_pruning_but_private_subtrees_do_not() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("build/private")).unwrap();
    fs::write(temp.path().join("build/important.stdout"), b"evidence").unwrap();
    fs::write(temp.path().join("build/other.bin"), b"rebuildable").unwrap();
    fs::write(
        temp.path().join("build/private/important.stdout"),
        b"private",
    )
    .unwrap();
    let rules = Rules::from_toml(
        r#"
name = "keep-evidence"
exclude_dirs = ["build"]
always_keep = ["**/important.stdout"]
always_exclude = ["build/private/**"]
"#,
    )
    .unwrap();
    let walked = trajfs_core::walk::walk(temp.path(), &rules, &[]).unwrap();
    assert_eq!(walked.kept.len(), 1);
    assert_eq!(walked.kept[0].rel, "build/important.stdout");
    assert!(walked
        .excluded
        .iter()
        .any(|e| e.rel == "build/private" && e.rule == "always_exclude"));
}
