//! `traj watch`: a batch is packed when the adapter's readiness marker appears (each marker once), nested runs
//! get unique store ids and round labels, and pack errors or a corrupt manifest are reported rather than
//! committed.

mod common;

use common::*;
use std::fs;
use std::os::unix::ffi::OsStringExt;

#[test]
fn watch_packs_when_the_adapter_reports_a_batch_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    git_init(&repo);
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    fs::copy(adapter(), repo.join("trajfs/adapter.toml")).unwrap();
    fs::copy(
        fixtures_dir().join("rounds-rules.toml"),
        repo.join("trajfs/rounds-rules.toml"),
    )
    .unwrap();
    let data = tmp.path().join("data");
    traj()
        .current_dir(&repo)
        .args(["init", "--data-root"])
        .arg(&data)
        .args(["--adapter", "trajfs/adapter.toml"])
        .assert()
        .success();
    fixture(&data.join("run7"));
    // nothing ready yet: watch with --max-batches 1 would block, so first check readiness through the marker
    write(&data.join("run7/rounds/round-0001/DONE"), b"");
    traj()
        .current_dir(&repo)
        .args(["watch", "--max-batches", "1", "--interval", "1"])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();
    let m = manifest(&repo.join("stores/run7.trajstore"));
    assert_eq!(m["batches"][0]["label"], "round-0001");
    // the same marker is not packed twice; a new one is
    write(&data.join("run7/rounds/round-0002/DONE"), b"");
    traj()
        .current_dir(&repo)
        .args(["watch", "--max-batches", "1", "--interval", "1"])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();
    let m = manifest(&repo.join("stores/run7.trajstore"));
    assert_eq!(m["batches"][1]["label"], "round-0002");
}

#[test]
fn watch_discovers_nested_runs_with_unique_ids_and_round_labels() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    git_init(&repo);
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    fs::copy(
        fixtures_dir().join("rounds-rules.toml"),
        repo.join("trajfs/rounds-rules.toml"),
    )
    .unwrap();
    write(
        &repo.join("trajfs/adapter.toml"),
        br#"name = "nested"
version = 1
rules = "rounds-rules.toml"
[batch_ready]
run_glob = "**/run7"
markers = ["rounds/round-*/reviewer/review.json"]
label_ancestor_pattern = '^round-\d+$'
"#,
    );
    let data = tmp.path().join("data");
    traj()
        .current_dir(&repo)
        .args(["init", "--data-root"])
        .arg(&data)
        .args(["--adapter", "trajfs/adapter.toml"])
        .assert()
        .success();
    fixture(&data.join("suite-a/task/run7"));
    fixture(&data.join("suite-b/task/run7"));
    traj()
        .current_dir(&repo)
        .args([
            "watch",
            "--max-batches",
            "2",
            "--interval",
            "1",
            "--no-derive",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();

    for id in ["suite-a%2Ftask%2Frun7", "suite-b%2Ftask%2Frun7"] {
        let value = manifest(&repo.join(format!("stores/{id}.trajstore")));
        assert_eq!(value["store_id"], id);
        assert_eq!(value["batches"][0]["label"], "round-0001");
        assert!(value["batches"][0]["derived"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}

#[test]
fn watch_does_not_commit_or_report_success_after_pack_errors() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    let adapter = temp.path().join("adapter.toml");
    fs::write(
        &adapter,
        "name = 'fixture'\nrules = 'none'\n[batch_ready]\nrun_glob = 'run-*'\nmarkers = ['DONE']\n",
    )
    .unwrap();
    init_repo(&repo, &data, adapter.to_str().unwrap());
    let source = data.join("run-1");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("DONE"), "done").unwrap();
    fs::write(
        source.join(std::ffi::OsString::from_vec(b"bad-\xff".to_vec())),
        "evidence",
    )
    .unwrap();
    let before = git(&repo, &["rev-parse", "HEAD"]);
    let output = traj()
        .current_dir(&repo)
        .args([
            "watch",
            "--max-batches",
            "1",
            "--interval",
            "1",
            "--no-derive",
            "--commit",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not valid UTF-8"));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before);
}

#[test]
fn watch_reports_corrupt_existing_manifest_even_without_a_new_marker() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    let adapter = temp.path().join("adapter.toml");
    fs::write(
        &adapter,
        "name = 'fixture'\nrules = 'none'\n[batch_ready]\nrun_glob = 'run-*'\nmarkers = ['DONE']\n",
    )
    .unwrap();
    init_repo(&repo, &data, adapter.to_str().unwrap());
    fs::create_dir(data.join("run-1")).unwrap();
    let store = repo.join("stores/run-1.trajstore");
    fs::create_dir(&store).unwrap();
    fs::write(store.join("MANIFEST.json"), "broken").unwrap();
    let output = traj()
        .current_dir(&repo)
        .args([
            "watch",
            "--max-batches",
            "1",
            "--interval",
            "1",
            "--no-derive",
        ])
        .timeout(std::time::Duration::from_secs(10))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("parse"));
    assert_eq!(
        fs::read_to_string(store.join("MANIFEST.json")).unwrap(),
        "broken"
    );
}
