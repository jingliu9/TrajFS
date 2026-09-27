use crate::config::{git_toplevel, resolve_store, Config};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::Path;
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
    /// Upload size per push, e.g. 1.5G (default: hook.max_push_bytes in trajfs.toml, 1.5 GiB).
    /// Larger uploads are pushed in parts that stay below a Git host's per-push limit.
    #[arg(long, value_name = "SIZE")]
    pub push_limit: Option<String>,
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
    // A deletion newer than the last batch is what this commit records.
    let deletion = m.deleted.last().filter(|d| d.created >= last.created);
    let msg = a.message.clone().unwrap_or_else(|| match deletion {
        Some(d) => format!(
            "trajstore {}: delete {}\n\n{} rows, {} events, {} blobs ({} raw) removed over {} batches; adapter {} v{}\nsource {}",
            m.store_id,
            d.path,
            d.paths,
            d.events,
            d.blobs,
            d.blob_bytes,
            m.batches.len(),
            m.adapter.name,
            m.adapter.version,
            m.source
        ),
        None => format!(
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
        ),
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
        let limit = match &a.push_limit {
            Some(s) => parse_bytes(s)?,
            None => cfg
                .as_ref()
                .map(|c| c.file.hook.max_push_bytes)
                .unwrap_or(crate::config::DEFAULT_MAX_PUSH_BYTES),
        };
        push_within_limit(&repo, limit)?;
    }
    Ok(0)
}

