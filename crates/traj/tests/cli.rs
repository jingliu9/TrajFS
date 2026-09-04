//! Integration tests for the `traj` CLI (docs/PLAN.md §11: T1–T5, T8, T9b, T11 on the synthetic fixture;
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
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rounds.toml")
        .display()
        .to_string()
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
    write(
        &r1.join("builder/logs/b.stderr"),
        b"Traceback: boom\nline two\n",
    );
    write(
        &r1.join("reviewer/review.json"),
        b"{\"verdict\":\"not done\"}\n",
    );
    write(&r1.join("builder/empty"), b"");
    write(&r1.join("builder/run"), b"#!/bin/sh\n");
    fs::set_permissions(r1.join("builder/run"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("logs/a.stdout", r1.join("builder/link")).unwrap();
    write(
        &r1.join("builder/sp ace/\u{00fc}nicode.txt"),
        "grüße\n".as_bytes(),
    );
    let big: Vec<u8> = (0..(2 * 1024 * 1024 + 123))
        .map(|i| (i % 251) as u8)
        .collect();
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
        let rel = e
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if skip
            .iter()
            .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
        {
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

/// stdout lines of an external binary run directly (no shell).
fn lines_of(cmd: &str, args: &[&str], cwd: &Path) -> Vec<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("{cmd}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    v.sort();
    v
}

fn traj_lines(store: &Path, args: &[&str]) -> Vec<String> {
    let out = traj().arg("-S").arg(store).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "traj {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    v.sort();
    v
}

fn packed(rules: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    fixture(&src);
    traj()
        .args(["pack"])
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", &adapter(), "--rules", rules, "--label", "one"])
        .assert()
        .success();
    Env {
        _tmp: tmp,
        src,
        store,
    }
}

#[test]
fn t2_round_trip_is_byte_identical() {
    let e = packed("none");
    let out = e._tmp.path().join("out");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    assert_eq!(snapshot(&e.src, &[]), snapshot(&out, &[]));
    // hard-link dedupe keeps content identical
    let out2 = e._tmp.path().join("out2");
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

mod t2_property {
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
fn t1_rules_exclude_build_products_and_record_them() {
    let e = packed("no-build-products");
    let out = e._tmp.path().join("out");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    assert_eq!(
        snapshot(&e.src, &[".cache", "node_modules"]),
        snapshot(&out, &[])
    );
    let ex = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["sql", "select path, rule from excluded order by path"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&ex.stdout);
    assert!(
        s.contains(".cache") && s.contains("node_modules") && s.contains("exclude_dirs"),
        "{s}"
    );
}

#[test]
fn t4_catalog_verbs_match_the_tree() {
    let e = packed("none");
    let ls = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["ls", "rounds/round-0001/builder"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&ls.stdout);
    assert_eq!(
        s.lines().collect::<Vec<_>>(),
        vec!["logs/", "sp ace/", "empty", "events.jsonl", "link", "run"]
    );
    let f = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["find", "--name", "*.stdout"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&f.stdout);
    assert_eq!(s.lines().count(), 2);
    let f = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["find", "--attr", "round=2", "--attr", "role=builder"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&f.stdout);
    assert_eq!(
        s.lines().collect::<Vec<_>>(),
        vec![
            "rounds/round-0002/builder/big.bin",
            "rounds/round-0002/builder/logs/a.stdout"
        ]
    );
    let st = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["stat", "rounds/round-0001/builder/link"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&st.stdout);
    assert!(
        s.contains("kind:     Symlink") && s.contains("round=1,role=builder"),
        "{s}"
    );
    let du = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["du", "rounds"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&du.stdout);
    assert!(s.contains("10 files") && s.contains("8 blobs"), "{s}");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["ls", "no/such/dir"])
        .assert()
        .failure();

    // equality against the real tools on the extracted tree
    let out = e._tmp.path().join("out");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    for d in ["rounds/round-0001/builder", "rounds", ""] {
        let mut want = lines_of("ls", &["-A", if d.is_empty() { "." } else { d }], &out);
        want.retain(|n| !n.is_empty());
        let mut got: Vec<String> = traj_lines(&e.store, &["ls", d])
            .into_iter()
            .map(|n| n.trim_end_matches('/').to_string())
            .collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "ls {d}");
    }
    let mut want = lines_of("find", &[".", "-type", "f", "-o", "-type", "l"], &out);
    want = want
        .into_iter()
        .map(|p| p.trim_start_matches("./").to_string())
        .collect();
    want.sort();
    let got = traj_lines(&e.store, &["find"]);
    assert_eq!(got, want, "find");
    let du = lines_of("du", &["-sb", "--apparent-size", "rounds"], &out);
    let want_bytes: i64 = du[0].split_whitespace().next().unwrap().parse().unwrap();
    // du -b counts directories too; compare the file bytes via the catalog sum instead
    let q = traj()
        .arg("-S")
        .arg(&e.store)
        .args([
            "sql",
            "--csv",
            "select sum(size) from files where path like 'rounds/%'",
        ])
        .output()
        .unwrap();
    let got_bytes: i64 = String::from_utf8_lossy(&q.stdout)
        .lines()
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        got_bytes <= want_bytes && want_bytes - got_bytes < 64 * 1024,
        "du {want_bytes} vs catalog {got_bytes}"
    );
    let t = traj_lines(&e.store, &["tree", "rounds", "--depth", "2"]);
    assert!(
        t.iter().any(|l| l.contains("round-0001/")) && t.iter().any(|l| l.contains("builder/")),
        "{t:?}"
    );
}

#[test]
fn t5_grep_cat_sql_events() {
    let e = packed("none");
    let g = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["grep", "-e", "Traceback"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&g.stdout),
        "rounds/round-0001/builder/logs/b.stderr:1:Traceback: boom\n"
    );
    // a hit in duplicated content maps to every path
    let g = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["grep", "-l", "-e", "^hello"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&g.stdout).lines().count(), 2);
    let c = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["cat", "rounds/round-0001/reviewer/review.json"])
        .output()
        .unwrap();
    assert_eq!(c.stdout, b"{\"verdict\":\"not done\"}\n");
    let q = traj()
        .arg("-S")
        .arg(&e.store)
        .args([
            "sql",
            "--csv",
            "select seq, type, tool_name, exit_code from events order by seq",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&q.stdout);
    assert!(s.contains("2,tool.execution_complete,bash,1"), "{s}");
    assert!(
        s.contains("3,_unparsed,,"),
        "unparsable lines must be kept: {s}"
    );
    let q = traj()
        .arg("-S")
        .arg(&e.store)
        .args([
            "sql",
            "--csv",
            "select count(*) from files where attrs['round']='1'",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&q.stdout).contains("\n8"),
        "{}",
        String::from_utf8_lossy(&q.stdout)
    );
}

