//! Git hooks (docs/PLAN.md §6.1). Logic lives here; `.git/hooks/pre-commit` is a symlink to the binary.

use crate::config::{git_toplevel, Config, DEFAULT_MAX_FILE_BYTES};
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
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
    let pats: Vec<String> = cfg
        .map(|c| c.file.hook.raw_patterns.clone())
        .unwrap_or_default();
    if pats.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        Regex::new(
            &pats
                .iter()
                .map(|p| format!("(?:{p})"))
                .collect::<Vec<_>>()
                .join("|"),
        )
        .context("hook raw_patterns")?,
    ))
}

pub fn run(a: HookArgs) -> Result<i32> {
    match a.which {
        Which::PreCommit => pre_commit(),
        Which::CheckTree { rev } => check_tree(&rev),
    }
}

fn git_lines(repo: &Path, args: &[&str]) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

fn git_success(repo: &Path, args: &[&str]) -> Result<bool> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?
        .status
        .success())
}

fn index_has(repo: &Path, path: &Path) -> Result<bool> {
    let spec = format!(":{}", path.display());
    git_success(repo, &["cat-file", "-e", &spec])
}

fn is_store_artifact(path: &Path) -> bool {
    let parts = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [file] => matches!(file.as_str(), "MANIFEST.json" | ".gitattributes" | ".lock"),
        [dir, file] if dir == "catalog" => file.ends_with(".parquet"),
        [dir, file] if dir == "packs" => file.ends_with(".pack") || file.ends_with(".parquet"),
        [dir, adapter, file] if dir == "derived" => {
            !adapter.is_empty() && file.ends_with(".parquet")
        }
        _ => false,
    }
}

fn validate_staged_stores(
    repo: &Path,
    cfg: &Config,
    changed: &[String],
    problems: &mut Vec<String>,
) -> Result<()> {
    let store_root = cfg.store_root();
    let Ok(store_root_rel) = store_root.strip_prefix(repo) else {
        return Ok(());
    };
    let mut touched = BTreeSet::<PathBuf>::new();
    for path in changed {
        let Ok(relative) = Path::new(path).strip_prefix(store_root_rel) else {
            continue;
        };
        let mut parts = relative.components();
        let Some(first) = parts.next() else {
            continue;
        };
        let first = first.as_os_str().to_string_lossy();
        if first.ends_with(".trajstore") {
            let store = store_root_rel.join(first.as_ref());
            if !is_store_artifact(parts.as_path()) {
                problems.push(format!("{path} is not a recognized TrajFS store artifact"));
            }
            touched.insert(store);
        } else if parts.next().is_some() {
            problems.push(format!(
                "{path} is nested under store_root but not inside a .trajstore"
            ));
        } else if !matches!(
            first.as_ref(),
            ".gitattributes" | ".gitkeep" | "ARCHIVE.md" | "INDEX.tsv" | "README.md"
        ) {
            problems.push(format!(
                "{path} is not a TrajFS store or allowed store_root metadata"
            ));
        }
    }

    for store_rel in touched {
        let store_name = store_rel.display().to_string();
        let manifest_rel = store_rel.join("MANIFEST.json");
        if !index_has(repo, &manifest_rel)? {
            problems.push(format!("{store_name} has no staged MANIFEST.json"));
            continue;
        }
        if !git_success(repo, &["diff", "--quiet", "--", &store_name])? {
            problems.push(format!(
                "{store_name} has unstaged changes; stage the complete store"
            ));
            continue;
        }
        let untracked = git_lines(
            repo,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                &store_name,
            ],
        )?;
        if !untracked.is_empty() {
            problems.push(format!(
                "{store_name} has untracked files, e.g. {}; stage the complete store",
                untracked[0]
            ));
            continue;
        }

        let store = match trajfs_core::Store::open(&repo.join(&store_rel)) {
            Ok(store) => store,
            Err(error) => {
                problems.push(format!(
                    "{store_name} is not a readable TrajFS store: {error:#}"
                ));
                continue;
            }
        };
        let mut missing = Vec::new();
        for batch in &store.manifest.batches {
            for segment in &batch.segments {
                let suffix = segment.strip_prefix("files-").unwrap_or(segment);
                for relative in [
                    format!("catalog/{segment}.parquet"),
                    format!("catalog/dirs-{suffix}.parquet"),
                    format!("catalog/excluded-{suffix}.parquet"),
                    format!("packs/index-{suffix}.parquet"),
                ] {
                    let path = store_rel.join(relative);
                    if !index_has(repo, &path)? {
                        missing.push(path.display().to_string());
                    }
                }
            }
            for pack in &batch.packs {
                let path = store_rel.join(format!("packs/{pack:04}.pack"));
                if !index_has(repo, &path)? {
                    missing.push(path.display().to_string());
                }
            }
        }
        if !missing.is_empty() {
            problems.push(format!(
                "{store_name} is incomplete in the staged snapshot, e.g. missing {}",
                missing[0]
            ));
            continue;
        }
        match store.verify(false) {
            Ok(report) if report.ok() => {}
            Ok(report) => problems.push(format!(
                "{store_name} fails TrajFS verification: {} missing packs, {} missing blobs, {} bad parts",
                report.missing_packs.len(),
                report.missing_blobs.len(),
                report.bad_parts.len()
            )),
            Err(error) => {
                problems.push(format!("{store_name} cannot be verified: {error:#}"));
            }
        }
    }
    Ok(())
}

