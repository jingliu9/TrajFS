//! `traj bench`: repeatable timings of the user-visible verbs on one store, written as one JSON report.
//!
//! Each scenario is a whole-process invocation of this same binary (so the numbers include CLI start-up,
//! which is what a user or an agent waits for). Every scenario runs `--warmup` untimed iterations and then
//! `--runs` timed ones; the report keeps every timed sample plus the minimum and the median. A separate
//! `in_process` section times the library calls behind `ls` and `cat` with the store already open, so the
//! cost of process start-up can be told apart from the cost of the lookup itself.
//!
//! This is a convenience timing report, not a controlled comparison: compare runs with the same store,
//! rules, and cache state, and use `traj verify --deep` separately for correctness.
//!
//! Report schema (`schema` = 1):
//!
//! ```text
//! {
//!   "schema": 1, "version": "<traj version>", "date": "<RFC 3339>",
//!   "host": {"os": "linux", "arch": "x86_64", "cpus": 8},
//!   "warmup": 1, "runs": 5,
//!   "store": {"path": "...", "paths": N, "bytes": N, "blobs": N, "packed_bytes": N, "store_bytes": N},
//!   "pack": {"name": "pack", "command": "...", "runs": [s], "min": s, "median": s, "ok": true}   // only with --source
//!   "scenarios": [{"name": "ls", "command": "traj -S ... ls <dir>", "runs": [s, ...], "min": s, "median": s, "ok": true}, ...],
//!   "in_process": [{"name": "children", ...}, {"name": "read_file", ...}],
//!   "mount": {"status": "measured", "mount_s": s, "first_ls_s": s, "scenarios": [...]}
//!            | {"status": "skipped", "reason": "..."} | {"status": "failed", "error": "..."}
//! }
//! ```
//!
//! Every duration is in seconds as a float.

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// The report format version; bump when a key changes meaning or disappears.
pub const SCHEMA: u32 = 1;

const SCHEMA_HELP: &str = "\
Report (JSON, schema 1; every duration is seconds as a float):
  schema, version, date, host{os,arch,cpus}, warmup, runs
  store{path, paths, bytes, blobs, packed_bytes, store_bytes}
      paths/bytes/blobs/packed_bytes describe the latest batch; store_bytes is every file in the store
  pack{...}          only with --source; the pack is timed once
  scenarios[]        ls, stat, cat, find_name, du, tree, grep, verify, and sql_count (builds with SQL):
                     whole-process runs of traj; ok is false when a run exits with an unexpected code
  in_process[]       children, read_file: the library calls behind ls and cat with the store already open
  mount              {status: measured, mount_s, first_ls_s, scenarios[mount_ls, mount_cat]}
                     | {status: skipped, reason} | {status: failed, error}
  each measurement   {name, command, runs[...timed samples], min, median, ok}

This is a convenience timing report, not a controlled comparison: compare runs made against the same store
with the same cache state, and use `traj verify --deep` separately for correctness.";

#[derive(Args, Debug)]
#[command(after_long_help = SCHEMA_HELP)]
pub struct BenchArgs {
    /// Source tree to pack into a temporary store (when no -S store is given); the pack itself is timed once
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Adapter for the pack step
    #[arg(long)]
    pub adapter: Option<String>,
    /// Directory (a timestamped file is created inside) or file path to write the report to (default: stdout)
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// A directory in the store to list
    #[arg(long, default_value = "")]
    pub ls_dir: String,
    /// A file in the store to stat/cat (default: the first file in catalog order)
    #[arg(long)]
    pub file: Option<String>,
    /// A basename to find
    #[arg(long, default_value = "COMPLETE")]
    pub find_name: String,
    /// A pattern for the grep scenario
    #[arg(long, default_value = "FAILED")]
    pub grep_pattern: String,
    /// Untimed iterations before each scenario is measured
    #[arg(long, default_value_t = 1)]
    pub warmup: usize,
    /// Timed iterations per scenario
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..))]
    pub runs: u16,
    /// Skip the mount scenarios even when FUSE is available
    #[arg(long)]
    pub no_mount: bool,
}

