//! `traj pack` and `traj extract`: a pack followed by an extract reproduces the source tree byte for byte
//! (files, modes, symlinks, empty files, unicode names, big files), later packs add only new content, a pack
//! killed half-way leaves the store readable, and store ids and file names survive unchanged.

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn round_trip_is_byte_identical() {
    let e = packed("none");
    let out = extracted(&e);
    assert_eq!(snapshot(&e.src, &[]), snapshot(&out, &[]));
    // hard-link dedupe keeps content identical
    let out2 = e.tmp.path().join("out2");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["extract", "", "--hardlink-dedupe"])
        .arg(&out2)
        .assert()
        .success();
    assert_eq!(snapshot(&e.src, &[]), snapshot(&out2, &[]));
    let a = fs::metadata(out2.join("rounds/round-0001/builder/logs/a.stdout")).unwrap();
    assert_eq!(std::os::unix::fs::MetadataExt::nlink(&a), 2);
}

/// Random trees (names with spaces, unicode and dots; tiny to multi-MB contents; 30 % shared content; symlinks;
/// empty and executable files) pack, extract byte-identically with and without hard-link dedupe, and verify.
mod property {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Node {
        File { exec: bool, content: Vec<u8> },
        Link(String),
        Empty,
    }

    fn name() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z][a-z0-9_.-]{0,7}",
            Just("with space".to_string()),
            Just("\u{00fc}ber".to_string()),
            Just(".hidden".to_string()),
            Just("日本".to_string()),
        ]
    }

    fn path() -> impl Strategy<Value = String> {
        prop::collection::vec(name(), 1..5).prop_map(|v| v.join("/"))
    }

    /// Sizes follow the measured distribution: mostly tiny, some KB, a few MB; 30 % share a content.
    fn content() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            6 => prop::collection::vec(any::<u8>(), 1..64),
            2 => prop::collection::vec(any::<u8>(), 64..4096),
            1 => (1usize..3).prop_map(|k| (0..k * 1024 * 1024 + 17).map(|i| (i % 253) as u8).collect()),
            3 => Just(b"shared content\n".to_vec()),
        ]
    }

    fn node() -> impl Strategy<Value = Node> {
        prop_oneof![
            7 => (any::<bool>(), content()).prop_map(|(exec, content)| Node::File { exec, content }),
            1 => name().prop_map(Node::Link),
            1 => Just(Node::Empty),
        ]
    }

    fn tree() -> impl Strategy<Value = Vec<(String, Node)>> {
        prop::collection::vec((path(), node()), 1..40)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]
        #[test]
        fn pack_then_extract_is_byte_identical(entries in tree()) {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("src");
            fs::create_dir_all(&src).unwrap();
            let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (p, n) in &entries {
                // a path cannot be both a file and a directory; skip conflicts
                let parts: Vec<&str> = p.split('/').collect();
                let conflict = (1..parts.len()).any(|i| used.contains(&parts[..i].join("/"))) || used.iter().any(|u| u.starts_with(&format!("{p}/")));
                if conflict || !used.insert(p.clone()) {
                    continue;
                }
                let abs = src.join(p);
                fs::create_dir_all(abs.parent().unwrap()).unwrap();
                match n {
                    Node::File { exec, content } => {
                        fs::write(&abs, content).unwrap();
                        if *exec {
                            fs::set_permissions(&abs, fs::Permissions::from_mode(0o755)).unwrap();
                        }
                    }
                    Node::Link(t) => std::os::unix::fs::symlink(t, &abs).unwrap(),
                    Node::Empty => fs::write(&abs, b"").unwrap(),
                }
            }
            let store = tmp.path().join("store");
            traj().args(["pack"]).arg(&src).arg("--out").arg(&store).args(["--rules", "none"]).assert().success();
            let out = tmp.path().join("out");
            traj().arg("-S").arg(&store).args(["extract", ""]).arg(&out).assert().success();
            prop_assert_eq!(snapshot(&src, &[]), snapshot(&out, &[]));
            let out2 = tmp.path().join("out2");
            traj().arg("-S").arg(&store).args(["extract", "", "--hardlink-dedupe", "--mtime"]).arg(&out2).assert().success();
            prop_assert_eq!(snapshot(&src, &[]), snapshot(&out2, &[]));
            traj().arg("-S").arg(&store).args(["verify", "--deep"]).assert().success();
        }
    }
}

