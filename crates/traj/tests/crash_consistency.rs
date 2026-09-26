//! Crash consistency of the `traj` binary: `pack`, `delete --yes` and `derive` are
//! SIGKILLed at random points. After every kill the store must still open,
//! `verify --deep` must pass, `ls` must work, and the manifest must describe either
//! the complete pre-operation state or the complete post-operation state, never a
//! partial batch, deletion or derived generation (docs/PLAN-deletion.md §3, §4.5).
//!
//! Self-contained: no `common` module, fixtures are generated into a tempdir.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const KILLS_PER_PHASE: usize = 15;
const ROUNDS: usize = 8;
const FILES_PER_ROUND: usize = 40;
const EVENTS_PER_TRAJECTORY: usize = 120;

// ------------------------------------------------------------------ helpers

/// Small deterministic generator (xorshift64*) so runs are reproducible from a seed.
struct Rng(u64);

impl Rng {
    fn from_env() -> Self {
        let seed = std::env::var("TRAJ_CRASH_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x9E37_79B9_7F4A_7C15u64);
        eprintln!("crash_consistency: seed {seed} (override with TRAJ_CRASH_SEED)");
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn range(&mut self, lo: u64, hi_inclusive: u64) -> u64 {
        lo + self.next() % (hi_inclusive - lo + 1)
    }
    fn fill(&mut self, n: usize) -> Vec<u8> {
        // printable pseudo-random text so nothing is excluded as a binary build product
        (0..n)
            .map(|_| {
                let v = (self.next() >> 33) % 64;
                if v == 63 {
                    b'\n'
                } else {
                    b'0' + (v % 10) as u8 + if v >= 10 { 7 + (v / 10) as u8 } else { 0 }
                }
            })
            .collect()
    }
}

fn traj() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_traj"));
    c.env_remove("TRAJ_STORE").env_remove("TRAJ_CONFIG");
    c
}

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{what}: spawn: {e}"));
    assert!(
        out.status.success(),
        "{what} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo).args(args);
    run_ok(&mut c, &format!("git {}", args.join(" ")))
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn sibling(store: &Path) -> PathBuf {
    let mut name = store.file_name().unwrap().to_os_string();
    name.push(".deleting");
    store.with_file_name(name)
}

/// Put `store` back to the snapshot in `from`, discarding any leftover sibling.
fn restore_from_copy(store: &Path, from: &Path) {
    if store.exists() {
        fs::remove_dir_all(store).unwrap();
    }
    let leftover = sibling(store);
    if leftover.exists() {
        fs::remove_dir_all(&leftover).unwrap();
    }
    copy_tree(from, store);
}

/// The manifest's published state with the run-dependent fields removed
/// (timestamps, elapsed time, compressed size), so two runs of the same
/// operation compare equal.
fn manifest_state(store: &Path) -> Value {
    let text = fs::read_to_string(store.join("MANIFEST.json")).expect("MANIFEST.json exists");
    let mut v: Value = serde_json::from_str(&text).expect("MANIFEST.json parses");
    let obj = v.as_object_mut().unwrap();
    obj.remove("source");
    if let Some(batches) = obj.get_mut("batches").and_then(Value::as_array_mut) {
        for b in batches {
            let b = b.as_object_mut().unwrap();
            for k in ["created", "elapsed_ms", "packed_bytes"] {
                b.remove(k);
            }
        }
    }
    if let Some(deleted) = obj.get_mut("deleted").and_then(Value::as_array_mut) {
        for d in deleted {
            d.as_object_mut().unwrap().remove("created");
        }
    }
    v
}

fn batch_ids(state: &Value) -> Vec<u64> {
    state["batches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_u64().unwrap())
        .collect()
}

/// The invariants every interrupted run must keep.
fn assert_store_consistent(store: &Path, context: &str) {
    let mut verify = traj();
    verify.arg("-S").arg(store).args(["verify", "--deep"]);
    let out = run_ok(&mut verify, &format!("{context}: verify --deep"));
    assert!(
        out.contains("OK (deep)"),
        "{context}: verify output:\n{out}"
    );
    let mut ls = traj();
    ls.arg("-S").arg(store).args(["ls", "-l"]);
    run_ok(&mut ls, &format!("{context}: ls"));
    let mut ls = traj();
    ls.arg("-S").arg(store).args(["ls", "-R", "rounds"]);
    run_ok(&mut ls, &format!("{context}: ls -R rounds"));
    // no half-written manifest may be visible under the published name
    let text = fs::read_to_string(store.join("MANIFEST.json")).unwrap();
    serde_json::from_str::<Value>(&text).expect("published manifest is complete JSON");
}

#[derive(Debug, Default)]
struct Outcomes {
    pre: usize,
    post: usize,
    finished_before_kill: usize,
}

/// Kill delays are drawn from 5 ms up to at least 400 ms, widened to cover the
/// whole measured duration of the uninterrupted run so kills land on both sides
/// of the commit point.
fn kill_window(uninterrupted_ms: u128) -> u64 {
    (uninterrupted_ms as u64 * 6 / 5).max(400)
}

/// Spawn `cmd`, kill it after a random delay of 5 ms to `max_delay_ms`, then classify the state.
fn kill_after_random_delay(
    rng: &mut Rng,
    cmd: &mut Command,
    what: &str,
    max_delay_ms: u64,
) -> (u64, bool) {
    let delay = rng.range(5, max_delay_ms);
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("{what}: spawn: {e}"));
    sleep(Duration::from_millis(delay));
    let finished = match child.try_wait().unwrap() {
        Some(status) => {
            assert!(status.success(), "{what}: exited {status} before the kill");
            true
        }
        None => {
            child.kill().unwrap();
            child.wait().unwrap();
            false
        }
    };
    (delay, finished)
}

// ------------------------------------------------------------------ fixture

fn event_lines(rng: &mut Rng, round: usize, n: usize, first_seq: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in first_seq..first_seq + n {
        let line = match i % 5 {
            0 => format!(
                "{{\"type\":\"assistant\",\"id\":\"r{round}-{i}\",\"timestamp\":{},\"role\":\"assistant\",\"text\":\"step {i}\"}}\n",
                1_756_947_600 + i
            ),
            1 => format!(
                "{{\"type\":\"tool\",\"id\":\"r{round}-{i}\",\"parent_id\":\"r{round}-{}\",\"tool_name\":\"bash\",\"exit_code\":{}}}\n",
                i - 1,
                rng.range(0, 2)
            ),
            2 => format!("{{\"type\":\"note\",\"payload\":\"{}\"}}\n", "x".repeat(rng.range(10, 60) as usize)),
            3 => "not json, kept as _unparsed\n".to_string(),
            _ => format!("{{\"event\":\"tick\",\"ts\":\"2026-09-04T01:00:{:02}Z\"}}\n", i % 60),
        };
        out.extend_from_slice(line.as_bytes());
    }
    out
}

/// Version 1 of the source tree: rounds with many small unique files and one trajectory each.
fn build_fixture(rng: &mut Rng, src: &Path) {
    for round in 1..=ROUNDS {
        let dir = src.join(format!("rounds/round-{round:04}"));
        for f in 0..FILES_PER_ROUND {
            let size = rng.range(1_500, 6_000) as usize;
            write(
                &dir.join(format!("builder/out/file-{f:03}.txt")),
                &rng.fill(size),
            );
        }
        write(
            &dir.join("builder/events.jsonl"),
            &event_lines(rng, round, EVENTS_PER_TRAJECTORY, 0),
        );
        write(
            &dir.join("reviewer/review.json"),
            format!("{{\"round\":{round},\"verdict\":\"ok\"}}\n").as_bytes(),
        );
        write(&dir.join("shared.txt"), b"identical in every round\n");
        write(&dir.join("DONE"), b"");
    }
    write(src.join("README").as_path(), b"crash fixture\n");
}

/// Version 2: grown trajectories, rewritten files, one new round.
fn grow_fixture(rng: &mut Rng, src: &Path) {
    for round in 1..=ROUNDS {
        let dir = src.join(format!("rounds/round-{round:04}"));
        let path = dir.join("builder/events.jsonl");
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend(event_lines(rng, round, 40, EVENTS_PER_TRAJECTORY));
        fs::write(&path, bytes).unwrap();
        for f in (0..FILES_PER_ROUND).step_by(3) {
            let size = rng.range(1_500, 6_000) as usize;
            write(
                &dir.join(format!("builder/out/file-{f:03}.txt")),
                &rng.fill(size),
            );
        }
    }
    let new_round = src.join(format!("rounds/round-{:04}", ROUNDS + 1));
    for f in 0..FILES_PER_ROUND {
        let size = rng.range(1_500, 6_000) as usize;
        write(
            &new_round.join(format!("builder/out/file-{f:03}.txt")),
            &rng.fill(size),
        );
    }
    write(
        &new_round.join("builder/events.jsonl"),
        &event_lines(rng, ROUNDS + 1, EVENTS_PER_TRAJECTORY, 0),
    );
}

fn pack_cmd(src: &Path, store: &Path, label: &str) -> Command {
    let mut c = traj();
    c.arg("pack").arg(src).arg("--out").arg(store).args([
        "--adapter",
        "jsonl",
        "--rules",
        "none",
        "--jobs",
        "4",
        "--label",
        label,
    ]);
    c
}

// ------------------------------------------------------------------ phases

fn pack_phase(rng: &mut Rng, src: &Path, store: &Path, base: &Path, scratch: &Path) -> PathBuf {
    let pre = manifest_state(store);
    assert_eq!(batch_ids(&pre), vec![1]);

    // one uninterrupted run defines the complete post-operation state
    let t0 = Instant::now();
    run_ok(&mut pack_cmd(src, store, "b2"), "pack b2 (uninterrupted)");
    let full_ms = t0.elapsed().as_millis();
    let post = manifest_state(store);
    assert_eq!(batch_ids(&post), vec![1, 2]);
    assert_store_consistent(store, "pack: uninterrupted");
    let after_pack = scratch.join("after-pack");
    copy_tree(store, &after_pack);
    eprintln!("pack: uninterrupted run took {full_ms} ms");

    let mut outcomes = Outcomes::default();
    for i in 0..KILLS_PER_PHASE {
        restore_from_copy(store, base);
        let (delay, finished) = kill_after_random_delay(
            rng,
            &mut pack_cmd(src, store, "b2"),
            "pack",
            kill_window(full_ms),
        );
        let context = format!("pack kill #{i} after {delay} ms");
        assert_store_consistent(store, &context);
        let state = manifest_state(store);
        if state == pre {
            outcomes.pre += 1;
        } else if state == post {
            outcomes.post += 1;
        } else {
            panic!(
                "{context}: manifest is neither the old nor the new snapshot:\n{}",
                serde_json::to_string_pretty(&state).unwrap()
            );
        }
        if finished {
            outcomes.finished_before_kill += 1;
        }
        // the next pack cleans leftovers and publishes the batch exactly once
        run_ok(
            &mut pack_cmd(src, store, "b2"),
            &format!("{context}: pack again"),
        );
        let healed = manifest_state(store);
        if state == pre {
            assert_eq!(
                healed, post,
                "{context}: repacking after an interrupted pack"
            );
        } else {
            // the batch was already published: a repack appends an empty batch
            assert_eq!(batch_ids(&healed), vec![1, 2, 3], "{context}");
            assert_eq!(healed["batches"][2]["paths"], 0, "{context}");
        }
        assert_store_consistent(store, &format!("{context}: after repack"));
        for name in ["catalog", "packs"] {
            for entry in fs::read_dir(store.join(name)).unwrap() {
                let n = entry.unwrap().file_name().to_string_lossy().into_owned();
                assert!(
                    !n.ends_with(".tmp"),
                    "{context}: temp file {name}/{n} survived a repack"
                );
            }
        }
    }
    eprintln!("pack: {outcomes:?}");
    after_pack
}

fn delete_phase(rng: &mut Rng, repo: &Path, store: &Path) {
    let rel = store
        .strip_prefix(repo)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    git(repo, &["add", "-A", "--", &rel]);
    git(repo, &["commit", "-q", "-m", "store before deletion"]);
    let restore = |store: &Path| {
        let leftover = sibling(store);
        if leftover.exists() {
            fs::remove_dir_all(&leftover).unwrap();
        }
        git(repo, &["checkout", "-q", "HEAD", "--", &rel]);
        git(repo, &["clean", "-fdxq", "--", &rel]);
        let status = git(repo, &["status", "--porcelain", "--", &rel]);
        assert!(
            status.trim().is_empty(),
            "store dirty after restore:\n{status}"
        );
    };
    let delete_cmd = |store: &Path| {
        let mut c = traj();
        c.arg("-S")
            .arg(store)
            .args(["delete", "rounds/round-0002", "--yes"]);
        c
    };
    let pre = manifest_state(store);
    assert!(pre["deleted"].as_array().unwrap().is_empty());

    // dry run must not change anything
    let mut dry = traj();
    dry.arg("-S")
        .arg(store)
        .args(["delete", "rounds/round-0002"]);
    let out = run_ok(&mut dry, "delete dry run");
    assert!(out.contains("dry run"), "{out}");
    assert_eq!(manifest_state(store), pre);

    let t0 = Instant::now();
    run_ok(&mut delete_cmd(store), "delete --yes (uninterrupted)");
    let full_ms = t0.elapsed().as_millis();
    let post = manifest_state(store);
    assert_eq!(post["deleted"].as_array().unwrap().len(), 1);
    assert_eq!(post["deleted"][0]["path"], "rounds/round-0002");
    assert_eq!(batch_ids(&post), batch_ids(&pre), "batch history is kept");
    assert!(!sibling(store).exists());
    assert_store_consistent(store, "delete: uninterrupted");
    eprintln!("delete: uninterrupted run took {full_ms} ms");

    let mut outcomes = Outcomes::default();
    let mut leftovers = 0;
    for i in 0..KILLS_PER_PHASE {
        restore(store);
        let (delay, finished) =
            kill_after_random_delay(rng, &mut delete_cmd(store), "delete", kill_window(full_ms));
        let context = format!("delete kill #{i} after {delay} ms");
        // whatever the sibling holds, the store directory itself is a complete store
        assert_store_consistent(store, &context);
        let state_before_recover = manifest_state(store);
        let had_leftover = sibling(store).exists();
        if had_leftover {
            leftovers += 1;
            // pack and derive refuse to touch a store with a pending deletion
            let out = traj().arg("-S").arg(store).arg("derive").output().unwrap();
            assert!(
                !out.status.success(),
                "{context}: derive ran despite a pending deletion"
            );
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("--recover"),
                "{context}: derive should name --recover"
            );
        }
        let mut recover = traj();
        recover.arg("-S").arg(store).args(["delete", "--recover"]);
        let out = run_ok(&mut recover, &format!("{context}: --recover"));
        assert!(
            !sibling(store).exists(),
            "{context}: sibling survived --recover"
        );
        assert_store_consistent(store, &format!("{context}: after --recover"));
        let state = manifest_state(store);
        assert_eq!(
            state, state_before_recover,
            "{context}: --recover changed the store"
        );
        if state == pre {
            outcomes.pre += 1;
            if had_leftover {
                assert!(out.contains("not applied"), "{context}: {out}");
            } else {
                assert!(out.contains("nothing to recover"), "{context}: {out}");
            }
        } else if state == post {
            outcomes.post += 1;
            if had_leftover {
                assert!(out.contains("verified replacement"), "{context}: {out}");
            }
            let mut ls = traj();
            ls.arg("-S").arg(store).args(["ls", "rounds"]);
            let listing = run_ok(&mut ls, &format!("{context}: ls rounds"));
            assert!(!listing.contains("round-0002"), "{context}: {listing}");
        } else {
            panic!(
                "{context}: manifest is neither the old nor the new store:\n{}",
                serde_json::to_string_pretty(&state).unwrap()
            );
        }
        if finished {
            outcomes.finished_before_kill += 1;
        }
    }
    eprintln!("delete: {outcomes:?}, leftover siblings: {leftovers}");
    restore(store);
}

fn derive_phase(rng: &mut Rng, store: &Path, base: &Path) {
    let derive_cmd = |store: &Path| {
        let mut c = traj();
        c.arg("-S").arg(store).arg("derive");
        c
    };
    restore_from_copy(store, base);
    let pre = manifest_state(store);
    let derived_of = |state: &Value| -> Vec<String> {
        state["batches"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|b| b["derived"].as_array().unwrap().iter())
            .map(|d| d.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        derived_of(&pre),
        vec!["jsonl/events-0001", "jsonl/events-0002"],
        "pack published one events table per batch"
    );

    let t0 = Instant::now();
    run_ok(&mut derive_cmd(store), "derive (uninterrupted)");
    let full_ms = t0.elapsed().as_millis();
    let post = manifest_state(store);
    assert_eq!(derived_of(&post), vec!["jsonl/events-rebuild-0001"]);
    assert_eq!(batch_ids(&post), batch_ids(&pre));
    assert_store_consistent(store, "derive: uninterrupted");
    eprintln!("derive: uninterrupted run took {full_ms} ms");

    let mut outcomes = Outcomes::default();
    for i in 0..KILLS_PER_PHASE {
        restore_from_copy(store, base);
        let (delay, finished) =
            kill_after_random_delay(rng, &mut derive_cmd(store), "derive", kill_window(full_ms));
        let context = format!("derive kill #{i} after {delay} ms");
        assert_store_consistent(store, &context);
        let state = manifest_state(store);
        if state == pre {
            outcomes.pre += 1;
        } else if state == post {
            outcomes.post += 1;
        } else {
            panic!(
                "{context}: manifest is neither generation:\n{}",
                serde_json::to_string_pretty(&state).unwrap()
            );
        }
        if finished {
            outcomes.finished_before_kill += 1;
        }
        // every published derived artifact exists; a later derive succeeds and leaves one generation
        for entry in derived_of(&state) {
            assert!(
                store
                    .join("derived")
                    .join(format!("{entry}.parquet"))
                    .is_file(),
                "{context}: published {entry} is missing"
            );
        }
        run_ok(&mut derive_cmd(store), &format!("{context}: derive again"));
        assert_store_consistent(store, &format!("{context}: after derive again"));
        let healed = manifest_state(store);
        let published = derived_of(&healed);
        assert_eq!(published.len(), 1, "{context}: {published:?}");
        assert!(
            published[0].starts_with("jsonl/events-rebuild-"),
            "{context}: {published:?}"
        );
        let on_disk: Vec<String> = fs::read_dir(store.join("derived/jsonl"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".parquet"))
            .collect();
        assert_eq!(
            on_disk,
            vec![format!(
                "{}.parquet",
                published[0].trim_start_matches("jsonl/")
            )],
            "{context}: unpublished derived artifacts survived a later derive"
        );
    }
    eprintln!("derive: {outcomes:?}");
}

// ------------------------------------------------------------------ the test

#[test]
fn killed_pack_delete_and_derive_leave_a_complete_store() {
    let mut rng = Rng::from_env();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let repo = tmp.path().join("repo");
    let store = repo.join("stores").join("run.trajstore");
    let scratch = tmp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    build_fixture(&mut rng, &src);

    // baseline: one committed-in-place batch, kept as a copy to restore from
    let t0 = Instant::now();
    run_ok(&mut pack_cmd(&src, &store, "b1"), "pack b1");
    eprintln!("baseline pack took {} ms", t0.elapsed().as_millis());
    assert_store_consistent(&store, "baseline");
    let base = scratch.join("base");
    copy_tree(&store, &base);
    grow_fixture(&mut rng, &src);

    let mut phases = 0;
    let after_pack = pack_phase(&mut rng, &src, &store, &base, &scratch);
    phases += 1;

    // delete needs Git and RENAME_EXCHANGE support in the tempdir
    let git_ok = Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if git_ok {
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "crash@example.com"]);
        git(&repo, &["config", "user.name", "crash"]);
        restore_from_copy(&store, &after_pack);
        let probe = traj()
            .arg("-S")
            .arg(&store)
            .args(["delete", "rounds/round-0002"])
            .output()
            .unwrap();
        if probe.status.success() {
            // the dry run above needs the store committed; commit happens inside the phase
            unreachable!("dry run cannot pass before the store is committed");
        }
        let stderr = String::from_utf8_lossy(&probe.stderr);
        if stderr.contains("not committed") {
            delete_phase(&mut rng, &repo, &store);
            phases += 1;
        } else {
            eprintln!(
                "skipping the delete phase: precondition not met: {}",
                stderr.trim()
            );
        }
    } else {
        eprintln!("skipping the delete phase: git is not available");
    }

    derive_phase(&mut rng, &store, &after_pack);
    phases += 1;
    assert!(phases >= 2, "at least pack and derive must run");
}