/// One timed scenario: every timed sample, its minimum and median, and whether every run succeeded.
#[derive(Serialize, Debug, Clone)]
pub struct Measurement {
    pub name: String,
    pub command: String,
    pub runs: Vec<f64>,
    pub min: f64,
    pub median: f64,
    pub ok: bool,
}

impl Measurement {
    fn new(name: &str, command: String, runs: Vec<f64>, ok: bool) -> Measurement {
        let min = runs.iter().copied().fold(f64::INFINITY, f64::min);
        Measurement {
            name: name.to_string(),
            command,
            median: median(&runs),
            runs,
            min,
            ok,
        }
    }
}

#[derive(Serialize, Debug)]
struct Host {
    os: &'static str,
    arch: &'static str,
    cpus: usize,
}

#[derive(Serialize, Debug)]
struct StoreInfo {
    path: PathBuf,
    /// Recorded paths and bytes of the latest batch (incremental stores: the last batch only).
    paths: Option<u64>,
    bytes: Option<u64>,
    blobs: Option<u64>,
    packed_bytes: Option<u64>,
    /// Size of every file in the store directory, derived tables included.
    store_bytes: u64,
}

#[derive(Serialize, Debug)]
#[serde(tag = "status", rename_all = "lowercase")]
enum MountSection {
    Measured {
        mount_s: f64,
        /// The first listing after the mount comes up (cold kernel and blob caches), timed once.
        first_ls_s: f64,
        scenarios: Vec<Measurement>,
    },
    Skipped {
        reason: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Serialize, Debug)]
struct Report {
    schema: u32,
    version: &'static str,
    date: String,
    host: Host,
    warmup: usize,
    runs: usize,
    store: StoreInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pack: Option<Measurement>,
    scenarios: Vec<Measurement>,
    in_process: Vec<Measurement>,
    mount: MountSection,
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// Runs `cmd` once with its output discarded; seconds elapsed and whether its exit code is in `ok_codes`.
fn timed(cmd: &mut Command, ok_codes: &[i32]) -> Result<(f64, bool)> {
    let t0 = Instant::now();
    let st = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("running {}", describe(cmd)))?;
    let ok = st.code().is_some_and(|c| ok_codes.contains(&c));
    Ok((t0.elapsed().as_secs_f64(), ok))
}

/// `warmup` untimed runs, then `runs` timed runs of the command built by `make`.
fn measure(
    name: &str,
    warmup: usize,
    runs: usize,
    ok_codes: &[i32],
    mut make: impl FnMut() -> Command,
) -> Result<Measurement> {
    let command = describe(&make());
    for _ in 0..warmup {
        timed(&mut make(), ok_codes)?;
    }
    let mut samples = Vec::with_capacity(runs);
    let mut ok = true;
    for _ in 0..runs {
        let (secs, success) = timed(&mut make(), ok_codes)?;
        samples.push(secs);
        ok &= success;
    }
    Ok(Measurement::new(name, command, samples, ok))
}

/// Same shape as [`measure`] for a closure run in this process; `Err` marks the scenario not ok.
fn measure_in_process(
    name: &str,
    warmup: usize,
    runs: usize,
    mut f: impl FnMut() -> Result<()>,
) -> Measurement {
    for _ in 0..warmup {
        let _ = f();
    }
    let mut samples = Vec::with_capacity(runs);
    let mut ok = true;
    for _ in 0..runs {
        let t0 = Instant::now();
        ok &= f().is_ok();
        samples.push(t0.elapsed().as_secs_f64());
    }
    Measurement::new(name, format!("in-process {name}"), samples, ok)
}

