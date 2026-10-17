use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use trajfs_core::ingest::{ingest, ingest_with_max_artifact_bytes, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::{NoAdapter, CHUNK_BYTES};

fn traj() -> Command {
    Command::new(env!("CARGO_BIN_EXE_traj"))
}

fn pack(source: &Path, store: &Path) {
    ingest(
        source,
        store,
        IngestOptions {
            rules: Rules::resolve("none").unwrap(),
            rules_name: "none".into(),
            adapter: &NoAdapter,
            label: String::new(),
            jobs: 1,
            derive: false,
            store_id: None,
        },
    )
    .unwrap();
}

#[test]
fn cli_requires_compaction_and_emits_bound_storage_numbers() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = temp.path().join("store");
    fs::create_dir(&source).unwrap();
    let mut data = vec![b'X'; CHUNK_BYTES + 1];
    data[CHUNK_BYTES] = b'A';
    fs::write(source.join("file"), &data).unwrap();
    pack(&source, &store);
    data[CHUNK_BYTES] = b'B';
    fs::write(source.join("file"), &data).unwrap();
    pack(&source, &store);
    let result = traj()
        .arg("-S")
        .arg(&store)
        .args([
            "verify",
            "--deep",
            "--require-compact",
            "--jobs",
            "4",
            "--json",
        ])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["storage"]["duplicate_frames"], 1);
    let out = temp.path().join("compact");
    let result = traj()
        .arg("-S")
        .arg(&store)
        .args(["compact", "--out"])
        .arg(&out)
        .args(["--no-recompress", "--jobs", "4", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let compact: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(compact["catalog_history_preserved"], true);
    assert_eq!(compact["compression_workers"], 4);
    assert_eq!(compact["after"]["duplicate_frames"], 0);
    let result = traj()
        .arg("-S")
        .arg(&out)
        .args([
            "verify",
            "--deep",
            "--require-compact",
            "--jobs",
            "4",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let verified: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(verified["ok"], true);
    assert_eq!(verified["storage"], compact["after"]);
    traj()
        .arg("-S")
        .arg(&out)
        .args(["cat", "file"])
        .assert()
        .success()
        .stdout(data);
    traj()
        .arg("-S")
        .arg(&store)
        .args(["compact", "--out"])
        .arg(&out)
        .assert()
        .failure();
}

fn traj_with_low_fd_limit() -> Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_traj"));
    command.env("RAYON_NUM_THREADS", "2");
    // Only the child receives the lower limit; concurrent tests keep their limits.
    unsafe {
        command.pre_exec(|| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = limit.rlim_max.min(64);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Command::from_std(command)
}

#[test]
fn verify_compact_and_read_more_packs_than_the_file_descriptor_limit() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = temp.path().join("store");
    fs::create_dir(&source).unwrap();
    let mut state = 17_u64;
    let data: Vec<_> = (0..3 * CHUNK_BYTES)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    fs::write(source.join("payload"), &data).unwrap();
    ingest_with_max_artifact_bytes(
        &source,
        &store,
        IngestOptions {
            rules: Rules::resolve("none").unwrap(),
            rules_name: "none".into(),
            adapter: &NoAdapter,
            label: String::new(),
            jobs: 1,
            derive: false,
            store_id: None,
        },
        64 * 1024,
    )
    .unwrap();
    let pack_count = fs::read_dir(store.join("packs"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "pack")
        })
        .count();
    assert!(pack_count > 64, "fixture must exceed the child's FD limit");

    traj_with_low_fd_limit()
        .arg("-S")
        .arg(&store)
        .args(["verify", "--deep", "--require-compact", "--json"])
        .assert()
        .success();
    let compacted = temp.path().join("compacted");
    traj_with_low_fd_limit()
        .arg("-S")
        .arg(&store)
        .args(["compact", "--level", "3", "--jobs", "4", "--out"])
        .arg(&compacted)
        .assert()
        .success();
    for candidate in [&store, &compacted] {
        traj_with_low_fd_limit()
            .arg("-S")
            .arg(candidate)
            .args(["cat", "payload"])
            .assert()
            .success()
            .stdout(data.clone());
    }
}
