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
    let before_manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(e.store.join("MANIFEST.json")).unwrap()).unwrap();
    let old_derived: Vec<PathBuf> = before_manifest["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| batch["derived"].as_array().unwrap())
        .map(|entry| {
            e.store
                .join("derived")
                .join(entry.as_str().unwrap())
                .with_extension("parquet")
        })
        .collect();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(fx.join("rounds-v2.toml"))
        .assert()
        .success();
    assert_eq!(snapshot(&e.store.join("packs"), &[]), before);
    assert_eq!(snapshot(&e.store.join("catalog"), &[]), before_cat);
    let after_manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(e.store.join("MANIFEST.json")).unwrap()).unwrap();
    let new_derived: Vec<&str> = after_manifest["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| batch["derived"].as_array().unwrap())
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(!new_derived.is_empty());
    assert!(new_derived
        .iter()
        .all(|entry| entry.contains("/events-rebuild-")));
    assert!(old_derived.iter().all(|path| !path.exists()));
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
fn mixed_codex_and_copilot_pack_and_explicit_derive_once() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("mixed");
    let store = temp.path().join("mixed.store");
    let adapter = temp.path().join("mixed.toml");
    fs::write(
        &adapter,
        "name='mixed'\nversion=2\n[trajectories]\nglobs=['**/events.jsonl']\nformat='auto'\n",
    )
    .unwrap();
    let copilot =
        b"{\"type\":\"tool.execution_complete\",\"data\":{\"toolName\":\"bash\",\"exitCode\":1}}\n";
    let codex = b"{\"type\":\"thread.started\",\"thread_id\":\"root\"}\n{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"command_execution\",\"exit_code\":7}}\n{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":3}}\n";
    write(&src.join("copilot/events.jsonl"), copilot);
    write(&src.join("codex/events.jsonl"), codex);
    write(
        &src.join("codex/model-events.jsonl"),
        b"{\"event\":\"codex.api_request\"}\n",
    );
    write(&src.join("codex/codex-rollouts/thread.jsonl"), codex);
    traj()
        .arg("pack")
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .arg("--adapter")
        .arg(&adapter)
        .args(["--rules", "none"])
        .assert()
        .success();
    for derive in [false, true] {
        if derive {
            traj()
                .arg("-S")
                .arg(&store)
                .arg("derive")
                .arg("--adapter")
                .arg(&adapter)
                .assert()
                .success();
        }
        let rows = traj_lines(
            &store,
            &[
                "sql",
                "--csv",
                "select count(*) n, count(distinct trajectory) trajectories from events",
            ],
        );
        assert!(rows.iter().any(|row| row == "4,2"), "{rows:?}");
        let tools = traj_lines(
            &store,
            &[
                "sql",
                "--csv",
                "select tool_name, exit_code from events where exit_code is not null",
            ],
        );
        assert!(tools.iter().any(|row| row == "bash,1"), "{tools:?}");
        assert!(
            tools.iter().any(|row| row == "command_execution,7"),
            "{tools:?}"
        );
        traj()
            .arg("-S")
            .arg(&store)
            .args(["verify", "--deep"])
            .assert()
            .success();
    }
    let output = traj()
        .arg("-S")
        .arg(&store)
        .args(["cat", "codex/events.jsonl"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, codex);
}

