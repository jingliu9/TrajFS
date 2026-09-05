//! `traj bench`: the regression baseline of docs/PLAN.md §8/§11 as one JSON document.

use anyhow::Result;
use clap::Args;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Source tree to pack into a temporary store (when no -S store is given)
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Adapter for the pack step
    #[arg(long)]
    pub adapter: Option<String>,
    /// Directory or path to append into (default: print to stdout)
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// A directory in the store to list
    #[arg(long, default_value = "")]
    pub ls_dir: String,
    /// A file in the store to stat/cat
    #[arg(long)]
    pub file: Option<String>,
    /// A basename to find
    #[arg(long, default_value = "COMPLETE")]
    pub find_name: String,
}

fn timed(mut c: Command) -> Result<(f64, bool)> {
    let t0 = Instant::now();
    let st = c
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok((t0.elapsed().as_secs_f64(), st.success()))
}

pub fn run(stores: &[String], a: BenchArgs) -> Result<i32> {
    let me = std::env::current_exe()?;
    let mut r = serde_json::Map::new();
    r.insert("version".into(), crate::VERSION.into());
    r.insert("date".into(), chrono::Utc::now().to_rfc3339().into());
    let tmp = tempfile_dir()?;
    let cfg = crate::config::Config::try_find()?;
    let store = match (stores.first(), &a.source) {
        (Some(s), _) => crate::config::resolve_store(s, cfg.as_ref())?,
        (None, Some(src)) => {
            let s = tmp.join("bench.trajstore");
            let mut c = Command::new(&me);
            c.arg("pack").arg(src).arg("--out").arg(&s);
            if let Some(ad) = &a.adapter {
                c.args(["--adapter", ad]);
            }
            let (secs, ok) = timed(c)?;
            r.insert("pack_s".into(), secs.into());
            r.insert("pack_ok".into(), ok.into());
            s
        }
        (None, None) => anyhow::bail!("give -S <store> or --source <tree>"),
    };
    let m = trajfs_core::Manifest::load(&store)?;
    if let Some(b) = m.batches.last() {
        r.insert("paths".into(), b.paths.into());
        r.insert("bytes".into(), b.bytes.into());
        r.insert("blobs".into(), b.new_blobs.into());
        r.insert("packed_bytes".into(), b.packed_bytes.into());
    }
    let mut total = 0u64;
    for e in walkdir::WalkDir::new(&store).into_iter().flatten() {
        if e.file_type().is_file() {
            total += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    r.insert("store_bytes".into(), total.into());
    let file = match &a.file {
        Some(f) => f.clone(),
        None => {
            let st = trajfs_core::Store::open(&store)?;
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
            first.unwrap_or_default()
        }
    };
    let verbs: Vec<(&str, Vec<String>)> = vec![
        ("ls_s", vec!["ls".into(), a.ls_dir.clone()]),
        ("stat_s", vec!["stat".into(), file.clone()]),
        ("cat_s", vec!["cat".into(), file.clone()]),
        (
            "find_name_s",
            vec!["find".into(), "--name".into(), a.find_name.clone()],
        ),
        ("du_s", vec!["du".into()]),
        ("tree_s", vec!["tree".into(), "--depth".into(), "1".into()]),
        (
            "grep_s",
            vec!["grep".into(), "-l".into(), "-e".into(), "FAILED".into()],
        ),
        (
            "sql_count_s",
            vec!["sql".into(), "select count(*) from files".into()],
        ),
        ("verify_s", vec!["verify".into()]),
    ];
    for (k, args) in verbs {
        let mut c = Command::new(&me);
        c.arg("-S").arg(&store).args(&args);
        let (secs, _) = timed(c)?;
        r.insert(k.into(), secs.into());
    }
    // the mount (docs/PLAN-fuse.md): time to come up, a listing cold and warm, one read; skipped without FUSE
    match bench_mount(&me, &store, &a.ls_dir, &file) {
        Ok(Some(m)) => {
            for (k, v) in m {
                r.insert(k, v);
            }
        }
        Ok(None) => {
            r.insert(
                "mount".into(),
                "skipped: no /dev/fuse or fusermount3".into(),
            );
        }
        Err(e) => {
            r.insert("mount".into(), format!("failed: {e:#}").into());
        }
    }
    let json = serde_json::Value::Object(r);
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
            std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
            println!("wrote {}", path.display());
        }
        None => println!("{}", serde_json::to_string_pretty(&json)?),
    }
    Ok(0)
}

/// Mount `store` in a temporary directory with the same binary, measure, unmount. `None` when FUSE is absent.
fn bench_mount(
    me: &std::path::Path,
    store: &std::path::Path,
    ls_dir: &str,
    file: &str,
) -> Result<Option<Vec<(String, serde_json::Value)>>> {
    let has_fusermount = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .any(|d| d.join("fusermount3").is_file() || d.join("fusermount").is_file())
        })
        .unwrap_or(false);
    if !cfg!(target_os = "linux") || !std::path::Path::new("/dev/fuse").exists() || !has_fusermount
    {
        return Ok(None);
    }
    let mnt = tempfile_dir()?.join("mnt");
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
    while !(crate::cmd::mount::is_mounted(&mnt) && std::fs::metadata(&mnt).is_ok()) {
        if child.try_wait()?.is_some() {
            anyhow::bail!("mount process exited");
        }
        if t0.elapsed().as_secs() > 10 {
            let _ = child.kill();
            anyhow::bail!("mount did not come up within 10 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut out = vec![("mount_s".to_string(), t0.elapsed().as_secs_f64().into())];
    let dir = mnt.join(ls_dir);
    for k in ["mount_ls_cold_s", "mount_ls_warm_s"] {
        let mut c = Command::new("ls");
        c.arg("-A").arg(&dir);
        let (secs, _) = timed(c)?;
        out.push((k.to_string(), secs.into()));
    }
    if !file.is_empty() {
        let mut c = Command::new("cat");
        c.arg(mnt.join(file));
        let (secs, _) = timed(c)?;
        out.push(("mount_cat_s".to_string(), secs.into()));
    }
    let r = crate::cmd::mount::fusermount_unmount(&mnt, false);
    let _ = child.wait();
    r?;
    Ok(Some(out))
}

fn tempfile_dir() -> Result<PathBuf> {
    let d = std::env::temp_dir().join(format!("traj-bench-{}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}
