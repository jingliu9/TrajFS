//! Helpers shared by the `traj` integration tests: the fixture tree, the packed-store builder, the `traj`
//! command runner, snapshot/hash helpers, git helpers, the FUSE mount guard and the synthetic rounds generator.
//! Every test file includes this module with `mod common;`, so items unused by one file are expected.
#![allow(dead_code)]

use assert_cmd::Command;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The built `traj` binary as an `assert_cmd` command, without any configuration inherited from the caller's
/// environment (`TRAJ_CONFIG`, `TRAJ_STORE`).
pub fn traj() -> Command {
    let mut command = Command::cargo_bin("traj").unwrap();
    command.env_remove("TRAJ_CONFIG").env_remove("TRAJ_STORE");
    command
}

/// Path of the built `traj` binary, for tests that need a raw `std::process::Command` (signals, custom stdio).
pub fn traj_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("traj")
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The test adapter: a generic "rounds" layout declared in TOML (trajfs ships no runner-specific adapter).
pub fn adapter() -> String {
    fixtures_dir().join("rounds.toml").display().to_string()
}

pub fn write(p: &Path, content: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A synthetic run tree: rounds, duplicates, symlink, empty file, exec bit, unicode name, big file, excluded dirs.
pub fn fixture(root: &Path) {
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

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, bytes);
    hex::encode(sha2::Digest::finalize(h))
}

/// Walk a tree into (relative path, kind/mode/content) for byte-exact comparison.
pub fn snapshot(root: &Path, skip: &[&str]) -> Vec<(String, String)> {
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
            format!("file:{}:{}", exec, sha256_hex(&fs::read(e.path()).unwrap()))
        };
        v.push((rel, desc));
    }
    v.sort();
    v
}

/// A packed copy of [`fixture`]: the source tree, the store, and the temp dir holding both.
pub struct Env {
    pub tmp: tempfile::TempDir,
    pub src: PathBuf,
    pub store: PathBuf,
}

/// stdout lines of an external binary run directly (no shell), sorted.
pub fn lines_of(cmd: &str, args: &[&str], cwd: &Path) -> Vec<String> {
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

/// stdout lines of `traj -S <store> <args>`, sorted; the command must succeed.
pub fn traj_lines(store: &Path, args: &[&str]) -> Vec<String> {
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

/// stdout of `traj -S <store> <args>` as a string; the command must succeed.
pub fn traj_stdout(store: &Path, args: &[&str]) -> String {
    let out = traj().arg("-S").arg(store).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "traj {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// `traj pack <src> --out <store>` with the rounds adapter.
pub fn pack_into(src: &Path, store: &Path, label: &str) {
    traj()
        .args(["pack"])
        .arg(src)
        .arg("--out")
        .arg(store)
        .args(["--adapter", &adapter(), "--rules", "none", "--label", label])
        .assert()
        .success();
}

/// [`fixture`] packed with the rounds adapter under the given rule profile (`none`, `no-build-products`).
pub fn packed(rules: &str) -> Env {
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
    Env { tmp, src, store }
}

/// `traj extract ""` of the whole store into `<tmp>/out`.
pub fn extracted(e: &Env) -> PathBuf {
    let out = e.tmp.path().join("out");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["extract", ""])
        .arg(&out)
        .assert()
        .success();
    out
}

pub fn manifest(store: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(store.join("MANIFEST.json")).unwrap()).unwrap()
}

/// Run `git <args>` in `repo` with literal pathspecs; panics on failure, returns stdout.
pub fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// `git init` plus a throwaway identity so commits work in a clean environment.
pub fn git_init(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "t"]);
}

/// A repository initialised with `traj init` (no skill), with one committed unrelated file.
pub fn init_repo(repo: &Path, data: &Path, adapter: &str) {
    git_init(repo);
    traj()
        .current_dir(repo)
        .args(["init", "--data-root"])
        .arg(data)
        .args(["--adapter", adapter, "--rules", "none", "--no-skill"])
        .assert()
        .success();
    fs::write(repo.join("note.txt"), "original\n").unwrap();
    git(
        repo,
        &[
            "add",
            "trajfs.toml",
            "stores/.gitattributes",
            "stores/.gitkeep",
            "note.txt",
        ],
    );
    git(repo, &["commit", "-qm", "initial"]);
}

/// A three-file store (`a.txt`, `b.txt`, an empty file) built through the library, for verb edge cases.
pub struct SmallStore {
    pub dir: tempfile::TempDir,
    pub store: PathBuf,
}

