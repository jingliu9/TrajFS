use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use trajfs_core::ingest::{ingest, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::{NoAdapter, Store};

#[test]
fn restoring_an_unchanged_shadowed_path_records_a_new_visible_version() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let store = directory.path().join("store");
    let child = source.join("node/child");
    fs::create_dir_all(child.parent().unwrap()).unwrap();
    let write_child = |path: &Path| {
        fs::write(path, b"original content").unwrap();
        fs::File::open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(10))
            .unwrap();
    };
    let pack = || {
        ingest(
            &source,
            &store,
            IngestOptions {
                rules: Rules::resolve("none").unwrap(),
                rules_name: "none".into(),
                adapter: &NoAdapter,
                label: String::new(),
                jobs: 2,
                derive: false,
                store_id: None,
            },
        )
        .unwrap()
    };

    write_child(&child);
    pack();
    fs::remove_file(&child).unwrap();
    fs::remove_dir(source.join("node")).unwrap();
    fs::write(source.join("node"), b"replacement file").unwrap();
    pack();
    assert!(Store::open(&store)
        .unwrap()
        .stat("node/child")
        .unwrap()
        .is_none());

    fs::remove_file(source.join("node")).unwrap();
    fs::create_dir(source.join("node")).unwrap();
    write_child(&child);
    let summary = pack();
    assert_eq!(
        summary.batch.paths, 1,
        "a shadowed historical row is not an unchanged visible row"
    );
    let store = Store::open(&store).unwrap();
    let current = store.stat("node/child").unwrap().unwrap();
    assert_eq!(current.batch, 3);
    assert_eq!(
        store.read_row(&mut store.reader(), &current, true).unwrap(),
        b"original content"
    );
    assert!(store.stat("node").unwrap().is_none());
    assert!(store.verify(true).unwrap().ok());
}