pub fn pre_commit() -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let repo = git_toplevel(&cwd).context("not in a git work tree")?;
    let cfg = Config::find_from(&repo);
    let (max_paths, max_bytes) = cfg
        .as_ref()
        .map(|c| (c.file.hook.max_added_paths, c.file.hook.max_file_bytes))
        .unwrap_or((10_000, DEFAULT_MAX_FILE_BYTES));
    let added = git_lines(
        &repo,
        &["diff", "--cached", "--name-only", "--diff-filter=A", "-z"],
    )?;
    // additions, modifications, copies and renames: deletions of raw paths are the migration and stay allowed
    let all = git_lines(
        &repo,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=AMCR",
            "-z",
        ],
    )?;
    let changed = git_lines(&repo, &["diff", "--cached", "--name-only", "-z"])?;
    let re = raw_run_regex(cfg.as_ref())?;
    let mut problems: Vec<String> = Vec::new();
    if added.len() > max_paths {
        problems.push(format!(
            "{} paths added in one commit (limit {max_paths})",
            added.len()
        ));
    }
    if let Some(re) = &re {
        let raw: Vec<&String> = all.iter().filter(|p| re.is_match(p)).collect();
        if !raw.is_empty() {
            problems.push(format!(
                "{} raw run paths staged, e.g. {}",
                raw.len(),
                raw[0]
            ));
        }
    }
    for p in &all {
        if let Ok(md) = std::fs::metadata(repo.join(p)) {
            if md.is_file() && md.len() > max_bytes {
                problems.push(format!("{p} is {} bytes (limit {max_bytes})", md.len()));
            }
        }
    }
    if let Some(cfg) = &cfg {
        validate_staged_stores(&repo, cfg, &changed, &mut problems)?;
    }
    if problems.is_empty() {
        return Ok(0);
    }
    eprintln!("traj pre-commit hook: commit refused");
    for p in &problems {
        eprintln!("  - {p}");
    }
    let hint = match cfg {
        Some(c) => format!(
            "traj pack <run-dir>   # store goes to {}\ntraj commit --push <store>",
            c.store_root().display()
        ),
        None => "traj init --data-root <abs dir>; traj pack <run-dir>; traj commit --push <store>"
            .to_string(),
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
    let Some(re) = raw_run_regex(Config::find_from(repo).as_ref())? else {
        return Ok(Vec::new());
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-z"])
        .output()
        .context("run git ls-files")?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty() && re.is_match(p))
        .take(1000)
        .map(|s| s.to_string())
        .collect())
}
