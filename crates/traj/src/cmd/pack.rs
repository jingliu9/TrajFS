use crate::config::{check_separation, Config};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use trajfs_core::ingest::{ingest, IngestOptions};
use trajfs_core::rules::Rules;

#[derive(Args, Debug)]
pub struct PackArgs {
    /// Source tree (a run directory)
    pub src: PathBuf,
    /// Destination store directory (default: <store_root>/<basename(src)>.trajstore from trajfs.toml)
    #[arg(long = "out", alias = "into")]
    pub out: Option<PathBuf>,
    /// Adapter: built-in (none, jsonl[:globs], copilot-cli, claude-code) or a path to an adapter TOML (default: trajfs.toml)
    #[arg(long)]
    pub adapter: Option<String>,
    /// Rule profile: none | no-build-products | <file.toml> (default: the adapter's, else trajfs.toml)
    #[arg(long)]
    pub rules: Option<String>,
    /// Batch label
    #[arg(long, default_value = "")]
    pub label: String,
    /// Hashing/compression workers
    #[arg(long, default_value_t = 16)]
    pub jobs: usize,
    /// Do not build derived tables (events)
    #[arg(long)]
    pub no_derive: bool,
    /// Refuse instead of warn when the source is inside a git work tree
    #[arg(long)]
    pub strict: bool,
    /// Store id recorded in the manifest (default: basename of src)
    #[arg(long)]
    pub id: Option<String>,
}

pub fn resolve(a: &PackArgs) -> Result<(PathBuf, String, String)> {
    let cfg = Config::find();
    let store = match (&a.out, &cfg) {
        (Some(s), _) => s.clone(),
        (None, Some(c)) => {
            let id = a.id.clone().unwrap_or_else(|| {
                a.src
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "store".into())
            });
            c.store_path(&id)
        }
        (None, None) => bail!("no --store given and no trajfs.toml found"),
    };
    let adapter = a
        .adapter
        .clone()
        .or_else(|| cfg.as_ref().map(|c| c.file.adapter.clone()))
        .unwrap_or_else(|| "none".into());
    let ad = trajfs_adapters::resolve(&adapter, cfg.as_ref().map(|c| c.dir.as_path()))?;
    let rules = a
        .rules
        .clone()
        .or_else(|| ad.rule_profile())
        .or_else(|| cfg.as_ref().map(|c| c.file.rules.clone()))
        .unwrap_or_else(|| "no-build-products".into());
    if let Some(c) = &cfg {
        for w in check_separation(c, Some(&a.src), Some(&store), a.strict)? {
            eprintln!("warning: {w}");
        }
    }
    Ok((store, adapter, rules))
}

pub fn run(a: PackArgs) -> Result<i32> {
    let (store, adapter, rules) = resolve(&a)?;
    let cfg = Config::find();
    let ad = trajfs_adapters::resolve(&adapter, cfg.as_ref().map(|c| c.dir.as_path()))?;
    let rules_obj = Rules::resolve_from(&rules, cfg.as_ref().map(|c| c.dir.as_path()))?;
    let sum = ingest(
        &a.src,
        &store,
        IngestOptions {
            rules: rules_obj,
            rules_name: rules.clone(),
            adapter: ad.as_ref(),
            label: a.label.clone(),
            jobs: a.jobs,
            derive: !a.no_derive,
            store_id: a.id.clone(),
        },
    )
    .with_context(|| format!("pack {} into {}", a.src.display(), store.display()))?;
    let b = &sum.batch;
    println!(
        "batch {} in {}: {} paths ({}), {} new blobs ({} raw, {} packed), {} excluded, {} unchanged skipped, {} packs, {:.1} s",
        b.id,
        sum.store.display(),
        b.paths,
        crate::human(b.bytes as i64),
        b.new_blobs,
        crate::human(b.new_blob_bytes as i64),
        crate::human(b.packed_bytes as i64),
        b.excluded,
        sum.skipped_unchanged,
        b.packs.len(),
        b.elapsed_ms as f64 / 1000.0
    );
    if !b.errors.is_empty() {
        for e in b.errors.iter().take(20) {
            eprintln!("error: {e}");
        }
        eprintln!(
            "{} paths could not be read (listed in MANIFEST.json)",
            b.errors.len()
        );
        return Ok(1);
    }
    println!(
        "next: traj verify -S {}  then  traj commit --push {}",
        sum.store.display(),
        sum.store.display()
    );
    Ok(0)
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Directory holding run directories (data_root by default)
    pub root: Option<PathBuf>,
    /// Poll interval in seconds
    #[arg(long, default_value_t = 60)]
    pub interval: u64,
    /// Commit each batch
    #[arg(long)]
    pub commit: bool,
    /// Push after each commit
    #[arg(long)]
    pub push: bool,
    /// Pack at most this many batches then exit (0 = forever)
    #[arg(long, default_value_t = 0)]
    pub max_batches: u32,
    #[arg(long, default_value_t = 16)]
    pub jobs: usize,
}

