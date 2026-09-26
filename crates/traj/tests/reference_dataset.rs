//! Checks against a reference run (a real multi-million-path trajectory tree that is not shipped with the
//! repository) and against a synthetic scaled-down counterpart that runs unconditionally.
//!
//! # Reference tests
//!
//! The `reference_*` tests are `#[ignore]`d; run them with
//! `cargo test -p traj --release --test reference_dataset -- --ignored`. They read:
//!
//! - `TRAJ_SLOW_SRC`: the run directory (`rounds/round-NNNN/<role>/...`).
//! - `TRAJ_SLOW_ADAPTER`: the adapter TOML for that layout.
//! - `TRAJ_SLOW_ROUND` (default `round-0037`): the round packed and extracted on its own.
//! - `TRAJ_SLOW_STORE`: an already packed `.trajstore` of the run, for the mount test.
//! - `TRAJ_SLOW_NAME` (default `reference-run`): the basename of the recorded expectations.
//!
//! Recorded expectations live in `tests/expected/<TRAJ_SLOW_NAME>.run.json` (the whole run) and
//! `tests/expected/<TRAJ_SLOW_NAME>.round.json` (one round): the counts of the first batch (`paths`, `bytes`,
//! `new_blobs`, `new_blob_bytes`, `excluded`) that a pack must reproduce. A missing file is written on the first
//! run and asserted afterwards. To record a new dataset, choose a `TRAJ_SLOW_NAME` (or delete the stale files)
//! and run the ignored tests once; commit the files that appear.
//!
//! Timings are printed for reference only; `traj bench` is the place for performance bounds.
//!
//! # Synthetic tests
//!
//! `synthetic_*` generate a rounds-style tree with [`common::synthetic_rounds_tree`] (duplicate-heavy content,
//! build products, `COMPLETE` markers, copilot-cli `events.jsonl`) and run the same checks: pack counts
//! consistent with the manifest, catalog and content verbs agreeing with the tree, deep verification, and a
//! clean `diff -r` of the mount when FUSE is available.

mod common;

use common::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// (run directory, adapter path, round name) from the environment; panics with the variables to set.
fn reference_run() -> (PathBuf, String, String) {
    let src = std::env::var("TRAJ_SLOW_SRC").ok();
    let ad = std::env::var("TRAJ_SLOW_ADAPTER").ok();
    let (Some(src), Some(ad)) = (src, ad) else {
        panic!(
            "set TRAJ_SLOW_SRC and TRAJ_SLOW_ADAPTER (see the module doc of reference_dataset.rs)"
        )
    };
    let round = std::env::var("TRAJ_SLOW_ROUND").unwrap_or_else(|_| "round-0037".into());
    (PathBuf::from(src), ad, round)
}

