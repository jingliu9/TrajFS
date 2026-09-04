//! Integration tests for the `traj` CLI (PLAN.md §11: T1–T5, T8, T9b, T11 on the synthetic fixture;
//! T6/T7 on the real dataset when TRAJ_SLOW_SRC is set).

use assert_cmd::Command;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn traj() -> Command {
    Command::cargo_bin("traj").unwrap()
}

/// The test adapter: a generic "rounds" layout declared in TOML (trajfs ships no runner-specific adapter).
fn adapter() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds.toml").display().to_string()
}

fn write(p: &Path, content: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A synthetic run tree: rounds, duplicates, symlink, empty file, exec bit, unicode name, big file, excluded dirs.
fn fixture(root: &Path) {
    let r1 = root.join("rounds/round-0001");
    let r2 = root.join("rounds/round-0002");
    write(&r1.join("builder/logs/a.stdout"), b"hello\n");
    write(&r2.join("builder/logs/a.stdout"), b"hello\n");
    write(&r1.join("builder/logs/b.stderr"), b"Traceback: boom\nline two\n");
    write(&r1.join("reviewer/review.json"), b"{\"verdict\":\"not done\"}\n");
    write(&r1.join("builder/empty"), b"");
    write(&r1.join("builder/run"), b"#!/bin/sh\n");
    fs::set_permissions(r1.join("builder/run"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("logs/a.stdout", r1.join("builder/link")).unwrap();
    write(&r1.join("builder/sp ace/\u{00fc}nicode.txt"), "grüße\n".as_bytes());
    let big: Vec<u8> = (0..(2 * 1024 * 1024 + 123)).map(|i| (i % 251) as u8).collect();
    write(&r2.join("builder/big.bin"), &big);
    write(&root.join(".cache/x"), b"junk");
    write(&root.join("node_modules/m/index.js"), b"x");
    write(&r1.join("builder/events.jsonl"), b"{\"type\":\"session.start\",\"timestamp\":\"2026-09-04T00:00:00Z\",\"id\":\"1\",\"data\":{}}\n{\"type\":\"tool.execution_start\",\"timestamp\":\"2026-09-04T00:00:01Z\",\"id\":\"2\",\"parentId\":\"1\",\"data\":{\"toolCallId\":\"c1\",\"toolName\":\"bash\"}}\n{\"type\":\"tool.execution_complete\",\"timestamp\":\"2026-09-04T00:00:02Z\",\"id\":\"3\",\"parentId\":\"2\",\"data\":{\"toolCallId\":\"c1\",\"exitCode\":1}}\nnot json\n");
}

/// Walk a tree into (relative path, kind/mode/content) for byte-exact comparison.
fn snapshot(root: &Path, skip: &[&str]) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for e in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let e = e.unwrap();
        let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().to_string();
        if skip.iter().any(|s| rel == *s || rel.starts_with(&format!("{s}/"))) {
            continue;
        }
        let ft = e.file_type();
        if ft.is_dir() {
            continue;
        }
        let desc = if ft.is_symlink() {
            format!("link:{}", fs::read_link(e.path()).unwrap().display())
        } else {
            let md = e.metadata().unwrap();
            let exec = md.permissions().mode() & 0o111 != 0;
            let mut h = <sha2::Sha256 as sha2::Digest>::new();
            sha2::Digest::update(&mut h, fs::read(e.path()).unwrap());
            format!("file:{}:{}", exec, hex::encode(sha2::Digest::finalize(h)))
        };
        v.push((rel, desc));
    }
    v.sort();
    v
}

struct Env {
    _tmp: tempfile::TempDir,
    src: PathBuf,
    store: PathBuf,
}

fn packed(rules: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    fixture(&src);
    traj().args(["pack"]).arg(&src).arg("--out").arg(&store).args(["--adapter", &adapter(), "--rules", rules, "--label", "one"]).assert().success();
    Env { _tmp: tmp, src, store }
}

#[test]
fn t2_round_trip_is_byte_identical() {
    let e = packed("none");
    let out = e._tmp.path().join("out");
    traj().arg("-S").arg(&e.store).args(["extract", ""]).arg(&out).assert().success();
    assert_eq!(snapshot(&e.src, &[]), snapshot(&out, &[]));
    // hard-link dedupe keeps content identical
    let out2 = e._tmp.path().join("out2");
    traj().arg("-S").arg(&e.store).args(["extract", "", "--hardlink-dedupe"]).arg(&out2).assert().success();
    assert_eq!(snapshot(&e.src, &[]), snapshot(&out2, &[]));
    let a = fs::metadata(out2.join("rounds/round-0001/builder/logs/a.stdout")).unwrap();
    assert_eq!(std::os::unix::fs::MetadataExt::nlink(&a), 2);
}

#[test]
fn t1_rules_exclude_build_products_and_record_them() {
    let e = packed("no-build-products");
    let out = e._tmp.path().join("out");
    traj().arg("-S").arg(&e.store).args(["extract", ""]).arg(&out).assert().success();
    assert_eq!(snapshot(&e.src, &[".cache", "node_modules"]), snapshot(&out, &[]));
    let ex = traj().arg("-S").arg(&e.store).args(["sql", "select path, rule from excluded order by path"]).output().unwrap();
    let s = String::from_utf8_lossy(&ex.stdout);
    assert!(s.contains(".cache") && s.contains("node_modules") && s.contains("exclude_dirs"), "{s}");
}

#[test]
fn t4_catalog_verbs_match_the_tree() {
    let e = packed("none");
    let ls = traj().arg("-S").arg(&e.store).args(["ls", "rounds/round-0001/builder"]).output().unwrap();
    let s = String::from_utf8_lossy(&ls.stdout);
    assert_eq!(s.lines().collect::<Vec<_>>(), vec!["logs/", "sp ace/", "empty", "events.jsonl", "link", "run"]);
    let f = traj().arg("-S").arg(&e.store).args(["find", "--name", "*.stdout"]).output().unwrap();
    let s = String::from_utf8_lossy(&f.stdout);
    assert_eq!(s.lines().count(), 2);
    let f = traj().arg("-S").arg(&e.store).args(["find", "--attr", "round=2", "--attr", "role=builder"]).output().unwrap();
    let s = String::from_utf8_lossy(&f.stdout);
    assert_eq!(s.lines().collect::<Vec<_>>(), vec!["rounds/round-0002/builder/big.bin", "rounds/round-0002/builder/logs/a.stdout"]);
    let st = traj().arg("-S").arg(&e.store).args(["stat", "rounds/round-0001/builder/link"]).output().unwrap();
    let s = String::from_utf8_lossy(&st.stdout);
    assert!(s.contains("kind:     Symlink") && s.contains("round=1,role=builder"), "{s}");
    let du = traj().arg("-S").arg(&e.store).args(["du", "rounds"]).output().unwrap();
    let s = String::from_utf8_lossy(&du.stdout);
    assert!(s.contains("10 files") && s.contains("8 blobs"), "{s}");
    traj().arg("-S").arg(&e.store).args(["ls", "no/such/dir"]).assert().failure();
}

#[test]
fn t5_grep_cat_sql_events() {
    let e = packed("none");
    let g = traj().arg("-S").arg(&e.store).args(["grep", "-e", "Traceback"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&g.stdout), "rounds/round-0001/builder/logs/b.stderr:1:Traceback: boom\n");
    // a hit in duplicated content maps to every path
    let g = traj().arg("-S").arg(&e.store).args(["grep", "-l", "-e", "^hello"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&g.stdout).lines().count(), 2);
    let c = traj().arg("-S").arg(&e.store).args(["cat", "rounds/round-0001/reviewer/review.json"]).output().unwrap();
    assert_eq!(c.stdout, b"{\"verdict\":\"not done\"}\n");
    let q = traj().arg("-S").arg(&e.store).args(["sql", "--csv", "select seq, type, tool_name, exit_code from events order by seq"]).output().unwrap();
    let s = String::from_utf8_lossy(&q.stdout);
    assert!(s.contains("2,tool.execution_complete,bash,1"), "{s}");
    assert!(s.contains("3,_unparsed,,"), "unparsable lines must be kept: {s}");
    let q = traj().arg("-S").arg(&e.store).args(["sql", "--csv", "select count(*) from files where attrs['round']='1'"]).output().unwrap();
    assert!(String::from_utf8_lossy(&q.stdout).contains("\n8"), "{}", String::from_utf8_lossy(&q.stdout));
}

#[test]
fn t3_integrity_detects_corruption() {
    let e = packed("none");
    traj().arg("-S").arg(&e.store).args(["verify", "--deep"]).assert().success();
    let pack = e.store.join("packs/0001.pack");
    let mut bytes = fs::read(&pack).unwrap();
    let i = bytes.len() / 2;
    bytes[i] ^= 0xff;
    fs::write(&pack, &bytes).unwrap();
    traj().arg("-S").arg(&e.store).args(["verify", "--deep"]).assert().code(1);
    // shallow verify still passes (structure intact), deep fails, cat of a corrupted blob fails
    traj().arg("-S").arg(&e.store).args(["verify"]).assert().success();
    fs::remove_file(&pack).unwrap();
    traj().arg("-S").arg(&e.store).args(["verify"]).assert().code(1);
    fs::remove_file(e.store.join("MANIFEST.json")).unwrap();
    traj().arg("-S").arg(&e.store).args(["ls"]).assert().failure();
}

#[test]
fn t8_incremental_batches_add_only_new_content() {
    let e = packed("none");
    // add a round; modify a file
    write(&e.src.join("rounds/round-0003/builder/logs/a.stdout"), b"hello\n");
    write(&e.src.join("rounds/round-0003/builder/new.txt"), b"brand new\n");
    write(&e.src.join("rounds/round-0001/reviewer/review.json"), b"{\"verdict\":\"done\"}\n");
    traj().args(["pack"]).arg(&e.src).arg("--out").arg(&e.store).args(["--adapter", &adapter(), "--rules", "none", "--label", "two"]).assert().success();
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(e.store.join("MANIFEST.json")).unwrap()).unwrap();
    let b = m["batches"].as_array().unwrap();
    assert_eq!(b.len(), 2);
    assert_eq!(b[1]["paths"], 3, "{}", b[1]);
    assert_eq!(b[1]["new_blobs"], 2, "{}", b[1]); // new.txt + modified review.json; hello is known
    let c = traj().arg("-S").arg(&e.store).args(["cat", "rounds/round-0001/reviewer/review.json"]).output().unwrap();
    assert_eq!(c.stdout, b"{\"verdict\":\"done\"}\n");
    traj().arg("-S").arg(&e.store).args(["verify", "--deep"]).assert().success();
    // unchanged re-pack is a no-op batch
    traj().args(["pack"]).arg(&e.src).arg("--out").arg(&e.store).args(["--adapter", &adapter(), "--rules", "none"]).assert().success();
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(e.store.join("MANIFEST.json")).unwrap()).unwrap();
    assert_eq!(m["batches"][2]["paths"], 0);
    // orphan cleanup: a stray next-batch segment and a tmp pack vanish on the next pack
    write(&e.store.join("catalog/files-0009.parquet"), b"junk");
    write(&e.store.join("packs/0009.pack.tmp"), b"junk");
    traj().args(["pack"]).arg(&e.src).arg("--out").arg(&e.store).args(["--adapter", &adapter(), "--rules", "none"]).assert().success();
    assert!(!e.store.join("catalog/files-0009.parquet").exists());
    assert!(!e.store.join("packs/0009.pack.tmp").exists());
}