#[test]
fn t5b_grep_matches_grep_rl_for_random_patterns() {
    let e = packed("none");
    let out = e._tmp.path().join("out");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    let patterns = [
        "hello",
        "^Trace",
        "o{2}",
        "not done",
        "line",
        "[0-9]+",
        "sh$",
        "grüße",
        "zzz-never",
        "e.*l",
        "\\{",
        "boom|verdict",
        "^$",
        "a",
        "e",
        "t.o",
        "run",
        "^#!",
        "n$",
        "json",
    ];
    for p in patterns {
        let out_grep = std::process::Command::new("grep")
            .args(["-rlE", "--", p, "."])
            .current_dir(&out)
            .output()
            .unwrap();
        let mut want: Vec<String> = String::from_utf8_lossy(&out_grep.stdout)
            .lines()
            .map(|l| l.trim_start_matches("./").to_string())
            .filter(|l| !l.contains("big.bin"))
            .collect();
        want.sort();
        let got = traj()
            .arg("-S")
            .arg(&e.store)
            .args(["grep", "-l", "-e", p])
            .output()
            .unwrap();
        let mut got: Vec<String> = String::from_utf8_lossy(&got.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect();
        got.sort();
        assert_eq!(got, want, "pattern {p}");
    }
}

#[test]
fn t5c_other_formats_and_rederive() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    // (b) claude-code session log
    let src = tmp.path().join("cc");
    fs::create_dir_all(&src).unwrap();
    fs::copy(fx.join("claude-session.jsonl"), src.join("session.jsonl")).unwrap();
    let store = tmp.path().join("cc.store");
    traj()
        .args(["pack"])
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "claude-code", "--rules", "none"])
        .assert()
        .success();
    let q = traj()
        .arg("-S")
        .arg(&store)
        .args([
            "sql",
            "--csv",
            "select seq, type, actor, id, parent_id, tool_name from events order by seq",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&q.stdout);
    assert_eq!(s.lines().count(), 6, "{s}");
    assert!(
        s.contains("1,user,user,u1,,")
            && s.contains("2,assistant,assistant,a1,u1,Bash")
            && s.contains("3,user,user,u2,a1,"),
        "{s}"
    );
    // (c) heterogeneous jsonl: nothing lost, envelope found by common keys
    let src = tmp.path().join("hj");
    fs::create_dir_all(&src).unwrap();
    fs::copy(fx.join("hetero.jsonl"), src.join("log.jsonl")).unwrap();
    let store = tmp.path().join("hj.store");
    traj()
        .args(["pack"])
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "jsonl", "--rules", "none"])
        .assert()
        .success();
    let q = traj().arg("-S").arg(&store).args(["sql", "--csv", "select seq, type, actor, tool_name, exit_code, parent_id, ts is not null from events order by seq"]).output().unwrap();
    let s = String::from_utf8_lossy(&q.stdout);
    assert_eq!(s.lines().count(), 6, "{s}");
    assert!(
        s.contains("0,start,,,,,true")
            && s.contains("1,tool,,grep,0,,true")
            && s.contains("2,end,assistant,,,e1,true")
            && s.contains("3,_untyped,")
            && s.contains("4,_unparsed,"),
        "{s}"
    );
    let q = traj()
        .arg("-S")
        .arg(&store)
        .args([
            "sql",
            "--csv",
            "select payload_json from events where seq=3",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&q.stdout).contains("recognisable"));
    // (d) re-derive with a bumped adapter version replaces the derived segment and leaves packs and catalog untouched
    let e = packed("none");
    let before = snapshot(&e.store.join("packs"), &[]);
    let before_cat = snapshot(&e.store.join("catalog"), &[]);
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(fx.join("rounds-v2.toml"))
        .assert()
        .success();
    assert_eq!(snapshot(&e.store.join("packs"), &[]), before);
    assert_eq!(snapshot(&e.store.join("catalog"), &[]), before_cat);
    let q = traj()
        .arg("-S")
        .arg(&e.store)
        .args([
            "sql",
            "--csv",
            "select distinct adapter_version from events",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&q.stdout).contains("\n2"),
        "{}",
        String::from_utf8_lossy(&q.stdout)
    );
}

