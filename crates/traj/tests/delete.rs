//! `traj delete`: one path removed from every batch with survivors byte-identical, a Git checkpoint required
//! first, resurrection by later packs suppressed and recorded, and recovery of an interrupted apply
//! (see docs/PLAN-deletion.md).

mod common;

use common::*;
use std::fs;
use std::path::Path;

#[test]
fn delete_removes_one_path_from_every_batch_and_keeps_git_as_the_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    git_init(&repo);
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
        .args(["--adapter", "trajfs/adapter.toml", "--rules", "none"])
        .assert()
        .success();
    fixture(&data.join("run1"));
    let store = repo.join("stores/run1.trajstore");
    let pack = |label: &str| {
        traj()
            .current_dir(&repo)
            .args(["pack"])
            .arg(data.join("run1"))
            .args(["--label", label])
            .assert()
            .success();
    };
    pack("b1");
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );

    // the first batch is committed (git add -A above); now a second batch, uncommitted
    write(
        &data.join("run1/rounds/round-0002/builder/logs/a.stdout"),
        b"hello again\n",
    );
    write(
        &data.join("run1/rounds/round-0003/builder/logs/c.stdout"),
        b"three\n",
    );
    pack("b2");
    let dry = traj()
        .arg("-S")
        .arg(&store)
        .args(["delete", "rounds/round-0001", "--yes"])
        .output()
        .unwrap();
    assert!(!dry.status.success());
    assert!(
        String::from_utf8_lossy(&dry.stderr).contains("differs from HEAD"),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    traj()
        .current_dir(&repo)
        .args(["commit"])
        .arg(&store)
        .assert()
        .success();
    let manifest_before = fs::read(store.join("MANIFEST.json")).unwrap();
    let survivors_before = traj_lines(&store, &["find", "rounds/round-0002"]);
    assert!(!survivors_before.is_empty());

    // dry run changes nothing; unknown paths are refused
    let dry = traj()
        .arg("-S")
        .arg(&store)
        .args(["delete", "rounds/round-0001"])
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let text = String::from_utf8_lossy(&dry.stdout);
    assert!(
        text.contains("dry run") && text.contains("event rows removed: 4"),
        "{text}"
    );
    assert_eq!(
        fs::read(store.join("MANIFEST.json")).unwrap(),
        manifest_before
    );
    traj()
        .arg("-S")
        .arg(&store)
        .args(["delete", "rounds/round-0009", "--yes"])
        .assert()
        .failure();
    assert_eq!(
        fs::read(store.join("MANIFEST.json")).unwrap(),
        manifest_before
    );

    // apply
    traj()
        .arg("-S")
        .arg(&store)
        .args(["delete", "rounds/round-0001", "--yes"])
        .assert()
        .success();
    assert!(!repo.join("stores/run1.trajstore.deleting").exists());
    traj()
        .arg("-S")
        .arg(&store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    let gone = |store: &Path| {
        traj_lines(store, &["find"])
            .iter()
            .all(|l| !l.contains("round-0001"))
    };
    assert!(gone(&store));
    assert!(traj_lines(&store, &["ls", "rounds"])
        .iter()
        .all(|l| !l.contains("round-0001")));
    assert_eq!(
        traj_lines(&store, &["find", "rounds/round-0002"]),
        survivors_before
    );
    let c = traj()
        .arg("-S")
        .arg(&store)
        .args(["cat", "rounds/round-0002/builder/logs/a.stdout"])
        .output()
        .unwrap();
    assert_eq!(c.stdout, b"hello again\n");
    let big = traj()
        .arg("-S")
        .arg(&store)
        .args(["cat", "rounds/round-0002/builder/big.bin"])
        .output()
        .unwrap();
    assert_eq!(
        big.stdout,
        fs::read(data.join("run1/rounds/round-0002/builder/big.bin")).unwrap()
    );
    let m = manifest(&store);
    assert_eq!(m["format"], trajfs_core::FORMAT_VERSION);
    assert_eq!(m["deleted"][0]["path"], "rounds/round-0001");
    assert_eq!(m["batches"].as_array().unwrap().len(), 2);
    if sql_available() {
        let events = traj_stdout(
            &store,
            &[
                "sql",
                "--csv",
                "select count(*) from events where trajectory like 'rounds/round-0001/%'",
            ],
        );
        assert!(events.contains('0'), "{events}");
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }

    // the deletion commit; the earlier version stays in Git
    traj()
        .current_dir(&repo)
        .args(["commit"])
        .arg(&store)
        .assert()
        .success();
    assert!(git(&repo, &["log", "-1", "--format=%s"]).contains("delete rounds/round-0001"));
    traj()
        .current_dir(&repo)
        .args(["hook", "check-tree", "HEAD"])
        .assert()
        .success();
    let old = git(
        &repo,
        &["show", "HEAD~1:stores/run1.trajstore/MANIFEST.json"],
    );
    assert!(!old.contains("round-0001") || !old.contains("\"deleted\": [\n"));
    assert!(git(&repo, &["status", "--porcelain"]).trim().is_empty());

    // the raw tree still has the round: a new pack skips it and records why
    write(
        &data.join("run1/rounds/round-0003/builder/logs/d.stdout"),
        b"four\n",
    );
    pack("b3");
    assert!(gone(&store));
    assert!(traj_lines(&store, &["find", "--name", "d.stdout"]).len() == 1);
    if sql_available() {
        let excluded = traj_stdout(
            &store,
            &[
                "sql",
                "--csv",
                "select path from excluded where rule = 'deleted' order by path",
            ],
        );
        assert!(excluded.contains("rounds/round-0001"), "{excluded}");
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
    traj()
        .current_dir(&repo)
        .args(["commit"])
        .arg(&store)
        .assert()
        .success();
    let again = traj()
        .arg("-S")
        .arg(&store)
        .args(["delete", "rounds/round-0001/builder", "--yes"])
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already deleted"));

    // an interrupted apply: the sibling blocks pack, --recover discards it
    fs::create_dir(repo.join("stores/run1.trajstore.deleting")).unwrap();
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(data.join("run1"))
        .assert()
        .failure();
    traj()
        .arg("-S")
        .arg(&store)
        .args(["delete", "--recover"])
        .assert()
        .success();
    assert!(!repo.join("stores/run1.trajstore.deleting").exists());
    traj()
        .arg("-S")
        .arg(&store)
        .args(["verify"])
        .assert()
        .success();
}