/// A shell-like rendering of the command for the report; not meant to be re-executed verbatim.
fn describe(cmd: &Command) -> String {
    let prog = Path::new(cmd.get_program())
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cmd.get_program().to_string_lossy().into_owned());
    std::iter::once(prog)
        .chain(cmd.get_args().map(|a| a.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run(stores: &[String], a: BenchArgs) -> Result<i32> {
    let me = std::env::current_exe()?;
    let tmp = tempfile::Builder::new()
        .prefix("traj-bench-")
        .tempdir()
        .context("creating the temporary directory")?;
    let cfg = crate::config::Config::try_find()?;

    let mut pack = None;
    let store = match (stores.first(), &a.source) {
        (Some(s), _) => crate::config::resolve_store(s, cfg.as_ref())?,
        (None, Some(src)) => {
            let s = tmp.path().join("bench.trajstore");
            let mut c = Command::new(&me);
            c.arg("pack").arg(src).arg("--out").arg(&s);
            if let Some(ad) = &a.adapter {
                c.args(["--adapter", ad]);
            }
            let command = describe(&c);
            let (secs, ok) = timed(&mut c, &[0])?;
            if !ok {
                anyhow::bail!("pack of {} failed", src.display());
            }
            pack = Some(Measurement::new("pack", command, vec![secs], ok));
            s
        }
        (None, None) => anyhow::bail!("give -S <store> or --source <tree>"),
    };

    let store_info = store_info(&store)?;
    let file = match &a.file {
        Some(f) => f.clone(),
        None => first_file(&store)?.unwrap_or_default(),
    };

    // (name, arguments after `-S <store>`, exit codes that count as success)
    let mut scenarios: Vec<(&str, Vec<String>, &[i32])> = vec![
        ("ls", vec!["ls".into(), a.ls_dir.clone()], &[0]),
        ("stat", vec!["stat".into(), file.clone()], &[0]),
        ("cat", vec!["cat".into(), file.clone()], &[0]),
        (
            "find_name",
            vec!["find".into(), "--name".into(), a.find_name.clone()],
            &[0],
        ),
        ("du", vec!["du".into()], &[0]),
        (
            "tree",
            vec!["tree".into(), "--depth".into(), "1".into()],
            &[0],
        ),
        // like grep(1), exit 1 means "no match", which is still a successful search
        (
            "grep",
            vec![
                "grep".into(),
                "-l".into(),
                "-e".into(),
                a.grep_pattern.clone(),
            ],
            &[0, 1],
        ),
        ("verify", vec!["verify".into()], &[0]),
    ];
    if cfg!(feature = "sql") {
        scenarios.push((
            "sql_count",
            vec!["sql".into(), "select count(*) from files".into()],
            &[0],
        ));
    }
    let runs = usize::from(a.runs);
    let mut measured = Vec::with_capacity(scenarios.len());
    for (name, args, ok_codes) in &scenarios {
        measured.push(measure(name, a.warmup, runs, ok_codes, || {
            let mut c = Command::new(&me);
            c.arg("-S").arg(&store).args(args);
            c
        })?);
    }

    let in_process = in_process(&store, &a.ls_dir, &file, a.warmup, runs)?;

    let mount = if a.no_mount {
        MountSection::Skipped {
            reason: "--no-mount".into(),
        }
    } else if !crate::cmd::mount::fuse_available() {
        MountSection::Skipped {
            reason: "no /dev/fuse or fusermount3 on this host".into(),
        }
    } else {
        match bench_mount(&me, &store, tmp.path(), &a.ls_dir, &file, a.warmup, runs) {
            Ok(m) => m,
            Err(e) => MountSection::Failed {
                error: format!("{e:#}"),
            },
        }
    };

    let report = Report {
        schema: SCHEMA,
        version: crate::VERSION,
        date: chrono::Utc::now().to_rfc3339(),
        host: Host {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        },
        warmup: a.warmup,
        runs,
        store: store_info,
        pack,
        scenarios: measured,
        in_process,
        mount,
    };
    let json = serde_json::to_string_pretty(&report)?;
    match &a.out {
        Some(p) => {
            let path = if p.is_dir() {
                p.join(format!(
                    "{}.json",
                    chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
                ))
            } else {
                p.clone()
            };
            std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        }
        None => println!("{json}"),
    }
    // `tmp` is removed here, together with any store packed from --source.
    Ok(0)
}

fn store_info(store: &Path) -> Result<StoreInfo> {
    let m = trajfs_core::Manifest::load(store)?;
    let last = m.batches.last();
    let mut store_bytes = 0u64;
    for e in walkdir::WalkDir::new(store).into_iter().flatten() {
        if e.file_type().is_file() {
            store_bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(StoreInfo {
        path: store.to_path_buf(),
        paths: last.map(|b| b.paths),
        bytes: last.map(|b| b.bytes),
        blobs: last.map(|b| b.new_blobs),
        packed_bytes: last.map(|b| b.packed_bytes),
        store_bytes,
    })
}

/// The first file path in catalog order, used when `--file` is not given.
fn first_file(store: &Path) -> Result<Option<String>> {
    let st = trajfs_core::Store::open(store)?;
    let mut first = None;
    st.scan_under(
        "",
        false,
        |_| true,
        |row| {
            if first.is_none() && row.kind == trajfs_core::Kind::File {
                first = Some(row.path.clone());
            }
        },
    )?;
    Ok(first)
}

/// Library-level timings with the store opened once: a directory listing and one verified file read.
fn in_process(
    store: &Path,
    ls_dir: &str,
    file: &str,
    warmup: usize,
    runs: usize,
) -> Result<Vec<Measurement>> {
    let st = trajfs_core::Store::open(store)?;
    let mut out = vec![measure_in_process("children", warmup, runs, || {
        st.children(ls_dir).map(|_| ())
    })];
    if !file.is_empty() {
        let row = st
            .stat(file)?
            .with_context(|| format!("{file}: not in the store"))?;
        let mut reader = st.reader();
        out.push(measure_in_process("read_file", warmup, runs, || {
            st.read_row(&mut reader, &row, true).map(|_| ())
        }));
    }
    Ok(out)
}

/// Mount `store` under `tmp` with this same binary, time the mount coming up, one cold listing, and the
/// repeated listing and read scenarios, then unmount.
fn bench_mount(
    me: &Path,
    store: &Path,
    tmp: &Path,
    ls_dir: &str,
    file: &str,
    warmup: usize,
    runs: usize,
) -> Result<MountSection> {
    let mnt = tmp.join("mnt");
    std::fs::create_dir_all(&mnt)?;
    let mut child = Command::new(me)
        .arg("-S")
        .arg(store)
        .arg("mount")
        .arg(&mnt)
        .arg("--no-vscode")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let t0 = Instant::now();
    let mount_s = loop {
        if crate::cmd::mount::is_mounted(&mnt) && std::fs::metadata(&mnt).is_ok() {
            break t0.elapsed().as_secs_f64();
        }
        if child.try_wait()?.is_some() {
            anyhow::bail!("mount process exited");
        }
        if t0.elapsed().as_secs() > 10 {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("mount did not come up within 10 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let result = (|| -> Result<MountSection> {
        let dir = mnt.join(ls_dir);
        let mut ls = Command::new("ls");
        ls.arg("-A").arg(&dir);
        let (first_ls_s, _) = timed(&mut ls, &[0])?;
        let mut scenarios = vec![measure("mount_ls", warmup, runs, &[0], || {
            let mut c = Command::new("ls");
            c.arg("-A").arg(&dir);
            c
        })?];
        if !file.is_empty() {
            let path = mnt.join(file);
            scenarios.push(measure("mount_cat", warmup, runs, &[0], || {
                let mut c = Command::new("cat");
                c.arg(&path);
                c
            })?);
        }
        Ok(MountSection::Measured {
            mount_s,
            first_ls_s,
            scenarios,
        })
    })();
    let unmounted = crate::cmd::mount::fusermount_unmount(&mnt, false);
    let _ = child.wait();
    unmounted?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_and_even_sample_counts() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert!(median(&[]).is_nan());
    }

    #[test]
    fn measurement_records_min_and_median() {
        let m = Measurement::new("x", "x".into(), vec![0.5, 0.2, 0.9], true);
        assert_eq!(m.min, 0.2);
        assert_eq!(m.median, 0.5);
        assert_eq!(m.runs.len(), 3);
    }

    #[test]
    fn describe_uses_the_program_basename() {
        let mut c = Command::new("/usr/bin/traj");
        c.args(["-S", "s", "ls", "dir"]);
        assert_eq!(describe(&c), "traj -S s ls dir");
    }
}
