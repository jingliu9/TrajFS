//! `traj delete`: remove one path (a file or a subtree) from every batch of a store
//! (tasks/PLAN-deletion.md). Dry run by default; `--yes` applies; `--recover` finishes
//! or discards an interrupted apply.

use crate::config::{artifact_target_bytes, git_toplevel, resolve_store, Config};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;
use trajfs_core::delete::{self, Plan, Recovered};

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Catalog path to remove from every batch: one file, or one directory subtree
    pub path: Option<String>,
    /// Apply the deletion (default: dry run)
    #[arg(long)]
    pub yes: bool,
    /// Resolve an interrupted deletion of this store, then exit
    #[arg(long)]
    pub recover: bool,
}

fn git_output(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// The store must be a committed, unchanged checkpoint: that commit is what recovery falls back to.
fn check_git_checkpoint(store: &Path) -> Result<String> {
    let repo = git_toplevel(store).context(
        "the store is not inside a Git work tree; deletion requires a committed checkpoint to recover from",
    )?;
    let rel = store
        .strip_prefix(&repo)
        .context("store is outside its Git work tree")?
        .display()
        .to_string();
    let manifest = format!("{rel}/MANIFEST.json");
    let tracked =
        git_output(&repo, &["ls-tree", "--name-only", "HEAD", "--", &manifest]).unwrap_or_default();
    if tracked.is_empty() {
        bail!("{rel} is not committed; run `traj commit {rel}` first");
    }
    let status = git_output(
        &repo,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--",
            &rel,
        ],
    )?;
    let dirty: Vec<String> = String::from_utf8_lossy(&status)
        .split('\0')
        .filter(|entry| entry.len() > 3)
        .map(|entry| entry[3..].to_string())
        .filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| name != ".lock")
        })
        .collect();
    if !dirty.is_empty() {
        bail!(
            "{rel} differs from HEAD ({} paths, e.g. {}); commit it with `traj commit {rel}` or restore it with `git checkout HEAD -- {rel}` first",
            dirty.len(),
            dirty[0]
        );
    }
    let head = String::from_utf8_lossy(&git_output(&repo, &["rev-parse", "--short", "HEAD"])?)
        .trim()
        .to_string();
    Ok(head)
}

fn check_not_mounted(store: &Path) -> Result<()> {
    let mounts = crate::cmd::mount::mounts_serving(store);
    if mounts.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = mounts
        .iter()
        .map(|mp| format!("  traj umount {}", mp.display()))
        .collect();
    bail!(
        "the store is mounted at {} mountpoint(s); please unmount it first:\n{}",
        mounts.len(),
        list.join("\n")
    );
}

fn print_plan(store: &Path, plan: &Plan, source: &Path, head: &str, n_batches: usize) {
    println!(
        "delete {} from {} ({} of {} batches affected)",
        plan.path,
        store.display(),
        plan.batches.len(),
        n_batches
    );
    println!(
        "  catalog rows removed: {} ({}) over all batches; {} paths leave the current view",
        plan.paths,
        crate::human(plan.bytes as i64),
        plan.latest_paths
    );
    println!(
        "  event rows removed: {}; exclusion records removed: {}",
        plan.events, plan.excluded
    );
    println!(
        "  content no longer referenced: {} blobs ({} raw); shared content stays",
        plan.blobs,
        crate::human(plan.blob_bytes as i64)
    );
    println!("  surviving catalog rows: {}", plan.survivors);
    println!("  git checkpoint: HEAD {head}; no mount serves the store");
    let raw = source.join(&plan.path);
    if std::fs::symlink_metadata(&raw).is_ok() {
        println!(
            "  source still contains {}; later packs skip it (recorded in the manifest)",
            raw.display()
        );
    }
}

pub fn run(stores: &[String], a: DeleteArgs) -> Result<i32> {
    let cfg = Config::try_find()?;
    let store_arg = match stores {
        [] => bail!("no store given: pass -S <store-dir|store-id> or set TRAJ_STORE"),
        [store] => store,
        _ => bail!("this verb takes exactly one store"),
    };
    let store = resolve_store(store_arg, cfg.as_ref())?.canonicalize()?;

    if a.recover {
        return match delete::recover(&store)? {
            None => {
                println!("nothing to recover for {}", store.display());
                Ok(0)
            }
            Some(Recovered::NotApplied) => {
                println!(
                    "discarded the unfinished candidate; {} is unchanged and the deletion was not applied",
                    store.display()
                );
                Ok(0)
            }
            Some(Recovered::Applied) => {
                println!(
                    "removed the pre-deletion copy; {} is the verified replacement\nnext: traj commit {}",
                    store.display(),
                    store.display()
                );
                Ok(0)
            }
        };
    }
    let path = crate::norm(a.path.as_deref().context("a path to delete is required")?);

    // Preconditions (§4.1), all before anything is written.
    if let Some(work) = delete::pending(&store) {
        bail!(
            "a deletion of this store was interrupted ({} exists); run `traj -S {} delete --recover` first",
            work.display(),
            store.display()
        );
    }
    let head = check_git_checkpoint(&store)?;
    check_not_mounted(&store)?;
    let parent = store.parent().context("store has no parent directory")?;
    delete::check_exchange_support(parent)?;
    let _lock = trajfs_core::store::lock_store_exclusive(&store).with_context(|| {
        format!(
            "another traj command holds {}",
            store.join(".lock").display()
        )
    })?;
    let st = trajfs_core::Store::open_unlocked(&store)?;
    if !st.verify(false)?.ok() {
        bail!(
            "store fails verification; repair it (or restore it from Git) before deleting from it"
        );
    }
    let plan = delete::plan(&st, &path)?;
    let source = PathBuf::from(&st.manifest.source);
    print_plan(&store, &plan, &source, &head, st.manifest.batches.len());
    if !a.yes {
        println!(
            "dry run; apply with: traj -S {} delete {} --yes",
            store.display(),
            path
        );
        return Ok(0);
    }

    let max_bytes = artifact_target_bytes(cfg.as_ref());
    let work = delete::rebuild(&st, &plan, max_bytes)?;
    drop(st);
    delete::apply(&store, &work)?;
    println!(
        "deleted {} from {}: {} rows and {} blobs removed, {} rows kept, verified\nnext: traj commit {}",
        path,
        store.display(),
        plan.paths,
        plan.blobs,
        plan.survivors,
        store.display()
    );
    Ok(0)
}