#[test]
fn t9b_separation_and_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let st = std::process::Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
        assert!(st.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&st.stderr));
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    let data = tmp.path().join("data");
    // init refuses a data_root inside the repo
    traj().current_dir(&repo).args(["init", "--data-root"]).arg(repo.join("runs")).assert().failure();
    // the adapter lives in the target repo
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    fs::copy(adapter(), repo.join("trajfs/adapter.toml")).unwrap();
    fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds-rules.toml"), repo.join("trajfs/rounds-rules.toml")).unwrap();
    traj().current_dir(&repo).args(["init", "--data-root"]).arg(&data).args(["--adapter", "trajfs/adapter.toml", "--rules", "none"]).assert().success();
    assert!(repo.join("trajfs.toml").is_file());
    let cfg = fs::read_to_string(repo.join("trajfs.toml")).unwrap();
    assert!(cfg.contains("rounds/round-"), "hook patterns copied from the adapter: {cfg}");
    assert!(fs::read_link(repo.join(".git/hooks/pre-commit")).is_ok());
    assert!(repo.join(".claude/skills/traj/SKILL.md").is_file());
    assert!(fs::read_to_string(repo.join("AGENTS.md")).unwrap().contains("traj-skill:start"));
    assert!(data.join(".gitignore").is_file());
    traj().current_dir(&repo).arg("doctor").assert().success();
    // pack refuses a store inside data_root and a source inside store_root
    fixture(&data.join("run1"));
    traj().current_dir(&repo).args(["pack"]).arg(data.join("run1")).arg("--out").arg(data.join("x")).assert().failure();
    fs::create_dir_all(repo.join("stores/fake")).unwrap();
    traj().current_dir(&repo).args(["pack"]).arg(repo.join("stores/fake")).assert().failure();
    // the hook refuses raw run paths
    fs::create_dir_all(repo.join("raw/rounds/round-0001")).unwrap();
    fs::write(repo.join("raw/rounds/round-0001/x.stdout"), "x").unwrap();
    git(&["add", "raw"]);
    let out = std::process::Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "raw"]).output().unwrap();
    assert!(!out.status.success(), "commit of raw paths must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("traj pack"));
    git(&["reset", "-q"]);
    // deleting tracked raw paths (the migration) is allowed
    fs::create_dir_all(repo.join("old/rounds/round-0001")).unwrap();
    fs::write(repo.join("old/rounds/round-0001/y.stdout"), "y").unwrap();
    git(&["add", "old"]);
    git(&["-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", "legacy raw tree"]);
    git(&["rm", "-r", "-q", "--cached", "old"]);
    git(&["commit", "-q", "-m", "migrate: stop tracking raw tree"]);
    // the intended path: pack into store_root, traj commit
    traj().current_dir(&repo).args(["pack"]).arg(data.join("run1")).args(["--label", "r1"]).assert().success();
    let store = repo.join("stores/run1.trajstore");
    assert!(store.join("MANIFEST.json").is_file());
    git(&["add", "trajfs.toml", "trajfs", "stores/.gitkeep", "stores/.gitattributes", ".claude", "AGENTS.md"]);
    git(&["commit", "-q", "-m", "init"]);
    traj().current_dir(&repo).args(["commit"]).arg(&store).assert().success();
    let log = std::process::Command::new("git").arg("-C").arg(&repo).args(["log", "--oneline"]).output().unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("trajstore run1: batch 1 r1"));
    let files = std::process::Command::new("git").arg("-C").arg(&repo).args(["ls-files", "stores"]).output().unwrap();
    let s = String::from_utf8_lossy(&files.stdout);
    assert!(s.contains("stores/run1.trajstore/MANIFEST.json") && s.contains("stores/run1.trajstore/packs/0001.pack"), "{s}");
    // a clone reads the store unchanged
    let clone = tmp.path().join("clone");
    let st = std::process::Command::new("git").args(["clone", "-q"]).arg(&repo).arg(&clone).status().unwrap();
    assert!(st.success());
    traj().arg("-S").arg(clone.join("stores/run1.trajstore")).args(["verify", "--deep"]).assert().success();
    traj().current_dir(&repo).args(["hook", "check-tree", "HEAD"]).assert().success();
}

