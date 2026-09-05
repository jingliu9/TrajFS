//! Reading a store: catalog queries, blob access, extract, verify (docs/PLAN.md §5).

use crate::catalog::{self, DirRow};
use crate::hash::sha_of_bytes;
use crate::manifest::{resolve_store_artifact, Manifest};
use crate::pack::{Loc, PackReader};
use crate::{FileRow, Kind, Sha};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct Store {
    pub root: PathBuf,
    pub manifest: Manifest,
    files_segments: Vec<PathBuf>,
    dirs_segments: Vec<PathBuf>,
    excluded_segments: Vec<PathBuf>,
    index_segments: Vec<PathBuf>,
    derived_segments: Vec<PathBuf>,
    pack_ids: HashSet<u32>,
    index: OnceLock<HashMap<Sha, Vec<Loc>>>,
    _lock: Option<File>,
}

/// Range `[dir/, dir0)` covering every path below `dir` (or everything for the root).
pub fn subtree_range(dir: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    if dir.is_empty() {
        return None;
    }
    let mut lo = dir.as_bytes().to_vec();
    lo.push(b'/');
    let mut hi = dir.as_bytes().to_vec();
    hi.push(b'0');
    Some((lo, hi))
}

fn open_regular_file(root: &Path, path: &Path) -> Result<File> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("open regular file {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(root) {
        bail!(
            "{} resolves outside store {}",
            path.display(),
            root.display()
        );
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn open_reader_lock(root: &Path) -> Result<File> {
    let lock_path = root.join(".lock");
    let file = match std::fs::symlink_metadata(&lock_path) {
        Ok(_) => open_regular_file(root, &lock_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let manifest = Manifest::path(root);
            open_regular_file(root, &manifest)?
        }
        Err(error) => return Err(error.into()),
    };
    FileExt::try_lock_shared(&file)
        .with_context(|| format!("store {} is being updated", root.display()))?;
    Ok(file)
}

pub struct StoreWriteLock {
    _legacy_manifest: Option<File>,
    _lock: File,
}

pub fn lock_store_exclusive(root: &Path) -> Result<StoreWriteLock> {
    let manifest_path = Manifest::path(root);
    let legacy_manifest = match std::fs::symlink_metadata(&manifest_path) {
        Ok(_) => {
            let file = open_regular_file(root, &manifest_path)?;
            FileExt::try_lock_exclusive(&file)
                .with_context(|| format!("store {} is in use", root.display()))?;
            Some(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    let lock_path = root.join(".lock");
    if let Ok(metadata) = std::fs::symlink_metadata(&lock_path) {
        if !metadata.file_type().is_file() {
            bail!("store lock {} is not a regular file", lock_path.display());
        }
        let resolved = lock_path.canonicalize()?;
        if !resolved.starts_with(root) {
            bail!(
                "store lock {} resolves outside {}",
                lock_path.display(),
                root.display()
            );
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let lock = options.open(&lock_path)?;
    if !std::fs::symlink_metadata(&lock_path)?.file_type().is_file() {
        bail!("store lock {} is not a regular file", lock_path.display());
    }
    FileExt::try_lock_exclusive(&lock)
        .with_context(|| format!("store {} is in use", root.display()))?;
    Ok(StoreWriteLock {
        _legacy_manifest: legacy_manifest,
        _lock: lock,
    })
}

#[derive(Clone, Debug, Default)]
pub struct DirEntry {
    pub name: String,
    pub dir: String,
    pub n_files: i64,
    pub n_dirs: i64,
    pub bytes: i64,
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub files: u64,
    pub blobs: u64,
    pub missing_blobs: Vec<String>,
    pub bad_parts: Vec<String>,
    pub missing_packs: Vec<String>,
    pub corrupt: Vec<String>,
    pub shadowed_paths: u64,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.missing_blobs.is_empty()
            && self.bad_parts.is_empty()
            && self.missing_packs.is_empty()
            && self.corrupt.is_empty()
    }
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        Self::open_with_options(root, true, true)
    }

    /// Open the immutable core of a store so a missing derived generation can
    /// be rebuilt. Normal readers must use `open`, which requires everything
    /// published by the manifest.
    pub fn open_for_derive_unlocked(root: &Path) -> Result<Store> {
        Self::open_with_options(root, false, false)
    }

    pub(crate) fn open_unlocked(root: &Path) -> Result<Store> {
        Self::open_with_options(root, true, false)
    }

    fn open_with_options(root: &Path, require_derived: bool, lock_shared: bool) -> Result<Store> {
        let root = root
            .canonicalize()
            .with_context(|| format!("store {}", root.display()))?;
        let lock = if lock_shared {
            Some(open_reader_lock(&root)?)
        } else {
            None
        };
        let manifest = Manifest::load(&root)?;
        let inventory = manifest.artifacts_with_legacy_derived(|relative| {
            resolve_store_artifact(&root, relative).is_ok()
        })?;
        let pack_ids = manifest
            .batches
            .iter()
            .flat_map(|batch| batch.packs.iter().copied())
            .collect();
        for relative in inventory
            .files
            .iter()
            .chain(&inventory.dirs)
            .chain(&inventory.excluded)
            .chain(&inventory.indexes)
            .chain(&inventory.packs)
            .chain(
                require_derived
                    .then_some(&inventory.derived)
                    .into_iter()
                    .flatten(),
            )
        {
            resolve_store_artifact(&root, relative)?;
        }
        let derived_segments = if require_derived {
            inventory
                .derived
                .iter()
                .map(|path| resolve_store_artifact(&root, path))
                .collect::<Result<_>>()?
        } else {
            inventory
                .derived
                .iter()
                .filter_map(|path| resolve_store_artifact(&root, path).ok())
                .collect()
        };
        Ok(Store {
            files_segments: inventory
                .files
                .iter()
                .map(|path| resolve_store_artifact(&root, path))
                .collect::<Result<_>>()?,
            dirs_segments: inventory
                .dirs
                .iter()
                .map(|path| resolve_store_artifact(&root, path))
                .collect::<Result<_>>()?,
            excluded_segments: inventory
                .excluded
                .iter()
                .map(|path| resolve_store_artifact(&root, path))
                .collect::<Result<_>>()?,
            index_segments: inventory
                .indexes
                .iter()
                .map(|path| resolve_store_artifact(&root, path))
                .collect::<Result<_>>()?,
            derived_segments,
            pack_ids,
            root,
            manifest,
            index: OnceLock::new(),
            _lock: lock,
        })
    }

    pub fn packs_dir(&self) -> PathBuf {
        self.root.join("packs")
    }

    pub fn files_segments(&self) -> &[PathBuf] {
        &self.files_segments
    }

    pub fn dirs_segments(&self) -> &[PathBuf] {
        &self.dirs_segments
    }

    pub fn excluded_segments(&self) -> &[PathBuf] {
        &self.excluded_segments
    }

    pub fn index_segments(&self) -> &[PathBuf] {
        &self.index_segments
    }

    pub fn derived_segments(&self) -> &[PathBuf] {
        &self.derived_segments
    }

    pub fn reader(&self) -> PackReader {
        PackReader::new(&self.packs_dir())
    }

    /// sha → parts (sorted by part). Loaded once.
    pub fn index(&self) -> Result<&HashMap<Sha, Vec<Loc>>> {
        if let Some(m) = self.index.get() {
            return Ok(m);
        }
        let mut m: HashMap<Sha, Vec<Loc>> = HashMap::new();
        for seg in &self.index_segments {
            let mut undeclared_pack = None;
            catalog::read_index(seg, |sha, loc| {
                if !self.pack_ids.contains(&loc.pack) {
                    undeclared_pack = Some(loc.pack);
                }
                m.entry(sha).or_default().push(loc);
            })?;
            if let Some(pack) = undeclared_pack {
                bail!(
                    "index {} references undeclared pack {}",
                    seg.display(),
                    crate::pack::pack_name(pack)
                );
            }
        }
        for v in m.values_mut() {
            v.sort_by_key(|l| l.part);
        }
        let _ = self.index.set(m);
        Ok(self.index.get().unwrap())
    }

    /// All rows below `dir` (the root when empty), newest batch winning for duplicate paths, in path order.
    pub fn files_under(&self, dir: &str, with_attrs: bool) -> Result<Vec<FileRow>> {
        let range = subtree_range(dir);
        let mut m: BTreeMap<Vec<u8>, FileRow> = BTreeMap::new();
        let resolve_shadowing = self.manifest.batches.len() > 1;
        let mut v = Vec::new();
        for seg in &self.files_segments {
            catalog::scan_files(
                seg,
                range.as_ref().map(|(l, h)| (l.as_slice(), h.as_slice())),
                with_attrs,
                |r| {
                    if resolve_shadowing {
                        m.insert(r.path.as_bytes().to_vec(), r);
                    } else {
                        v.push(r);
                    }
                },
            )?;
        }
        if resolve_shadowing {
            Ok(m.into_values().collect())
        } else {
            Ok(v)
        }
    }

    /// Stream rows below `dir` whose path passes `pre`, newest batch winning for duplicate paths.
    /// Single-batch stores stream without buffering across physical segments;
    /// multi-batch stores buffer to resolve duplicates.
    pub fn scan_under(
        &self,
        dir: &str,
        with_attrs: bool,
        pre: impl Fn(&str) -> bool,
        mut f: impl FnMut(FileRow),
    ) -> Result<()> {
        let range = subtree_range(dir);
        let r = range.as_ref().map(|(l, h)| (l.as_slice(), h.as_slice()));
        if self.manifest.batches.len() <= 1 {
            for seg in &self.files_segments {
                catalog::scan_files_filtered(seg, r, with_attrs, &pre, &mut f)?;
            }
            return Ok(());
        }
        let mut m: BTreeMap<Vec<u8>, FileRow> = BTreeMap::new();
        for seg in &self.files_segments {
            catalog::scan_files_filtered(seg, r, with_attrs, &pre, |row| {
                m.insert(row.path.as_bytes().to_vec(), row);
            })?;
        }
        for row in m.into_values() {
            f(row);
        }
        Ok(())
    }

    /// Visit every row (all segments; duplicates across batches are both visited, newest last).
    pub fn for_each_file(&self, with_attrs: bool, mut f: impl FnMut(FileRow)) -> Result<()> {
        for seg in &self.files_segments {
            catalog::scan_files(seg, None, with_attrs, &mut f)?;
        }
        Ok(())
    }

    pub fn stat(&self, path: &str) -> Result<Option<FileRow>> {
        let lo = path.as_bytes().to_vec();
        let mut hi = lo.clone();
        hi.push(0);
        let mut found: Option<FileRow> = None;
        for seg in &self.files_segments {
            // only the matching row is materialised; the row group is located through the path statistics
            catalog::scan_files_filtered(
                seg,
                Some((&lo, &hi)),
                true,
                |p| p == path,
                |r| found = Some(r),
            )?;
        }
        Ok(found)
    }

    /// Direct children of `dir`: (subdirectories, files). Directory counts are summed over batches.
    pub fn children(&self, dir: &str) -> Result<(Vec<DirEntry>, Vec<FileRow>)> {
        let depth = if dir.is_empty() {
            0
        } else {
            dir.matches('/').count() as u16 + 1
        };
        let range = subtree_range(dir);
        let mut subs: BTreeMap<String, DirEntry> = BTreeMap::new();
        for seg in &self.dirs_segments {
            catalog::scan_dirs(
                seg,
                range.as_ref().map(|(l, h)| (l.as_slice(), h.as_slice())),
                |d| {
                    if d.depth == depth + 1 && d.parent() == dir {
                        let e = subs.entry(d.dir.clone()).or_insert_with(|| DirEntry {
                            name: d.name().to_string(),
                            dir: d.dir.clone(),
                            ..Default::default()
                        });
                        e.n_files += d.n_files;
                        e.n_dirs += d.n_dirs;
                        e.bytes += d.bytes;
                    }
                },
            )?;
        }
        let mut files: Vec<FileRow> = Vec::new();
        if self.manifest.batches.len() <= 1 {
            for seg in &self.files_segments {
                catalog::scan_direct_children(seg, dir, true, |r| files.push(r))?;
            }
        } else {
            let mut m: BTreeMap<Vec<u8>, FileRow> = BTreeMap::new();
            for seg in &self.files_segments {
                catalog::scan_direct_children(seg, dir, true, |r| {
                    m.insert(r.path.as_bytes().to_vec(), r);
                })?;
            }
            files = m.into_values().collect();
        }
        Ok((subs.into_values().collect(), files))
    }

    /// Aggregated info for one directory (root when empty).
    pub fn dir_info(&self, dir: &str) -> Result<Option<DirRow>> {
        let lo = dir.as_bytes().to_vec();
        let mut hi = lo.clone();
        hi.push(0);
        let mut acc: Option<DirRow> = None;
        for seg in &self.dirs_segments {
            catalog::scan_dirs(seg, Some((&lo, &hi)), |d| {
                if d.dir == dir {
                    let a = acc.get_or_insert_with(|| DirRow {
                        dir: d.dir.clone(),
                        depth: d.depth,
                        ..Default::default()
                    });
                    a.n_files += d.n_files;
                    a.n_dirs += d.n_dirs;
                    a.bytes += d.bytes;
                }
            })?;
        }
        Ok(acc)
    }

    /// Subdirectories below `dir` (all depths), summed over batches, in path order.
    pub fn dirs_under(&self, dir: &str) -> Result<Vec<DirRow>> {
        let range = subtree_range(dir);
        let mut m: BTreeMap<String, DirRow> = BTreeMap::new();
        for seg in &self.dirs_segments {
            catalog::scan_dirs(
                seg,
                range.as_ref().map(|(l, h)| (l.as_slice(), h.as_slice())),
                |d| {
                    let e = m.entry(d.dir.clone()).or_insert_with(|| DirRow {
                        dir: d.dir.clone(),
                        depth: d.depth,
                        ..Default::default()
                    });
                    e.n_files += d.n_files;
                    e.n_dirs += d.n_dirs;
                    e.bytes += d.bytes;
                },
            )?;
        }
        Ok(m.into_values().collect())
    }

    pub fn parts(&self, sha: &Sha) -> Result<&[Loc]> {
        match self.index()?.get(sha) {
            Some(v) => Ok(v),
            None => bail!("blob {} is not in the index", hex::encode(sha)),
        }
    }

    /// Bytes of a catalog row (symlink: its target; empty: empty). Verifies the sha when `verify`.
    pub fn read_row(
        &self,
        reader: &mut PackReader,
        row: &FileRow,
        verify: bool,
    ) -> Result<Vec<u8>> {
        if row.kind == Kind::Empty {
            return Ok(Vec::new());
        }
        let bytes = reader.blob(self.parts(&row.sha)?)?;
        if verify && sha_of_bytes(&bytes) != row.sha {
            bail!("{}: content does not match its recorded sha", row.path);
        }
        Ok(bytes)
    }

    pub fn read_sha(&self, reader: &mut PackReader, sha: &Sha) -> Result<Vec<u8>> {
        reader.blob(self.parts(sha)?)
    }

    /// Materialise everything below `prefix` (a file or a directory) into `dst`.
    pub fn extract(
        &self,
        prefix: &str,
        dst: &Path,
        hardlink_dedupe: bool,
        set_mtime: bool,
        verify: bool,
    ) -> Result<u64> {
        let rows = if let Some(r) = self.stat(prefix)? {
            vec![r]
        } else {
            let rows = self.files_under(prefix, false)?;
            if rows.is_empty() {
                bail!("{prefix}: no such file or directory in the store");
            }
            rows
        };
        let strip = if prefix.is_empty() {
            0
        } else {
            prefix.len() + 1
        };
        let mut reader = self.reader();
        // hard links share the inode, hence the mode: only identical (content, mode) pairs may be linked
        let mut first_path: HashMap<(Sha, u16), PathBuf> = HashMap::new();
        let mut n = 0u64;
        for r in &rows {
            let rel = if rows.len() == 1 && r.path == prefix {
                crate::basename_of(&r.path).to_string()
            } else {
                r.path[strip..].to_string()
            };
            let out = dst.join(&rel);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            match r.kind {
                Kind::Symlink => {
                    let target = self.read_row(&mut reader, r, verify)?;
                    let target =
                        String::from_utf8(target).context("symlink target is not UTF-8")?;
                    let _ = std::fs::remove_file(&out);
                    std::os::unix::fs::symlink(target, &out)?;
                }
                Kind::Empty => {
                    std::fs::File::create(&out)?;
                    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(r.mode as u32))?;
                }
                Kind::File => {
                    if hardlink_dedupe {
                        if let Some(src) = first_path.get(&(r.sha, r.mode)) {
                            let _ = std::fs::remove_file(&out);
                            std::fs::hard_link(src, &out)?;
                            n += 1;
                            continue;
                        }
                    }
                    let mut f = std::fs::File::create(&out)?;
                    if verify {
                        let bytes = self.read_row(&mut reader, r, true)?;
                        f.write_all(&bytes)?;
                    } else {
                        reader.copy_blob(self.parts(&r.sha)?, &mut f)?;
                    }
                    drop(f);
                    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(r.mode as u32))?;
                    if hardlink_dedupe {
                        first_path.insert((r.sha, r.mode), out.clone());
                    }
                }
            }
            if set_mtime && r.kind != Kind::Symlink {
                let t = std::time::UNIX_EPOCH
                    + std::time::Duration::from_nanos(r.mtime_ns.max(0) as u64);
                let f = std::fs::File::open(&out)?;
                f.set_modified(t)?;
            }
            n += 1;
        }
        Ok(n)
    }

    /// Consistency check; `deep` re-hashes every blob.
    pub fn verify(&self, deep: bool) -> Result<VerifyReport> {
        let mut rep = VerifyReport::default();
        let index = self.index()?;
        // packs present and long enough
        let mut pack_len: HashMap<u32, u64> = HashMap::new();
        for (sha, parts) in index {
            for (i, l) in parts.iter().enumerate() {
                if l.part as usize != i {
                    rep.bad_parts.push(format!(
                        "{}: part {} at position {}",
                        hex::encode(sha),
                        l.part,
                        i
                    ));
                }
                let len = match pack_len.get(&l.pack) {
                    Some(v) => *v,
                    None => {
                        let p = self.packs_dir().join(crate::pack::pack_name(l.pack));
                        let v = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        if v == 0 {
                            rep.missing_packs.push(crate::pack::pack_name(l.pack));
                        }
                        pack_len.insert(l.pack, v);
                        v
                    }
                };
                if len > 0 && (l.chunk_offset as u64 + l.chunk_len as u64) > len {
                    rep.bad_parts.push(format!(
                        "{}: frame beyond the end of pack {}",
                        hex::encode(sha),
                        l.pack
                    ));
                }
            }
        }
        rep.missing_packs.sort();
        rep.missing_packs.dedup();
        // every catalog sha has a location
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut shas_needed: HashSet<Sha> = HashSet::new();
        let mut total = 0u64;
        self.for_each_file(false, |r| {
            total += 1;
            if !seen.insert(r.path.as_bytes().to_vec()) {
                rep.shadowed_paths += 1;
            }
            if r.kind != Kind::Empty {
                if let Some(parts) = index.get(&r.sha) {
                    let s: i64 = parts.iter().map(|l| l.size).sum();
                    if s != r.size {
                        rep.bad_parts.push(format!(
                            "{}: parts sum to {} bytes, catalog says {}",
                            r.path, s, r.size
                        ));
                    }
                    shas_needed.insert(r.sha);
                } else {
                    rep.missing_blobs.push(r.path.clone());
                }
            }
        })?;
        rep.files = total;
        rep.blobs = index.len() as u64;
        if deep && rep.missing_packs.is_empty() {
            let mut reader = self.reader();
            let mut shas: Vec<&Sha> = index.keys().collect();
            shas.sort_by_key(|s| {
                let l = index[*s][0];
                (l.pack, l.chunk_offset, l.offset)
            });
            for sha in shas {
                match reader.blob(&index[sha]) {
                    Ok(bytes) => {
                        if sha_of_bytes(&bytes) != *sha {
                            rep.corrupt.push(hex::encode(sha));
                        }
                    }
                    Err(e) => rep.corrupt.push(format!("{}: {e:#}", hex::encode(sha))),
                }
            }
        }
        Ok(rep)
    }

    /// Paths (in catalog) for a given sha, for mapping grep hits back.
    pub fn paths_by_sha(
        &self,
        filter: impl Fn(&FileRow) -> bool,
    ) -> Result<HashMap<Sha, Vec<String>>> {
        let mut m: HashMap<Sha, Vec<String>> = HashMap::new();
        self.for_each_file(true, |r| {
            if r.kind != Kind::Empty && filter(&r) {
                m.entry(r.sha).or_default().push(r.path);
            }
        })?;
        Ok(m)
    }
}
