//! The read-only FUSE projection (`traj mount`, `traj umount`): byte-identical to `extract`, equal to the
//! catalog verbs, read-only with xattrs, correct under concurrent readers, live across new batches, `EIO` for
//! corrupted files only, a stale/busy/lazy/daemon lifecycle, refusals of unsafe mountpoints, the multi-store
//! root, the memory budget and the VS Code settings. Each test skips, with a message, where `/dev/fuse` or
//! `fusermount3` is missing (most CI containers); the `Mounted` guard tears the mount down even on panic.

mod common;

use common::*;
use std::fs;
use std::io::{Read, Seek};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[test]
fn mount_is_byte_identical_and_sigterm_unmounts() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
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
fn mount_matches_catalog_verbs() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
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
fn mount_is_read_only_and_exposes_xattrs() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
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
    let st = traj_stdout(
        &e.store,
        &["stat", "rounds/round-0001/reviewer/review.json"],
    );
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
fn grep_on_the_mount_equals_traj_grep() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
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

/// Eight threads read every small file of the mount for two seconds through a 1 MB blob cache; every read
/// must return the recorded bytes and mode, and the single session thread must keep up.
///
/// `big.bin` (2 MiB) is left out on purpose: it never fits a 1 MB cache, so each of its reads is a full
/// decompress-and-hash on the one FUSE worker (about 0.4 s in a debug build), which turns the throughput
/// floor below into a CPU-speed check. Reading it concurrently is covered by `read_only_and_xattrs` and
/// the byte-identical diff.
#[test]
fn concurrent_readers() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
    let _m = mount(Some(&e.store), &mp, &["--blob-cache", "1M"], &[]);
    let want = snapshot(&mp, &[]);
    let files: Vec<(PathBuf, String)> = want
        .iter()
        .filter(|(rel, d)| d.starts_with("file:") && !rel.contains("big.bin"))
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
                    let exec = md.permissions().mode() & 0o111 != 0;
                    assert_eq!(
                        &format!("file:{}:{}", exec, sha256_hex(&bytes)),
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
    let n = reads.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("concurrent readers: {n} reads in 2 s across 8 threads");
    // Throughput floor: every file fits the cache, so a read is a cache hit on the session thread and
    // even a slow host manages thousands in 2 s; fewer than 100 means reads are blocking on each other.
    assert!(n > 100, "only {n} reads in 2 s");
}

#[test]
fn new_batch_appears_without_remount() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
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
fn corruption_is_eio_for_the_affected_file_only() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let pack = e.store.join("packs/0001.pack");
    let mut bytes = fs::read(&pack).unwrap();
    let i = bytes.len() / 2;
    bytes[i] ^= 0xff;
    fs::write(&pack, &bytes).unwrap();
    let mp = e.tmp.path().join("mnt");
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
fn stale_busy_lazy_and_daemon_lifecycle() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
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
    let mp2 = e.tmp.path().join("mnt2");
    fs::create_dir_all(&mp2).unwrap();
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["mount", "--daemon"])
        .env("HOME", e.tmp.path())
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
fn mount_refuses_unsafe_or_occupied_mountpoints() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let tmp = e.tmp.path();
    let cfg = write_mount_config(tmp);
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
        .arg("mount")
        .arg(bare.path().join("mnt3"))
        .current_dir(bare.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("no store given"));
}

#[test]
fn multi_store_root_lists_stores_and_picks_up_new_ones() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let tmp = e.tmp.path();
    let cfg = write_mount_config(tmp);
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
fn memory_budget_trims_and_stays_correct() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let mp = e.tmp.path().join("mnt");
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
    let mp2 = e.tmp.path().join("mnt2");
    let m2 = mount(Some(&e.store), &mp2, &["--memory", "10%"], &[]);
    assert!(
        m2.log().contains("memory target") && !m2.log().contains("unbounded"),
        "{}",
        m2.log()
    );
    drop(m2);
    let mp3 = e.tmp.path().join("mnt3");
    let m3 = mount(Some(&e.store), &mp3, &["--memory", "0"], &[]);
    assert!(m3.log().contains("memory target unbounded"), "{}", m3.log());
}

#[test]
fn mount_writes_the_vscode_machine_settings() {
    if !fuse_available() {
        return;
    }
    let e = packed("none");
    let home = e.tmp.path().join("home");
    let machine = home.join(".vscode-server/data/Machine");
    fs::create_dir_all(&machine).unwrap();
    fs::write(machine.join("settings.json"), "{ \"x\": 1 }").unwrap();
    let mp = e.tmp.path().join("traj-mnt");
    let m = mount(Some(&e.store), &mp, &[], &[("HOME", home.as_path())]);
    assert!(m.log().contains("VS Code settings for"), "{}", m.log());
    let s: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(machine.join("settings.json")).unwrap()).unwrap();
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
    let bare = e.tmp.path().join("home2");
    fs::create_dir_all(&bare).unwrap();
    let m = mount(Some(&e.store), &mp, &[], &[("HOME", bare.as_path())]);
    assert!(!bare.join(".vscode-server").exists() && !bare.join(".config").exists());
    drop(m);
}

#[cfg(all(feature = "mount", target_os = "linux"))]
#[test]
fn invalid_mount_limits_fail_before_creating_a_mountpoint() {
    let fixture = SmallStore::new();
    let mountpoint = fixture.root().join("mount");
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
fn mount_reports_an_invalid_explicit_config() {
    let fixture = SmallStore::new();
    let config = fixture.root().join("invalid.toml");
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