#[test]
fn t9c_init_scaffolds_an_adapter_for_the_target_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(["init", "-q"]).status().unwrap().success());
    traj().current_dir(&repo).args(["init", "--scaffold-adapter", "--data-root"]).arg(tmp.path().join("data")).assert().success();
    assert!(repo.join("trajfs/adapter.toml").is_file() && repo.join("trajfs/rules.toml").is_file());
    let cfg = fs::read_to_string(repo.join("trajfs.toml")).unwrap();
    assert!(cfg.contains("adapter = \"trajfs/adapter.toml\""), "{cfg}");
    traj().current_dir(&repo).arg("doctor").assert().success();
    // the template loads as-is (name my-runner, no attrs) and packs
    fixture(&tmp.path().join("data/run1"));
    traj().current_dir(&repo).args(["pack"]).arg(tmp.path().join("data/run1")).assert().success();
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(repo.join("stores/run1.trajstore/MANIFEST.json")).unwrap()).unwrap();
    assert_eq!(m["adapter"]["name"], "my-runner");
    // without --scaffold-adapter, --no-adapter or an --adapter, a non-tty init falls back to the built-in `none`
    let repo2 = tmp.path().join("repo2");
    fs::create_dir_all(&repo2).unwrap();
    assert!(std::process::Command::new("git").arg("-C").arg(&repo2).args(["init", "-q"]).status().unwrap().success());
    traj().current_dir(&repo2).args(["init", "--data-root"]).arg(tmp.path().join("data2")).assert().success();
    assert!(fs::read_to_string(repo2.join("trajfs.toml")).unwrap().contains("adapter = \"none\""));
}

