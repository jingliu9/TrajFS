use crate::config::{git_toplevel, resolve_store, Config};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::process::Command;

#[derive(Args, Debug)]
pub struct CommitArgs {
    /// Store directory or id
    pub store: String,
    #[arg(long)]
    pub push: bool,
    /// Commit message (default: generated from the manifest's last batch)
    #[arg(short = 'm', long)]
    pub message: Option<String>,
}

pub fn run(a: CommitArgs) -> Result<i32> {
    let cfg = Config::try_find()?;
    let store = resolve_store(&a.store, cfg.as_ref())?;
    let store = store.canonicalize()?;
    let repo = git_toplevel(&store).context("the store is not inside a git work tree")?;
    let m = trajfs_core::Manifest::load(&store)?;
    let Some(last) = m.batches.last() else {
        bail!("store has no batches")
    };
    let rel = store
        .strip_prefix(&repo)
        .context("store is outside its Git worktree")?;
    let relative = rel.display().to_string();
    let snapshot = trajfs_core::Store::open(&store)?;
    if !snapshot.verify(false)?.ok() {
        bail!("store fails verification; refusing to stage or commit it");
    }
    let msg = a.message.clone().unwrap_or_else(|| {
        format!(
            "trajstore {}: batch {} {}\n\n{} paths, {} new blobs ({} packed), adapter {} v{}, rules {} v{}\nsource {}",
            m.store_id,
            last.id,
            last.label,
            last.paths,
            last.new_blobs,
            last.packed_bytes,
            m.adapter.name,
            m.adapter.version,
            m.rules.name,
            m.rules.version,
            m.source
        )
    });
    let git = |args: &[&str]| -> Result<()> {
        let st = Command::new("git")
            .arg("--literal-pathspecs")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .context("run git")?;
        if !st.success() {
            bail!("git {} failed", args.join(" "));
        }
        Ok(())
    };
    // only the store's own files: new batch files are the only untracked/changed ones
    git(&["add", "-A", "--", &relative])?;
    let staged = Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(&repo)
        .args(["diff", "--cached", "--name-only", "-z", "--", &relative])
        .output()?;
    if !staged.status.success() {
        bail!(
            "cannot inspect staged store changes: {}",
            String::from_utf8_lossy(&staged.stderr)
        );
    }
    let n = String::from_utf8_lossy(&staged.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .count();
    if n == 0 {
        println!("nothing to commit for {}", rel.display());
        return Ok(0);
    }
    git(&["commit", "--only", "-q", "-m", &msg, "--", &relative])?;
    println!("committed {n} files of {}", rel.display());
    if a.push {
        git(&["push"])?;
        println!("pushed");
    }
    Ok(0)
}