#[test]
fn derive_failure_keeps_the_previous_generation_published() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let raw = tmp.path().join("raw");
    let store = root.join("stores/atomic.trajstore");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&raw).unwrap();
    let mut x = 7u64;
    let payload: String = (0..120_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (b'a' + (x % 26) as u8) as char
        })
        .collect();
    write(
        &raw.join("events.jsonl"),
        format!(
            "{{\"type\":\"small\",\"payload\":\"ok\"}}\n\
             {{\"type\":\"large\",\"payload\":\"{payload}\"}}\n"
        )
        .as_bytes(),
    );
    let config = |max: Option<u64>| {
        let hook = max
            .map(|bytes| format!("[hook]\nmax_file_bytes = {bytes}\n"))
            .unwrap_or_default();
        format!(
            "data_root = {:?}\nstore_root = \"stores\"\nadapter = \"jsonl\"\nrules = \"none\"\n{hook}",
            raw
        )
    };
    fs::write(root.join("trajfs.toml"), config(None)).unwrap();
    traj()
        .current_dir(&root)
        .args(["pack"])
        .arg(&raw)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "jsonl", "--rules", "none", "--no-derive"])
        .assert()
        .success();
    traj()
        .current_dir(&root)
        .arg("-S")
        .arg(&store)
        .args(["derive", "--adapter", "jsonl"])
        .assert()
        .success();

    let manifest_path = store.join("MANIFEST.json");
    let published = fs::read(&manifest_path).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&published).unwrap();
    let active: Vec<PathBuf> = manifest["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| batch["derived"].as_array().unwrap())
        .map(|entry| {
            store
                .join("derived")
                .join(entry.as_str().unwrap())
                .with_extension("parquet")
        })
        .collect();
    assert!(!active.is_empty());

    fs::write(root.join("trajfs.toml"), config(Some(8192))).unwrap();
    traj()
        .current_dir(&root)
        .arg("-S")
        .arg(&store)
        .args(["derive", "--adapter", "jsonl"])
        .assert()
        .failure();
    assert_eq!(fs::read(&manifest_path).unwrap(), published);
    assert!(active.iter().all(|path| path.is_file()));
    assert!(!fs::read_dir(store.join("derived/jsonl"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .any(|name| name.starts_with("events-rebuild-0002")));
    let query = traj()
        .current_dir(&root)
        .arg("-S")
        .arg(&store)
        .args(["sql", "--csv", "select count(*) from events"])
        .output()
        .unwrap();
    assert!(query.status.success());
    assert!(String::from_utf8_lossy(&query.stdout).contains("\n2"));
}

#[test]
fn derive_does_not_delete_files_held_by_an_active_reader() {
    let e = packed("none");
    let reader = trajfs_core::Store::open(&e.store).unwrap();
    let old_derived = reader.derived_segments().to_vec();
    let blocked = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds-v2.toml"))
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(old_derived.iter().all(|path| path.is_file()));
    drop(reader);

    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds-v2.toml"))
        .assert()
        .success();
    assert!(old_derived.iter().all(|path| !path.exists()));
}

#[test]
fn readers_do_not_need_write_access_or_create_legacy_locks() {
    let e = packed("none");
    let lock = e.store.join(".lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o444)).unwrap();
    trajfs_core::Store::open(&e.store)
        .unwrap()
        .verify(false)
        .unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(&lock).unwrap();
    fs::set_permissions(&e.store, fs::Permissions::from_mode(0o555)).unwrap();

    let reader = trajfs_core::Store::open(&e.store).unwrap();
    assert!(!lock.exists());
    reader.verify(false).unwrap();
    let blocked = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rounds-v2.toml"))
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(!lock.exists());
    drop(reader);
    fs::set_permissions(&e.store, fs::Permissions::from_mode(0o755)).unwrap();
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

#[test]
fn malicious_adapter_names_cannot_escape_derived_root() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let store = tmp.path().join("store");
    write(&src.join("file.txt"), b"content\n");
    let adapter = tmp.path().join("bad-adapter.toml");
    fs::write(
        &adapter,
        "name = \"../../../escaped\"\nversion = 1\n[trajectories]\n\
         globs = [\"**/*.jsonl\"]\nformat = \"jsonl\"\n",
    )
    .unwrap();
    traj()
        .args(["pack"])
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", adapter.to_str().unwrap(), "--rules", "none"])
        .assert()
        .failure();
    assert!(!store.exists());
    assert!(!tmp.path().join("escaped").exists());

    let e = packed("none");
    let bad = e._tmp.path().join("bad-adapter.toml");
    fs::write(
        &bad,
        "name = \"../../../escaped\"\nversion = 1\n[trajectories]\n\
         globs = [\"**/*.jsonl\"]\nformat = \"jsonl\"\n",
    )
    .unwrap();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(&bad)
        .assert()
        .failure();
    assert!(!e._tmp.path().join("escaped").exists());
    assert!(!e.store.join("escaped").exists());
}

#[test]
fn legacy_rederived_store_remains_readable() {
    let e = packed("none");
    let manifest_path = e.store.join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let adapter_name = manifest["adapter"]["name"].as_str().unwrap();
    let declared = manifest["batches"][0]["derived"][0].as_str().unwrap();
    let old_path = e
        .store
        .join("derived")
        .join(declared)
        .with_extension("parquet");
    let legacy_path = e
        .store
        .join("derived")
        .join(adapter_name)
        .join("events-0000.parquet");
    fs::rename(old_path, legacy_path).unwrap();
    manifest["format"] = serde_json::json!(1);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    let query = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["sql", "--csv", "select count(*) from events"])
        .output()
        .unwrap();
    assert!(query.status.success());
    assert!(String::from_utf8_lossy(&query.stdout).contains("\n4"));
    traj()
        .args(["pack"])
        .arg(&e.src)
        .arg("--out")
        .arg(&e.store)
        .args(["--adapter", &adapter(), "--rules", "none"])
        .assert()
        .success();
    let upgraded: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    assert_eq!(upgraded["format"], trajfs_core::FORMAT_VERSION);
    assert!(upgraded["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| batch["derived"].as_array().unwrap())
        .any(|entry| entry == "rounds-layout/events-0000"));
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
    // Missing manifest-declared artifacts fail both quick and deep verification.
    let idx = e.store.join("packs/index-0001.parquet");
    let idx_bytes = fs::read(&idx).unwrap();
    fs::remove_file(&idx).unwrap();
    for args in [vec!["verify"], vec!["verify", "--deep"]] {
        let v = traj().arg("-S").arg(&e.store).args(args).output().unwrap();
        assert!(!v.status.success());
        assert!(
            String::from_utf8_lossy(&v.stderr).contains("manifest declares missing artifact"),
            "{}",
            String::from_utf8_lossy(&v.stderr)
        );
    }
    fs::write(&idx, idx_bytes).unwrap();
    fs::remove_file(&pack).unwrap();
    let missing_pack = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify"])
        .output()
        .unwrap();
    assert!(!missing_pack.status.success());
    assert!(String::from_utf8_lossy(&missing_pack.stderr)
        .contains("manifest declares missing artifact"));
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
    traj()
        .args(["pack"])
        .arg(&e.src)
        .arg("--out")
        .arg(&e.store)
        .args(["--adapter", &adapter(), "--rules", "none"])
        .assert()
        .success();
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
    git(&["add", "trajfs.toml"]);
    git(&[
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-q",
        "-m",
        "traj policy",
    ]);
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

#[test]
fn t9d_init_mount_root_writes_the_vscode_watcher_exclude() {
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

/// T10 (docs/PLAN-fuse.md §12): the read-only FUSE projection. Each test skips, with a message, where
/// `/dev/fuse` or `fusermount3` is missing (most CI containers). A `Mounted` guard tears the mount down even when a
/// test panics.
mod t10 {
    use super::*;
    use std::io::{Read, Seek};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, Instant};

    fn fuse_available() -> bool {
        if !Path::new("/dev/fuse").exists() {
            eprintln!("skipped: no /dev/fuse");
            return false;
        }
        let has = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .any(|d| d.join("fusermount3").is_file() || d.join("fusermount").is_file())
            })
            .unwrap_or(false);
        if !has {
            eprintln!("skipped: no fusermount3 on PATH");
        }
        has
    }

    fn mounted(mp: &Path) -> bool {
        let want = mp.canonicalize().unwrap_or_else(|_| mp.to_path_buf());
        fs::read_to_string("/proc/self/mounts")
            .unwrap_or_default()
            .lines()
            .any(|l| {
                l.split(' ')
                    .nth(1)
                    .map(|m| Path::new(m) == want || Path::new(m) == mp)
                    .unwrap_or(false)
            })
    }

    fn wait_for(cond: impl Fn() -> bool, secs: f64) -> bool {
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < secs {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        cond()
    }

    fn bin() -> PathBuf {
        assert_cmd::cargo::cargo_bin("traj")
    }

    struct Mounted {
        child: std::process::Child,
        mp: PathBuf,
        log: PathBuf,
    }

    /// `traj [-S store] mount <mp> <extra>` in the foreground; returns once the mount is live.
    fn mount(store: Option<&Path>, mp: &Path, extra: &[&str], env: &[(&str, &Path)]) -> Mounted {
        let _ = fs::create_dir_all(mp); // fails on a stale mountpoint, which `traj mount` clears itself
        let log = PathBuf::from(format!("{}.log", mp.display()));
        let f = fs::File::create(&log).unwrap();
        let mut c = std::process::Command::new(bin());
        if let Some(s) = store {
            c.arg("-S").arg(s);
        }
        c.arg("mount").arg(mp).args(extra);
        // never touch the developer's real VS Code settings: an empty home unless the test gives one
        let home = mp.with_extension("home");
        let _ = fs::create_dir_all(&home);
        c.env("HOME", &home);
        for (k, v) in env {
            c.env(k, v);
        }
        c.stdin(std::process::Stdio::null())
            .stdout(f.try_clone().unwrap())
            .stderr(f);
        let mut child = c.spawn().unwrap();
        // live = listed in /proc/self/mounts and answering (a stale entry from a killed process is listed too)
        let live = || mounted(mp) && fs::metadata(mp).is_ok();
        let start = Instant::now();
        while !live()
            && child.try_wait().unwrap().is_none()
            && start.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            live(),
            "mount did not come up: {}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        Mounted {
            child,
            mp: mp.to_path_buf(),
            log,
        }
    }

    impl Mounted {
        fn signal(&self, sig: i32) {
            unsafe {
                libc::kill(self.child.id() as i32, sig);
            }
        }
        fn log(&self) -> String {
            fs::read_to_string(&self.log).unwrap_or_default()
        }
        fn exited(&mut self) -> bool {
            self.child.try_wait().unwrap().is_some()
        }
    }

    impl Drop for Mounted {
        fn drop(&mut self) {
            if !self.exited() {
                self.signal(libc::SIGTERM);
            }
            if !wait_for(|| !mounted(&self.mp), 5.0) {
                let _ = std::process::Command::new("fusermount3")
                    .args(["-u", "-z"])
                    .arg(&self.mp)
                    .status();
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn extracted(e: &Env) -> PathBuf {
        let out = e._tmp.path().join("out");
        traj()
            .arg("-S")
            .arg(&e.store)
            .args(["extract", ""])
            .arg(&out)
            .assert()
            .success();
        out
    }

    fn write_config(tmp: &Path) -> PathBuf {
        let data = tmp.join("data");
        let stores = tmp.join("stores");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&stores).unwrap();
        let cfg = tmp.join("trajfs.toml");
        fs::write(
            &cfg,
            format!(
                "data_root = \"{}\"\nstore_root = \"{}\"\nadapter = \"none\"\nrules = \"none\"\n",
                data.display(),
                stores.display()
            ),
        )
        .unwrap();
        cfg
    }

    fn pack_into(src: &Path, store: &Path, label: &str) {
        traj()
            .args(["pack"])
            .arg(src)
            .arg("--out")
            .arg(store)
            .args(["--adapter", &adapter(), "--rules", "none", "--label", label])
            .assert()
            .success();
    }

    #[test]
    fn t10_mount_is_byte_identical_and_sigterm_unmounts() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        let m = mount(Some(&e.store), &mp, &[], &[]);
        let out = extracted(&e);
        assert_eq!(snapshot(&mp, &[]), snapshot(&out, &[]));
        // no write bit anywhere; exec bit and symlinks preserved
        for ent in walkdir::WalkDir::new(&mp).min_depth(1) {
            let ent = ent.unwrap();
            let md = fs::symlink_metadata(ent.path()).unwrap();
            if md.file_type().is_symlink() {
                continue; // symlink modes are always 0777 on Linux
            }
            assert_eq!(md.mode() & 0o222, 0, "{}", ent.path().display());
        }
        assert_eq!(
            fs::metadata(mp.join("rounds/round-0001/builder/run"))
                .unwrap()
                .mode()
                & 0o777,
            0o555
        );
        assert_eq!(
            fs::read_link(mp.join("rounds/round-0001/builder/link")).unwrap(),
            Path::new("logs/a.stdout")
        );
        assert_eq!(
            fs::metadata(mp.join("rounds/round-0001/builder/empty"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            fs::metadata(mp.join("rounds/round-0002/builder/big.bin"))
                .unwrap()
                .len(),
            2 * 1024 * 1024 + 123
        );
        assert_eq!(
            fs::read(mp.join("rounds/round-0001/reviewer/review.json")).unwrap(),
            b"{\"verdict\":\"not done\"}\n"
        );
        // SIGTERM: clean unmount, no fusermount needed
        m.signal(libc::SIGTERM);
        assert!(
            wait_for(|| !mounted(&mp), 5.0),
            "still mounted after SIGTERM"
        );
        drop(m);
    }

    #[test]
    fn t10b_mount_matches_catalog_verbs() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        let _m = mount(Some(&e.store), &mp, &[], &[]);
        for d in ["rounds/round-0001/builder", "rounds", ""] {
            let mut want = lines_of("ls", &["-A", if d.is_empty() { "." } else { d }], &mp);
            want.retain(|n| !n.is_empty());
            let mut got: Vec<String> = traj_lines(&e.store, &["ls", d])
                .into_iter()
                .map(|n| n.trim_end_matches('/').to_string())
                .collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "ls {d}");
        }
        let mut want: Vec<String> = lines_of("find", &[".", "-type", "f", "-o", "-type", "l"], &mp)
            .into_iter()
            .map(|p| p.trim_start_matches("./").to_string())
            .collect();
        want.sort();
        assert_eq!(traj_lines(&e.store, &["find"]), want, "find");
        let du = lines_of("du", &["-sb", "--apparent-size", "rounds"], &mp);
        let bytes: i64 = du[0].split_whitespace().next().unwrap().parse().unwrap();
        assert!(bytes > 2 * 1024 * 1024, "du {bytes}");
        // statfs reports the store's kept bytes
        let df = lines_of("df", &["-B1", "--output=size", mp.to_str().unwrap()], &mp);
        assert!(
            df.iter()
                .any(|l| l.trim().parse::<i64>().map(|v| v > 0).unwrap_or(false)),
            "{df:?}"
        );
    }

    #[test]
    fn t10c_read_only_and_xattrs() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        let _m = mount(Some(&e.store), &mp, &[], &[]);
        let erofs = |r: std::io::Result<()>, what: &str| {
            let err = r.expect_err(what);
            assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{what}: {err}");
        };
        erofs(fs::File::create(mp.join("x")).map(|_| ()), "create");
        erofs(fs::create_dir(mp.join("d")), "mkdir");
        erofs(
            fs::remove_file(mp.join("rounds/round-0001/builder/empty")),
            "unlink",
        );
        erofs(
            fs::OpenOptions::new()
                .write(true)
                .open(mp.join("rounds/round-0001/reviewer/review.json"))
                .map(|_| ()),
            "open for write",
        );
        erofs(
            fs::set_permissions(
                mp.join("rounds/round-0001/builder/run"),
                fs::Permissions::from_mode(0o644),
            ),
            "chmod",
        );
        erofs(fs::rename(mp.join("rounds"), mp.join("r2")), "rename");
        // xattrs: sha, batch and adapter attrs
        let p = std::ffi::CString::new(
            mp.join("rounds/round-0001/reviewer/review.json")
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let get = |name: &str| -> Option<String> {
            let n = std::ffi::CString::new(name).unwrap();
            let mut buf = vec![0u8; 256];
            let r = unsafe {
                libc::getxattr(
                    p.as_ptr(),
                    n.as_ptr(),
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                )
            };
            if r < 0 {
                None
            } else {
                Some(String::from_utf8_lossy(&buf[..r as usize]).to_string())
            }
        };
        let st = traj()
            .arg("-S")
            .arg(&e.store)
            .args(["stat", "rounds/round-0001/reviewer/review.json"])
            .output()
            .unwrap();
        let st = String::from_utf8_lossy(&st.stdout);
        let sha = st
            .lines()
            .find_map(|l| l.strip_prefix("sha256:"))
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(get("user.traj.sha256").as_deref(), Some(sha.as_str()));
        assert_eq!(get("user.traj.batch").as_deref(), Some("1"));
        assert_eq!(get("user.traj.attr.round").as_deref(), Some("1"));
        assert_eq!(get("user.traj.attr.role").as_deref(), Some("reviewer"));
        assert_eq!(get("user.traj.attr.nope"), None);
        let mut buf = vec![0u8; 1024];
        let n = unsafe { libc::listxattr(p.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
        assert!(n > 0);
        let names = String::from_utf8_lossy(&buf[..n as usize]).to_string();
        assert!(
            names.contains("user.traj.attr.role\0") && names.contains("user.traj.sha256\0"),
            "{names:?}"
        );
    }

    #[test]
    fn t10d_grep_on_the_mount_equals_traj_grep() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        let _m = mount(Some(&e.store), &mp, &[], &[]);
        for p in [
            "hello",
            "^Trace",
            "not done",
            "[0-9]+",
            "grüße",
            "zzz-never",
            "^#!",
            "json",
            "e.*l",
            "^$",
        ] {
            let out_grep = std::process::Command::new("grep")
                .args(["-rlE", "--", p, "."])
                .current_dir(&mp)
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
    fn t10e_concurrent_readers() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        let _m = mount(Some(&e.store), &mp, &["--blob-cache", "1M"], &[]);
        let want = snapshot(&mp, &[]);
        let files: Vec<(PathBuf, String)> = want
            .iter()
            .filter(|(_, d)| d.starts_with("file:"))
            .map(|(rel, d)| (mp.join(rel), d.clone()))
            .collect();
        assert!(files.len() >= 8);
        let reads = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|s| {
            for t in 0..8 {
                let files = &files;
                let reads = &reads;
                s.spawn(move || {
                    let start = Instant::now();
                    let mut i = t;
                    while start.elapsed() < Duration::from_secs(2) {
                        let (p, d) = &files[i % files.len()];
                        let md = fs::metadata(p).unwrap();
                        let bytes = fs::read(p).unwrap();
                        let mut h = <sha2::Sha256 as sha2::Digest>::new();
                        sha2::Digest::update(&mut h, &bytes);
                        let exec = md.permissions().mode() & 0o111 != 0;
                        assert_eq!(
                            &format!("file:{}:{}", exec, hex::encode(sha2::Digest::finalize(h))),
                            d,
                            "{}",
                            p.display()
                        );
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        i += 7;
                    }
                });
            }
        });
        assert!(reads.load(std::sync::atomic::Ordering::Relaxed) > 100);
    }

    #[test]
    fn t10f_new_batch_appears_without_remount() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        let m = mount(Some(&e.store), &mp, &["--ttl", "1"], &[]);
        let review = mp.join("rounds/round-0001/reviewer/review.json");
        assert_eq!(fs::read(&review).unwrap(), b"{\"verdict\":\"not done\"}\n");
        let mut held = fs::File::open(&review).unwrap();
        // list the parent so the kernel caches it, then land a batch behind the mount's back
        assert!(!mp.join("rounds/round-0003").exists());
        write(
            &e.src.join("rounds/round-0003/builder/new.txt"),
            b"brand new\n",
        );
        write(
            &e.src.join("rounds/round-0001/reviewer/review.json"),
            b"{\"verdict\":\"done\"}\n",
        );
        pack_into(&e.src, &e.store, "two");
        assert!(
            wait_for(
                || mp.join("rounds/round-0003/builder/new.txt").is_file(),
                8.0
            ),
            "new path not visible: {}",
            m.log()
        );
        assert_eq!(
            fs::read(mp.join("rounds/round-0003/builder/new.txt")).unwrap(),
            b"brand new\n"
        );
        assert!(
            wait_for(
                || fs::read(&review).unwrap() == b"{\"verdict\":\"done\"}\n",
                8.0
            ),
            "re-recorded path still old: {}",
            m.log()
        );
        // the handle opened before the batch still reads the old bytes
        let mut old = Vec::new();
        held.seek(std::io::SeekFrom::Start(0)).unwrap();
        held.read_to_end(&mut old).unwrap();
        assert_eq!(old, b"{\"verdict\":\"not done\"}\n");
        assert!(m.log().contains("new batch, store reopened"), "{}", m.log());
    }

    #[test]
    fn t10g_corruption_is_eio_for_the_affected_file_only() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let pack = e.store.join("packs/0001.pack");
        let mut bytes = fs::read(&pack).unwrap();
        let i = bytes.len() / 2;
        bytes[i] ^= 0xff;
        fs::write(&pack, &bytes).unwrap();
        let mp = e._tmp.path().join("mnt");
        let m = mount(Some(&e.store), &mp, &[], &[]);
        let (mut ok, mut eio) = (0, 0);
        for ent in walkdir::WalkDir::new(&mp).min_depth(1) {
            let ent = ent.unwrap();
            if !ent.file_type().is_file() {
                continue;
            }
            match fs::read(ent.path()) {
                Ok(_) => ok += 1,
                Err(err) => {
                    assert_eq!(
                        err.raw_os_error(),
                        Some(libc::EIO),
                        "{}: {err}",
                        ent.path().display()
                    );
                    eio += 1;
                }
            }
        }
        assert!(eio >= 1 && ok >= 1, "ok {ok} eio {eio}: {}", m.log());
        assert!(
            m.log().contains("does not match") || m.log().contains("decompress"),
            "{}",
            m.log()
        );
    }

    #[test]
    fn t10h_lifecycle_stale_busy_lazy_daemon() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        // SIGKILL leaves a stale mount; the next `traj mount` clears it
        {
            let mut m = mount(Some(&e.store), &mp, &[], &[]);
            m.signal(libc::SIGKILL);
            let _ = m.child.wait();
            assert!(
                wait_for(|| fs::metadata(&mp).is_err(), 5.0),
                "mountpoint should be stale"
            );
            std::mem::forget(m); // no cleanup: the point is the stale state
        }
        let m = mount(Some(&e.store), &mp, &[], &[]);
        assert!(m.log().contains("stale"), "{}", m.log());
        assert!(mp.join("rounds").is_dir());
        // busy: an open file blocks a plain umount; --lazy detaches it
        let held = fs::File::open(mp.join("rounds/round-0001/reviewer/review.json")).unwrap();
        traj().arg("umount").arg(&mp).assert().failure();
        assert!(mounted(&mp));
        traj()
            .args(["umount", "--lazy"])
            .arg(&mp)
            .assert()
            .success();
        assert!(wait_for(|| !mounted(&mp), 5.0));
        drop(held);
        drop(m);
        // --daemon returns once the mount is live; umount ends it
        let mp2 = e._tmp.path().join("mnt2");
        fs::create_dir_all(&mp2).unwrap();
        traj()
            .arg("-S")
            .arg(&e.store)
            .args(["mount", "--daemon"])
            .env("HOME", e._tmp.path())
            .arg(&mp2)
            .assert()
            .success()
            .stdout(predicates::str::contains("mounted "));
        assert!(mounted(&mp2));
        assert!(PathBuf::from(format!("{}.log", mp2.display())).is_file());
        assert_eq!(
            fs::read(mp2.join("rounds/round-0001/builder/logs/a.stdout")).unwrap(),
            b"hello\n"
        );
        traj().arg("umount").arg(&mp2).assert().success();
        assert!(wait_for(|| !mounted(&mp2), 5.0));
        // no traj mounts left in this test's tmp (other tests may hold theirs)
        traj().arg("umount").arg(&mp2).assert().failure();
    }

    #[test]
    fn t10i_refusals() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let tmp = e._tmp.path();
        let cfg = write_config(tmp);
        // inside a git work tree
        fs::create_dir_all(tmp.join("repo/.git")).unwrap();
        traj()
            .arg("-S")
            .arg(&e.store)
            .arg("mount")
            .arg(tmp.join("repo/mnt"))
            .assert()
            .failure()
            .stderr(predicates::str::contains("git work tree"));
        // inside data_root / store_root
        for (sub, msg) in [
            ("data/mnt", "inside data_root"),
            ("stores/mnt", "inside store_root"),
        ] {
            traj()
                .env("TRAJ_CONFIG", &cfg)
                .arg("-S")
                .arg(&e.store)
                .arg("mount")
                .arg(tmp.join(sub))
                .assert()
                .failure()
                .stderr(predicates::str::contains(msg));
        }
        // not empty
        write(&tmp.join("full/keep"), b"x");
        traj()
            .arg("-S")
            .arg(&e.store)
            .arg("mount")
            .arg(tmp.join("full"))
            .assert()
            .failure()
            .stderr(predicates::str::contains("not empty"));
        // already mounted
        let mp = tmp.join("mnt");
        let _m = mount(Some(&e.store), &mp, &[], &[]);
        traj()
            .arg("-S")
            .arg(&e.store)
            .arg("mount")
            .arg(&mp)
            .assert()
            .failure()
            .stderr(predicates::str::contains("already mounted"));
        // no store and no config (a directory with no trajfs.toml above it)
        let bare = tempfile::tempdir().unwrap();
        traj()
            .env_remove("TRAJ_STORE")
            .env_remove("TRAJ_CONFIG")
            .arg("mount")
            .arg(bare.path().join("mnt3"))
            .current_dir(bare.path())
            .assert()
            .failure()
            .stderr(predicates::str::contains("no store given"));
    }

    #[test]
    fn t10j_multi_store_root_lists_stores_and_picks_up_new_ones() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let tmp = e._tmp.path();
        let cfg = write_config(tmp);
        pack_into(&e.src, &tmp.join("stores/a.trajstore"), "a");
        pack_into(&e.src, &tmp.join("stores/b.trajstore"), "b");
        let mp = tmp.join("mnt");
        let m = mount(
            None,
            &mp,
            &["--ttl", "1"],
            &[("TRAJ_CONFIG", cfg.as_path())],
        );
        let mut ids = lines_of("ls", &["-A", "."], &mp);
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(
            fs::read(mp.join("a/rounds/round-0001/reviewer/review.json")).unwrap(),
            b"{\"verdict\":\"not done\"}\n"
        );
        assert!(fs::metadata(mp.join("b/rounds")).unwrap().is_dir());
        assert!(!mp.join("c").exists());
        pack_into(&e.src, &tmp.join("stores/c.trajstore"), "c");
        assert!(
            wait_for(|| mp.join("c/rounds/round-0001").is_dir(), 10.0),
            "new store not visible: {}",
            m.log()
        );
        // explicit several -S: same shape
        drop(m);
        let mp2 = tmp.join("mnt2");
        let _m2 = mount(
            None,
            &mp2,
            &[
                "-S",
                tmp.join("stores/a.trajstore").to_str().unwrap(),
                "-S",
                tmp.join("stores/b.trajstore").to_str().unwrap(),
            ],
            &[],
        );
        let mut ids = lines_of("ls", &["-A", "."], &mp2);
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn t10l_memory_budget_trims_and_stays_correct() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let mp = e._tmp.path().join("mnt");
        // a budget far below what the fixture needs, so every trim path runs; a short TTL so nudges apply
        let m = mount(
            Some(&e.store),
            &mp,
            &["--memory", "4K", "--ttl", "0.2", "--blob-cache", "8M"],
            &[],
        );
        assert!(m.log().contains("memory target 0 MB"), "{}", m.log());
        let want = snapshot(&extracted(&e), &[]);
        // walks spaced beyond the TTL window (served more than ttl+1 s ago), so the trim may nudge the kernel;
        // a listing after the pause triggers the trim that observes the forgets
        for _ in 0..3 {
            assert_eq!(snapshot(&mp, &[]), want);
            std::thread::sleep(Duration::from_millis(2500));
            let _ = fs::read_dir(&mp).unwrap().count(); // trim: invalidates stale entries
            std::thread::sleep(Duration::from_millis(1200));
            let _ = fs::read_dir(&mp).unwrap().count(); // trim: reports the count after the kernel's forgets
        }
        let log = m.log();
        assert!(log.contains("memory budget 0 MB exceeded"), "{log}");
        assert!(log.contains("asked the kernel to forget"), "{log}");
        // the kernel released lookups on invalidated entries: a later trim reports fewer inodes than the peak
        let counts: Vec<u64> = log
            .lines()
            .filter_map(|l| {
                let i = l.find(" inodes):")?;
                l[..i].rsplit(' ').next()?.parse().ok()
            })
            .collect();
        assert!(counts.len() >= 2, "{log}");
        let peak = *counts.iter().max().unwrap();
        assert!(
            counts.iter().any(|&c| c < peak),
            "inode count never dropped below the peak {peak}: {counts:?}\n{log}"
        );
        // percent and unbounded forms parse
        drop(m);
        let mp2 = e._tmp.path().join("mnt2");
        let m2 = mount(Some(&e.store), &mp2, &["--memory", "10%"], &[]);
        assert!(
            m2.log().contains("memory target") && !m2.log().contains("unbounded"),
            "{}",
            m2.log()
        );
        drop(m2);
        let mp3 = e._tmp.path().join("mnt3");
        let m3 = mount(Some(&e.store), &mp3, &["--memory", "0"], &[]);
        assert!(m3.log().contains("memory target unbounded"), "{}", m3.log());
    }

    #[test]
    fn t12_bench_reports_the_verbs_and_the_mount() {
        let e = packed("none");
        let out = traj()
            .arg("-S")
            .arg(&e.store)
            .args(["bench", "--ls-dir", "rounds"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        for k in [
            "ls_s",
            "stat_s",
            "cat_s",
            "find_name_s",
            "verify_s",
            "paths",
        ] {
            assert!(j.get(k).is_some(), "missing {k}: {j}");
        }
        if fuse_available() {
            assert!(j["mount_s"].as_f64().unwrap() > 0.0, "{j}");
            assert!(j["mount_ls_cold_s"].as_f64().unwrap() > 0.0, "{j}");
            assert!(j["mount_cat_s"].as_f64().unwrap() > 0.0, "{j}");
        } else {
            assert!(j["mount"].as_str().unwrap().starts_with("skipped"), "{j}");
        }
        assert!(!mounted(&std::env::temp_dir()), "bench left a mount behind");
    }

    #[test]
    fn t10m_mount_writes_the_vscode_machine_settings() {
        if !fuse_available() {
            return;
        }
        let e = packed("none");
        let home = e._tmp.path().join("home");
        let machine = home.join(".vscode-server/data/Machine");
        fs::create_dir_all(&machine).unwrap();
        fs::write(machine.join("settings.json"), "{ \"x\": 1 }").unwrap();
        let mp = e._tmp.path().join("traj-mnt");
        let m = mount(Some(&e.store), &mp, &[], &[("HOME", home.as_path())]);
        assert!(m.log().contains("VS Code settings for"), "{}", m.log());
        let s: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(machine.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(s["x"], 1, "{s}");
        let key = format!("{}/**", mp.canonicalize().unwrap().display());
        assert_eq!(s["files.watcherExclude"][&key], true, "{s}");
        assert_eq!(s["files.readonlyInclude"][&key], true, "{s}");
        drop(m);
        // --no-vscode leaves it alone; a home without VS Code is not touched either
        fs::write(machine.join("settings.json"), "{ \"x\": 2 }").unwrap();
        let m = mount(
            Some(&e.store),
            &mp,
            &["--no-vscode"],
            &[("HOME", home.as_path())],
        );
        assert!(!m.log().contains("VS Code settings"), "{}", m.log());
        assert_eq!(
            fs::read_to_string(machine.join("settings.json")).unwrap(),
            "{ \"x\": 2 }"
        );
        drop(m);
        let bare = e._tmp.path().join("home2");
        fs::create_dir_all(&bare).unwrap();
        let m = mount(Some(&e.store), &mp, &[], &[("HOME", bare.as_path())]);
        assert!(!bare.join(".vscode-server").exists() && !bare.join(".config").exists());
        drop(m);
    }

    /// T10k (feature `slow`): the reference store named by TRAJ_SLOW_STORE (a `.trajstore` directory, e.g. the
    /// rank 1 run). Prints the docs/PLAN-fuse.md §11 numbers and asserts their bounds. "The round" is the first
    /// directory two levels below the root (`rounds/round-0001` in the onesw layout).
    #[cfg(feature = "slow")]
    #[test]
    fn t10k_reference_store() {
        if !fuse_available() {
            return;
        }
        let Some(store) = std::env::var_os("TRAJ_SLOW_STORE") else {
            eprintln!("skipped: TRAJ_SLOW_STORE not set");
            return;
        };
        let store = PathBuf::from(store);
        let tmp = tempfile::tempdir().unwrap();
        let mp = tmp.path().join("mnt");
        let t = Instant::now();
        let m = mount(Some(&store), &mp, &[], &[]);
        let mount_time = t.elapsed();
        let first_dir = |d: &Path| -> PathBuf {
            let mut v: Vec<PathBuf> = fs::read_dir(d)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            v.sort();
            v[0].clone()
        };
        // the top-level directory with the most children (rounds/, not preflight/), then its first child
        let widest = |d: &Path| -> PathBuf {
            let mut v: Vec<(usize, PathBuf)> = fs::read_dir(d)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .map(|p| (fs::read_dir(&p).map(|r| r.count()).unwrap_or(0), p))
                .collect();
            v.sort();
            v.last().unwrap().1.clone()
        };
        let t = Instant::now();
        let round = first_dir(&widest(&mp));
        let cold = t.elapsed();
        let t = Instant::now();
        let n_direct = fs::read_dir(&round).unwrap().count();
        let warm = t.elapsed();
        let t = Instant::now();
        let n_mount = walkdir::WalkDir::new(&round).into_iter().count();
        let walk_cold = t.elapsed();
        let t = Instant::now();
        let _ = walkdir::WalkDir::new(&round).into_iter().count();
        let walk_warm = t.elapsed();
        let rel = round
            .strip_prefix(&mp)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let out = tmp.path().join("out");
        let t = Instant::now();
        traj()
            .arg("-S")
            .arg(&store)
            .args(["extract", &rel])
            .arg(&out)
            .assert()
            .success();
        let extract_time = t.elapsed();
        let t = Instant::now();
        let n_native = walkdir::WalkDir::new(&out).into_iter().count();
        let walk_native = t.elapsed();
        let files: Vec<PathBuf> = walkdir::WalkDir::new(&round)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .take(2000)
            .collect();
        let mut lat: Vec<Duration> = files
            .iter()
            .step_by(10)
            .map(|p| {
                let t = Instant::now();
                let _ = fs::read(p).unwrap();
                t.elapsed()
            })
            .collect();
        lat.sort();
        let p50 = lat[lat.len() / 2];
        let t = Instant::now();
        assert_eq!(snapshot(&round, &[]), snapshot(&out, &[]));
        let diff_time = t.elapsed();
        let rss_kb: u64 = fs::read_to_string(format!("/proc/{}/status", m.child.id()))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .unwrap()
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .unwrap();
        eprintln!(
            "t10k {}: mount {mount_time:?}; first descent {cold:?}; readdir warm {warm:?} ({n_direct} entries); \
             walk of {rel}: {n_mount} entries, cold {walk_cold:?}, warm {walk_warm:?}, native {n_native} in \
             {walk_native:?}; extract {extract_time:?}; read p50 {p50:?}; byte-identical in {diff_time:?}; \
             RSS {} MB",
            store.display(),
            rss_kb / 1024
        );
        assert!(mount_time < Duration::from_secs(2), "mount {mount_time:?}");
        assert!(cold < Duration::from_secs(3), "first descent {cold:?}");
        assert!(warm < Duration::from_millis(50), "warm readdir {warm:?}");
        assert_eq!(n_mount, n_native);
        assert!(
            walk_warm.as_secs_f64() <= 8.0 * walk_native.as_secs_f64().max(0.05),
            "walk {walk_warm:?} vs native {walk_native:?}"
        );
        assert!(p50 < Duration::from_millis(5), "read p50 {p50:?}");
        assert!(rss_kb < 400 * 1024, "RSS {} MB", rss_kb / 1024);
    }
}

/// T12 Delete (tasks/PLAN-deletion.md): one round removed from every batch, survivors byte-identical,
/// Git checkpoint required, resurrection suppressed, recovery of an interrupted apply.
#[test]
fn t12_delete_removes_one_path_from_every_batch_and_keeps_git_as_the_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("trajfs")).unwrap();
    let git = |args: &[&str]| -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
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
    git(&["add", "-A"]);
    git(&[
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-q",
        "-m",
        "init",
    ]);

    // not committed as a store yet? it is (git add -A above); now a second batch, uncommitted
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
    let m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(store.join("MANIFEST.json")).unwrap()).unwrap();
    assert_eq!(m["format"], trajfs_core::FORMAT_VERSION);
    assert_eq!(m["deleted"][0]["path"], "rounds/round-0001");
    assert_eq!(m["batches"].as_array().unwrap().len(), 2);
    let events = traj()
        .arg("-S")
        .arg(&store)
        .args([
            "sql",
            "--csv",
            "select count(*) from events where trajectory like 'rounds/round-0001/%'",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&events.stdout).contains('0'),
        "{}",
        String::from_utf8_lossy(&events.stdout)
    );

    // the deletion commit; the earlier version stays in Git
    traj()
        .current_dir(&repo)
        .args(["commit"])
        .arg(&store)
        .assert()
        .success();
    assert!(git(&["log", "-1", "--format=%s"]).contains("delete rounds/round-0001"));
    traj()
        .current_dir(&repo)
        .args(["hook", "check-tree", "HEAD"])
        .assert()
        .success();
    let old = git(&["show", "HEAD~1:stores/run1.trajstore/MANIFEST.json"]);
    assert!(!old.contains("round-0001") || !old.contains("\"deleted\": [\n"));
    assert!(git(&["status", "--porcelain"]).trim().is_empty());

    // the raw tree still has the round: a new pack skips it and records why
    write(
        &data.join("run1/rounds/round-0003/builder/logs/d.stdout"),
        b"four\n",
    );
    pack("b3");
    assert!(gone(&store));
    assert!(traj_lines(&store, &["find", "--name", "d.stdout"]).len() == 1);
    let excluded = traj()
        .arg("-S")
        .arg(&store)
        .args([
            "sql",
            "--csv",
            "select path from excluded where rule = 'deleted' order by path",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&excluded.stdout).contains("rounds/round-0001"),
        "{}",
        String::from_utf8_lossy(&excluded.stdout)
    );
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
