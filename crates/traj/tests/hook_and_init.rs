//! `traj init`, `traj commit` and the pre-commit hook: the repository keeps stores but never raw run trees,
//! `init` scaffolds configuration (adapter template, VS Code settings, linked worktrees) without clobbering
//! what the user has, configuration selection never falls back silently, `commit` stages only the store, and
//! the hook accepts only complete, symlink-free stores under `store_root`.

mod common;

use common::*;
use std::fs;
use std::path::Path;

#[test]
fn init_pack_and_commit_keep_raw_trees_out_of_the_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    git_init(&repo);
    let data = tmp.path().join("data");
    // init refuses a data_root inside the repo
    traj()
        .current_dir(&repo)
        .args(["init", "--data-root"])
        .arg(repo.join("runs"))
        .assert()
        .failure();
    // the adapter lives in the target repo
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    fs::copy(adapter(), repo.join("trajfs/adapter.toml")).unwrap();
    fs::copy(
        fixtures_dir().join("rounds-rules.toml"),
        repo.join("trajfs/rounds-rules.toml"),
    )
    .unwrap();
    traj()
        .current_dir(&repo)
        .args(["init", "--data-root"])
        .arg(&data)
        .args(["--adapter", "trajfs/adapter.toml", "--rules", "none"])
        .assert()
        .success();
    assert!(repo.join("trajfs.toml").is_file());
    let cfg = fs::read_to_string(repo.join("trajfs.toml")).unwrap();
    assert!(
        cfg.contains("rounds/round-"),
        "hook patterns copied from the adapter: {cfg}"
    );
    assert!(fs::read_link(repo.join(".git/hooks/pre-commit")).is_ok());
    assert!(repo.join(".claude/skills/traj/SKILL.md").is_file());
    assert!(fs::read_to_string(repo.join("AGENTS.md"))
        .unwrap()
        .contains("traj-skill:start"));
    assert!(data.join(".gitignore").is_file());
    traj().current_dir(&repo).arg("doctor").assert().success();
    git(&repo, &["add", "trajfs.toml"]);
    git(
        &repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "traj policy",
        ],
    );
    // pack refuses a store inside data_root and a source inside store_root
    fixture(&data.join("run1"));
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(data.join("run1"))
        .arg("--out")
        .arg(data.join("x"))
        .assert()
        .failure();
    fs::create_dir_all(repo.join("stores/fake")).unwrap();
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(repo.join("stores/fake"))
        .assert()
        .failure();
    // the hook refuses raw run paths
    fs::create_dir_all(repo.join("raw/rounds/round-0001")).unwrap();
    fs::write(repo.join("raw/rounds/round-0001/x.stdout"), "x").unwrap();
    git(&repo, &["add", "raw"]);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-q", "-m", "raw"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "commit of raw paths must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("traj pack"));
    git(&repo, &["reset", "-q"]);
    // deleting tracked raw paths (the migration) is allowed
    fs::create_dir_all(repo.join("old/rounds/round-0001")).unwrap();
    fs::write(repo.join("old/rounds/round-0001/y.stdout"), "y").unwrap();
    git(&repo, &["add", "old"]);
    git(
        &repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "-m",
            "legacy raw tree",
        ],
    );
    git(&repo, &["rm", "-r", "-q", "--cached", "old"]);
    git(
        &repo,
        &["commit", "-q", "-m", "migrate: stop tracking raw tree"],
    );
    // the intended path: pack into store_root, traj commit
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(data.join("run1"))
        .args(["--label", "r1"])
        .assert()
        .success();
    let store = repo.join("stores/run1.trajstore");
    assert!(store.join("MANIFEST.json").is_file());
    git(
        &repo,
        &[
            "add",
            "trajfs.toml",
            "trajfs",
            "stores/.gitkeep",
            "stores/.gitattributes",
            ".claude",
            "AGENTS.md",
        ],
    );
    git(&repo, &["commit", "-q", "-m", "init"]);
    traj()
        .current_dir(&repo)
        .args(["commit"])
        .arg(&store)
        .assert()
        .success();
    let log = git(&repo, &["log", "--oneline"]);
    assert!(log.contains("trajstore run1: batch 1 r1"));
    let s = git(&repo, &["ls-files", "stores"]);
    assert!(
        s.contains("stores/run1.trajstore/MANIFEST.json")
            && s.contains("stores/run1.trajstore/packs/0001.pack"),
        "{s}"
    );
    // a clone reads the store unchanged
    let clone = tmp.path().join("clone");
    let st = std::process::Command::new("git")
        .args(["clone", "-q"])
        .arg(&repo)
        .arg(&clone)
        .status()
        .unwrap();
    assert!(st.success());
    traj()
        .arg("-S")
        .arg(clone.join("stores/run1.trajstore"))
        .args(["verify", "--deep"])
        .assert()
        .success();
    traj()
        .current_dir(&repo)
        .args(["hook", "check-tree", "HEAD"])
        .assert()
        .success();

    // push of the store vs the raw tree against a local bare remote: the store is fewer git paths
    // (the elapsed times are printed for reference only; `traj bench` is the place for timing)
    let bare = tmp.path().join("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare)
        .status()
        .unwrap()
        .success());
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    let t0 = std::time::Instant::now();
    git(&repo, &["push", "-q", "origin", "HEAD:store"]);
    let push_store = t0.elapsed();
    let raw_repo = tmp.path().join("rawrepo");
    git_init(&raw_repo);
    fixture(&raw_repo.join("run1"));
    git(&raw_repo, &["add", "."]);
    git(&raw_repo, &["commit", "-q", "-m", "raw"]);
    git(
        &raw_repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    let t0 = std::time::Instant::now();
    git(&raw_repo, &["push", "-q", "origin", "HEAD:raw"]);
    let push_raw = t0.elapsed();
    let n_store = git(&repo, &["ls-files", "stores"]).lines().count();
    let n_raw = git(&raw_repo, &["ls-files"]).lines().count();
    eprintln!("push: store {n_store} files in {push_store:?}; raw {n_raw} files in {push_raw:?}");
    assert!(
        n_store < n_raw,
        "the store must be fewer git paths than the raw tree ({n_store} vs {n_raw})"
    );
}