impl SmallStore {
    pub fn new() -> Self {
        use trajfs_core::ingest::{ingest, IngestOptions};
        use trajfs_core::rules::Rules;
        use trajfs_core::NoAdapter;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let source = base.join("source");
        let store = base.join("store");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a.txt"), b"match aaa\n").unwrap();
        fs::write(source.join("b.txt"), b"match bbb\n").unwrap();
        fs::write(source.join("empty"), b"").unwrap();
        ingest(
            &source,
            &store,
            IngestOptions {
                rules: Rules::resolve("none").unwrap(),
                rules_name: "none".into(),
                adapter: &NoAdapter,
                label: "small store".into(),
                jobs: 2,
                derive: false,
                store_id: None,
            },
        )
        .unwrap();
        Self { dir, store }
    }

    /// The canonical temp root (the tempdir path may go through a symlink such as `/tmp` on macOS).
    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// `traj -S <store>` run from the temp dir.
    pub fn traj(&self) -> Command {
        let mut cmd = traj();
        cmd.current_dir(self.dir.path()).arg("-S").arg(&self.store);
        cmd
    }
}

impl Default for SmallStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------------------
// FUSE
// ---------------------------------------------------------------------------------------------------------

/// Whether `traj sql` exists in this build (the `sql` cargo feature, on by default). Tests that only *inspect*
/// a store through SQL wrap those assertions in `if sql_available()` so the rest still runs without it.
pub fn sql_available() -> bool {
    cfg!(feature = "sql")
}

