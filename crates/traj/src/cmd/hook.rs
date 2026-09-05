//! Git hooks (docs/PLAN.md §6.1). Logic lives here; `.git/hooks/pre-commit` is a symlink to the binary.

use crate::config::{git_toplevel, Config, ConfigFile, CONFIG_NAME, DEFAULT_MAX_FILE_BYTES};
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
    /// Check a committed tree for raw-run paths and complete stores (for CI)
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
        .arg("--literal-pathspecs")
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
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?
        .status
        .success())
}

#[derive(Clone, Debug)]
struct GitEntry {
    mode: String,
    path: String,
}

fn parse_git_entries(bytes: &[u8]) -> Result<Vec<GitEntry>> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let (metadata, path) = record
                .split_once('\t')
                .context("git entry has no path separator")?;
            let fields: Vec<&str> = metadata.split_whitespace().collect();
            let [mode, _, _] = fields.as_slice() else {
                bail!("unexpected git entry metadata {metadata:?}");
            };
            Ok(GitEntry {
                mode: (*mode).to_string(),
                path: path.to_string(),
            })
        })
        .collect()
}

fn index_entries(repo: &Path, path: &str) -> Result<Vec<GitEntry>> {
    let out = Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "--stage", "-z", "--", path])
        .output()
        .context("run git ls-files --stage")?;
    if !out.status.success() {
        bail!(
            "git ls-files --stage failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_git_entries(&out.stdout)
}

fn tree_entries(repo: &Path, rev: &str) -> Result<Vec<GitEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-tree", "-r", "-z", rev])
        .output()
        .context("run git ls-tree")?;
    if !out.status.success() {
        bail!(
            "git ls-tree {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_git_entries(&out.stdout)
}

fn regular_blob(entry: &GitEntry) -> bool {
    matches!(entry.mode.as_str(), "100644" | "100755")
}

fn index_object_size(repo: &Path, path: &str) -> Result<Option<u64>> {
    let spec = format!(":{path}");
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-s", &spec])
        .output()
        .context("run git cat-file -s")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().parse()?))
}