#[test]
fn init_scaffolds_an_adapter_for_the_target_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    git_init(&repo);
    traj()
        .current_dir(&repo)
        .args(["init", "--scaffold-adapter", "--data-root"])
        .arg(tmp.path().join("data"))
        .assert()
        .success();
    assert!(repo.join("trajfs/adapter.toml").is_file() && repo.join("trajfs/rules.toml").is_file());
    let cfg = fs::read_to_string(repo.join("trajfs.toml")).unwrap();
    assert!(cfg.contains("adapter = \"trajfs/adapter.toml\""), "{cfg}");
    traj().current_dir(&repo).arg("doctor").assert().success();
    // the template loads as-is (name my-runner, no attrs) and packs
    fixture(&tmp.path().join("data/run1"));
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(tmp.path().join("data/run1"))
        .assert()
        .success();
    let m = manifest(&repo.join("stores/run1.trajstore"));
    assert_eq!(m["adapter"]["name"], "my-runner");
    // without --scaffold-adapter, --no-adapter or an --adapter, a non-tty init falls back to the built-in `none`
    let repo2 = tmp.path().join("repo2");
    git_init(&repo2);
    traj()
        .current_dir(&repo2)
        .args(["init", "--data-root"])
        .arg(tmp.path().join("data2"))
        .assert()
        .success();
    assert!(fs::read_to_string(repo2.join("trajfs.toml"))
        .unwrap()
        .contains("adapter = \"none\""));
}