/// Polling watcher: discover matching run directories without descending into
/// them, ask the adapter whether a batch is ready, and pack it.
pub fn watch(a: WatchArgs) -> Result<i32> {
    let cfg = Config::require()?;
    let root = a.root.clone().unwrap_or_else(|| cfg.data_root());
    let ad = cfg.adapter()?;
    let run_glob = ad.run_glob().unwrap_or_else(|| "*".into());
    let matcher = globset::Glob::new(&run_glob)?.compile_matcher();
    let mut done = 0u32;
    loop {
        let runs = discover_runs(&root, &matcher)?;
        for run_dir in runs {
            let id = store_id(&root, &run_dir)?;
            let store = cfg.store_path(&id);
            let already: Vec<String> = trajfs_core::Manifest::load(&store)
                .map(|m| m.batches.iter().map(|b| b.label.clone()).collect())
                .unwrap_or_default();
            if let Some(label) = ad.batch_ready(&run_dir, &already) {
                eprintln!("{}: packing {label}", run_dir.display());
                let r = run_args_for(&run_dir, &store, &cfg, &label, &id, a.jobs)?;
                run(r)?;
                if a.commit {
                    crate::cmd::commit::run(crate::cmd::commit::CommitArgs {
                        store: store.display().to_string(),
                        push: a.push,
                        message: None,
                    })?;
                }
                done += 1;
                if a.max_batches > 0 && done >= a.max_batches {
                    return Ok(0);
                }
            }
        }
        if a.max_batches > 0 && done >= a.max_batches {
            return Ok(0);
        }
        std::thread::sleep(std::time::Duration::from_secs(a.interval.max(1)));
    }
}

fn discover_runs(root: &Path, matcher: &globset::GlobMatcher) -> Result<Vec<PathBuf>> {
    let mut runs = Vec::new();
    let mut entries = walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = entries.next() {
        let entry = entry?;
        if !entry.file_type().is_dir() {
            continue;
        }
        let relative = entry.path().strip_prefix(root)?;
        if matcher.is_match(relative) {
            runs.push(entry.path().to_path_buf());
            entries.skip_current_dir();
        }
    }
    runs.sort();
    Ok(runs)
}

fn store_id(root: &Path, run: &Path) -> Result<String> {
    let relative = run
        .strip_prefix(root)
        .with_context(|| format!("run {} is outside {}", run.display(), root.display()))?;
    let mut id = String::new();
    for byte in relative.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            id.push(*byte as char);
        } else {
            use std::fmt::Write as _;
            write!(&mut id, "%{byte:02X}")?;
        }
    }
    if id.is_empty() {
        bail!("run path has no store id: {}", run.display());
    }
    Ok(id)
}

fn run_args_for(
    run: &Path,
    store: &Path,
    cfg: &Config,
    label: &str,
    id: &str,
    jobs: usize,
) -> Result<PackArgs> {
    Ok(PackArgs {
        src: run.to_path_buf(),
        out: Some(store.to_path_buf()),
        adapter: Some(cfg.file.adapter.clone()),
        rules: None,
        label: label.to_string(),
        jobs,
        no_derive: false,
        strict: false,
        id: Some(id.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_run_ids_are_collision_free_and_readable() {
        let root = Path::new("/runs");
        assert_eq!(store_id(root, Path::new("/runs/run7")).unwrap(), "run7");
        assert_eq!(
            store_id(root, Path::new("/runs/suite/task/run7")).unwrap(),
            "suite%2Ftask%2Frun7"
        );
        assert_ne!(
            store_id(root, Path::new("/runs/suite/task/run7")).unwrap(),
            store_id(root, Path::new("/runs/other/task/run7")).unwrap()
        );
    }
}