#[test]
fn t3_integrity_detects_corruption() {
    let e = packed("none");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    let pack = e.store.join("packs/0001.pack");
    let mut bytes = fs::read(&pack).unwrap();
    let i = bytes.len() / 2;
    bytes[i] ^= 0xff;
    fs::write(&pack, &bytes).unwrap();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .code(1);
    // shallow verify still passes (structure intact), deep fails, cat of a corrupted blob fails
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify"])
        .assert()
        .success();
    // truncated pack: shallow verify sees frames beyond the end; other packs stay readable
    let orig = fs::read(&pack).unwrap();
    fs::write(&pack, &orig[..orig.len() / 3]).unwrap();
    let v = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify"])
        .output()
        .unwrap();
    assert_eq!(v.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&v.stdout).contains("beyond the end"),
        "{}",
        String::from_utf8_lossy(&v.stdout)
    );
    fs::write(&pack, &orig).unwrap();
    // missing index segment: catalog rows without a location
    let idx = e.store.join("packs/index-0001.parquet");
    let idx_bytes = fs::read(&idx).unwrap();
    fs::remove_file(&idx).unwrap();
    let v = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify"])
        .output()
        .unwrap();
    assert_eq!(v.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&v.stdout).contains("without a blob"));
    fs::write(&idx, idx_bytes).unwrap();
    fs::remove_file(&pack).unwrap();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify"])
        .assert()
        .code(1);
    fs::remove_file(e.store.join("MANIFEST.json")).unwrap();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["ls"])
        .assert()
        .failure();
}