fn git_blob(repo: &Path, spec: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", spec])
        .output()
        .context("run git show")?;
    if !out.status.success() {
        bail!(
            "git show {} failed: {}",
            spec,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    String::from_utf8(out.stdout).context("git object is not UTF-8")
}

fn index_config(repo: &Path) -> Result<Option<Config>> {
    let entries = index_entries(repo, CONFIG_NAME)?;
    if entries.is_empty() {
        return Ok(None);
    }
    if entries.len() != 1 || entries[0].path != CONFIG_NAME {
        bail!("staged {CONFIG_NAME} has unresolved index entries");
    }
    if !regular_blob(&entries[0]) {
        bail!(
            "staged {CONFIG_NAME} must be a regular Git blob, not mode {}",
            entries[0].mode
        );
    }
    let text = git_blob(repo, &format!(":{CONFIG_NAME}"))?;
    let file: ConfigFile = toml::from_str(&text).context("parse staged trajfs.toml")?;
    Ok(Some(Config {
        file,
        dir: repo.to_path_buf(),
    }))
}

fn tree_config(repo: &Path, rev: &str, entries: &[GitEntry]) -> Result<Option<Config>> {
    let matches: Vec<&GitEntry> = entries
        .iter()
        .filter(|entry| entry.path == CONFIG_NAME)
        .collect();
    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() != 1 || !regular_blob(matches[0]) {
        bail!("{rev}:{CONFIG_NAME} must be a regular Git blob");
    }
    let text = git_blob(repo, &format!("{rev}:{CONFIG_NAME}"))?;
    let file: ConfigFile = toml::from_str(&text).context("parse committed trajfs.toml")?;
    Ok(Some(Config {
        file,
        dir: repo.to_path_buf(),
    }))
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

fn is_finalized_data_artifact(path: &Path) -> bool {
    let parts = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [dir, file] if dir == "catalog" => file.ends_with(".parquet"),
        [dir, file] if dir == "packs" => file.ends_with(".pack") || file.ends_with(".parquet"),
        [dir, adapter, file] if dir == "derived" => {
            !adapter.is_empty() && file.ends_with(".parquet")
        }
        _ => false,
    }
}

fn store_paths(paths: &[String], store_rel: &Path) -> BTreeSet<PathBuf> {
    paths
        .iter()
        .filter_map(|path| {
            Path::new(path)
                .strip_prefix(store_rel)
                .ok()
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(Path::to_path_buf)
        })
        .collect()
}

fn validate_inventory(
    store_name: &str,
    manifest: &trajfs_core::Manifest,
    actual: &BTreeSet<PathBuf>,
    snapshot: &str,
    problems: &mut Vec<String>,
) -> Result<bool> {
    let inventory = manifest.artifacts_with_legacy_derived(|path| actual.contains(path))?;
    let expected = inventory.paths();
    if let Some(path) = expected.iter().find(|path| !actual.contains(*path)) {
        problems.push(format!(
            "{store_name} is incomplete in {snapshot}, e.g. missing {}",
            Path::new(store_name).join(path).display()
        ));
        return Ok(false);
    }
    if let Some(path) = actual
        .iter()
        .find(|path| is_finalized_data_artifact(path) && !expected.contains(*path))
    {
        problems.push(format!(
            "{store_name} contains unreferenced finalized artifact {}",
            Path::new(store_name).join(path).display()
        ));
        return Ok(false);
    }
    if let Some(path) = actual.iter().find(|path| !is_store_artifact(path)) {
        problems.push(format!(
            "{} is not a recognized TrajFS store artifact",
            Path::new(store_name).join(path).display()
        ));
        return Ok(false);
    }
    Ok(true)
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

        let entries = index_entries(repo, &store_name)?;
        let Some(manifest_entry) = entries
            .iter()
            .find(|entry| entry.path == manifest_rel.display().to_string())
        else {
            problems.push(format!("{store_name} has no staged MANIFEST.json"));
            continue;
        };
        if !regular_blob(manifest_entry) {
            problems.push(format!(
                "{} must be a regular Git blob, not mode {}",
                manifest_rel.display(),
                manifest_entry.mode
            ));
            continue;
        }
        if let Some(entry) = entries.iter().find(|entry| !regular_blob(entry)) {
            problems.push(format!(
                "{} must be a regular Git blob, not mode {}",
                entry.path, entry.mode
            ));
            continue;
        }
        let tracked: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
        let actual = store_paths(&tracked, &store_rel);
        let manifest_spec = format!(":{}", manifest_rel.display());
        let manifest = match git_blob(repo, &manifest_spec)
            .and_then(|text| trajfs_core::Manifest::parse(&text))
        {
            Ok(manifest) => manifest,
            Err(error) => {
                problems.push(format!(
                    "{store_name} has an invalid staged MANIFEST.json: {error:#}"
                ));
                continue;
            }
        };
        match validate_inventory(
            &store_name,
            &manifest,
            &actual,
            "the staged snapshot",
            problems,
        ) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                problems.push(format!(
                    "{store_name} has an invalid artifact inventory: {error:#}"
                ));
                continue;
            }
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
    let mut problems: Vec<String> = Vec::new();
    let cfg = match index_config(&repo) {
        Ok(config) => config,
        Err(error) => {
            problems.push(format!("invalid staged {CONFIG_NAME}: {error:#}"));
            None
        }
    };
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
    let re = match raw_run_regex(cfg.as_ref()) {
        Ok(regex) => regex,
        Err(error) => {
            problems.push(format!("invalid staged hook configuration: {error:#}"));
            None
        }
    };
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
        if let Some(size) = index_object_size(&repo, p)? {
            if size > max_bytes {
                problems.push(format!("{p} is {size} bytes (limit {max_bytes})"));
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
    let entries = tree_entries(&repo, rev)?;
    let paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    let cfg = tree_config(&repo, rev, &entries)?;
    let mut problems = Vec::new();
    if let Some(re) = raw_run_regex(cfg.as_ref())? {
        let bad: Vec<&String> = paths.iter().filter(|p| re.is_match(p)).collect();
        if !bad.is_empty() {
            problems.push(format!("{} raw-run paths, e.g. {}", bad.len(), bad[0]));
        }
    }
    if let Some(cfg) = &cfg {
        let store_root = cfg.store_root();
        if let Ok(store_root_rel) = store_root.strip_prefix(&repo) {
            let mut stores = BTreeSet::new();
            for path in &paths {
                let Ok(relative) = Path::new(path).strip_prefix(store_root_rel) else {
                    continue;
                };
                let Some(first) = relative.components().next() else {
                    continue;
                };
                if first.as_os_str().to_string_lossy().ends_with(".trajstore") {
                    stores.insert(store_root_rel.join(first));
                }
            }
            for store_rel in stores {
                let store_name = store_rel.display().to_string();
                let store_entries: Vec<&GitEntry> = entries
                    .iter()
                    .filter(|entry| {
                        Path::new(&entry.path) == store_rel
                            || Path::new(&entry.path).starts_with(&store_rel)
                    })
                    .collect();
                if let Some(entry) = store_entries.iter().find(|entry| !regular_blob(entry)) {
                    problems.push(format!(
                        "{} must be a regular Git blob, not mode {}",
                        entry.path, entry.mode
                    ));
                    continue;
                }
                let actual = store_paths(&paths, &store_rel);
                if !actual.contains(Path::new("MANIFEST.json")) {
                    problems.push(format!("{store_name} has no MANIFEST.json"));
                    continue;
                }
                let manifest_path = store_rel.join("MANIFEST.json");
                if !store_entries
                    .iter()
                    .any(|entry| entry.path == manifest_path.display().to_string())
                {
                    problems.push(format!("{store_name} has no MANIFEST.json"));
                    continue;
                }
                let spec = format!("{rev}:{}", manifest_path.display());
                let manifest = match git_blob(&repo, &spec)
                    .and_then(|text| trajfs_core::Manifest::parse(&text))
                {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        problems.push(format!("{store_name} has an invalid manifest: {error:#}"));
                        continue;
                    }
                };
                if let Err(error) = validate_inventory(
                    &store_name,
                    &manifest,
                    &actual,
                    &format!("commit {rev}"),
                    &mut problems,
                ) {
                    problems.push(format!(
                        "{store_name} has an invalid artifact inventory: {error:#}"
                    ));
                }
            }
        }
    }
    if problems.is_empty() {
        println!("{rev}: no raw-run paths or incomplete TrajFS stores");
        return Ok(0);
    }
    println!("{rev}: check failed");
    for problem in problems {
        println!("  - {problem}");
    }
    Ok(1)
}

/// Raw-run paths tracked in the index (used by `doctor`).
pub fn tracked_raw_paths(repo: &Path) -> Result<Vec<String>> {
    let Some(re) = raw_run_regex(Config::try_find_from(repo)?.as_ref())? else {
        return Ok(Vec::new());
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-z"])
        .output()
        .context("run git ls-files")?;
    if !out.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty() && re.is_match(p))
        .take(1000)
        .map(|s| s.to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_rederive_inventory_is_hook_compatible() {
        let manifest = trajfs_core::Manifest::parse(
            r#"{
              "format": 1,
              "store_id": "legacy",
              "source": "/source",
              "adapter": {"name": "none", "version": 1},
              "rules": {"name": "none", "version": 1},
              "batches": [{
                "id": 1,
                "created": "2026-09-05T00:00:00Z",
                "paths": 1,
                "bytes": 1,
                "new_blobs": 1,
                "new_blob_bytes": 1,
                "packed_bytes": 1,
                "packs": [1],
                "segments": ["files-0001"],
                "derived": ["none/events-0001"]
              }]
            }"#,
        )
        .unwrap();
        let actual = [
            "MANIFEST.json",
            "catalog/files-0001.parquet",
            "catalog/dirs-0001.parquet",
            "catalog/excluded-0001.parquet",
            "packs/index-0001.parquet",
            "packs/0001.pack",
            "derived/none/events-0000.parquet",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let mut problems = Vec::new();
        assert!(validate_inventory(
            "stores/legacy.trajstore",
            &manifest,
            &actual,
            "test",
            &mut problems
        )
        .unwrap());
        assert!(problems.is_empty());
    }
}