#[test]
fn init_mount_root_writes_the_vscode_watcher_exclude() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    git_init(&repo);
    // an existing settings file with unrelated keys is merged, not replaced
    write(
        &repo.join(".vscode/settings.json"),
        b"{ \"editor.tabSize\": 2, \"files.watcherExclude\": { \"**/target/**\": true } }",
    );
    let mnt = tmp.path().join("traj-mnt");
    traj()
        .current_dir(&repo)
        .args(["init", "--no-adapter", "--data-root"])
        .arg(tmp.path().join("data"))
        .arg("--mount-root")
        .arg(&mnt)
        .assert()
        .success();
    let cfg = fs::read_to_string(repo.join("trajfs.toml")).unwrap();
    assert!(cfg.contains("mount_root = "), "{cfg}");
    let s: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".vscode/settings.json")).unwrap())
            .unwrap();
    assert_eq!(s["editor.tabSize"], 2);
    let ex = s["files.watcherExclude"].as_object().unwrap();
    assert_eq!(ex["**/target/**"], true);
    let key = format!(
        "{}/**",
        mnt.canonicalize().unwrap_or_else(|_| mnt.clone()).display()
    );
    assert_eq!(ex[&key], true, "{ex:?}");
    assert!(
        !ex.contains_key("**/traj-mnt/**"),
        "no basename pattern: {ex:?}"
    );
    assert!(
        ex.keys()
            .any(|k| k.ends_with("/traj-mnt/**") && k.starts_with('/')),
        "{ex:?}"
    );
    assert_eq!(s["files.readonlyInclude"][&key], true);
    assert_eq!(s["search.followSymlinks"], false);
    // a non-JSON settings file is left alone and reported
    fs::write(repo.join(".vscode/settings.json"), "not json").unwrap();
    traj()
        .current_dir(&repo)
        .args(["init", "--no-adapter", "--force", "--data-root"])
        .arg(tmp.path().join("data"))
        .arg("--mount-root")
        .arg(&mnt)
        .assert()
        .failure()
        .stderr(predicates::str::contains("not JSON"));
    assert_eq!(
        fs::read_to_string(repo.join(".vscode/settings.json")).unwrap(),
        "not json"
    );
}

#[test]
fn init_supports_linked_worktrees_and_preserves_existing_attributes() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    init_repo(&repo, &data, "none");
    let linked = temp.path().join("linked");
    git(
        &repo,
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    let other_data = temp.path().join("other-data");
    traj()
        .current_dir(&linked)
        .args([
            "init",
            "--adapter",
            "none",
            "--rules",
            "none",
            "--no-skill",
            "--data-root",
        ])
        .arg(&other_data)
        .assert()
        .success();
    assert!(linked.join(".git").is_file());
    assert!(fs::read_link(repo.join(".git/hooks/pre-commit")).is_ok());
    let attributes = linked.join("stores/.gitattributes");
    fs::write(&attributes, "*.custom binary\n").unwrap();
    traj()
        .current_dir(&linked)
        .args([
            "init",
            "--adapter",
            "none",
            "--rules",
            "none",
            "--no-skill",
            "--data-root",
        ])
        .arg(&other_data)
        .assert()
        .success();
    assert_eq!(fs::read_to_string(attributes).unwrap(), "*.custom binary\n");
}

#[test]
fn rejected_hook_replacement_does_not_overwrite_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    init_repo(&repo, &data, "none");
    let hook = repo.join(".git/hooks/pre-commit");
    fs::remove_file(&hook).unwrap();
    fs::write(&hook, "existing user hook").unwrap();
    let config = fs::read(repo.join("trajfs.toml")).unwrap();
    let new_data = temp.path().join("new-data");
    traj()
        .current_dir(&repo)
        .args(["init", "--adapter", "none", "--no-skill", "--data-root"])
        .arg(&new_data)
        .assert()
        .failure();
    assert_eq!(fs::read(repo.join("trajfs.toml")).unwrap(), config);
    assert_eq!(fs::read_to_string(hook).unwrap(), "existing user hook");
    assert!(!new_data.exists());
}

#[test]
fn commit_scopes_literal_store_paths_and_preserves_other_staging() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    init_repo(&repo, &data, "none");
    let run = data.join("run[1]");
    fs::create_dir(&run).unwrap();
    fs::write(run.join("stdout"), "retained\n").unwrap();
    traj()
        .current_dir(&repo)
        .arg("pack")
        .arg(&run)
        .arg("--out")
        .arg(repo.join("stores/run[1].trajstore"))
        .arg("--no-derive")
        .assert()
        .success();
    fs::write(repo.join("note.txt"), "staged\n").unwrap();
    git(&repo, &["add", "note.txt"]);
    fs::write(repo.join("note.txt"), "unstaged\n").unwrap();
    traj()
        .current_dir(&repo)
        .args(["commit", "stores/run[1].trajstore"])
        .assert()
        .success();
    assert_eq!(git(&repo, &["show", "HEAD:note.txt"]), "original\n");
    assert_eq!(git(&repo, &["show", ":note.txt"]), "staged\n");
    assert_eq!(
        fs::read_to_string(repo.join("note.txt")).unwrap(),
        "unstaged\n"
    );
    let changed = git(&repo, &["diff", "--name-only", "HEAD^", "HEAD"]);
    assert!(changed
        .lines()
        .all(|path| path.starts_with("stores/run[1].trajstore/")));
    let head = git(&repo, &["rev-parse", "HEAD"]);
    traj()
        .current_dir(&repo)
        .args(["commit", "stores/run[1].trajstore"])
        .assert()
        .success();
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repo, &["show", ":note.txt"]), "staged\n");
}

