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
    git(repo, &["commit", "-q", "-m", "missing segment"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "check-tree", "HEAD"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("is incomplete in commit HEAD"));
}

#[test]
fn pre_commit_uses_staged_config_and_rejects_config_symlinks() {
    let tmp = setup();
    let repo = tmp.path();
    fs::write(
        repo.join("trajfs.toml"),
        format!(
            "data_root = {:?}\nstore_root = \"stores\"\nadapter = \"none\"\nrules = \"none\"\n\
             [hook]\nraw_patterns = ['^raw/']\nmax_file_bytes = 1024\n",
            repo.join("raw")
        ),
    )
    .unwrap();
    git(repo, &["add", "trajfs.toml"]);
    git(repo, &["commit", "-q", "-m", "strict policy"]);

    fs::create_dir_all(repo.join("raw")).unwrap();
    fs::write(repo.join("raw/result.json"), "{}\n").unwrap();
    git(repo, &["add", "raw/result.json"]);
    fs::write(
        repo.join("trajfs.toml"),
        format!(
            "data_root = {:?}\nstore_root = \"stores\"\nadapter = \"none\"\nrules = \"none\"\n\
             [hook]\nraw_patterns = []\nmax_file_bytes = 999999999\n",
            repo.join("raw")
        ),
    )
    .unwrap();
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("raw run paths staged"));

    git(repo, &["reset", "-q", "HEAD", "--", "raw/result.json"]);
    fs::remove_dir_all(repo.join("raw")).unwrap();
    git(repo, &["checkout", "HEAD", "--", "trajfs.toml"]);
    fs::write(repo.join("large.bin"), vec![0u8; 2048]).unwrap();
    git(repo, &["add", "large.bin"]);
    fs::write(repo.join("large.bin"), b"x").unwrap();
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("large.bin is 2048 bytes"));
    git(repo, &["reset", "-q", "HEAD", "--", "large.bin"]);
    fs::remove_file(repo.join("large.bin")).unwrap();

    fs::remove_file(repo.join("trajfs.toml")).unwrap();
    std::os::unix::fs::symlink("stores/.gitkeep", repo.join("trajfs.toml")).unwrap();
    git(repo, &["add", "trajfs.toml"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular Git blob"));

    git(repo, &["commit", "-q", "-m", "symlink config"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(repo)
        .args(["hook", "check-tree", "HEAD"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular Git blob"));
}

#[test]
fn segmented_store_stays_under_limit_and_is_hook_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let raw = tmp.path().join("raw");
    fs::create_dir_all(repo.join("stores")).unwrap();
    fs::create_dir_all(&raw).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    let max_bytes = 16 << 10;
    fs::write(
        repo.join("trajfs.toml"),
        format!(
            "data_root = {:?}\nstore_root = \"stores\"\nadapter = \"jsonl\"\nrules = \"none\"\n\
             [hook]\nmax_file_bytes = {max_bytes}\n",
            raw
        ),
    )
    .unwrap();
    fs::write(repo.join("stores/.gitkeep"), "").unwrap();
    git(&repo, &["add", "trajfs.toml", "stores/.gitkeep"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    for i in 0..600u32 {
        let path = raw.join(format!(
            "rounds/round-{i:04}/builder/long-component-{i:08x}/events.jsonl"
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "{{\"type\":\"event-{i}\",\"id\":\"{i:08x}\",\"payload\":\"{}\"}}\n",
                format!("{:08x}", i.wrapping_mul(2_654_435_761)).repeat(32)
            ),
        )
        .unwrap();
    }
    let store = repo.join("stores/segmented.trajstore");
    Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["pack"])
        .arg(&raw)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "jsonl", "--rules", "none"])
        .assert()
        .success();

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(store.join("MANIFEST.json")).unwrap()).unwrap();
    let segments = manifest["batches"][0]["segments"].as_array().unwrap();
    assert!(segments.len() > 1, "{segments:?}");
    let derived = manifest["batches"][0]["derived"].as_array().unwrap();
    assert!(derived.len() > 1);
    let segment = segments[1].as_str().unwrap().to_string();
    let derived_path = store
        .join("derived")
        .join(derived[0].as_str().unwrap())
        .with_extension("parquet");
    for entry in walkdir::WalkDir::new(&store).min_depth(1) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            assert!(
                entry.metadata().unwrap().len() <= max_bytes,
                "{} exceeds {max_bytes} bytes",
                entry.path().display()
            );
        }
    }

    Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .arg("-S")
        .arg(&store)
        .args(["verify"])
        .assert()
        .success();
    Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .arg("-S")
        .arg(&store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    let find = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .arg("-S")
        .arg(&store)
        .args(["find"])
        .output()
        .unwrap();
    assert!(find.status.success());
    let found: Vec<&str> = std::str::from_utf8(&find.stdout).unwrap().lines().collect();
    assert_eq!(found.len(), 600);
    assert!(found
        .iter()
        .all(|path| path.starts_with("rounds/") && !path.starts_with('/')));
    assert!(found.windows(2).all(|pair| pair[0] < pair[1]));
    let sql = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .arg("-S")
        .arg(&store)
        .args([
            "sql",
            "--csv",
            "select (select count(*) from files where batch = 1 and path not like '/%') as files, \
             (select count(*) from events) as events",
        ])
        .output()
        .unwrap();
    assert!(sql.status.success());
    assert!(
        String::from_utf8_lossy(&sql.stdout).contains("\n600,600"),
        "{}",
        String::from_utf8_lossy(&sql.stdout)
    );

    git(&repo, &["add", "stores/segmented.trajstore"]);
    Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "pre-commit"])
        .assert()
        .success();
    git(&repo, &["commit", "-q", "-m", "valid store"]);
    Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "check-tree", "HEAD"])
        .assert()
        .success();

    let manifest_path = store.join("MANIFEST.json");
    fs::remove_file(&manifest_path).unwrap();
    std::os::unix::fs::symlink("../../trajfs.toml", &manifest_path).unwrap();
    git(
        &repo,
        &[
            "add",
            manifest_path.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular Git blob"));
    git(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--",
            manifest_path.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );

    fs::remove_file(&derived_path).unwrap();
    std::os::unix::fs::symlink(repo.join("trajfs.toml"), &derived_path).unwrap();
    git(
        &repo,
        &[
            "add",
            derived_path.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular Git blob"));
    git(&repo, &["commit", "-q", "-m", "symlink artifact"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "check-tree", "HEAD"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("regular Git blob"));
    git(
        &repo,
        &[
            "checkout",
            "HEAD^",
            "--",
            derived_path.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );
    git(&repo, &["commit", "-q", "-m", "restore regular artifact"]);

    let surplus = store.join("catalog/files-9999.parquet");
    fs::write(&surplus, b"junk").unwrap();
    git(
        &repo,
        &[
            "add",
            surplus.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("contains unreferenced finalized artifact"));
    git(&repo, &["commit", "-q", "-m", "surplus"]);
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "check-tree", "HEAD"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("contains unreferenced finalized artifact"));
    git(
        &repo,
        &[
            "rm",
            "-q",
            surplus.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );
    git(&repo, &["commit", "-q", "-m", "remove surplus"]);

    let surplus_derived = store.join("derived/jsonl/events-surplus-0001.parquet");
    fs::write(&surplus_derived, b"junk").unwrap();
    git(
        &repo,
        &[
            "add",
            surplus_derived
                .strip_prefix(&repo)
                .unwrap()
                .to_str()
                .unwrap(),
        ],
    );
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("contains unreferenced finalized artifact"));
    fs::remove_file(&surplus_derived).unwrap();
    git(
        &repo,
        &[
            "add",
            "-A",
            surplus_derived
                .strip_prefix(&repo)
                .unwrap()
                .to_str()
                .unwrap(),
        ],
    );

    fs::remove_file(&derived_path).unwrap();
    git(
        &repo,
        &[
            "add",
            derived_path.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is incomplete in the staged snapshot")
    );
    git(
        &repo,
        &[
            "checkout",
            "HEAD",
            "--",
            derived_path.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );

    let suffix = segment.strip_prefix("files-").unwrap();
    let missing = store.join(format!("catalog/dirs-{suffix}.parquet"));
    fs::remove_file(&missing).unwrap();
    git(
        &repo,
        &[
            "add",
            missing.strip_prefix(&repo).unwrap().to_str().unwrap(),
        ],
    );
    let output = Command::cargo_bin("traj")
        .unwrap()
        .current_dir(&repo)
        .args(["hook", "pre-commit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is incomplete in the staged snapshot")
    );
}