#[test]
fn t8_incremental_batches_add_only_new_content() {
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
    traj()
        .args(["pack"])
        .arg(&e.src)
        .arg("--out")
        .arg(&e.store)
        .args(["--adapter", &adapter(), "--rules", "none", "--label", "two"])
        .assert()
        .success();
    let m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(e.store.join("MANIFEST.json")).unwrap()).unwrap();
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
    traj()
        .args(["pack"])
        .arg(&e.src)
        .arg("--out")
        .arg(&e.store)
        .args(["--adapter", &adapter(), "--rules", "none"])
        .assert()
        .success();
    let m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(e.store.join("MANIFEST.json")).unwrap()).unwrap();
    assert_eq!(m["batches"][2]["paths"], 0);
    // orphan cleanup: a stray next-batch segment and a tmp pack vanish on the next pack
    write(&e.store.join("catalog/files-0009.parquet"), b"junk");
    write(&e.store.join("packs/0009.pack.tmp"), b"junk");
    traj()
        .args(["pack"])
        .arg(&e.src)
        .arg("--out")
        .arg(&e.store)
        .args(["--adapter", &adapter(), "--rules", "none"])
        .assert()
        .success();
    assert!(!e.store.join("catalog/files-0009.parquet").exists());
    assert!(!e.store.join("packs/0009.pack.tmp").exists());
}