#[test]
fn incremental_batches_add_only_new_content() {
    let e = packed("none");
    // add a round; modify a file
    write(
        &e.src.join("rounds/round-0003/builder/logs/a.stdout"),
        b"hello\n",
    );
    write(
        &e.src.join("rounds/round-0003/builder/new.txt"),
        b"brand new\n",
    );
    write(
        &e.src.join("rounds/round-0001/reviewer/review.json"),
        b"{\"verdict\":\"done\"}\n",
    );
    pack_into(&e.src, &e.store, "two");
    let m = manifest(&e.store);
    let b = m["batches"].as_array().unwrap();
    assert_eq!(b.len(), 2);
    assert_eq!(b[1]["paths"], 3, "{}", b[1]);
    assert_eq!(b[1]["new_blobs"], 2, "{}", b[1]); // new.txt + modified review.json; hello is known
    let c = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["cat", "rounds/round-0001/reviewer/review.json"])
        .output()
        .unwrap();
    assert_eq!(c.stdout, b"{\"verdict\":\"done\"}\n");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    // unchanged re-pack is a no-op batch
    pack_into(&e.src, &e.store, "three");
    let m = manifest(&e.store);
    assert_eq!(m["batches"][2]["paths"], 0);
    // Unpublished finalized files are invisible to readers and SQL, then
    // orphan cleanup removes them on the next pack.
    let adapter_name = m["adapter"]["name"].as_str().unwrap();
    write(&e.store.join("catalog/files-0009.parquet"), b"junk");
    write(&e.store.join("catalog/dirs-0009-0002.parquet"), b"junk");
    write(&e.store.join("packs/index-0009-0002.parquet"), b"junk");
    write(
        &e.store
            .join(format!("derived/{adapter_name}/events-0009-0002.parquet")),
        b"junk",
    );
    write(&e.store.join("packs/9999.pack"), b"junk");
    write(&e.store.join("packs/0009.pack.tmp"), b"junk");
    write(&e.store.join("MANIFEST.json.tmp"), b"junk");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    if sql_available() {
        traj()
            .arg("-S")
            .arg(&e.store)
            .args([
                "sql",
                "--csv",
                "select (select count(*) from files), (select count(*) from events)",
            ])
            .assert()
            .success();
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
    pack_into(&e.src, &e.store, "four");
    assert!(!e.store.join("catalog/files-0009.parquet").exists());
    assert!(!e.store.join("catalog/dirs-0009-0002.parquet").exists());
    assert!(!e.store.join("packs/index-0009-0002.parquet").exists());
    assert!(!e
        .store
        .join(format!("derived/{adapter_name}/events-0009-0002.parquet"))
        .exists());
    assert!(!e.store.join("packs/9999.pack").exists());
    assert!(!e.store.join("packs/0009.pack.tmp").exists());
    assert!(!e.store.join("MANIFEST.json.tmp").exists());
}

#[test]
fn sigkill_mid_pack_leaves_a_readable_store() {
    let e = packed("none");
    // a batch big enough to be killed while packing: 30k small files with distinct content
    for i in 0..30_000u32 {
        write(
            &e.src.join(format!(
                "rounds/round-0009/builder/logs/{:02}/{i}.stdout",
                i % 50
            )),
            format!("call {i}\n").as_bytes(),
        );
    }
    let mut child = std::process::Command::new(traj_bin())
        .args(["pack"])
        .arg(&e.src)
        .arg("--out")
        .arg(&e.store)
        .args(["--adapter", &adapter(), "--rules", "none", "--jobs", "2"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    child.kill().unwrap(); // SIGKILL
    let _ = child.wait();
    // the previous batch is intact and readable
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["cat", "rounds/round-0001/reviewer/review.json"])
        .assert()
        .success();
    // the next pack removes whatever the killed one left and completes the batch
    pack_into(&e.src, &e.store, "two");
    let m = manifest(&e.store);
    assert_eq!(m["batches"].as_array().unwrap().len(), 2);
    assert_eq!(m["batches"][1]["paths"], 30_000);
    for entry in fs::read_dir(e.store.join("packs")).unwrap() {
        assert!(!entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"));
    }
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    let c = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["cat", "rounds/round-0009/builder/logs/07/29957.stdout"])
        .output()
        .unwrap();
    assert_eq!(c.stdout, b"call 29957\n");
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
fn oversized_manifest_is_never_published() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let raw = tmp.path().join("raw");
    let store = root.join("stores/manifest-limit.trajstore");
    fs::create_dir_all(&root).unwrap();
    write(&raw.join("file.txt"), b"content\n");
    let max_bytes = 16 << 10;
    fs::write(
        root.join("trajfs.toml"),
        format!(
            "data_root = {:?}\nstore_root = \"stores\"\nadapter = \"none\"\nrules = \"none\"\n\
             [hook]\nmax_file_bytes = {max_bytes}\n",
            raw
        ),
    )
    .unwrap();
    let long_label = "x".repeat(20_000);
    traj()
        .current_dir(&root)
        .args(["pack"])
        .arg(&raw)
        .arg("--out")
        .arg(&store)
        .args([
            "--adapter",
            "none",
            "--rules",
            "none",
            "--no-derive",
            "--label",
            &long_label,
        ])
        .assert()
        .failure();
    assert!(!store.join("MANIFEST.json").exists());
    assert!(!store.join("MANIFEST.json.tmp").exists());

    traj()
        .current_dir(&root)
        .args(["pack"])
        .arg(&raw)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "none", "--rules", "none", "--no-derive"])
        .assert()
        .success();
    assert!(store.join("MANIFEST.json").metadata().unwrap().len() <= max_bytes);
}
