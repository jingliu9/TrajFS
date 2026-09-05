//! Read-only FUSE projection of one store, or of every store under `store_root` (docs/PLAN-fuse.md).
//!
//! The namespace comes from the catalog through `Store::children` (one listing per directory, cached), bytes come
//! from the packs through `Store::read_blob` (whole blob per open, cached by sha). Nothing is ever written.

pub mod fs;

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use trajfs_core::pack::PackReader;
use trajfs_core::{FileRow, Kind, Sha, Store};

/// How often a store's `MANIFEST.json` mtime is re-read (a new batch changes it).
const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// How often `store_root` is re-scanned for new or removed stores.
const RESCAN_EVERY: Duration = Duration::from_secs(5);
/// Blobs above this size (multi-part, docs/PLAN.md §3) bypass the blob cache.
const BLOB_CACHE_MAX_ITEM: usize = 64 << 20;
/// Estimated resident bytes per cached listing entry and per inode (docs/PLAN-fuse.md §5.1, measured §16).
const LISTING_ENTRY_BYTES: usize = 150;
const INODE_BYTES: usize = 300;
/// How often the memory budget is checked.
const TRIM_EVERY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EKind {
    Dir,
    File,
    Symlink,
    Empty,
}

/// One row of a directory listing: everything `getattr`/`lookup` need, without a per-path catalog scan.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub kind: EKind,
    pub mode: u16,
    pub size: i64,
    pub mtime_ns: i64,
    pub sha: Sha,
    /// Direct subdirectories (directories only).
    pub n_dirs: i64,
}

pub struct Listing {
    pub entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

impl Listing {
    fn new(entries: Vec<Entry>) -> Listing {
        let by_name = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.clone(), i))
            .collect();
        Listing { entries, by_name }
    }
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.by_name.get(name).map(|&i| &self.entries[i])
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub ttl: Duration,
    pub blob_cache_bytes: usize,
    /// Directory entries kept per store.
    pub listing_cache: usize,
    /// Best-effort total for blobs + listing entries + inodes (§5.1); 0 = unbounded.
    pub memory_bytes: usize,
    pub verify: bool,
    /// Log forgets and invalidations (TRAJ_MOUNT_DEBUG).
    pub debug: bool,
}

/// A directory whose subtree holds at most this many files is listed with one catalog scan for the whole
/// subtree (`files_under` + `dirs_under`, ~50 ms for a 106 K-file round) instead of one scan per directory
/// (~70 ms each; a round has 17 K directories). Bigger subtrees (a whole run) are listed one level at a time.
const PREFETCH_MAX_FILES: i64 = 250_000;

/// Directory listings of one store, bounded by entry count; the oldest listings go first. There is no
/// time-based expiry: a listing changes only with a new batch, and `StoreHandle::refresh` clears the cache then.
struct ListingCache {
    cap_entries: usize,
    total: usize,
    seq: u64,
    map: HashMap<String, (u64, Arc<Listing>)>,
}