#[test]
fn t8b_sigkill_mid_pack_leaves_a_readable_store() {
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
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("traj"))
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
    traj()
        .args(["pack"])
        .arg(&e.src)
        .arg("--out")
        .arg(&e.store)
        .args(["--adapter", &adapter(), "--rules", "none"])
        .assert()
        .success();
    let m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(e.store.join("MANIFEST.json")).unwrap()).unwrap();
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
fn t8c_watch_packs_when_the_adapter_reports_a_batch_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success());
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    fs::copy(adapter(), repo.join("trajfs/adapter.toml")).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds-rules.toml"),
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
    let m: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join("stores/run7.trajstore/MANIFEST.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(m["batches"][0]["label"], "round-0001");
    // the same marker is not packed twice; a new one is
    write(&data.join("run7/rounds/round-0002/DONE"), b"");
    traj()
        .current_dir(&repo)
        .args(["watch", "--max-batches", "1", "--interval", "1"])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();
    let m: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join("stores/run7.trajstore/MANIFEST.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(m["batches"][1]["label"], "round-0002");
}

#[test]
fn t8d_watch_discovers_nested_runs_with_unique_ids_and_round_labels() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success());
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds-rules.toml"),
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
        let manifest = repo.join(format!("stores/{id}.trajstore/MANIFEST.json"));
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(manifest).unwrap()).unwrap();
        assert_eq!(value["store_id"], id);
        assert_eq!(value["batches"][0]["label"], "round-0001");
        assert!(value["batches"][0]["derived"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}

#[test]
fn t9b_separation_and_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&st.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
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
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds-rules.toml"),
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
    git(&["add", "raw"]);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-q", "-m", "raw"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "commit of raw paths must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("traj pack"));
    git(&["reset", "-q"]);
    // deleting tracked raw paths (the migration) is allowed
    fs::create_dir_all(repo.join("old/rounds/round-0001")).unwrap();
    fs::write(repo.join("old/rounds/round-0001/y.stdout"), "y").unwrap();
    git(&["add", "old"]);
    git(&[
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-q",
        "-m",
        "legacy raw tree",
    ]);
    git(&["rm", "-r", "-q", "--cached", "old"]);
    git(&["commit", "-q", "-m", "migrate: stop tracking raw tree"]);
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
    git(&[
        "add",
        "trajfs.toml",
        "trajfs",
        "stores/.gitkeep",
        "stores/.gitattributes",
        ".claude",
        "AGENTS.md",
    ]);
    git(&["commit", "-q", "-m", "init"]);
    traj()
        .current_dir(&repo)
        .args(["commit"])
        .arg(&store)
        .assert()
        .success();
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["log", "--oneline"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("trajstore run1: batch 1 r1"));
    let files = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["ls-files", "stores"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&files.stdout);
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

    // T9 timing: push + clone of the store vs the raw tree, against a local bare remote
    let bare = tmp.path().join("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare)
        .status()
        .unwrap()
        .success());
    git(&["remote", "add", "origin", bare.to_str().unwrap()]);
    let t0 = std::time::Instant::now();
    git(&["push", "-q", "origin", "HEAD:store"]);
    let push_store = t0.elapsed();
    let raw_repo = tmp.path().join("rawrepo");
    fs::create_dir_all(&raw_repo).unwrap();
    let rgit = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&raw_repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&st.stderr)
        );
    };
    rgit(&["init", "-q"]);
    rgit(&["config", "user.email", "t@example.com"]);
    rgit(&["config", "user.name", "t"]);
    fixture(&raw_repo.join("run1"));
    rgit(&["add", "."]);
    rgit(&["commit", "-q", "-m", "raw"]);
    rgit(&["remote", "add", "origin", bare.to_str().unwrap()]);
    let t0 = std::time::Instant::now();
    rgit(&["push", "-q", "origin", "HEAD:raw"]);
    let push_raw = t0.elapsed();
    let n_store = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["ls-files", "stores"])
            .output()
            .unwrap()
            .stdout,
    )
    .lines()
    .count();
    let n_raw = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(&raw_repo)
            .args(["ls-files"])
            .output()
            .unwrap()
            .stdout,
    )
    .lines()
    .count();
    eprintln!(
        "T9 push: store {n_store} files in {push_store:?}; raw {n_raw} files in {push_raw:?}"
    );
    assert!(
        n_store < n_raw,
        "the store must be fewer git paths than the raw tree ({n_store} vs {n_raw})"
    );
}

#[test]
fn t9c_init_scaffolds_an_adapter_for_the_target_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success());
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
    let m: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join("stores/run1.trajstore/MANIFEST.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(m["adapter"]["name"], "my-runner");
    // without --scaffold-adapter, --no-adapter or an --adapter, a non-tty init falls back to the built-in `none`
    let repo2 = tmp.path().join("repo2");
    fs::create_dir_all(&repo2).unwrap();
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&repo2)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success());
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
fn t11_skill_mentions_every_verb_and_carries_the_version() {
    let out = traj()
        .args(["skill", "export", "--stdout"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("{{"), "unrendered placeholders");
    let verbs = traj().args(["skill", "verbs"]).output().unwrap();
    for v in String::from_utf8_lossy(&verbs.stdout).lines() {
        assert!(
            s.contains(&format!("traj {v}")),
            "skill does not mention `traj {v}`"
        );
    }
    let help = traj().arg("--help").output().unwrap();
    let h = String::from_utf8_lossy(&help.stdout);
    for v in String::from_utf8_lossy(&verbs.stdout).lines() {
        assert!(
            h.lines().any(|l| l.trim_start().starts_with(v)),
            "verb {v} missing from --help"
        );
    }
    assert!(s.contains("traj-skill-version: "));

    // every worked example runs against the fixture store
    let e = packed("none");
    let mut ran = 0;
    let mut in_examples = false;
    for line in s.lines() {
        if line.starts_with("## ") {
            in_examples = line.contains("Worked examples");
        }
        if !in_examples || !line.starts_with("traj ") {
            continue;
        }
        let args = shell_words(line);
        let args: Vec<String> = args
            .into_iter()
            .skip(1)
            .map(|a| {
                if a == "S" {
                    e.store.display().to_string()
                } else {
                    a
                }
            })
            .collect();
        let out = traj().args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "example failed: {line}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        ran += 1;
    }
    assert!(ran >= 5, "expected the worked examples to run, ran {ran}");
}

/// Minimal quoting-aware splitter for the skill's one-line examples (single and double quotes).
fn shell_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                has = true;
            }
            (None, ' ') => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// T6/T7 (feature `slow`): real dataset named by TRAJ_SLOW_SRC (a run directory) and TRAJ_SLOW_ADAPTER (its adapter