/// Whether the host can serve a FUSE mount; prints why not so a skipped test is visible in the log.
/// False when the binary was built without the `mount` cargo feature: `traj mount` then bails before touching
/// FUSE, so every mount test is skipped rather than failed.
pub fn fuse_available() -> bool {
    if !cfg!(feature = "mount") {
        eprintln!("skipped: built without the mount feature");
        return false;
    }
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

/// Whether `mp` is listed in `/proc/self/mounts`.
pub fn mounted(mp: &Path) -> bool {
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

/// Poll `cond` every 50 ms for up to `secs` seconds.
pub fn wait_for(cond: impl Fn() -> bool, secs: f64) -> bool {
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < secs {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// A live foreground `traj mount`; dropping it unmounts (SIGTERM, then a lazy `fusermount3 -u`) even when the
/// test panics.
pub struct Mounted {
    pub child: std::process::Child,
    pub mp: PathBuf,
    pub log: PathBuf,
}

/// `traj [-S store] mount <mp> <extra>` in the foreground; returns once the mount is live.
pub fn mount(store: Option<&Path>, mp: &Path, extra: &[&str], env: &[(&str, &Path)]) -> Mounted {
    let _ = fs::create_dir_all(mp); // fails on a stale mountpoint, which `traj mount` clears itself
    let log = PathBuf::from(format!("{}.log", mp.display()));
    let f = fs::File::create(&log).unwrap();
    let mut c = std::process::Command::new(traj_bin());
    c.env_remove("TRAJ_CONFIG").env_remove("TRAJ_STORE");
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
    pub fn signal(&self, sig: i32) {
        unsafe {
            libc::kill(self.child.id() as i32, sig);
        }
    }
    pub fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
    pub fn exited(&mut self) -> bool {
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

/// A `trajfs.toml` under `tmp` with `data/` and `stores/` next to it (adapter `none`), for multi-store mounts and
/// the mountpoint refusals.
pub fn write_mount_config(tmp: &Path) -> PathBuf {
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

// ---------------------------------------------------------------------------------------------------------
// Synthetic rounds tree
// ---------------------------------------------------------------------------------------------------------

/// What [`synthetic_rounds_tree`] wrote, so a test can predict the pack result exactly.
pub struct SyntheticTree {
    /// Every kept file as (relative path, bytes), sorted by path; none is executable or a symlink.
    pub kept: Vec<(String, Vec<u8>)>,
    /// The rows the `no-build-products` profile records: one per pruned directory, one per excluded file.
    pub excluded: Vec<String>,
    /// Distinct non-empty contents among `kept` (what a pack reports as `new_blobs`).
    pub distinct_blobs: usize,
    /// Event rows the copilot-cli adapter derives from the `events.jsonl` files.
    pub events: usize,
}

impl SyntheticTree {
    /// The kept paths, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.kept.iter().map(|(p, _)| p.clone()).collect()
    }

    pub fn bytes(&self) -> u64 {
        self.kept.iter().map(|(_, b)| b.len() as u64).sum()
    }

    pub fn content(&self, path: &str) -> &[u8] {
        &self.kept.iter().find(|(p, _)| p == path).unwrap().1
    }

    /// What [`snapshot`] returns for an exact copy of the kept files.
    pub fn snapshot(&self) -> Vec<(String, String)> {
        self.kept
            .iter()
            .map(|(p, b)| (p.clone(), format!("file:false:{}", sha256_hex(b))))
            .collect()
    }

    /// Kept paths whose content contains `needle`, sorted.
    pub fn paths_containing(&self, needle: &[u8]) -> Vec<String> {
        self.kept
            .iter()
            .filter(|(_, b)| b.windows(needle.len()).any(|w| w == needle))
            .map(|(p, _)| p.clone())
            .collect()
    }
}

/// A deterministic, scaled-down stand-in for a reference run in the rounds layout:
/// `rounds/round-NNNN/<role>/...` with duplicate-heavy content (most files draw from a small pool of shared
/// blobs), a few unique files (some carrying a `Traceback`), an empty file per round, a `COMPLETE` marker in every
/// round but the last, one copilot-cli `events.jsonl` per round, and build products (`node_modules`, `.cache`,
/// `__pycache__`, `*.pyc`) that the `no-build-products` profile must leave out.
pub fn synthetic_rounds_tree(root: &Path, rounds: usize, files_per_round: usize) -> SyntheticTree {
    const ROLES: [&str; 3] = ["builder", "reviewer", "tester"];
    const SUBDIRS: [&str; 4] = ["logs", "workspace/src", "workspace/tests", "notes"];
    const EXTS: [&str; 5] = ["stdout", "stderr", "txt", "json", "log"];
    const POOL: usize = 40;
    let mut kept: Vec<(String, Vec<u8>)> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    let mut events = 0;
    for r in 1..=rounds {
        let round = format!("rounds/round-{r:04}");
        for i in 0..files_per_round {
            let role = ROLES[i % ROLES.len()];
            let sub = SUBDIRS[i % SUBDIRS.len()];
            let ext = EXTS[i % EXTS.len()];
            let rel = format!("{round}/{role}/{sub}/item-{i:04}.{ext}");
            let content: Vec<u8> = match i % 10 {
                _ if i % 50 == 49 => Vec::new(),
                0..=5 => {
                    let k = (i * 31 + r) % POOL;
                    format!("shared blob {k}: the same bytes in many rounds\n").repeat(k + 1).into_bytes()
                }
                6 => format!(
                    "Traceback (most recent call last):\n  round {r} file {i}\nError: boom {r}-{i}\n"
                )
                .into_bytes(),
                _ => (0..(i % 7 + 1))
                    .map(|n| format!("round {r} file {i} line {n}\n"))
                    .collect::<String>()
                    .into_bytes(),
            };
            kept.push((rel, content));
        }
        if r < rounds {
            kept.push((format!("{round}/COMPLETE"), b"complete\n".to_vec()));
        }
        let lines = [
            format!("{{\"type\":\"session.start\",\"timestamp\":\"2026-09-04T00:{r:02}:00Z\",\"id\":\"s{r}\",\"data\":{{}}}}"),
            format!("{{\"type\":\"tool.execution_start\",\"timestamp\":\"2026-09-04T00:{r:02}:01Z\",\"id\":\"t{r}\",\"parentId\":\"s{r}\",\"data\":{{\"toolCallId\":\"c{r}\",\"toolName\":\"bash\"}}}}"),
            format!("{{\"type\":\"tool.execution_complete\",\"timestamp\":\"2026-09-04T00:{r:02}:02Z\",\"id\":\"d{r}\",\"parentId\":\"t{r}\",\"data\":{{\"toolCallId\":\"c{r}\",\"exitCode\":{}}}}}", r % 2),
        ];
        events += lines.len();
        let log: String = lines.iter().map(|l| format!("{l}\n")).collect();
        kept.push((format!("{round}/builder/events.jsonl"), log.into_bytes()));
        // build products: pruned directories are one row each, excluded files one row each
        write(
            &root.join(format!("{round}/builder/node_modules/pkg/index.js")),
            b"module.exports = 1;\n",
        );
        excluded.push(format!("{round}/builder/node_modules"));
        write(
            &root.join(format!("{round}/builder/.cache/state")),
            b"cache\n",
        );
        excluded.push(format!("{round}/builder/.cache"));
        write(
            &root.join(format!("{round}/tester/__pycache__/mod.cpython-312.pyc")),
            b"\x00pyc",
        );
        excluded.push(format!("{round}/tester/__pycache__"));
        write(
            &root.join(format!("{round}/builder/workspace/src/mod.pyc")),
            b"\x00pyc",
        );
        excluded.push(format!("{round}/builder/workspace/src/mod.pyc"));
    }
    for (rel, content) in &kept {
        write(&root.join(rel), content);
    }
    kept.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    excluded.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let distinct_blobs = kept
        .iter()
        .filter(|(_, b)| !b.is_empty())
        .map(|(_, b)| sha256_hex(b))
        .collect::<std::collections::HashSet<_>>()
        .len();
    SyntheticTree {
        kept,
        excluded,
        distinct_blobs,
        events,
    }
}