impl ListingCache {
    fn get(&self, dir: &str) -> Option<Arc<Listing>> {
        self.map.get(dir).map(|(_, l)| l.clone())
    }
    fn insert(&mut self, dir: String, l: Arc<Listing>) {
        self.total += l.entries.len();
        if let Some((_, old)) = self.map.insert(dir, (self.seq, l)) {
            self.total -= old.entries.len();
        }
        self.seq += 1;
        if self.total > self.cap_entries.max(1) {
            let mut by_age: Vec<(u64, String)> =
                self.map.iter().map(|(k, (s, _))| (*s, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age {
                if self.total <= self.cap_entries / 2 {
                    break;
                }
                if let Some((_, l)) = self.map.remove(&k) {
                    self.total -= l.entries.len();
                }
            }
        }
    }
    fn clear(&mut self) {
        self.map.clear();
        self.total = 0;
    }
    /// Evict the oldest listings until `n` entries are gone (or the cache is empty); returns the entries freed.
    fn evict_entries(&mut self, n: usize) -> usize {
        let mut by_age: Vec<(u64, String)> =
            self.map.iter().map(|(k, (s, _))| (*s, k.clone())).collect();
        by_age.sort();
        let mut freed = 0;
        for (_, k) in by_age {
            if freed >= n {
                break;
            }
            if let Some((_, l)) = self.map.remove(&k) {
                self.total -= l.entries.len();
                freed += l.entries.len();
            }
        }
        freed
    }
}

fn file_entry(f: &FileRow) -> Entry {
    Entry {
        name: f.name().to_string(),
        kind: match f.kind {
            Kind::File => EKind::File,
            Kind::Symlink => EKind::Symlink,
            Kind::Empty => EKind::Empty,
        },
        mode: f.mode,
        size: f.size,
        mtime_ns: f.mtime_ns,
        sha: f.sha,
        n_dirs: 0,
    }
}

fn dir_entry(name: &str, n_dirs: i64) -> Entry {
    Entry {
        name: name.to_string(),
        kind: EKind::Dir,
        mode: 0o555,
        size: 4096,
        mtime_ns: 0,
        sha: [0u8; 32],
        n_dirs,
    }
}

/// One mounted store: the `Store` (swapped on a new batch), its pack reader and listing cache.
pub struct StoreHandle {
    pub id: String,
    pub path: PathBuf,
    store: RwLock<Arc<Store>>,
    reader: Mutex<PackReader>,
    /// (manifest mtime at open, when it was last checked)
    manifest: Mutex<(Option<SystemTime>, Instant)>,
    listings: Mutex<ListingCache>,
    /// Set when the store directory vanished from `store_root`.
    pub gone: AtomicBool,
}

fn manifest_mtime(store_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(store_dir.join("MANIFEST.json"))
        .and_then(|m| m.modified())
        .ok()
}

impl StoreHandle {
    pub fn open(id: &str, path: &Path, listing_cache: usize) -> Result<Arc<StoreHandle>> {
        // unlocked: a mount lives for hours and must not block `traj pack`; it tolerates appends (§7)
        let store =
            Store::open_unlocked(path).with_context(|| format!("open store {}", path.display()))?;
        store.index()?; // load the sha → location index once, up front
        Ok(Arc::new(StoreHandle {
            id: id.to_string(),
            path: path.to_path_buf(),
            reader: Mutex::new(store.reader()),
            manifest: Mutex::new((manifest_mtime(path), Instant::now())),
            store: RwLock::new(Arc::new(store)),
            listings: Mutex::new(ListingCache {
                cap_entries: listing_cache,
                total: 0,
                seq: 0,
                map: HashMap::new(),
            }),
            gone: AtomicBool::new(false),
        }))
    }

    pub fn store(&self) -> Arc<Store> {
        self.store.read().unwrap().clone()
    }

    /// The time of the newest batch, used as the mtime of every directory.
    pub fn batch_time(&self) -> SystemTime {
        self.manifest
            .lock()
            .unwrap()
            .0
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    /// (paths, kept bytes) over all batches, for statfs.
    pub fn totals(&self) -> (u64, u64) {
        let st = self.store();
        st.manifest
            .batches
            .iter()
            .fold((0, 0), |(p, b), x| (p + x.paths, b + x.bytes))
    }

    /// Reopen the store when a batch landed since the last check. Returns true when it did.
    pub fn refresh(&self) -> bool {
        let mut m = self.manifest.lock().unwrap();
        if m.1.elapsed() < REFRESH_EVERY {
            return false;
        }
        m.1 = Instant::now();
        let now = manifest_mtime(&self.path);
        if now == m.0 {
            return false;
        }
        match Store::open_unlocked(&self.path) {
            Ok(st) => {
                if st.index().is_err() {
                    return false;
                }
                *self.store.write().unwrap() = Arc::new(st);
                *self.reader.lock().unwrap() = PackReader::new(&self.path.join("packs"));
                self.listings.lock().unwrap().clear();
                m.0 = now;
                eprintln!(
                    "traj mount: {}: new batch, store reopened",
                    self.id_or_path()
                );
                true
            }
            Err(e) => {
                eprintln!("traj mount: {}: reopen failed: {e:#}", self.id_or_path());
                false
            }
        }
    }

    fn id_or_path(&self) -> String {
        if self.id.is_empty() {
            self.path.display().to_string()
        } else {
            self.id.clone()
        }
    }

    /// The listing of `dir` (the store root when empty). A miss costs one catalog scan: of the whole subtree when
    /// it holds at most `PREFETCH_MAX_FILES` files (every directory below is cached at once), else of this
    /// directory's direct children.
    pub fn listing(&self, dir: &str) -> Result<Arc<Listing>> {
        if let Some(l) = self.listings.lock().unwrap().get(dir) {
            return Ok(l);
        }
        let st = self.store();
        let Some(info) = st.dir_info(dir)? else {
            // not a directory of this store (or an empty store): nothing to list, nothing to cache
            return Ok(Arc::new(Listing::new(Vec::new())));
        };
        if info.n_files <= PREFETCH_MAX_FILES {
            let built = self.prefetch(&st, dir)?;
            let mut cache = self.listings.lock().unwrap();
            let mut mine = None;
            for (d, l) in built {
                if d == dir {
                    mine = Some(l.clone());
                }
                cache.insert(d, l);
            }
            return Ok(mine.unwrap_or_else(|| Arc::new(Listing::new(Vec::new()))));
        }
        let (dirs, files) = st.children(dir)?;
        let mut entries = Vec::with_capacity(dirs.len() + files.len());
        for d in dirs {
            entries.push(dir_entry(&d.name, d.n_dirs));
        }
        for f in &files {
            entries.push(file_entry(f));
        }
        let l = Arc::new(Listing::new(entries));
        self.listings
            .lock()
            .unwrap()
            .insert(dir.to_string(), l.clone());
        Ok(l)
    }

    /// Listings of `dir` and of every directory below it, from one `dirs_under` and one `files_under` scan.
    fn prefetch(&self, st: &Store, dir: &str) -> Result<Vec<(String, Arc<Listing>)>> {
        let mut m: BTreeMap<String, (Vec<Entry>, Vec<Entry>)> = BTreeMap::new();
        m.entry(dir.to_string()).or_default();
        for d in st.dirs_under(dir)? {
            if d.dir.is_empty() || d.dir == dir {
                continue;
            }
            m.entry(d.dir.clone()).or_default();
            m.entry(d.parent().to_string())
                .or_default()
                .0
                .push(dir_entry(d.name(), d.n_dirs));
        }
        for f in st.files_under(dir, false)? {
            m.entry(f.dir().to_string())
                .or_default()
                .1
                .push(file_entry(&f));
        }
        Ok(m.into_iter()
            .map(|(d, (mut dirs, files))| {
                dirs.extend(files);
                (d, Arc::new(Listing::new(dirs)))
            })
            .collect())
    }

    pub fn stat(&self, path: &str) -> Result<Option<FileRow>> {
        self.store().stat(path)
    }

    pub fn listing_entries(&self) -> usize {
        self.listings.lock().unwrap().total
    }

    pub fn evict_listing_entries(&self, n: usize) -> usize {
        self.listings.lock().unwrap().evict_entries(n)
    }

    pub fn blob(&self, sha: &Sha, verify: bool) -> Result<Vec<u8>> {
        let st = self.store();
        let mut r = self.reader.lock().unwrap();
        st.read_blob(&mut r, sha, verify)
    }
}

/// Attributes of a file or symlink inode at creation: what the inode keeps answering with once a later batch has
/// re-recorded its path with other content (the kernel may still hold the old inode open).
#[derive(Clone, Copy, Debug)]
pub struct Snap {
    pub kind: EKind,
    pub mode: u16,
    pub size: i64,
    pub mtime_ns: i64,
}

impl Snap {
    pub fn entry(&self, info: &InodeInfo) -> Entry {
        Entry {
            name: info.name.to_string(),
            kind: self.kind,
            mode: self.mode,
            size: self.size,
            mtime_ns: self.mtime_ns,
            sha: info.sha,
            n_dirs: 0,
        }
    }
}

/// One inode: how it was reached (parent, name), which store, and its content identity. The path inside the
/// store is rebuilt from the parent chain (`TrajFs::path_of`), so a full-run walk stays around 150 B per inode.
pub struct InodeInfo {
    pub ino: u64,
    pub parent: u64,
    pub name: Box<str>,
    /// Index into `TrajFs::stores`; `None` for the root of a multi-store mount.
    pub store: Option<usize>,
    /// Content identity; zero for directories and store roots. A changed sha means a new inode.
    pub sha: Sha,
    /// `None` for directories and roots.
    pub snap: Option<Snap>,
    /// Seconds since mount when this inode's attributes were last handed to the kernel.
    pub served: AtomicU64,
    /// Lookups the kernel holds on this inode (each `lookup` reply and `readdirplus` entry adds one; `forget`
    /// subtracts). Zero means the kernel does not know the inode number, so it may be dropped.
    pub nlookup: AtomicU64,
}

pub struct Inodes {
    by_ino: HashMap<u64, Arc<InodeInfo>>,
    by_key: HashMap<(u64, Box<str>), u64>,
    next: u64,
}

impl Inodes {
    pub fn get(&self, ino: u64) -> Option<Arc<InodeInfo>> {
        self.by_ino.get(&ino).cloned()
    }
    pub fn len(&self) -> usize {
        self.by_ino.len()
    }
    /// The kernel released `n` lookups on `ino`; drops the inode when none remain. Never the root.
    pub fn forget(&mut self, ino: u64, n: u64) -> bool {
        if ino == 1 {
            return false;
        }
        let Some(info) = self.by_ino.get(&ino) else {
            return false;
        };
        let left = info.nlookup.load(Ordering::Relaxed).saturating_sub(n);
        info.nlookup.store(left, Ordering::Relaxed);
        if left > 0 {
            return false;
        }
        self.remove(ino)
    }
    /// Drop an inode the kernel holds no lookups on (a plain-`readdir` entry it never looked up).
    pub fn remove_unreferenced(&mut self, ino: u64) -> bool {
        match self.by_ino.get(&ino) {
            Some(info) if ino != 1 && info.nlookup.load(Ordering::Relaxed) == 0 => self.remove(ino),
            _ => false,
        }
    }
    fn remove(&mut self, ino: u64) -> bool {
        let Some(info) = self.by_ino.remove(&ino) else {
            return false;
        };
        let key = (info.parent, info.name.clone());
        if self.by_key.get(&key) == Some(&ino) {
            self.by_key.remove(&key);
        }
        true
    }
    /// The inode for `name` under `parent`, allocated on first sight.
    pub fn intern(
        &mut self,
        parent: u64,
        name: &str,
        store: Option<usize>,
        sha: Sha,
        snap: Option<Snap>,
    ) -> Arc<InodeInfo> {
        let key = (parent, Box::<str>::from(name));
        if let Some(&ino) = self.by_key.get(&key) {
            let info = self.by_ino.get(&ino).unwrap();
            if info.store == store && info.sha == sha {
                return info.clone();
            }
            // same name, different content (a later batch) or a different store: a fresh inode, so the kernel's
            // page cache and any open handle of the old one stay what they were
        }
        let ino = self.next;
        self.next += 1;
        let info = Arc::new(InodeInfo {
            ino,
            parent,
            name: key.1.clone(),
            store,
            sha,
            snap,
            served: AtomicU64::new(0),
            nlookup: AtomicU64::new(0),
        });
        self.by_ino.insert(ino, info.clone());
        self.by_key.insert(key, ino);
        info
    }
}

/// Whole blobs by sha, bounded in bytes, FIFO eviction (content per sha never changes).
struct BlobCache {
    cap: usize,
    bytes: usize,
    map: HashMap<Sha, Arc<Vec<u8>>>,
    order: VecDeque<Sha>,
}

impl BlobCache {
    fn get(&self, sha: &Sha) -> Option<Arc<Vec<u8>>> {
        self.map.get(sha).cloned()
    }
    /// Evict oldest-first until `n` bytes are gone (or the cache is empty); returns the bytes freed.
    fn evict_bytes(&mut self, n: usize) -> usize {
        let mut freed = 0;
        while freed < n {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.map.remove(&old) {
                self.bytes -= v.len();
                freed += v.len();
            }
        }
        freed
    }
    fn insert(&mut self, sha: Sha, b: Arc<Vec<u8>>) {
        let n = b.len();
        if n > BLOB_CACHE_MAX_ITEM || n > self.cap / 4 || self.map.contains_key(&sha) {
            return;
        }
        while self.bytes + n > self.cap {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.map.remove(&old) {
                self.bytes -= v.len();
            }
        }
        self.bytes += n;
        self.map.insert(sha, b);
        self.order.push_back(sha);
    }
}

pub struct TrajFs {
    pub opts: Options,
    /// The root lists store ids (several `-S`, or `store_root`); store roots are then the children of ino 1.
    multi: bool,
    /// Scanned for new stores when set (multi mode without explicit `-S`).
    store_root: Option<PathBuf>,
    stores: RwLock<Vec<Arc<StoreHandle>>>,
    by_id: RwLock<HashMap<String, usize>>,
    root_scanned: Mutex<Instant>,
    inodes: RwLock<Inodes>,
    blobs: Mutex<BlobCache>,
    handles: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
    next_fh: AtomicU64,
    pub uid: u32,
    pub gid: u32,
    started: Instant,
    last_trim: Mutex<Instant>,
    /// (parent ino, name, ino, also the data cache) whose kernel caches must be dropped; consumed by the notifier
    /// thread. With `false` only the entry goes, which is how the memory budget asks the kernel to forget.
    inval: Mutex<Option<mpsc::Sender<(u64, String, u64, bool)>>>,
}

impl TrajFs {
    fn new(
        stores: Vec<Arc<StoreHandle>>,
        multi: bool,
        store_root: Option<PathBuf>,
        opts: Options,
        uid: u32,
        gid: u32,
    ) -> TrajFs {
        let mut inodes = Inodes {
            by_ino: HashMap::new(),
            by_key: HashMap::new(),
            next: 2,
        };
        inodes.by_ino.insert(
            1,
            Arc::new(InodeInfo {
                ino: 1,
                parent: 1,
                name: Box::from(""),
                store: if multi { None } else { Some(0) },
                sha: [0u8; 32],
                snap: None,
                served: AtomicU64::new(0),
                nlookup: AtomicU64::new(0),
            }),
        );
        let by_id = stores
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), i))
            .collect();
        TrajFs {
            blobs: Mutex::new(BlobCache {
                cap: opts.blob_cache_bytes,
                bytes: 0,
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            opts,
            multi,
            store_root,
            stores: RwLock::new(stores),
            by_id: RwLock::new(by_id),
            root_scanned: Mutex::new(Instant::now()),
            inodes: RwLock::new(inodes),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            uid,
            gid,
            started: Instant::now(),
            last_trim: Mutex::new(Instant::now()),
            inval: Mutex::new(None),
        }
    }

    /// One store: the mountpoint is its tree.
    pub fn single(store: Arc<StoreHandle>, opts: Options, uid: u32, gid: u32) -> TrajFs {
        Self::new(vec![store], false, None, opts, uid, gid)
    }

    /// Several stores: one directory per store id; rescanned when `store_root` is given.
    pub fn multi(
        stores: Vec<Arc<StoreHandle>>,
        store_root: Option<PathBuf>,
        opts: Options,
        uid: u32,
        gid: u32,
    ) -> TrajFs {
        Self::new(stores, true, store_root, opts, uid, gid)
    }

    pub fn set_invalidator(&self, tx: mpsc::Sender<(u64, String, u64, bool)>) {
        *self.inval.lock().unwrap() = Some(tx);
    }

    pub fn inode(&self, ino: u64) -> Option<Arc<InodeInfo>> {
        self.inodes.read().unwrap().get(ino)
    }

    pub fn intern(
        &self,
        parent: u64,
        name: &str,
        store: Option<usize>,
        sha: Sha,
        snap: Option<Snap>,
    ) -> Arc<InodeInfo> {
        self.inodes
            .write()
            .unwrap()
            .intern(parent, name, store, sha, snap)
    }

    /// The root of a store: ino 1 in single mode, a child of ino 1 in multi mode.
    pub fn is_store_root(&self, info: &InodeInfo) -> bool {
        if self.multi {
            info.parent == 1 && info.ino != 1
        } else {
            info.ino == 1
        }
    }

    /// Path of an inode inside its store (empty for a store root), rebuilt from the parent chain.
    pub fn path_of(&self, info: &InodeInfo) -> String {
        let inodes = self.inodes.read().unwrap();
        let mut parts: Vec<Arc<InodeInfo>> = Vec::new();
        let mut cur = inodes.get(info.ino);
        while let Some(c) = cur {
            if self.is_store_root(&c) || c.ino == 1 {
                break;
            }
            cur = inodes.get(c.parent);
            parts.push(c);
        }
        let mut s = String::with_capacity(parts.iter().map(|p| p.name.len() + 1).sum());
        for p in parts.iter().rev() {
            if !s.is_empty() {
                s.push('/');
            }
            s.push_str(&p.name);
        }
        s
    }

    pub fn store(&self, i: usize) -> Option<Arc<StoreHandle>> {
        self.stores.read().unwrap().get(i).cloned()
    }

    pub fn store_by_id(&self, id: &str) -> Option<(usize, Arc<StoreHandle>)> {
        let i = *self.by_id.read().unwrap().get(id)?;
        self.store(i).map(|s| (i, s))
    }

    /// Store ids in root order (gone ones skipped).
    pub fn store_ids(&self) -> Vec<(usize, String)> {
        self.stores
            .read()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.gone.load(Ordering::Relaxed))
            .map(|(i, s)| (i, s.id.clone()))
            .collect()
    }

    pub fn all_stores(&self) -> Vec<Arc<StoreHandle>> {
        self.stores.read().unwrap().clone()
    }

    /// Record that this inode's attributes were handed to the kernel now.
    pub fn mark_served(&self, info: &InodeInfo) {
        info.served
            .store(self.started.elapsed().as_secs() + 1, Ordering::Relaxed);
    }

    /// Reopen store `i` if a batch landed; drop the kernel's entries for what it may still cache.
    pub fn refresh_store(&self, i: usize, h: &StoreHandle) {
        if !h.refresh() {
            return;
        }
        let tx = self.inval.lock().unwrap().clone();
        let Some(tx) = tx else { return };
        let now = self.started.elapsed().as_secs() + 1;
        let horizon = now.saturating_sub(self.opts.ttl.as_secs() + 1);
        let inodes = self.inodes.read().unwrap();
        for info in inodes.by_ino.values() {
            if info.store == Some(i) && info.ino != 1 && !info.name.is_empty() {
                if info.served.load(Ordering::Relaxed) >= horizon {
                    let _ = tx.send((info.parent, info.name.to_string(), info.ino, true));
                }
            }
        }
    }

    /// Re-read `store_root` (multi mode): new stores are opened, vanished ones marked gone.
    pub fn rescan_root(&self) {
        let Some(root) = &self.store_root else { return };
        {
            let mut t = self.root_scanned.lock().unwrap();
            if t.elapsed() < RESCAN_EVERY {
                return;
            }
            *t = Instant::now();
        }
        let mut seen: HashMap<String, PathBuf> = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.join("MANIFEST.json").is_file() {
                    seen.insert(store_id_of(&p), p);
                }
            }
        }
        let mut stores = self.stores.write().unwrap();
        let mut by_id = self.by_id.write().unwrap();
        for s in stores.iter() {
            let present = seen.contains_key(&s.id);
            s.gone.store(!present, Ordering::Relaxed);
        }
        for (id, p) in seen {
            if by_id.contains_key(&id) {
                continue;
            }
            match StoreHandle::open(&id, &p, self.opts.listing_cache) {
                Ok(h) => {
                    by_id.insert(id.clone(), stores.len());
                    stores.push(h);
                    eprintln!("traj mount: new store {id}");
                }
                Err(e) => eprintln!("traj mount: {}: {e:#}", p.display()),
            }
        }
    }

    /// The kernel released `n` lookups on `ino` (§5.1).
    pub fn forget_inode(&self, ino: u64, n: u64) {
        self.inodes.write().unwrap().forget(ino, n);
    }

    /// Estimated memory of the caches: (blob bytes, listing entries over all stores, inodes).
    pub fn estimate(&self) -> (usize, usize, usize) {
        let blobs = self.blobs.lock().unwrap().bytes;
        let entries: usize = self.all_stores().iter().map(|h| h.listing_entries()).sum();
        let inodes = self.inodes.read().unwrap().len();
        (blobs, entries, inodes)
    }

    /// Keep the estimate under `--memory` (§5.1), at most once a second: blobs go first, then the oldest
    /// listings, then inodes: those the kernel never looked up are dropped, the rest are invalidated so the
    /// kernel forgets them when it can. Best effort: the kernel keeps what is open or recently used.
    pub fn trim(&self) {
        let budget = self.opts.memory_bytes;
        if budget == 0 {
            return;
        }
        {
            let mut t = self.last_trim.lock().unwrap();
            if t.elapsed() < TRIM_EVERY {
                return;
            }
            *t = Instant::now();
        }
        let (blob_b, list_e, inodes_n) = self.estimate();
        let est = blob_b + list_e * LISTING_ENTRY_BYTES + inodes_n * INODE_BYTES;
        if est <= budget {
            return;
        }
        let mut over = est - budget;
        let freed_blobs = self.blobs.lock().unwrap().evict_bytes(over);
        over = over.saturating_sub(freed_blobs);
        let mut freed_entries = 0;
        for h in self.all_stores() {
            if over == 0 {
                break;
            }
            let n = h.evict_listing_entries(over.div_ceil(LISTING_ENTRY_BYTES));
            freed_entries += n;
            over = over.saturating_sub(n * LISTING_ENTRY_BYTES);
        }
        let (mut dropped, mut nudged) = (0usize, 0usize);
        if over > 0 {
            let want = over.div_ceil(INODE_BYTES);
            let now = self.started.elapsed().as_secs() + 1;
            let horizon = now.saturating_sub(self.opts.ttl.as_secs() + 1);
            let mut to_drop = Vec::new();
            let mut to_nudge = Vec::new();
            {
                let inodes = self.inodes.read().unwrap();
                for info in inodes.by_ino.values() {
                    if to_drop.len() + to_nudge.len() >= want {
                        break;
                    }
                    if info.ino == 1 || (self.multi && info.parent == 1) {
                        continue;
                    }
                    if info.nlookup.load(Ordering::Relaxed) == 0 {
                        to_drop.push(info.ino);
                    } else if info.served.load(Ordering::Relaxed) < horizon {
                        to_nudge.push((info.parent, info.name.to_string(), info.ino, false));
                    }
                }
            }
            {
                let mut w = self.inodes.write().unwrap();
                for ino in to_drop {
                    if w.remove_unreferenced(ino) {
                        dropped += 1;
                    }
                }
            }
            if let Some(tx) = self.inval.lock().unwrap().as_ref() {
                for t in to_nudge {
                    if tx.send(t).is_ok() {
                        nudged += 1;
                    }
                }
            }
        }
        eprintln!(
            "traj mount: memory budget {} MB exceeded (estimate {} MB: {} MB of blobs, {} listing entries, {} inodes): freed {} MB of blobs and {} listing entries; dropped {} inodes, asked the kernel to forget {}",
            budget >> 20,
            est >> 20,
            blob_b >> 20,
            list_e,
            inodes_n,
            freed_blobs >> 20,
            freed_entries,
            dropped,
            nudged
        );
    }

    pub fn blob(&self, h: &StoreHandle, sha: &Sha) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.blobs.lock().unwrap().get(sha) {
            return Ok(b);
        }
        let b = Arc::new(h.blob(sha, self.opts.verify)?);
        self.blobs.lock().unwrap().insert(*sha, b.clone());
        Ok(b)
    }

    pub fn open_handle(&self, bytes: Arc<Vec<u8>>) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, bytes);
        fh
    }

    pub fn handle(&self, fh: u64) -> Option<Arc<Vec<u8>>> {
        self.handles.lock().unwrap().get(&fh).cloned()
    }

    pub fn close_handle(&self, fh: u64) {
        self.handles.lock().unwrap().remove(&fh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_drops_an_inode_only_when_the_kernel_holds_no_lookups() {
        let mut t = Inodes {
            by_ino: HashMap::new(),
            by_key: HashMap::new(),
            next: 2,
        };
        let a = t.intern(1, "a", Some(0), [1u8; 32], None);
        assert_eq!(a.ino, 2);
        a.nlookup.fetch_add(2, Ordering::Relaxed);
        assert!(!t.forget(2, 1), "one lookup left");
        assert!(t.get(2).is_some());
        assert!(!t.remove_unreferenced(2), "still referenced");
        assert!(t.forget(2, 1), "last lookup released");
        assert!(t.get(2).is_none());
        // re-interned after the drop: a new number, the old one is never reused
        let a2 = t.intern(1, "a", Some(0), [1u8; 32], None);
        assert_eq!(a2.ino, 3);
        assert_eq!(t.len(), 1);
        // an entry the kernel never looked up can go at once; the root never goes
        assert!(t.remove_unreferenced(3));
        assert!(!t.forget(1, 100));
        // a new sha under the same name is a new inode; forgetting the old one leaves the new key intact
        let b1 = t.intern(1, "b", Some(0), [1u8; 32], None);
        let b2 = t.intern(1, "b", Some(0), [2u8; 32], None);
        assert_ne!(b1.ino, b2.ino);
        b1.nlookup.fetch_add(1, Ordering::Relaxed);
        assert!(t.forget(b1.ino, 1));
        assert_eq!(t.intern(1, "b", Some(0), [2u8; 32], None).ino, b2.ino);
    }
}

/// `<store_root>/<id>.trajstore` → `id`; any other directory name is used as is.
pub fn store_id_of(p: &Path) -> String {
    let n = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    n.trim_end_matches(".trajstore").to_string()
}

/// `256M`, `1G`, `4096` → bytes.
pub fn parse_size(s: &str) -> Result<usize> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1usize << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1usize << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1usize << 30),
        _ => (s, 1usize),
    };
    let n: usize = num
        .trim()
        .parse()
        .with_context(|| format!("{s}: not a size (use e.g. 256M)"))?;
    Ok(n * mul)
}
