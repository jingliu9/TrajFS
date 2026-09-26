//! Event derivation: trajectories in copilot-cli, claude-code, codex and heterogeneous JSONL formats become
//! rows of the `events` table without losing a line, `traj derive` re-derives atomically (a failure keeps the
//! previous generation, active readers are never pulled from under), and adapter names cannot escape the store.

mod common;

use common::*;
use std::fs;
use std::path::PathBuf;

#[test]
fn claude_code_session_log_derives_events() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("cc");
    fs::create_dir_all(&src).unwrap();
    fs::copy(
        fixtures_dir().join("claude-session.jsonl"),
        src.join("session.jsonl"),
    )
    .unwrap();
    let store = tmp.path().join("cc.store");
    traj()
        .args(["pack"])
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "claude-code", "--rules", "none"])
        .assert()
        .success();
    assert!(
        !derived_segments(&store).is_empty(),
        "no derived events segment"
    );
    if !sql_available() {
        eprintln!("skipping SQL checks: built without the sql feature");
        return;
    }
    let s = traj_stdout(
        &store,
        &[
            "sql",
            "--csv",
            "select seq, type, actor, id, parent_id, tool_name from events order by seq",
        ],
    );
    assert_eq!(s.lines().count(), 6, "{s}");
    assert!(
        s.contains("1,user,user,u1,,")
            && s.contains("2,assistant,assistant,a1,u1,Bash")
            && s.contains("3,user,user,u2,a1,"),
        "{s}"
    );
}

/// The published event segments of every batch, as parquet paths under `<store>/derived`.
fn derived_segments(store: &std::path::Path) -> Vec<PathBuf> {
    manifest(store)["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| batch["derived"].as_array().unwrap().clone())
        .map(|entry| {
            store
                .join("derived")
                .join(entry.as_str().unwrap())
                .with_extension("parquet")
        })
        .collect()
}

#[test]
fn heterogeneous_jsonl_keeps_every_line_and_finds_the_envelope_by_common_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("hj");
    fs::create_dir_all(&src).unwrap();
    fs::copy(fixtures_dir().join("hetero.jsonl"), src.join("log.jsonl")).unwrap();
    let store = tmp.path().join("hj.store");
    traj()
        .args(["pack"])
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", "jsonl", "--rules", "none"])
        .assert()
        .success();
    assert!(
        !derived_segments(&store).is_empty(),
        "no derived events segment"
    );
    if !sql_available() {
        eprintln!("skipping SQL checks: built without the sql feature");
        return;
    }
    let s = traj_stdout(&store, &["sql", "--csv", "select seq, type, actor, tool_name, exit_code, parent_id, ts is not null from events order by seq"]);
    assert_eq!(s.lines().count(), 6, "{s}");
    assert!(
        s.contains("0,start,,,,,true")
            && s.contains("1,tool,,grep,0,,true")
            && s.contains("2,end,assistant,,,e1,true")
            && s.contains("3,_untyped,")
            && s.contains("4,_unparsed,"),
        "{s}"
    );
    let s = traj_stdout(
        &store,
        &[
            "sql",
            "--csv",
            "select payload_json from events where seq=3",
        ],
    );
    assert!(s.contains("recognisable"));
}

#[test]
fn rederive_with_a_bumped_adapter_replaces_only_the_derived_segment() {
    let e = packed("none");
    let before = snapshot(&e.store.join("packs"), &[]);
    let before_cat = snapshot(&e.store.join("catalog"), &[]);
    let old_derived = derived_segments(&e.store);
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(fixtures_dir().join("rounds-v2.toml"))
        .assert()
        .success();
    assert_eq!(snapshot(&e.store.join("packs"), &[]), before);
    assert_eq!(snapshot(&e.store.join("catalog"), &[]), before_cat);
    let after = manifest(&e.store);
    let new_derived: Vec<&str> = after["batches"]
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
    assert!(derived_segments(&e.store).iter().all(|path| path.is_file()));
    if sql_available() {
        let s = traj_stdout(
            &e.store,
            &[
                "sql",
                "--csv",
                "select distinct adapter_version from events",
            ],
        );
        assert!(s.contains("\n2"), "{s}");
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
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
        assert!(
            !derived_segments(&store).is_empty(),
            "no derived events segment (derive = {derive})"
        );
        if sql_available() {
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
        } else {
            eprintln!("skipping SQL checks: built without the sql feature");
        }
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
    let active = derived_segments(&store);
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
    if !sql_available() {
        eprintln!("skipping SQL checks: built without the sql feature");
        return;
    }
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
        .arg(fixtures_dir().join("rounds-v2.toml"))
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(old_derived.iter().all(|path| path.is_file()));
    drop(reader);

    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["derive", "--adapter"])
        .arg(fixtures_dir().join("rounds-v2.toml"))
        .assert()
        .success();
    assert!(old_derived.iter().all(|path| !path.exists()));
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
    let bad = e.tmp.path().join("bad-adapter.toml");
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
    assert!(!e.tmp.path().join("escaped").exists());
    assert!(!e.store.join("escaped").exists());
}
