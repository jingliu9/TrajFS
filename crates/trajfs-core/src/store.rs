//! Reading a store: catalog queries, blob access, extract, verify (docs/PLAN.md §5).

use crate::catalog::{self, DirRow};
use crate::hash::sha_of_bytes;
use crate::manifest::{resolve_store_artifact, Manifest};
use crate::pack::{Loc, PackReader};
use crate::{FileRow, Kind, Sha};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

static EXTRACT_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

fn exact_range(path: &str) -> (Vec<u8>, Vec<u8>) {
    let lo = path.as_bytes().to_vec();
    let mut hi = lo.clone();
    hi.push(0);
    (lo, hi)
}

fn insert_latest(files: &mut BTreeMap<Vec<u8>, FileRow>, row: FileRow) {
    // Keep absent historical paths, but never expose a path as both a file and
    // a directory after a newer batch changes its type.
    let mut parent = row.dir();
    while !parent.is_empty() {
        files.remove(parent.as_bytes());
        parent = crate::parent_of(parent);
    }
    if let Some((lo, hi)) = subtree_range(&row.path) {
        let descendants: Vec<_> = files.range(lo..hi).map(|(path, _)| path.clone()).collect();
        for path in descendants {
            files.remove(&path);
        }
    }
    files.insert(row.path.as_bytes().to_vec(), row);
}

fn open_at(parent: &File, name: &CStr, flags: i32, mode: u32) -> std::io::Result<File> {
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn at_result(result: i32) -> std::io::Result<()> {
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn extract_directory(parent: &File, name: &CStr, create: bool) -> Result<File> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    match open_at(parent, name, flags, 0) {
        Ok(file) => return Ok(file),
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("extraction directory is not a real directory"),
    }
    let result = at_result(unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) });
    if let Err(error) = result {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    Ok(open_at(parent, name, flags, 0)?)
}

fn extract_root(path: &Path) -> Result<File> {
    let mut parent = File::open(if path.is_absolute() { "/" } else { "." })?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => CString::new(name.as_bytes())?,
            Component::ParentDir => CString::new("..")?,
            Component::Prefix(_) => bail!("unsupported extraction destination {}", path.display()),
        };
        parent = extract_directory(&parent, &name, true)
            .with_context(|| format!("extraction destination {}", path.display()))?;
    }
    Ok(parent)
}

fn extract_subdir(root: &File, path: &str, create: bool) -> Result<File> {
    let mut parent = root.try_clone()?;
    if !path.is_empty() {
        for part in path.split('/') {
            parent = extract_directory(&parent, &CString::new(part)?, create)?;
        }
    }
    Ok(parent)
}

// Hold the parent directory open so path replacement cannot redirect an
// extraction, and publish each completed entry without following its old leaf.
struct ExtractEntry<'a> {
    parent: &'a File,
    name: CString,
    published: bool,
}