/// `git push`, split into several pushes when the unpushed content is larger than `limit`.
///
/// Git hosts cap a single push (GitHub: 2 GB). A store batch is committed as one commit, and one
/// commit cannot be pushed in halves, so the split happens one level down: the blobs that the
/// remote lacks are grouped into parts below the limit, each part is committed to a throwaway tree
/// (no parent, never on any branch, no hook involved) and pushed to a temporary ref, which makes
/// the remote hold those objects. The real push then transfers only what is left, and the
/// temporary refs are deleted. History is unchanged: still one commit per batch.
pub fn push_within_limit(repo: &Path, limit: u64) -> Result<()> {
    let git_out = |args: &[&str]| -> Result<Vec<u8>> {
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
        Ok(out.stdout)
    };
    let git = |args: &[&str]| -> Result<()> {
        let st = Command::new("git")
            .arg("--literal-pathspecs")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .context("run git")?;
        if !st.success() {
            bail!("git {} failed", args.join(" "));
        }
        Ok(())
    };
    // what the remote already has: the upstream of the current branch, when there is one
    let upstream = git_out(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| !s.is_empty());
    let remote = upstream
        .as_deref()
        .and_then(|u| u.split_once('/').map(|(r, _)| r.to_string()))
        .unwrap_or_else(|| "origin".to_string());
    let blobs = unpushed_blobs(repo, upstream.as_deref())?;
    let total: u64 = blobs.iter().map(|b| b.size).sum();
    if total <= limit {
        git(&["push"])?;
        println!("pushed ({} of new content)", human_bytes(total));
        return Ok(());
    }
    // greedy parts below the limit, in path order so a pack and its index travel together
    let mut parts: Vec<Vec<&Blob>> = vec![Vec::new()];
    let mut acc = 0u64;
    for b in &blobs {
        if acc + b.size > limit && !parts.last().unwrap().is_empty() {
            parts.push(Vec::new());
            acc = 0;
        }
        if b.size > limit {
            eprintln!(
                "traj commit: {} is {} by itself, above the {} push limit; pushing it alone",
                b.path,
                human_bytes(b.size),
                human_bytes(limit)
            );
        }
        parts.last_mut().unwrap().push(b);
        acc += b.size;
    }
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    let index_file = repo
        .join(".git")
        .join(format!("traj-upload-index-{run_id}"));
    let mut pushed_refs: Vec<String> = Vec::new();
    let result = (|| -> Result<()> {
        println!(
            "pushing {} of new content in {} parts of at most {} (limit for one push)",
            human_bytes(total),
            parts.len(),
            human_bytes(limit)
        );
        for (k, part) in parts.iter().enumerate() {
            let part_bytes: u64 = part.iter().map(|b| b.size).sum();
            // a tree holding exactly this part's blobs, built in a private index
            let index_info: String = part
                .iter()
                .map(|b| format!("100644 blob {}\t{}\n", b.sha, b.path))
                .collect();
            let with_index = |args: &[&str], stdin: Option<&str>| -> Result<Vec<u8>> {
                let mut cmd = Command::new("git");
                cmd.arg("--literal-pathspecs")
                    .arg("-C")
                    .arg(repo)
                    .env("GIT_INDEX_FILE", &index_file)
                    .args(args)
                    .stdin(if stdin.is_some() {
                        std::process::Stdio::piped()
                    } else {
                        std::process::Stdio::null()
                    })
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = cmd.spawn().context("run git")?;
                if let Some(text) = stdin {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(text.as_bytes())?;
                }
                let out = child.wait_with_output()?;
                if !out.status.success() {
                    bail!(
                        "git {} failed: {}",
                        args.join(" "),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Ok(out.stdout)
            };
            with_index(&["read-tree", "--empty"], None)?;
            with_index(&["update-index", "--index-info"], Some(&index_info))?;
            let tree = String::from_utf8_lossy(&with_index(&["write-tree"], None)?)
                .trim()
                .to_string();
            let msg = format!("traj upload part {}/{}", k + 1, parts.len());
            let commit = String::from_utf8_lossy(&git_out(&["commit-tree", &tree, "-m", &msg])?)
                .trim()
                .to_string();
            let temp_ref = format!("refs/traj-upload/{run_id}/{}", k + 1);
            git(&["push", "-q", &remote, &format!("{commit}:{temp_ref}")])?;
            pushed_refs.push(temp_ref);
            println!(
                "  part {}/{}: {} files, {} pushed",
                k + 1,
                parts.len(),
                part.len(),
                human_bytes(part_bytes)
            );
        }
        // the remote now holds every blob; the real push carries commits and trees only
        git(&["push"])?;
        println!("pushed");
        Ok(())
    })();
    let _ = std::fs::remove_file(&index_file);
    if !pushed_refs.is_empty() {
        let mut args: Vec<String> = vec![
            "push".into(),
            "-q".into(),
            remote.clone(),
            "--delete".into(),
        ];
        args.extend(pushed_refs.iter().cloned());
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        if let Err(e) = git(&refs) {
            eprintln!(
                "traj commit: could not delete the temporary upload refs on {remote} ({e:#}); \
                 remove refs/traj-upload/{run_id}/* by hand"
            );
        }
    }
    result
}

struct Blob {
    sha: String,
    path: String,
    size: u64,
}

/// Blobs reachable from HEAD that the upstream does not have (everything, when there is no upstream).
fn unpushed_blobs(repo: &Path, upstream: Option<&str>) -> Result<Vec<Blob>> {
    let mut args = vec!["rev-list", "--objects", "HEAD"];
    let exclude;
    if let Some(u) = upstream {
        exclude = format!("^{u}");
        args.push(&exclude);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .output()
        .context("run git rev-list")?;
    if !out.status.success() {
        bail!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut by_sha: Vec<(String, String)> = Vec::new();
    for line in listing.lines() {
        let (sha, path) = line.split_once(' ').unwrap_or((line, ""));
        if !path.is_empty() {
            by_sha.push((sha.to_string(), path.to_string()));
        }
    }
    // types and sizes in one batch call
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("run git cat-file")?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        for (sha, _) in &by_sha {
            writeln!(stdin, "{sha}")?;
        }
    }
    let out = child.wait_with_output()?;
    let mut blobs = Vec::new();
    for (line, (sha, path)) in String::from_utf8_lossy(&out.stdout).lines().zip(&by_sha) {
        let mut it = line.split_whitespace();
        let (Some(name), Some(kind), Some(size)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if name == sha && kind == "blob" {
            blobs.push(Blob {
                sha: sha.clone(),
                path: path.clone(),
                size: size.parse().unwrap_or(0),
            });
        }
    }
    blobs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(blobs)
}

/// "1.5G", "800M", "64K", "123" (bytes); binary units.
pub fn parse_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => (&s[..i], s[i..].trim().to_ascii_uppercase()),
        None => (s, String::new()),
    };
    let value: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size {s:?}"))?;
    let mult: f64 = match unit.trim_end_matches("IB").trim_end_matches('B') {
        "" => 1.0,
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => bail!("invalid size unit in {s:?}; use K, M, G, or T"),
    };
    Ok((value * mult) as u64)
}

fn human_bytes(b: u64) -> String {
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    if b as f64 >= G {
        format!("{:.2} GiB", b as f64 / G)
    } else if b as f64 >= M {
        format!("{:.1} MiB", b as f64 / M)
    } else {
        format!("{:.0} KiB", b as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_bytes("123").unwrap(), 123);
        assert_eq!(parse_bytes("64K").unwrap(), 64 * 1024);
        assert_eq!(parse_bytes("1.5G").unwrap(), 1536 * 1024 * 1024);
        assert_eq!(parse_bytes("800MiB").unwrap(), 800 * 1024 * 1024);
        assert!(parse_bytes("12X").is_err());
        assert!(parse_bytes("").is_err());
    }
}