#[test]
fn malformed_or_missing_selected_config_never_falls_back_to_default_rules() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("evidence.txt"), "data").unwrap();
    fs::write(temp.path().join("trajfs.toml"), "not valid = [toml").unwrap();
    let output = traj()
        .current_dir(temp.path())
        .arg("pack")
        .arg(&source)
        .arg("--out")
        .arg(temp.path().join("store"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("parse configuration"));
    assert!(!temp.path().join("store").exists());

    let output = traj()
        .current_dir(temp.path())
        .env("TRAJ_CONFIG", "absent.toml")
        .arg("pack")
        .arg(&source)
        .arg("--out")
        .arg(temp.path().join("store"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("read configuration"));
    assert!(!temp.path().join("store").exists());
}

#[test]
fn relative_explicit_config_resolves_its_own_adapter_and_store_root() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("data/run");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir(temp.path().join("settings")).unwrap();
    fs::write(source.join("evidence.txt"), "data").unwrap();
    fs::write(
        temp.path().join("settings/adapter.toml"),
        "name = 'fixture'\nrules = 'none'\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("settings/custom.toml"),
        format!(
            "data_root = {:?}\nstore_root = 'stores'\nadapter = 'adapter.toml'\nrules = 'none'\n",
            temp.path().join("data")
        ),
    )
    .unwrap();
    traj()
        .current_dir(temp.path())
        .env("TRAJ_CONFIG", "settings/custom.toml")
        .arg("pack")
        .arg(&source)
        .arg("--no-derive")
        .assert()
        .success();
    let store = temp.path().join("settings/stores/run.trajstore");
    assert!(store.join("MANIFEST.json").is_file());
    assert!(trajfs_core::Store::open(&store)
        .unwrap()
        .verify(true)
        .unwrap()
        .ok());
}

/// A repository with a committed `trajfs.toml` (adapter `none`) and an empty `stores/`, for the hook tests.
fn hook_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    git_init(repo);
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

