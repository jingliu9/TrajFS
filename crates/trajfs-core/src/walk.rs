//! Source-tree walk with rule filtering (PLAN.md §4 step 1).

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

#[derive(Clone, Debug)]
pub struct Excluded {
    pub rel: String,
    pub size: u64,
    pub rule: &'static str,
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
    let root = root.canonicalize().with_context(|| format!("source {}", root.display()))?;
    let mut kept = Vec::new();
    let mut excluded = Vec::new();
    let mut errors = Vec::new();
    let excl_dirs = &rules.file.exclude_dirs;
    let mut it = walkdir::WalkDir::new(&root).follow_links(false).into_iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(err.to_string());
                continue;
            }
        };
        if entry.depth() > 0 && entry.file_type().is_dir() {
            if skip.iter().any(|s| s == entry.path()) {
                it.skip_current_dir();
                continue;
            }
            let n = entry.file_name().to_string_lossy();
            if excl_dirs.iter().any(|x| x.as_str() == n) {
                // the whole subtree is left out; recorded once as a directory row
                let rel = entry.path().strip_prefix(&root).unwrap().to_string_lossy().to_string();
                excluded.push(Excluded { rel, size: 0, rule: "exclude_dirs" });
                it.skip_current_dir();
                continue;
            }
        }
        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        let rel_os = entry.path().strip_prefix(&root).unwrap();
        let rel = match rel_os.to_str() {
            Some(s) => s.to_string(),
            None => {
                errors.push(format!("{}: path is not valid UTF-8", rel_os.display()));
                continue;
            }
        };
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                errors.push(format!("{rel}: {err}"));
                continue;
            }
        };
        let ft = entry.file_type();
        let (kind, size, mode) = if ft.is_symlink() {
            let target_len = std::fs::read_link(entry.path()).map(|t| t.as_os_str().len()).unwrap_or(0);
            (Kind::Symlink, target_len as u64, 0o777u16)
        } else if ft.is_file() {
            let exec = md.permissions().mode() & 0o111 != 0;
            (if md.len() == 0 { Kind::Empty } else { Kind::File }, md.len(), if exec { 0o755 } else { 0o644 })
        } else {
            excluded.push(Excluded { rel, size: 0, rule: "special-file" });
            continue;
        };
        let abs = entry.path().to_path_buf();
        match rules.decide(&rel, size, kind == Kind::Symlink, &|| is_elf(&abs)) {
            Decision::Keep => kept.push(Candidate {
                rel,
                abs,
                kind,
                mode,
                size,
                mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
            }),
            Decision::Exclude(rule) => excluded.push(Excluded { rel, size, rule }),
        }
    }
    kept.sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    excluded.sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    Ok(WalkResult { kept, excluded, errors })
}
