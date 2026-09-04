//! `traj bench`: the regression baseline of docs/PLAN.md §8/§11 as one JSON document.

use anyhow::Result;
use clap::Args;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Source tree to pack (packs into a temporary store unless --store is given)
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Existing store to measure the verbs on
    #[arg(long)]
    pub store: Option<PathBuf>,
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

pub fn run(a: BenchArgs) -> Result<i32> {
    let me = std::env::current_exe()?;
    let mut r = serde_json::Map::new();
    r.insert("version".into(), crate::VERSION.into());
    r.insert("date".into(), chrono::Utc::now().to_rfc3339().into());
    let tmp = tempfile_dir()?;
    let store = match (&a.store, &a.source) {
        (Some(s), _) => s.clone(),
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
        (None, None) => anyhow::bail!("give --store or --source"),
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

fn tempfile_dir() -> Result<PathBuf> {
    let d = std::env::temp_dir().join(format!("traj-bench-{}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}