fn hook(repo: &Path, args: &[&str]) -> std::process::Output {
    traj()
        .current_dir(repo)
        .arg("hook")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn pre_commit_requires_trajfs_stores_under_store_root() {
    let tmp = hook_repo();
    let repo = tmp.path();

    fs::create_dir(repo.join("stores/raw-result")).unwrap();
    fs::write(repo.join("stores/raw-result/events.jsonl"), "{}\n").unwrap();
    git(repo, &["add", "stores/raw-result"]);
    let output = hook(repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("nested under store_root but not inside a .trajstore"));

    git(repo, &["reset", "-q", "HEAD"]);
    fs::remove_dir_all(repo.join("stores/raw-result")).unwrap();
    fs::create_dir(repo.join("stores/broken.trajstore")).unwrap();
    fs::write(repo.join("stores/broken.trajstore/events.jsonl"), "{}\n").unwrap();
    git(repo, &["add", "stores/broken.trajstore"]);
    let output = hook(repo, &["pre-commit"]);
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
    let output = hook(repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is incomplete in the staged snapshot")
    );
    git(repo, &["commit", "-q", "-m", "missing segment"]);
    let output = hook(repo, &["check-tree", "HEAD"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("is incomplete in commit HEAD"));
}

#[test]
fn pre_commit_uses_staged_config_and_rejects_config_symlinks() {
    let tmp = hook_repo();
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
    let output = hook(repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("raw run paths staged"));

    git(repo, &["reset", "-q", "HEAD", "--", "raw/result.json"]);
    fs::remove_dir_all(repo.join("raw")).unwrap();
    git(repo, &["checkout", "HEAD", "--", "trajfs.toml"]);
    fs::write(repo.join("large.bin"), vec![0u8; 2048]).unwrap();
    git(repo, &["add", "large.bin"]);
    fs::write(repo.join("large.bin"), b"x").unwrap();
    let output = hook(repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("large.bin is 2048 bytes"));
    git(repo, &["reset", "-q", "HEAD", "--", "large.bin"]);
    fs::remove_file(repo.join("large.bin")).unwrap();

    fs::remove_file(repo.join("trajfs.toml")).unwrap();
    std::os::unix::fs::symlink("stores/.gitkeep", repo.join("trajfs.toml")).unwrap();
    git(repo, &["add", "trajfs.toml"]);
    let output = hook(repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular Git blob"));

    git(repo, &["commit", "-q", "-m", "symlink config"]);
    let output = hook(repo, &["check-tree", "HEAD"]);
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
    git_init(&repo);
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
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(&raw)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "jsonl", "--rules", "none"])
        .assert()
        .success();

    let m = manifest(&store);
    let segments = m["batches"][0]["segments"].as_array().unwrap();
    assert!(segments.len() > 1, "{segments:?}");
    let derived = m["batches"][0]["derived"].as_array().unwrap();
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

    traj()
        .current_dir(&repo)
        .arg("-S")
        .arg(&store)
        .args(["verify"])
        .assert()
        .success();
    traj()
        .current_dir(&repo)
        .arg("-S")
        .arg(&store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    let find = traj()
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
    if sql_available() {
        let sql = traj()
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
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }

    git(&repo, &["add", "stores/segmented.trajstore"]);
    assert!(hook(&repo, &["pre-commit"]).status.success());
    git(&repo, &["commit", "-q", "-m", "valid store"]);
    assert!(hook(&repo, &["check-tree", "HEAD"]).status.success());

    let rel = |p: &Path| p.strip_prefix(&repo).unwrap().to_str().unwrap().to_string();
    let manifest_path = store.join("MANIFEST.json");
    fs::remove_file(&manifest_path).unwrap();
    std::os::unix::fs::symlink("../../trajfs.toml", &manifest_path).unwrap();
    git(&repo, &["add", &rel(&manifest_path)]);
    let output = hook(&repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular Git blob"));
    git(&repo, &["checkout", "HEAD", "--", &rel(&manifest_path)]);

    fs::remove_file(&derived_path).unwrap();
    std::os::unix::fs::symlink(repo.join("trajfs.toml"), &derived_path).unwrap();
    git(&repo, &["add", &rel(&derived_path)]);
    let output = hook(&repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular Git blob"));
    git(&repo, &["commit", "-q", "-m", "symlink artifact"]);
    let output = hook(&repo, &["check-tree", "HEAD"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("regular Git blob"));
    git(&repo, &["checkout", "HEAD^", "--", &rel(&derived_path)]);
    git(&repo, &["commit", "-q", "-m", "restore regular artifact"]);

    let surplus = store.join("catalog/files-9999.parquet");
    fs::write(&surplus, b"junk").unwrap();
    git(&repo, &["add", &rel(&surplus)]);
    let output = hook(&repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("contains unreferenced finalized artifact"));
    git(&repo, &["commit", "-q", "-m", "surplus"]);
    let output = hook(&repo, &["check-tree", "HEAD"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("contains unreferenced finalized artifact"));
    git(&repo, &["rm", "-q", &rel(&surplus)]);
    git(&repo, &["commit", "-q", "-m", "remove surplus"]);

    let surplus_derived = store.join("derived/jsonl/events-surplus-0001.parquet");
    fs::write(&surplus_derived, b"junk").unwrap();
    git(&repo, &["add", &rel(&surplus_derived)]);
    let output = hook(&repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("contains unreferenced finalized artifact"));
    fs::remove_file(&surplus_derived).unwrap();
    git(&repo, &["add", "-A", &rel(&surplus_derived)]);

    fs::remove_file(&derived_path).unwrap();
    git(&repo, &["add", &rel(&derived_path)]);
    let output = hook(&repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is incomplete in the staged snapshot")
    );
    git(&repo, &["checkout", "HEAD", "--", &rel(&derived_path)]);

    let suffix = segment.strip_prefix("files-").unwrap();
    let missing = store.join(format!("catalog/dirs-{suffix}.parquet"));
    fs::remove_file(&missing).unwrap();
    git(&repo, &["add", &rel(&missing)]);
    let output = hook(&repo, &["pre-commit"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is incomplete in the staged snapshot")
    );
}
