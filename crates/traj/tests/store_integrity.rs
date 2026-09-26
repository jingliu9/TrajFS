//! `traj verify` and store readers: corrupted, truncated and missing artifacts are detected (deep vs shallow
//! verification), readers never need write access, and stores written by earlier releases stay readable.

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn deep_verify_detects_a_flipped_byte_that_shallow_verify_misses() {
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
    // shallow verify still passes: the structure is intact
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify"])
        .assert()
        .success();
}

#[test]
fn truncated_pack_is_reported_by_shallow_verify() {
    let e = packed("none");
    let pack = e.store.join("packs/0001.pack");
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
}

#[test]
fn missing_declared_artifacts_fail_both_verification_modes() {
    let e = packed("none");
    let pack = e.store.join("packs/0001.pack");
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
}

#[test]
fn missing_manifest_makes_the_store_unreadable() {
    let e = packed("none");
    fs::remove_file(e.store.join("MANIFEST.json")).unwrap();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["ls"])
        .assert()
        .failure();
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
        .arg(fixtures_dir().join("rounds-v2.toml"))
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(!lock.exists());
    drop(reader);
    fs::set_permissions(&e.store, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn legacy_rederived_store_remains_readable() {
    let e = packed("none");
    let manifest_path = e.store.join("MANIFEST.json");
    let mut m = manifest(&e.store);
    let adapter_name = m["adapter"]["name"].as_str().unwrap();
    let declared = m["batches"][0]["derived"][0].as_str().unwrap();
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
    m["format"] = serde_json::json!(1);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&m).unwrap()).unwrap();

    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["verify", "--deep"])
        .assert()
        .success();
    if sql_available() {
        let query = traj_stdout(&e.store, &["sql", "--csv", "select count(*) from events"]);
        assert!(query.contains("\n4"));
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
    pack_into(&e.src, &e.store, "two");
    let upgraded = manifest(&e.store);
    assert_eq!(upgraded["format"], trajfs_core::FORMAT_VERSION);
    assert!(upgraded["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| batch["derived"].as_array().unwrap())
        .any(|entry| entry == "rounds-layout/events-0000"));
}
