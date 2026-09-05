use assert_cmd::Command;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

fn traj() -> Command {
    let mut command = Command::cargo_bin("traj").unwrap();
    command.env_remove("TRAJ_CONFIG").env_remove("TRAJ_STORE");
    command
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init_repo(repo: &Path, data: &Path, adapter: &str) {
    fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "test@example.invalid"]);
    git(repo, &["config", "user.name", "Test"]);
    traj()
        .current_dir(repo)
        .args(["init", "--data-root"])
        .arg(data)
        .args(["--adapter", adapter, "--rules", "none", "--no-skill"])
        .assert()
        .success();
    fs::write(repo.join("note.txt"), "original\n").unwrap();
    git(
        repo,
        &[
            "add",
            "trajfs.toml",
            "stores/.gitattributes",
            "stores/.gitkeep",
            "note.txt",
        ],
    );
    git(repo, &["commit", "-qm", "initial"]);
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

#[test]
fn default_pack_ids_distinguish_nested_runs_with_the_same_name() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    init_repo(&repo, &data, "none");
    for task in ["task-a", "task-b"] {
        let source = data.join(task).join("run1");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("evidence.txt"), task).unwrap();
        traj()
            .current_dir(&repo)
            .arg("pack")
            .arg(&source)
            .arg("--no-derive")
            .assert()
            .success();
        let store = repo.join(format!("stores/{task}%2Frun1.trajstore"));
        let saved = trajfs_core::Store::open(&store).unwrap();
        let row = saved.stat("evidence.txt").unwrap().unwrap();
        assert_eq!(
            saved.read_sha(&mut saved.reader(), &row.sha).unwrap(),
            task.as_bytes()
        );
    }

    assert!(!repo.join("stores/run1.trajstore").exists());
}

#[test]
fn stored_filename_whitespace_is_not_trimmed_by_cli_normalization() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    init_repo(&repo, &data, "none");
    let run = data.join("run");
    fs::create_dir(&run).unwrap();
    fs::write(run.join(" leading and trailing "), "retained").unwrap();
    traj()
        .current_dir(&repo)
        .arg("pack")
        .arg(&run)
        .arg("--no-derive")
        .assert()
        .success();
    let output = traj()
        .current_dir(&repo)
        .args([
            "-S",
            "stores/run.trajstore",
            "cat",
            " leading and trailing ",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"retained");
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
