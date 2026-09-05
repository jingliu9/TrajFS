use assert_cmd::Command;
use std::fs;
use std::path::PathBuf;
use trajfs_core::catalog;
use trajfs_core::ingest::{ingest, IngestOptions};
use trajfs_core::pack::IndexRow;
use trajfs_core::rules::Rules;
use trajfs_core::{NoAdapter, Store};

struct Fixture {
    dir: tempfile::TempDir,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("review-viewer-")
            .tempdir_in(".")
            .unwrap();
        let base = dir.path().canonicalize().unwrap();
        let source = base.join("source");
        let store = base.join("store");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a.txt"), b"match aaa\n").unwrap();
        fs::write(source.join("b.txt"), b"match bbb\n").unwrap();
        fs::write(source.join("empty"), b"").unwrap();
        ingest(
            &source,
            &store,
            IngestOptions {
                rules: Rules::resolve("none").unwrap(),
                rules_name: "none".into(),
                adapter: &NoAdapter,
                label: "viewer regression".into(),
                jobs: 2,
                derive: false,
                store_id: None,
            },
        )
        .unwrap();
        Self { dir, store }
    }

    fn traj(&self) -> Command {
        let mut cmd = Command::cargo_bin("traj").unwrap();
        cmd.current_dir(self.dir.path())
            .env_remove("TRAJ_CONFIG")
            .env_remove("TRAJ_STORE")
            .arg("-S")
            .arg(&self.store);
        cmd
    }
}

#[test]
fn review_tree_rejects_a_missing_directory() {
    let fixture = Fixture::new();
    fixture.traj().args(["tree", "missing"]).assert().code(2);
}

#[test]
fn review_find_rejects_a_missing_directory() {
    let fixture = Fixture::new();
    fixture.traj().args(["find", "missing"]).assert().code(2);
}

#[test]
fn review_grep_rejects_a_missing_directory() {
    let fixture = Fixture::new();
    fixture
        .traj()
        .args(["grep", "-e", "match", "--path", "missing"])
        .assert()
        .code(2);
}

#[test]
fn review_recursive_ls_lists_the_root_once() {
    let fixture = Fixture::new();
    let output = fixture.traj().args(["ls", "-R"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|line| *line == ".:")
            .count(),
        1
    );
}

#[test]
#[cfg(unix)]
fn review_find_reports_stdout_write_errors() {
    let fixture = Fixture::new();
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(reader);
    let result = std::process::Command::new(assert_cmd::cargo::cargo_bin("traj"))
        .current_dir(fixture.dir.path())
        .env_remove("TRAJ_CONFIG")
        .env_remove("TRAJ_STORE")
        .arg("-S")
        .arg(&fixture.store)
        .arg("find")
        .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            writer,
        )))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2), "{result:?}");
}

#[test]
fn review_grep_verifies_content_and_reports_errors_even_with_matches() {
    let fixture = Fixture::new();
    let store = Store::open(&fixture.store).unwrap();
    let a = store.stat("a.txt").unwrap().unwrap();
    let b = store.stat("b.txt").unwrap().unwrap();
    let other = store.parts(&b.sha).unwrap()[0].clone();
    drop(store);
    let index = fs::read_dir(fixture.store.join("packs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("index-")
        })
        .unwrap();
    let mut rows = Vec::new();
    catalog::read_index(&index, |sha, loc| {
        rows.push(IndexRow {
            sha,
            loc: if sha == a.sha { other.clone() } else { loc },
        });
    })
    .unwrap();
    catalog::write_index(&index, &rows).unwrap();
    fixture
        .traj()
        .args(["grep", "-e", "match", "-l"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("b.txt"));
}

#[cfg(all(feature = "mount", target_os = "linux"))]
#[test]
fn review_invalid_mount_limits_fail_before_creating_a_mountpoint() {
    let fixture = Fixture::new();
    let mountpoint = fixture.dir.path().canonicalize().unwrap().join("mount");
    for (option, value) in [
        ("--ttl", "inf"),
        ("--ttl", "NaN"),
        ("--ttl", "1e99"),
        ("--blob-cache", "18446744073709551615G"),
        ("--memory", "NaN%"),
    ] {
        fixture
            .traj()
            .arg("mount")
            .arg(&mountpoint)
            .args(["--no-vscode", "--allow-in-repo", option, value])
            .assert()
            .code(2);
        assert!(!mountpoint.exists());
    }
}

#[cfg(all(feature = "mount", target_os = "linux"))]
#[test]
fn review_mount_reports_an_invalid_explicit_config() {
    let fixture = Fixture::new();
    let config = fixture
        .dir
        .path()
        .canonicalize()
        .unwrap()
        .join("invalid.toml");
    fs::write(&config, "invalid = [").unwrap();
    for selected in [
        config,
        PathBuf::from("invalid.toml"),
        PathBuf::from("missing.toml"),
    ] {
        fixture
            .traj()
            .env("TRAJ_CONFIG", &selected)
            .args(["mount", "--no-vscode"])
            .assert()
            .code(2)
            .stderr(predicates::str::contains(
                selected.file_name().unwrap().to_str().unwrap(),
            ));
    }
}

#[test]
fn review_healthy_browse_and_empty_file_reads_still_work() {
    let fixture = Fixture::new();
    fixture
        .traj()
        .args(["cat", "empty", "a.txt"])
        .assert()
        .success()
        .stdout("match aaa\n");
    fixture
        .traj()
        .args(["stat", "empty"])
        .assert()
        .success()
        .stdout(predicates::str::contains("size:     0"));
    fixture
        .traj()
        .args(["find", "--name", "*.txt"])
        .assert()
        .success()
        .stdout("a.txt\nb.txt\n");
    fixture
        .traj()
        .args(["grep", "-e", "aaa"])
        .assert()
        .success()
        .stdout("a.txt:1:match aaa\n");
    fixture
        .traj()
        .args(["grep", "-e", "not-present"])
        .assert()
        .code(1);
}

#[cfg(unix)]
#[test]
fn review_edit_accepts_editor_flags_and_keeps_the_store_unchanged() {
    let fixture = Fixture::new();
    let root = fixture.dir.path().canonicalize().unwrap();
    let script = root.join("editor script");
    fs::write(
        &script,
        "test \"$1\" = --wait || exit 9\nprintf 'edited\\n' > \"$2\"\n",
    )
    .unwrap();
    let copies = root.join("copies");
    fs::create_dir(&copies).unwrap();
    fixture
        .traj()
        .env("TMPDIR", &copies)
        .env("EDITOR", format!("/bin/sh '{}' --wait", script.display()))
        .args(["edit", "a.txt"])
        .assert()
        .success()
        .stdout(predicates::str::contains("+edited"));
    fixture
        .traj()
        .args(["cat", "a.txt"])
        .assert()
        .success()
        .stdout("match aaa\n");
    let edits: Vec<_> = fs::read_dir(&copies)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(fs::read(edits[0].join("a.txt")).unwrap(), b"edited\n");
    fixture
        .traj()
        .env("TMPDIR", &copies)
        .env("EDITOR", "/bin/true")
        .args(["edit", "a.txt"])
        .assert()
        .success()
        .stderr(predicates::str::contains("unchanged"));
    assert_eq!(fs::read_dir(copies).unwrap().count(), 1);
}
