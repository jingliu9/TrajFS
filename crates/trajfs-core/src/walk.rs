//! Source-tree walk with rule filtering (docs/PLAN.md §4 step 1).

use crate::rules::{Decision, Rules};
use crate::Kind;
use anyhow::{Context, Result};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Candidate {
    pub rel: String,
    pub abs: PathBuf,
    pub kind: Kind,
    pub mode: u16,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excluded {
    pub rel: String,
    pub size: u64,
    pub rule: std::borrow::Cow<'static, str>,
}

/// Rule name recorded for source entries at or below a path removed by `traj delete`.
pub const DELETED_RULE: &str = "deleted";

/// Is `path` equal to `prefix` or below it? An empty prefix matches everything.
pub fn is_under(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

pub struct WalkResult {
    pub kept: Vec<Candidate>,
    pub excluded: Vec<Excluded>,
    pub errors: Vec<String>,
}

fn is_elf(path: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 4];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)) {
        Ok(()) => b == [0x7f, b'E', b'L', b'F'],
        Err(_) => false,
    }
}

/// Walk `root`, never following symlinks, never descending into `skip` (absolute paths, e.g. a store inside the tree).
pub fn walk(root: &Path, rules: &Rules, skip: &[PathBuf]) -> Result<WalkResult> {
    walk_with_deleted(root, rules, skip, &[])
}

/// `walk`, additionally leaving out every entry at or below a `deleted` relative path
/// (recorded once per pruned directory, or per file, with rule `deleted`), whatever the rules say.
///
/// Directories are visited in parallel on the global rayon pool: the walk of a million-file tree is
/// bound by `stat` and directory reads, which scale with cores. Results are sorted by path, so the
/// order of discovery does not matter.
pub fn walk_with_deleted(
    root: &Path,
    rules: &Rules,
    skip: &[PathBuf],
    deleted: &[String],
) -> Result<WalkResult> {
    let root = root
        .canonicalize()
        .with_context(|| format!("source {}", root.display()))?;
    let ctx = WalkCtx {
        root: &root,
        rules,
        skip,
        deleted,
    };
    let mut out = WalkResult {
        kept: Vec::new(),
        excluded: Vec::new(),
        errors: Vec::new(),
    };
    walk_dir(&ctx, &root, &mut out);
    out.kept
        .sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    out.excluded
        .sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    out.errors.sort();
    Ok(out)
}

struct WalkCtx<'a> {
    root: &'a Path,
    rules: &'a Rules,
    skip: &'a [PathBuf],
    deleted: &'a [String],
}

impl WalkResult {
    fn absorb(&mut self, other: WalkResult) {
        self.kept.extend(other.kept);
        self.excluded.extend(other.excluded);
        self.errors.extend(other.errors);
    }
}

/// One directory: classify its files here, then its subdirectories in parallel.
fn walk_dir(ctx: &WalkCtx, dir: &Path, out: &mut WalkResult) {
    use rayon::prelude::*;
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(err) => {
            out.errors
                .push(format!("{}: {err}", rel_lossy(ctx.root, dir)));
            return;
        }
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                out.errors
                    .push(format!("{}: {err}", rel_lossy(ctx.root, dir)));
                continue;
            }
        };
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(err) => {
                out.errors
                    .push(format!("{}: {err}", rel_lossy(ctx.root, &path)));
                continue;
            }
        };
        if ft.is_dir() {
            if ctx.skip.iter().any(|s| s == &path) {
                continue;
            }
            let rel = rel_lossy(ctx.root, &path);
            let rule = if ctx.deleted.iter().any(|d| is_under(&rel, d)) {
                Some(DELETED_RULE)
            } else {
                ctx.rules.prune_directory(&rel)
            };
            if let Some(rule) = rule {
                // the whole subtree is left out; recorded once as a directory row
                out.excluded.push(Excluded {
                    rel,
                    size: 0,
                    rule: rule.into(),
                });
                continue;
            }
            subdirs.push(path);
            continue;
        }
        classify(ctx, &path, ft, out);
    }
    if subdirs.is_empty() {
        return;
    }
    let results: Vec<WalkResult> = subdirs
        .par_iter()
        .map(|d| {
            let mut r = WalkResult {
                kept: Vec::new(),
                excluded: Vec::new(),
                errors: Vec::new(),
            };
            walk_dir(ctx, d, &mut r);
            r
        })
        .collect();
    for r in results {
        out.absorb(r);
    }
}

fn rel_lossy(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// A non-directory entry: keep, exclude, or record an error, exactly as the rules say.
fn classify(ctx: &WalkCtx, path: &Path, ft: std::fs::FileType, out: &mut WalkResult) {
    let rel_os = path.strip_prefix(ctx.root).unwrap_or(path);
    let rel = match rel_os.to_str() {
        Some(s) => s.to_string(),
        None => {
            out.errors
                .push(format!("{}: path is not valid UTF-8", rel_os.display()));
            return;
        }
    };
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) => {
            out.errors.push(format!("{rel}: {err}"));
            return;
        }
    };
    let (kind, size, mode) = if ft.is_symlink() {
        let target_len = std::fs::read_link(path)
            .map(|t| t.as_os_str().len())
            .unwrap_or(0);
        (Kind::Symlink, target_len as u64, 0o777u16)
    } else if ft.is_file() {
        let exec = md.permissions().mode() & 0o111 != 0;
        (
            if md.len() == 0 {
                Kind::Empty
            } else {
                Kind::File
            },
            md.len(),
            if exec { 0o755 } else { 0o644 },
        )
    } else {
        out.excluded.push(Excluded {
            rel,
            size: 0,
            rule: "special-file".into(),
        });
        return;
    };
    if ctx.deleted.iter().any(|d| is_under(&rel, d)) {
        out.excluded.push(Excluded {
            rel,
            size,
            rule: DELETED_RULE.into(),
        });
        return;
    }
    match ctx
        .rules
        .decide(&rel, size, kind == Kind::Symlink, &|| is_elf(path))
    {
        Decision::Keep => out.kept.push(Candidate {
            rel,
            abs: path.to_path_buf(),
            kind,
            mode,
            size,
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        }),
        Decision::Exclude(rule) => out.excluded.push(Excluded {
            rel,
            size,
            rule: rule.into(),
        }),
    }
}
