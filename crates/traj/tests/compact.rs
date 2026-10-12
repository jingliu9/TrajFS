use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::Path;
use trajfs_core::ingest::{ingest, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::{NoAdapter, CHUNK_BYTES};

fn traj() -> Command {
    Command::new(env!("CARGO_BIN_EXE_traj"))
}

fn pack(source: &Path, store: &Path) {
    ingest(
        source,
        store,
        IngestOptions {
            rules: Rules::resolve("none").unwrap(),
            rules_name: "none".into(),
            adapter: &NoAdapter,
            label: String::new(),
            jobs: 1,
            derive: false,
            store_id: None,
        },
    )
    .unwrap();
}

#[test]
fn cli_requires_compaction_and_emits_bound_storage_numbers() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = temp.path().join("store");
    fs::create_dir(&source).unwrap();
    let mut data = vec![b'X'; CHUNK_BYTES + 1];
    data[CHUNK_BYTES] = b'A';
    fs::write(source.join("file"), &data).unwrap();
    pack(&source, &store);
    data[CHUNK_BYTES] = b'B';
    fs::write(source.join("file"), &data).unwrap();
    pack(&source, &store);
    let result = traj()
        .arg("-S")
        .arg(&store)
        .args(["verify", "--deep", "--require-compact", "--json"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["storage"]["duplicate_frames"], 1);
    let out = temp.path().join("compact");
    let result = traj()
        .arg("-S")
        .arg(&store)
        .args(["compact", "--out"])
        .arg(&out)
        .args(["--no-recompress", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let compact: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(compact["catalog_history_preserved"], true);
    assert_eq!(compact["after"]["duplicate_frames"], 0);
    let result = traj()
        .arg("-S")
        .arg(&out)
        .args(["verify", "--deep", "--require-compact", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let verified: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(verified["ok"], true);
    assert_eq!(verified["storage"], compact["after"]);
    traj()
        .arg("-S")
        .arg(&out)
        .args(["cat", "file"])
        .assert()
        .success()
        .stdout(data);
    traj()
        .arg("-S")
        .arg(&store)
        .args(["compact", "--out"])
        .arg(&out)
        .assert()
        .failure();
}