#[test]
fn t11_skill_mentions_every_verb_and_carries_the_version() {
    let out = traj().args(["skill", "export", "--stdout"]).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("{{"), "unrendered placeholders");
    let verbs = traj().args(["skill", "verbs"]).output().unwrap();
    for v in String::from_utf8_lossy(&verbs.stdout).lines() {
        assert!(s.contains(&format!("traj {v}")), "skill does not mention `traj {v}`");
    }
    let help = traj().arg("--help").output().unwrap();
    let h = String::from_utf8_lossy(&help.stdout);
    for v in String::from_utf8_lossy(&verbs.stdout).lines() {
        assert!(h.lines().any(|l| l.trim_start().starts_with(v)), "verb {v} missing from --help");
    }
    assert!(s.contains("traj-skill-version: "));
}

/// T6: pack a real round directory when TRAJ_SLOW_SRC points at a run (expects the onesw layout).
#[test]
fn t6_reference_round_when_available() {
    let Ok(src) = std::env::var("TRAJ_SLOW_SRC") else { return };
    let slow_adapter = std::env::var("TRAJ_SLOW_ADAPTER").unwrap_or_else(|_| adapter());
    let round = Path::new(&src).join("rounds/round-0037");
    if !round.is_dir() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("r37");
    let t0 = std::time::Instant::now();
    traj().args(["pack"]).arg(&round).arg("--out").arg(&store).args(["--adapter", &slow_adapter]).assert().success();
    let pack_s = t0.elapsed().as_secs_f64();
    traj().arg("-S").arg(&store).args(["verify", "--deep"]).assert().success();
    let out = tmp.path().join("out");
    traj().arg("-S").arg(&store).args(["extract", ""]).arg(&out).assert().success();
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(store.join("MANIFEST.json")).unwrap()).unwrap();
    eprintln!("round-0037: {} paths, {} blobs, packed {} bytes, {pack_s:.1} s", m["batches"][0]["paths"], m["batches"][0]["new_blobs"], m["batches"][0]["packed_bytes"]);
    assert!(pack_s < 60.0);
    let t0 = std::time::Instant::now();
    traj().arg("-S").arg(&store).args(["ls", "builder/workspace/logs/round10"]).assert().success();
    assert!(t0.elapsed().as_millis() < 500, "ls took {:?}", t0.elapsed());
}