/// TOML). Expected counts live in tests/expected/<run-id>.json: written on the first run, asserted afterwards.
#[cfg(feature = "slow")]
mod slow {
    use super::*;

    fn env_or_skip() -> Option<(PathBuf, String)> {
        let src = std::env::var("TRAJ_SLOW_SRC").ok()?;
        let ad = std::env::var("TRAJ_SLOW_ADAPTER").ok()?;
        Some((PathBuf::from(src), ad))
    }

    fn expected_path(run: &Path, what: &str) -> PathBuf {
        let id = run.file_name().unwrap().to_string_lossy().to_string();
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/expected")
            .join(format!("{id}.{what}.json"))
    }

    fn check_or_record(path: &Path, got: &serde_json::Value) {
        match fs::read_to_string(path) {
            Ok(t) => {
                let want: serde_json::Value = serde_json::from_str(&t).unwrap();
                assert_eq!(
                    got,
                    &want,
                    "{} differs from the recorded expectation",
                    path.display()
                );
            }
            Err(_) => {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, serde_json::to_string_pretty(got).unwrap()).unwrap();
                eprintln!("recorded {}", path.display());
            }
        }
    }

    fn counts(store: &Path) -> serde_json::Value {
        let m: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.join("MANIFEST.json")).unwrap())
                .unwrap();
        let b = &m["batches"][0];
        serde_json::json!({"paths": b["paths"], "bytes": b["bytes"], "new_blobs": b["new_blobs"], "new_blob_bytes": b["new_blob_bytes"], "excluded": b["excluded"]})
    }

    #[test]
    fn t6_reference_round() {
        let Some((src, ad)) = env_or_skip() else {
            panic!("set TRAJ_SLOW_SRC and TRAJ_SLOW_ADAPTER")
        };
        let round = src.join("rounds/round-0037");
        assert!(round.is_dir(), "{} has no rounds/round-0037", src.display());
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("r37");
        let t0 = std::time::Instant::now();
        traj()
            .args(["pack"])
            .arg(&round)
            .arg("--out")
            .arg(&store)
            .args(["--adapter", &ad])
            .assert()
            .success();
        let pack_s = t0.elapsed().as_secs_f64();
        assert!(pack_s < 60.0, "pack took {pack_s:.1} s");
        check_or_record(&expected_path(&src, "round-0037"), &counts(&store));
        traj()
            .arg("-S")
            .arg(&store)
            .args(["verify", "--deep"])
            .assert()
            .success();
        let out = tmp.path().join("out");
        let t0 = std::time::Instant::now();
        traj()
            .arg("-S")
            .arg(&store)
            .args(["extract", ""])
            .arg(&out)
            .assert()
            .success();
        assert!(t0.elapsed().as_secs_f64() < 30.0);
        // byte-identical for every kept path
        let got = snapshot(&out, &[]);
        let src_snap = snapshot(&round, &[]);
        let kept: std::collections::HashSet<&String> = got.iter().map(|(p, _)| p).collect();
        let want: Vec<(String, String)> = src_snap
            .into_iter()
            .filter(|(p, _)| kept.contains(p))
            .collect();
        assert_eq!(got, want);
        // cat latency after warm-up: p50 <= 5 ms over 200 random paths
        let paths: Vec<String> = traj_lines(&store, &["find", "--kind", "f"])
            .into_iter()
            .step_by(500)
            .take(200)
            .collect();
        let st = trajfs_core::Store::open(&store).unwrap();
        let mut reader = st.reader();
        let mut times: Vec<f64> = Vec::new();
        for p in &paths {
            let t0 = std::time::Instant::now();
            let row = st.stat(p).unwrap().unwrap();
            let _ = st.read_row(&mut reader, &row, true).unwrap();
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "cat p50 {:.2} ms p99 {:.2} ms",
            times[times.len() / 2],
            times[times.len() * 99 / 100]
        );
        assert!(times[times.len() / 2] <= 5.0);
        let t0 = std::time::Instant::now();
        traj()
            .arg("-S")
            .arg(&store)
            .args(["ls", "builder/workspace/logs/round10"])
            .assert()
            .success();
        assert!(t0.elapsed().as_millis() < 500);
    }

    #[test]
    fn t7_whole_run() {
        let Some((src, ad)) = env_or_skip() else {
            panic!("set TRAJ_SLOW_SRC and TRAJ_SLOW_ADAPTER")
        };
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("run");
        let t0 = std::time::Instant::now();
        traj()
            .args(["pack"])
            .arg(&src)
            .arg("--out")
            .arg(&store)
            .args(["--adapter", &ad, "--no-derive"])
            .assert()
            .success();
        let pack_s = t0.elapsed().as_secs_f64();
        let c = counts(&store);
        eprintln!("whole run: {c} in {pack_s:.1} s");
        check_or_record(&expected_path(&src, "run"), &c);
        let paths = c["paths"].as_u64().unwrap();
        assert!(
            pack_s <= 120.0 * (paths as f64 / 2_000_000.0).max(1.0),
            "pack {pack_s:.1} s for {paths} paths"
        );
        let mut store_bytes = 0u64;
        let mut files = 0;
        for e in walkdir::WalkDir::new(&store).into_iter().flatten() {
            if e.file_type().is_file() {
                store_bytes += e.metadata().unwrap().len();
                files += 1;
            }
        }
        let kept = c["bytes"].as_u64().unwrap();
        assert!(
            store_bytes * 20 <= kept,
            "store {store_bytes} B is more than 5 % of {kept} B"
        );
        assert!(files <= 100, "{files} files");
        traj()
            .arg("-S")
            .arg(&store)
            .args(["verify", "--deep"])
            .assert()
            .success();
        for (args, limit_ms) in [
            (vec!["ls", "rounds"], 500u128),
            (vec!["stat", "rounds/round-0037/reviewer/review.json"], 200),
            (vec!["cat", "rounds/round-0037/reviewer/review.json"], 200),
            (vec!["find", "--name", "COMPLETE"], 2000),
            (vec!["sql", "select count(*) from files"], 2000),
        ] {
            let t0 = std::time::Instant::now();
            traj().arg("-S").arg(&store).args(&args).assert().success();
            let ms = t0.elapsed().as_millis();
            assert!(ms <= limit_ms, "{args:?} took {ms} ms");
        }
        // one round extracted and byte-identical to the source
        let out = tmp.path().join("r37");
        traj()
            .arg("-S")
            .arg(&store)
            .args(["extract", "rounds/round-0037"])
            .arg(&out)
            .assert()
            .success();
        let got = snapshot(&out, &[]);
        let kept: std::collections::HashSet<&String> = got.iter().map(|(p, _)| p).collect();
        let want: Vec<(String, String)> = snapshot(&src.join("rounds/round-0037"), &[])
            .into_iter()
            .filter(|(p, _)| kept.contains(p))
            .collect();
        assert_eq!(got, want);
        // compatibility: the DuckDB CLI reads every parquet file when present on the host
        if let Ok(out) = std::process::Command::new("duckdb")
            .args([
                "-csv",
                "-c",
                &format!(
                    "select count(*) from read_parquet('{}/catalog/files-*.parquet')",
                    store.display()
                ),
            ])
            .output()
        {
            if out.status.success() {
                assert!(String::from_utf8_lossy(&out.stdout).contains(&paths.to_string()));
            }
        }
    }
}