impl<'a> ExtractEntry<'a> {
    fn create<T>(
        parent: &'a File,
        create: impl Fn(&CStr) -> std::io::Result<T>,
    ) -> Result<(Self, T)> {
        for _ in 0..128 {
            let id = EXTRACT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(".trajfs-extract-{}-{id}", std::process::id()))?;
            match create(&name) {
                Ok(value) => {
                    return Ok((
                        Self {
                            parent,
                            name,
                            published: false,
                        },
                        value,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not create an exclusive extraction entry")
    }

    fn publish(mut self, name: &CStr) -> Result<()> {
        at_result(unsafe {
            libc::renameat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                self.parent.as_raw_fd(),
                name.as_ptr(),
            )
        })?;
        self.published = true;
        Ok(())
    }
}

impl Drop for ExtractEntry<'_> {
    fn drop(&mut self) {
        if !self.published {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
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
    let root = root.canonicalize()?;
    let root = root.as_path();
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

    /// Open without the shared reader lock: for a long-lived reader such as `traj mount`, which would otherwise
    /// block every `pack` for as long as it runs. Safe because a store only grows (packs and segments are
    /// append-only and the manifest is published last); such a reader re-opens when the manifest changes and
    /// keeps its current view if the new one does not validate.
    pub fn open_unlocked(root: &Path) -> Result<Store> {
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

    /// Visible rows below `dir` (the root when empty), in path order. Newer
    /// batches replace duplicate paths and incompatible ancestor/descendant entries.
    pub fn files_under(&self, dir: &str, with_attrs: bool) -> Result<Vec<FileRow>> {
        catalog::validate_catalog_path(dir, true)?;
        let range = subtree_range(dir);
        let mut m: BTreeMap<Vec<u8>, FileRow> = BTreeMap::new();
        let resolve_shadowing = self.manifest.batches.len() > 1;
        let mut v = Vec::new();
        for seg in &self.files_segments {
            if resolve_shadowing {
                let mut ancestor = dir;
                while !ancestor.is_empty() {
                    let (lo, hi) = exact_range(ancestor);
                    catalog::scan_files_filtered(
                        seg,
                        Some((&lo, &hi)),
                        false,
                        |p| p == ancestor,
                        |_| m.clear(),
                    )?;
                    ancestor = crate::parent_of(ancestor);
                }
            }
            catalog::scan_files(
                seg,
                range.as_ref().map(|(l, h)| (l.as_slice(), h.as_slice())),
                with_attrs,
                |r| {
                    if resolve_shadowing {
                        insert_latest(&mut m, r);
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
        catalog::validate_catalog_path(dir, true)?;
        let range = subtree_range(dir);
        let r = range.as_ref().map(|(l, h)| (l.as_slice(), h.as_slice()));
        if self.manifest.batches.len() <= 1 {
            for seg in &self.files_segments {
                catalog::scan_files_filtered(seg, r, with_attrs, &pre, &mut f)?;
            }
            return Ok(());
        }
        for row in self.files_under(dir, with_attrs)? {
            if pre(&row.path) {
                f(row);
            }
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
        catalog::validate_catalog_path(path, true)?;
        if path.is_empty() {
            return Ok(None);
        }
        let Some(found) = self.latest_exact_file(path)? else {
            return Ok(None);
        };
        if self.manifest.batches.len() > 1 {
            let mut ancestor = crate::parent_of(path);
            while !ancestor.is_empty() {
                if self
                    .latest_exact_file(ancestor)?
                    .is_some_and(|row| row.batch > found.batch)
                {
                    return Ok(None);
                }
                ancestor = crate::parent_of(ancestor);
            }
            let (lo, hi) = exact_range(path);
            let mut became_directory = false;
            for seg in &self.dirs_segments {
                catalog::scan_dirs(seg, Some((&lo, &hi)), |dir| {
                    became_directory |= dir.dir == path && dir.batch > found.batch;
                })?;
            }
            if became_directory {
                return Ok(None);
            }
        }
        Ok(Some(found))
    }

    fn latest_exact_file(&self, path: &str) -> Result<Option<FileRow>> {
        let (lo, hi) = exact_range(path);
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

    /// Direct children of `dir`: (subdirectories, files), with counts from the visible namespace.
    pub fn children(&self, dir: &str) -> Result<(Vec<DirEntry>, Vec<FileRow>)> {
        catalog::validate_catalog_path(dir, true)?;
        if self.manifest.batches.len() > 1 {
            let rows = self.files_under(dir, true)?;
            let dirs = catalog::dirs_from_files(rows.iter(), 0)
                .into_iter()
                .filter(|d| !d.dir.is_empty() && d.parent() == dir)
                .map(|d| DirEntry {
                    name: d.name().to_owned(),
                    dir: d.dir,
                    n_files: d.n_files,
                    n_dirs: d.n_dirs,
                    bytes: d.bytes,
                })
                .collect();
            let files = rows.into_iter().filter(|r| r.dir() == dir).collect();
            return Ok((dirs, files));
        }
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
        for seg in &self.files_segments {
            catalog::scan_direct_children(seg, dir, true, |r| files.push(r))?;
        }
        Ok((subs.into_values().collect(), files))
    }

    /// Aggregated info for one directory (root when empty).
    pub fn dir_info(&self, dir: &str) -> Result<Option<DirRow>> {
        catalog::validate_catalog_path(dir, true)?;
        if self.manifest.batches.len() > 1 {
            let rows = self.files_under(dir, false)?;
            return Ok(catalog::dirs_from_files(rows.iter(), 0)
                .into_iter()
                .find(|row| row.dir == dir));
        }
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

    /// Subdirectories below `dir` (all depths), with visible counts, in path order.
    pub fn dirs_under(&self, dir: &str) -> Result<Vec<DirRow>> {
        catalog::validate_catalog_path(dir, true)?;
        let range = subtree_range(dir);
        if self.manifest.batches.len() > 1 {
            let rows = self.files_under(dir, false)?;
            return Ok(catalog::dirs_from_files(rows.iter(), 0)
                .into_iter()
                .filter(|row| {
                    range.as_ref().is_none_or(|(lo, hi)| {
                        row.dir.as_bytes() >= lo.as_slice() && row.dir.as_bytes() < hi.as_slice()
                    })
                })
                .collect());
        }
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
        if *sha == sha_of_bytes(b"") {
            return Ok(&[]);
        }
        match self.index()?.get(sha) {
            Some(v) => {
                if v.iter().enumerate().any(|(i, loc)| loc.part as usize != i) {
                    bail!("blob {} has missing or duplicate parts", hex::encode(sha));
                }
                Ok(v)
            }
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
            if row.size != 0 || row.sha != sha_of_bytes(b"") {
                bail!("{}: invalid empty-file metadata", row.path);
            }
            return Ok(Vec::new());
        }
        let bytes = reader.blob(self.parts(&row.sha)?)?;
        if row.size < 0 || bytes.len() as u64 != row.size as u64 {
            bail!(
                "{}: content length does not match its recorded size",
                row.path
            );
        }
        if verify && sha_of_bytes(&bytes) != row.sha {
            bail!("{}: content does not match its recorded sha", row.path);
        }
        Ok(bytes)
    }

    pub fn read_sha(&self, reader: &mut PackReader, sha: &Sha) -> Result<Vec<u8>> {
        reader.blob(self.parts(sha)?)
    }

    /// Bytes of a blob by sha; verifies the content against the sha when `verify` (used by `traj mount`).
    pub fn read_blob(&self, reader: &mut PackReader, sha: &Sha, verify: bool) -> Result<Vec<u8>> {
        let bytes = reader.blob(self.parts(sha)?)?;
        if verify && sha_of_bytes(&bytes) != *sha {
            bail!("blob {}: content does not match its sha", hex::encode(sha));
        }
        Ok(bytes)
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
            if rows.is_empty() && !prefix.is_empty() {
                bail!("{prefix}: no such file or directory in the store");
            }
            rows
        };
        let destination = extract_root(dst)?;
        let mut reader = self.reader();
        // Hard links share both mode and mtime.
        let mut first_path: HashMap<(Sha, u16, Option<i64>), String> = HashMap::new();
        let mut previous_dir = String::new();
        let mut parent = destination.try_clone()?;
        let mut n = 0u64;
        for r in &rows {
            let rel = if rows.len() == 1 && r.path == prefix {
                crate::basename_of(&r.path)
            } else if prefix.is_empty() {
                &r.path
            } else {
                r.path
                    .strip_prefix(prefix)
                    .and_then(|path| path.strip_prefix('/'))
                    .context("catalog row is outside the extraction prefix")?
            };
            catalog::validate_catalog_path(rel, false)?;
            let dir = crate::parent_of(rel);
            if dir != previous_dir {
                parent = extract_subdir(&destination, dir, true)?;
                previous_dir = dir.to_owned();
            }
            let name = CString::new(crate::basename_of(rel))?;
            let key = (r.sha, r.mode, set_mtime.then_some(r.mtime_ns));
            match r.kind {
                Kind::Symlink => {
                    let target = self.read_row(&mut reader, r, verify)?;
                    let target = CString::new(target).context("symlink target contains NUL")?;
                    let (entry, ()) = ExtractEntry::create(&parent, |temporary| {
                        at_result(unsafe {
                            libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), temporary.as_ptr())
                        })
                    })?;
                    entry.publish(&name)?;
                }
                Kind::File | Kind::Empty => {
                    if hardlink_dedupe && r.kind == Kind::File {
                        if let Some(src) = first_path.get(&key) {
                            let src_parent =
                                extract_subdir(&destination, crate::parent_of(src), false)?;
                            let src_name = CString::new(crate::basename_of(src))?;
                            let (entry, ()) = ExtractEntry::create(&parent, |temporary| {
                                at_result(unsafe {
                                    libc::linkat(
                                        src_parent.as_raw_fd(),
                                        src_name.as_ptr(),
                                        parent.as_raw_fd(),
                                        temporary.as_ptr(),
                                        0,
                                    )
                                })
                            })?;
                            entry.publish(&name)?;
                            n += 1;
                            continue;
                        }
                    }
                    let (entry, mut f) = ExtractEntry::create(&parent, |temporary| {
                        open_at(
                            &parent,
                            temporary,
                            libc::O_WRONLY
                                | libc::O_CREAT
                                | libc::O_EXCL
                                | libc::O_NOFOLLOW
                                | libc::O_CLOEXEC,
                            0o600,
                        )
                    })?;
                    if verify || r.kind == Kind::Empty {
                        let bytes = self.read_row(&mut reader, r, verify)?;
                        f.write_all(&bytes)?;
                    } else {
                        let size = reader.copy_blob(self.parts(&r.sha)?, &mut f)?;
                        if r.size < 0 || size != r.size as u64 {
                            bail!("{}: extracted size does not match the catalog", r.path);
                        }
                    }
                    f.set_permissions(std::fs::Permissions::from_mode(r.mode as u32))?;
                    if set_mtime {
                        let duration = std::time::Duration::from_nanos(r.mtime_ns.unsigned_abs());
                        let time = if r.mtime_ns < 0 {
                            std::time::UNIX_EPOCH.checked_sub(duration)
                        } else {
                            std::time::UNIX_EPOCH.checked_add(duration)
                        }
                        .context("catalog modification time is out of range")?;
                        f.set_modified(time)?;
                    }
                    drop(f);
                    entry.publish(&name)?;
                    if hardlink_dedupe && r.kind == Kind::File {
                        first_path.insert(key, rel.to_owned());
                    }
                }
            }
            n += 1;
        }
        Ok(n)
    }

    /// Validate published catalogs and blob locations. `deep` also reads
    /// auxiliary Parquet pages and re-hashes every blob.
    pub fn verify(&self, deep: bool) -> Result<VerifyReport> {
        let mut rep = VerifyReport::default();
        for segment in &self.excluded_segments {
            catalog::verify_excluded(segment, deep)
                .with_context(|| format!("verify {}", segment.display()))?;
        }
        for segment in &self.derived_segments {
            catalog::verify_parquet(segment, deep)
                .with_context(|| format!("verify {}", segment.display()))?;
        }
        let index = self.index()?;
        // packs present and long enough
        let mut pack_len: HashMap<u32, u64> = HashMap::new();
        for pack in &self.pack_ids {
            let path = self.packs_dir().join(crate::pack::pack_name(*pack));
            let mut file = open_regular_file(&self.root, &path)?;
            pack_len.insert(*pack, file.metadata()?.len());
            let mut magic = [0u8; crate::pack::MAGIC.len()];
            if file.read_exact(&mut magic).is_err() || magic != *crate::pack::MAGIC {
                rep.corrupt
                    .push(format!("{}: invalid pack header", path.display()));
            }
        }
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
        let mut seen: HashMap<Vec<u8>, u32> = HashMap::new();
        let mut total = 0u64;
        let mut batches: Vec<_> = self.manifest.batches.iter().collect();
        batches.sort_by_key(|batch| batch.id);
        let mut first_segment = 0;
        for batch in batches {
            let segment_count = batch.segments.len().max(1);
            let mut paths = 0u64;
            let mut bytes = 0u128;
            let mut dirs = catalog::DirAccumulator::new(batch.id);
            let mut previous_path: Option<String> = None;
            for segment in &self.files_segments[first_segment..first_segment + segment_count] {
                catalog::scan_files(segment, None, deep, |r| {
                    paths += 1;
                    bytes += r.size as u128;
                    if r.batch != batch.id {
                        rep.corrupt.push(format!(
                            "{}: catalog batch {} differs from manifest batch {}",
                            r.path, r.batch, batch.id
                        ));
                    }
                    if previous_path.as_ref().is_some_and(|path| path >= &r.path) {
                        rep.corrupt.push(format!(
                            "batch {}: catalog paths are duplicated or out of order at {}",
                            batch.id, r.path
                        ));
                    }
                    previous_path = Some(r.path.clone());
                    if seen.insert(r.path.as_bytes().to_vec(), batch.id).is_some() {
                        rep.shadowed_paths += 1;
                    }
                    let mut ancestor = r.dir();
                    while !ancestor.is_empty() {
                        if seen.get(ancestor.as_bytes()) == Some(&batch.id) {
                            rep.corrupt.push(format!(
                                "batch {}: {} is both a file and a directory",
                                batch.id, ancestor
                            ));
                        }
                        ancestor = crate::parent_of(ancestor);
                    }
                    dirs.push(&r);
                    if r.kind != Kind::Empty {
                        if let Some(parts) = index.get(&r.sha) {
                            let s: i64 = parts.iter().map(|l| l.size).sum();
                            if s != r.size {
                                rep.bad_parts.push(format!(
                                    "{}: parts sum to {} bytes, catalog says {}",
                                    r.path, s, r.size
                                ));
                            }
                        } else {
                            rep.missing_blobs.push(r.path.clone());
                        }
                    }
                })?;
            }
            total += paths;
            if paths != batch.paths || bytes != batch.bytes as u128 {
                rep.corrupt.push(format!(
                    "batch {}: catalog has {paths} paths / {bytes} bytes, manifest says {} / {}",
                    batch.id, batch.paths, batch.bytes
                ));
            }
            let mut expected_dirs = dirs.into_rows();
            for segment in &self.dirs_segments[first_segment..first_segment + segment_count] {
                catalog::scan_dirs(segment, None, |dir| {
                    if expected_dirs.remove(&dir.dir).as_ref() != Some(&dir) {
                        rep.corrupt.push(format!(
                            "batch {}: directory {:?} does not match its file rows",
                            batch.id, dir.dir
                        ));
                    }
                })
                .with_context(|| format!("verify {}", segment.display()))?;
            }
            for missing in expected_dirs.keys() {
                rep.corrupt.push(format!(
                    "batch {}: missing directory row {missing:?}",
                    batch.id
                ));
            }
            first_segment += segment_count;
        }
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
        self.scan_under(
            "",
            true,
            |_| true,
            |r| {
                if r.kind != Kind::Empty && filter(&r) {
                    m.entry(r.sha).or_default().push(r.path);
                }
            },
        )?;
        Ok(m)
    }
}
