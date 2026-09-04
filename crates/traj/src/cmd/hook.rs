//! Git hooks (PLAN.md §6.1). Logic lives here; `.git/hooks/pre-commit` is a symlink to the binary.

use crate::config::{git_toplevel, Config};
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use regex::Regex;
use std::path::Path;
use std::process::Command;

#[derive(Args, Debug)]
pub struct HookArgs {
    #[command(subcommand)]
    pub which: Which,
}

#[derive(Subcommand, Debug)]
pub enum Which {
    /// Check the staged changes (what the installed hook runs)
    PreCommit,
    /// Check a committed tree for raw-run paths (for CI): `traj hook check-tree HEAD`
    CheckTree { rev: String },
}

/// Patterns come from trajfs.toml `[hook] raw_patterns` (filled by `traj init` from the adapter file).
pub fn raw_run_regex(cfg: Option<&Config>) -> Result<Option<Regex>> {
    let pats: Vec<String> = cfg.map(|c| c.file.hook.raw_patterns.clone()).unwrap_or_default();
    if pats.is_empty() {
        return Ok(None);
    }
    Ok(Some(Regex::new(&pats.iter().map(|p| format!("(?:{p})")).collect::<Vec<_>>().join("|")).context("hook raw_patterns")?))
}

pub fn run(a: HookArgs) -> Result<i32> {
    match a.which {
        Which::PreCommit => pre_commit(),
        Which::CheckTree { rev } => check_tree(&rev),
    }
}

fn git_lines(repo: &Path, args: &[&str]) -> Result<Vec<String>> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().context("run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).split('\0').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect())
}

pub fn pre_commit() -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let repo = git_toplevel(&cwd).context("not in a git work tree")?;
    let cfg = Config::find_from(&repo);
    let (max_paths, max_bytes) = cfg.as_ref().map(|c| (c.file.hook.max_added_paths, c.file.hook.max_file_bytes)).unwrap_or((10_000, 64 << 20));
    let added = git_lines(&repo, &["diff", "--cached", "--name-only", "--diff-filter=A", "-z"])?;
    let all = git_lines(&repo, &["diff", "--cached", "--name-only", "-z"])?;
    let re = raw_run_regex(cfg.as_ref())?;
    let mut problems: Vec<String> = Vec::new();
    if added.len() > max_paths {
        problems.push(format!("{} paths added in one commit (limit {max_paths})", added.len()));
    }
    if let Some(re) = &re {
        let raw: Vec<&String> = all.iter().filter(|p| re.is_match(p)).collect();
        if !raw.is_empty() {
            problems.push(format!("{} raw run paths staged, e.g. {}", raw.len(), raw[0]));
        }
    }
    for p in &all {
        if let Ok(md) = std::fs::metadata(repo.join(p)) {
            if md.is_file() && md.len() > max_bytes {
                problems.push(format!("{p} is {} bytes (limit {max_bytes})", md.len()));
            }
        }
    }
    if problems.is_empty() {
        return Ok(0);
    }
    eprintln!("traj pre-commit hook: commit refused");
    for p in &problems {
        eprintln!("  - {p}");
    }
    let hint = match cfg {
        Some(c) => format!("traj pack <run-dir>   # store goes to {}\ntraj commit --push <store>", c.store_root().display()),
        None => "traj init --data-root <abs dir>; traj pack <run-dir>; traj commit --push <store>".to_string(),
    };
    eprintln!("raw run trees are never committed; pack them instead:\n{hint}");
    Ok(1)
}

pub fn check_tree(rev: &str) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let repo = git_toplevel(&cwd).context("not in a git work tree")?;
    let paths = git_lines(&repo, &["ls-tree", "-r", "--name-only", "-z", rev])?;
    let Some(re) = raw_run_regex(Config::find_from(&repo).as_ref())? else {
        println!("{rev}: no raw_patterns configured in trajfs.toml [hook]; nothing to check");
        return Ok(0);
    };
    let bad: Vec<&String> = paths.iter().filter(|p| re.is_match(p)).collect();
    if bad.is_empty() {
        println!("{rev}: no raw-run paths");
        Ok(0)
    } else {
        println!("{rev}: {} raw-run paths, e.g. {}", bad.len(), bad[0]);
        Ok(1)
    }
}

/// Raw-run paths tracked in the index (used by `doctor`).
pub fn tracked_raw_paths(repo: &Path) -> Result<Vec<String>> {
    let Some(re) = raw_run_regex(Config::find_from(repo).as_ref())? else { return Ok(Vec::new()) };
    let out = Command::new("git").arg("-C").arg(repo).args(["ls-files", "-z"]).output().context("run git ls-files")?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty() && re.is_match(p)).take(1000).map(|s| s.to_string()).collect())
}