/// `tests/expected/<TRAJ_SLOW_NAME>.<what>.json`, `what` being `run` or `round`.
fn expected_path(what: &str) -> PathBuf {
    let name = std::env::var("TRAJ_SLOW_NAME").unwrap_or_else(|_| "reference-run".into());
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/expected")
        .join(format!("{name}.{what}.json"))
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

/// The recorded counts of the first batch.
fn counts(store: &Path) -> serde_json::Value {
    let m = manifest(store);
    let b = &m["batches"][0];
    serde_json::json!({"paths": b["paths"], "bytes": b["bytes"], "new_blobs": b["new_blobs"], "new_blob_bytes": b["new_blob_bytes"], "excluded": b["excluded"]})
}

/// `snapshot(src)` restricted to the paths present in `got` (the pack may leave paths out by rule).
fn kept_snapshot(src: &Path, got: &[(String, String)]) -> Vec<(String, String)> {
    let kept: std::collections::HashSet<&String> = got.iter().map(|(p, _)| p).collect();
    snapshot(src, &[])
        .into_iter()
        .filter(|(p, _)| kept.contains(p))
        .collect()
}

#[test]
#[ignore = "needs TRAJ_SLOW_SRC/TRAJ_SLOW_ADAPTER pointing at a reference run"]
fn reference_round_packs_verifies_and_extracts_byte_identically() {
    let (src, ad, round_name) = reference_run();
    let round = src.join("rounds").join(&round_name);
    assert!(
        round.is_dir(),
        "{} has no rounds/{round_name}",
        src.display()
    );
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("round");
    let t0 = Instant::now();
    traj()
        .args(["pack"])
        .arg(&round)
        .arg("--out")
        .arg(&store)
        .args(["--adapter", &ad])
        .assert()
        .success();
    let pack_s = t0.elapsed().as_secs_f64();
    check_or_record(&expected_path("round"), &counts(&store));
    traj()
        .arg("-S")
        .arg(&store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    let out = tmp.path().join("out");
    let t0 = Instant::now();
    traj()
        .arg("-S")
        .arg(&store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    let extract_s = t0.elapsed().as_secs_f64();
    // byte-identical for every kept path
    let got = snapshot(&out, &[]);
    assert_eq!(got, kept_snapshot(&round, &got));
    // cat latency after warm-up over 200 spread-out paths
    let paths: Vec<String> = traj_lines(&store, &["find", "--kind", "f"])
        .into_iter()
        .step_by(500)
        .take(200)
        .collect();
    let st = trajfs_core::Store::open(&store).unwrap();
    let mut reader = st.reader();
    let mut times: Vec<f64> = Vec::new();
    for p in &paths {
        let t0 = Instant::now();
        let row = st.stat(p).unwrap().unwrap();
        let _ = st.read_row(&mut reader, &row, true).unwrap();
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let t0 = Instant::now();
    traj()
        .arg("-S")
        .arg(&store)
        .args(["ls", "builder/workspace/logs/round10"])
        .assert()
        .success();
    let ls_ms = t0.elapsed().as_millis();
    eprintln!(
        "reference round {round_name}: pack {pack_s:.1} s; extract {extract_s:.1} s; cat p50 {:.2} ms p99 {:.2} ms; ls {ls_ms} ms",
        times[times.len() / 2],
        times[times.len() * 99 / 100]
    );
}

#[test]
#[ignore = "needs TRAJ_SLOW_SRC/TRAJ_SLOW_ADAPTER pointing at a reference run"]
fn reference_run_packs_compactly_and_reads_back() {
    let (src, ad, round_name) = reference_run();
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("run");
    let t0 = Instant::now();
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
    eprintln!("reference run: {c} packed in {pack_s:.1} s");
    check_or_record(&expected_path("run"), &c);
    let paths = c["paths"].as_u64().unwrap();
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
    let review = format!("rounds/{round_name}/reviewer/review.json");
    for args in [
        vec!["ls", "rounds"],
        vec!["stat", &review],
        vec!["cat", &review],
        vec!["find", "--name", "COMPLETE"],
        vec!["sql", "select count(*) from files"],
    ] {
        if args[0] == "sql" && !sql_available() {
            eprintln!("skipping SQL checks: built without the sql feature");
            continue;
        }
        let t0 = Instant::now();
        traj().arg("-S").arg(&store).args(&args).assert().success();
        eprintln!(
            "reference run: {args:?} took {} ms",
            t0.elapsed().as_millis()
        );
    }
    // one round extracted and byte-identical to the source
    let out = tmp.path().join("round");
    let rel = format!("rounds/{round_name}");
    traj()
        .arg("-S")
        .arg(&store)
        .args(["extract", &rel])
        .arg(&out)
        .assert()
        .success();
    let got = snapshot(&out, &[]);
    assert_eq!(got, kept_snapshot(&src.join(&rel), &got));
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

/// The reference store named by `TRAJ_SLOW_STORE` mounted: "the round" is the first directory two levels
/// below the root (`rounds/round-0001` in the rounds layout); its walk has the same entries as a native extract
/// and every byte matches. The timings of docs/PLAN-fuse.md, section "Performance", are printed, not asserted.
#[test]
#[ignore = "needs TRAJ_SLOW_STORE pointing at a packed reference store"]
fn reference_store_mount_walks_and_reads_byte_identically() {
    if !fuse_available() {
        return;
    }
    let Some(store) = std::env::var_os("TRAJ_SLOW_STORE") else {
        panic!("set TRAJ_SLOW_STORE (see the module doc of reference_dataset.rs)")
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
        "reference store {}: mount {mount_time:?}; first descent {cold:?}; readdir warm {warm:?} ({n_direct} entries); \
         walk of {rel}: {n_mount} entries, cold {walk_cold:?}, warm {walk_warm:?}, native {n_native} in \
         {walk_native:?}; extract {extract_time:?}; read p50 {p50:?}; byte-identical in {diff_time:?}; \
         RSS {} MB",
        store.display(),
        rss_kb / 1024
    );
    assert_eq!(n_mount, n_native);
}

// -----------------------------------------------------------------------------------------------------
// Synthetic counterpart
// -----------------------------------------------------------------------------------------------------

const SYNTHETIC_ROUNDS: usize = 10;
const SYNTHETIC_FILES_PER_ROUND: usize = 300;

struct Synthetic {
    tmp: tempfile::TempDir,
    src: PathBuf,
    store: PathBuf,
    tree: SyntheticTree,
}

/// The synthetic tree packed with the rounds adapter under `no-build-products`.
fn synthetic_store() -> Synthetic {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("run");
    let store = tmp.path().join("run.trajstore");
    let tree = synthetic_rounds_tree(&src, SYNTHETIC_ROUNDS, SYNTHETIC_FILES_PER_ROUND);
    let t0 = Instant::now();
    traj()
        .args(["pack"])
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .args([
            "--adapter",
            &adapter(),
            "--rules",
            "no-build-products",
            "--label",
            "synthetic",
        ])
        .assert()
        .success();
    eprintln!(
        "synthetic run: {} kept paths packed in {:.2} s",
        tree.kept.len(),
        t0.elapsed().as_secs_f64()
    );
    Synthetic {
        tmp,
        src,
        store,
        tree,
    }
}

fn csv_rows(store: &Path, query: &str) -> Vec<String> {
    traj_stdout(store, &["sql", "--csv", query])
        .lines()
        .skip(1)
        .map(|l| l.to_string())
        .collect()
}

#[test]
fn synthetic_run_pack_counts_match_the_generated_tree() {
    let s = synthetic_store();
    let tree = &s.tree;
    let c = counts(&s.store);
    assert_eq!(c["paths"], tree.kept.len(), "{c}");
    assert_eq!(c["bytes"], tree.bytes(), "{c}");
    assert_eq!(c["new_blobs"], tree.distinct_blobs, "{c}");
    assert_eq!(c["excluded"], tree.excluded.len(), "{c}");
    assert!(tree.excluded.len() >= 4 * SYNTHETIC_ROUNDS);
    assert!(
        tree.distinct_blobs * 2 < tree.kept.len(),
        "the synthetic tree must be duplicate-heavy: {} blobs for {} paths",
        tree.distinct_blobs,
        tree.kept.len()
    );
    // the catalog agrees with the manifest
    if sql_available() {
        assert_eq!(
            csv_rows(&s.store, "select count(*) from files"),
            vec![tree.kept.len().to_string()]
        );
        assert_eq!(
            csv_rows(
                &s.store,
                "select count(distinct sha) from files where size > 0"
            ),
            vec![tree.distinct_blobs.to_string()]
        );
        assert_eq!(
            csv_rows(&s.store, "select path from excluded order by path"),
            tree.excluded
        );
        assert_eq!(
            csv_rows(&s.store, "select count(*) from events"),
            vec![tree.events.to_string()]
        );
        assert_eq!(
            csv_rows(
                &s.store,
                "select count(*) from events where type = 'tool.execution_complete'"
            ),
            vec![SYNTHETIC_ROUNDS.to_string()]
        );
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
    let t0 = Instant::now();
    traj()
        .arg("-S")
        .arg(&s.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    eprintln!(
        "synthetic run: deep verify in {:.2} s",
        t0.elapsed().as_secs_f64()
    );
}

#[test]
fn synthetic_run_verbs_agree_with_the_generated_tree() {
    let s = synthetic_store();
    let tree = &s.tree;
    // find: every kept path, nothing else
    assert_eq!(traj_lines(&s.store, &["find"]), tree.paths());
    // ls: the round directories, then one role directory
    let rounds: Vec<String> = (1..=SYNTHETIC_ROUNDS)
        .map(|r| format!("round-{r:04}/"))
        .collect();
    assert_eq!(traj_lines(&s.store, &["ls", "rounds"]), rounds);
    let mut want: Vec<String> = tree
        .paths()
        .iter()
        .filter_map(|p| p.strip_prefix("rounds/round-0003/builder/"))
        .map(|rest| match rest.split_once('/') {
            Some((dir, _)) => format!("{dir}/"),
            None => rest.to_string(),
        })
        .collect();
    want.sort();
    want.dedup();
    assert_eq!(
        traj_lines(&s.store, &["ls", "rounds/round-0003/builder"]),
        want
    );
    // COMPLETE markers: every round but the last
    let complete = traj_lines(&s.store, &["find", "--name", "COMPLETE"]);
    assert_eq!(complete.len(), SYNTHETIC_ROUNDS - 1);
    assert!(!complete
        .iter()
        .any(|p| p.contains(&format!("round-{SYNTHETIC_ROUNDS:04}"))));
    // attrs: the adapter tags every path with its round and role
    let in_round_3 = tree
        .paths()
        .iter()
        .filter(|p| p.starts_with("rounds/round-0003/"))
        .count();
    if sql_available() {
        assert_eq!(
            csv_rows(
                &s.store,
                "select count(*) from files where attrs['round'] = '3'"
            ),
            vec![in_round_3.to_string()]
        );
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
    let mut testers = traj_lines(&s.store, &["find", "--attr", "role=tester"]);
    testers.sort();
    let want: Vec<String> = tree
        .paths()
        .into_iter()
        .filter(|p| p.contains("/tester/"))
        .collect();
    assert_eq!(testers, want);
    // cat: exact bytes for a spread of files, including an empty one
    for (path, bytes) in tree.kept.iter().step_by(97) {
        let c = traj()
            .arg("-S")
            .arg(&s.store)
            .args(["cat", path])
            .output()
            .unwrap();
        assert!(c.status.success(), "cat {path}");
        assert_eq!(&c.stdout, bytes, "cat {path}");
    }
    let empty = tree.kept.iter().find(|(_, b)| b.is_empty()).unwrap();
    let c = traj()
        .arg("-S")
        .arg(&s.store)
        .args(["cat", &empty.0])
        .output()
        .unwrap();
    assert!(c.status.success() && c.stdout.is_empty(), "cat {}", empty.0);
    // grep: file lists for a marker that lives in unique files and one that lives in a shared blob
    assert_eq!(
        traj_lines(&s.store, &["grep", "-l", "-e", "^Traceback"]),
        tree.paths_containing(b"Traceback")
    );
    assert_eq!(
        traj_lines(&s.store, &["grep", "-l", "-e", "shared blob 7:"]),
        tree.paths_containing(b"shared blob 7:")
    );
    let hit = traj_stdout(&s.store, &["grep", "-e", "boom 4-16$"]);
    assert_eq!(
        hit,
        "rounds/round-0004/reviewer/logs/item-0016.stderr:3:Error: boom 4-16\n"
    );
    // du: the file count of one round
    let in_round_5 = tree
        .paths()
        .iter()
        .filter(|p| p.starts_with("rounds/round-0005/"))
        .count();
    let du = traj_stdout(&s.store, &["du", "rounds/round-0005"]);
    assert!(du.contains(&format!("{in_round_5} files")), "{du}");
    // extract: byte-identical for every kept path
    let out = s.tmp.path().join("out");
    traj()
        .arg("-S")
        .arg(&s.store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    assert_eq!(snapshot(&out, &[]), tree.snapshot());
    assert_eq!(snapshot(&out, &[]), kept_snapshot(&s.src, &tree.snapshot()));
}

#[test]
fn synthetic_run_mount_is_byte_identical() {
    if !fuse_available() {
        return;
    }
    let s = synthetic_store();
    let mp = s.tmp.path().join("mnt");
    let t0 = Instant::now();
    let m = mount(Some(&s.store), &mp, &[], &[]);
    let mount_s = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    assert_eq!(snapshot(&mp, &[]), s.tree.snapshot());
    let walk_s = t0.elapsed().as_secs_f64();
    let out = s.tmp.path().join("out");
    traj()
        .arg("-S")
        .arg(&s.store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    let diff = std::process::Command::new("diff")
        .arg("-r")
        .arg("-q")
        .arg(&mp)
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        diff.status.success(),
        "diff -r mount extract: {}{}",
        String::from_utf8_lossy(&diff.stdout),
        String::from_utf8_lossy(&diff.stderr)
    );
    eprintln!(
        "synthetic run: mount {mount_s:.2} s; walk and hash of {} paths {walk_s:.2} s",
        s.tree.kept.len()
    );
    drop(m);
}
