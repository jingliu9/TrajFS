use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "t"]);
    fs::create_dir(repo.join("stores")).unwrap();
    fs::write(
        repo.join("trajfs.toml"),
        format!(
            "data_root = {:?}\nstore_root = \"stores\"\nadapter = \"none\"\nrules = \"none\"\n",
            repo.join("raw")
        ),
    )
    .unwrap();
    fs::write(repo.join("stores/.gitkeep"), "").unwrap();
    git(repo, &["add", "trajfs.toml", "stores/.gitkeep"]);
    git(repo, &["commit", "-q", "-m", "init"]);
    tmp
}

#[test]
fn pre_commit_requires_trajfs_stores_under_store_root() {
    let tmp = setup();
    let repo = tmp.path();

    fs::create_dir(repo.join("stores/raw-result")).unwrap();
    fs::write(repo.join("stores/raw-result/events.jsonl"), "{}\n").unwrap();
    git(repo, &["add", "stores/raw-result"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("nested under store_root but not inside a .trajstore"));

    git(repo, &["reset", "-q", "HEAD"]);
    fs::remove_dir_all(repo.join("stores/raw-result")).unwrap();
    fs::create_dir(repo.join("stores/broken.trajstore")).unwrap();
    fs::write(repo.join("stores/broken.trajstore/events.jsonl"), "{}\n").unwrap();
    git(repo, &["add", "stores/broken.trajstore"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is not a recognized TrajFS store artifact"));
    assert!(stderr.contains("has no staged MANIFEST.json"));

    git(repo, &["reset", "-q", "HEAD"]);
    fs::remove_dir_all(repo.join("stores/broken.trajstore")).unwrap();
    fs::create_dir(repo.join("stores/incomplete.trajstore")).unwrap();
    fs::write(
        repo.join("stores/incomplete.trajstore/MANIFEST.json"),
        r#"{
  "format": 1,
  "store_id": "incomplete",
  "source": "/tmp/raw",
  "adapter": {"name": "none", "version": 1},
  "rules": {"name": "none", "version": 1},
  "batches": [{
    "id": 1,
    "created": "2026-09-04T00:00:00Z",
    "label": "test",
    "paths": 1,
    "bytes": 1,
    "new_blobs": 1,
    "new_blob_bytes": 1,
    "packed_bytes": 1,
    "packs": [1],
    "segments": ["files-0001"],
    "derived": [],
    "excluded": 0,
    "errors": [],
    "elapsed_ms": 1
  }]
}
"#,
    )
    .unwrap();
    git(repo, &["add", "stores/incomplete.trajstore"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is incomplete in the staged snapshot")
    );
}
