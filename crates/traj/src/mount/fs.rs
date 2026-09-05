//! `fuser::Filesystem` for `TrajFs` (docs/PLAN-fuse.md §4–§6). Lookups resolve through parent listings;
//! open directories pin their entries and offsets. Only xattrs scan individual catalog rows.

use super::{EKind, Entry, InodeInfo, Snap, StoreHandle, TrajFs};
use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const XATTR_SHA: &str = "user.traj.sha256";
const XATTR_BATCH: &str = "user.traj.batch";
const XATTR_ATTR_PREFIX: &str = "user.traj.attr.";

/// What an inode resolves to.
enum Node {
    /// The root of a multi-store mount.
    Root,
    /// The root of one store.
    StoreRoot(Arc<StoreHandle>),
    /// A row of the parent's listing.
    Child(Arc<StoreHandle>, Entry),
}

pub(super) struct Item {
    info: Arc<InodeInfo>,
    kind: FileType,
    name: String,
    attr: FileAttr,
}

fn ns_to_time(ns: i64) -> SystemTime {
    if ns < 0 {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    } else {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    }
}

fn snap_of(e: &Entry) -> Option<Snap> {
    match e.kind {
        EKind::Dir => None,
        kind => Some(Snap {
            kind,
            mode: e.mode,
            size: e.size,
            mtime_ns: e.mtime_ns,
            batch: e.batch,
        }),
    }
}

impl TrajFs {
    fn ttl(&self) -> Duration {
        self.opts.ttl
    }

    fn dir_attr(&self, ino: u64, mtime: SystemTime, nlink: u32) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: 4096,
            blocks: 8,
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: FileType::Directory,
            perm: 0o555,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn entry_attr(&self, ino: u64, e: &Entry, h: &StoreHandle) -> FileAttr {
        match e.kind {
            EKind::Dir => self.dir_attr(ino, h.batch_time(), 2 + e.n_dirs.max(0) as u32),
            _ => {
                let t = ns_to_time(e.mtime_ns);
                let (kind, perm) = match e.kind {
                    EKind::Symlink => (FileType::Symlink, 0o777),
                    _ => (FileType::RegularFile, (e.mode & 0o7777) & 0o555),
                };
                let size = e.size.max(0) as u64;
                FileAttr {
                    ino: INodeNo(ino),
                    size,
                    blocks: size.div_ceil(512),
                    atime: t,
                    mtime: t,
                    ctime: t,
                    crtime: t,
                    kind,
                    perm,
                    nlink: 1,
                    uid: self.uid,
                    gid: self.gid,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                }
            }
        }
    }

    fn root_mtime(&self) -> SystemTime {
        self.all_stores()
            .iter()
            .filter(|s| !s.gone.load(Ordering::Relaxed))
            .map(|s| s.batch_time())
            .max()
            .unwrap_or(UNIX_EPOCH)
    }

    fn filesystem_totals(&self) -> (u64, u64) {
        self.all_stores()
            .iter()
            .filter(|s| !s.gone.load(Ordering::Relaxed))
            .map(|s| s.totals())
            .fold((0u64, 0u64), |(p, b), (x, y)| (p + x, b + y))
    }

    /// The store, after a freshness check; ENOENT when the store index is gone.
    fn live_store(&self, i: usize) -> Result<Arc<StoreHandle>, Errno> {
        let Some(h) = self.store(i) else {
            return Err(Errno::ENOENT);
        };
        if h.gone.load(Ordering::Relaxed) {
            return Err(Errno::ENOENT);
        }
        self.refresh_store(i, &h).map_err(|e| {
            eprintln!("traj mount: {}: {e:#}", h.path.display());
            Errno::EIO
        })?;
        Ok(h)
    }

    fn node(&self, info: &InodeInfo) -> Result<Node, Errno> {
        let Some(i) = info.store else {
            return Ok(Node::Root);
        };
        let h = self.live_store(i)?;
        if self.is_store_root(info) {
            return Ok(Node::StoreRoot(h));
        }
        let parent_path = self
            .inode(info.parent)
            .map(|p| self.path_of(&p))
            .unwrap_or_default();
        let listing = h.listing(&parent_path).map_err(|e| {
            eprintln!("traj mount: {}/{}: {e:#}", parent_path, info.name);
            Errno::EIO
        })?;
        match (listing.get(&info.name), &info.snap) {
            // the path was re-recorded with other content by a later batch: this inode keeps what it was
            (Some(e), Some(snap)) if e.sha != info.sha || snap_of(e) != info.snap => {
                Ok(Node::Child(h, snap.entry(info)))
            }
            (Some(e), None) if e.kind != EKind::Dir => Err(Errno::ENOENT),
            (Some(e), _) => Ok(Node::Child(h, e.clone())),
            (None, Some(snap)) => Ok(Node::Child(h, snap.entry(info))),
            (None, None) => Err(Errno::ENOENT),
        }
    }

    fn attr_of(&self, info: &InodeInfo) -> Result<FileAttr, Errno> {
        let a = match self.node(info)? {
            Node::Root => self.dir_attr(info.ino, self.root_mtime(), 2),
            Node::StoreRoot(h) => self.dir_attr(info.ino, h.batch_time(), 2),
            Node::Child(h, e) => self.entry_attr(info.ino, &e, &h),
        };
        self.mark_served(info);
        Ok(a)
    }

    /// `.`, `..` and the children of a directory inode, with attributes.
    fn dir_items(&self, info: &Arc<InodeInfo>) -> Result<Vec<Item>, Errno> {
        self.trim();
        let mut items = Vec::new();
        let self_attr = self.attr_of(info)?;
        if self_attr.kind != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        items.push(Item {
            info: info.clone(),
            kind: FileType::Directory,
            name: ".".into(),
            attr: self_attr,
        });
        let parent = self.inode(info.parent).unwrap_or_else(|| info.clone());
        let parent_attr = self.attr_of(&parent)?;
        items.push(Item {
            info: parent.clone(),
            kind: FileType::Directory,
            name: "..".into(),
            attr: parent_attr,
        });
        match info.store {
            None => {
                self.rescan_root();
                for (i, id) in self.store_ids() {
                    let Some(h) = self.store(i) else { continue };
                    let child = self.intern(1, &id, Some(i), [0u8; 32], None);
                    items.push(Item {
                        info: child.clone(),
                        kind: FileType::Directory,
                        name: id,
                        attr: self.dir_attr(child.ino, h.batch_time(), 2),
                    });
                }
            }
            Some(i) => {
                let h = self.live_store(i)?;
                let path = self.path_of(info);
                let listing = h.listing(&path).map_err(|e| {
                    eprintln!("traj mount: {path}: {e:#}");
                    Errno::EIO
                })?;
                for e in &listing.entries {
                    let child = self.intern(info.ino, &e.name, Some(i), e.sha, snap_of(e));
                    let kind = match e.kind {
                        EKind::Dir => FileType::Directory,
                        EKind::Symlink => FileType::Symlink,
                        _ => FileType::RegularFile,
                    };
                    items.push(Item {
                        info: child.clone(),
                        kind,
                        name: e.name.clone(),
                        attr: self.entry_attr(child.ino, e, &h),
                    });
                }
            }
        }
        Ok(items)
    }

    fn open_dir(&self, info: &Arc<InodeInfo>) -> Result<u64, Errno> {
        let items = Arc::new(self.dir_items(info)?);
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.dir_handles.lock().unwrap().insert(fh, items);
        Ok(fh)
    }

    fn dir_handle(&self, fh: u64) -> Option<Arc<Vec<Item>>> {
        self.dir_handles.lock().unwrap().get(&fh).cloned()
    }

    fn entry_bytes(&self, h: &StoreHandle, entry: &Entry) -> anyhow::Result<Arc<Vec<u8>>> {
        let bytes = if entry.kind == EKind::Empty {
            Arc::new(Vec::new())
        } else {
            self.blob(h, &entry.sha)?
        };
        anyhow::ensure!(
            entry.size >= 0 && bytes.len() as u64 == entry.size as u64,
            "{}: content length does not match its recorded size",
            entry.name
        );
        Ok(bytes)
    }

    /// The catalog row of a file inode (for xattrs); `None` for directories and roots.
    fn row_of(
        &self,
        info: &InodeInfo,
    ) -> Result<Option<(Arc<StoreHandle>, trajfs_core::FileRow)>, Errno> {
        let (h, entry) = match self.node(info)? {
            Node::Child(h, e) if e.kind != EKind::Dir => (h, e),
            _ => return Ok(None),
        };
        let path = self.path_of(info);
        let row = h
            .stat_version(&path, entry.batch)
            .map_err(|_| Errno::EIO)?
            .filter(|row| row.sha == entry.sha)
            .ok_or(Errno::EIO)?;
        Ok(Some((h, row)))
    }

    fn xattr_names(&self, info: &InodeInfo) -> Result<Vec<String>, Errno> {
        let Some((_, row)) = self.row_of(info)? else {
            return Ok(Vec::new());
        };
        let mut names = vec![XATTR_SHA.to_string(), XATTR_BATCH.to_string()];
        for (k, _) in &row.attrs {
            names.push(format!("{XATTR_ATTR_PREFIX}{k}"));
        }
        Ok(names)
    }

    fn xattr_value(&self, info: &InodeInfo, name: &str) -> Result<Vec<u8>, Errno> {
        let Some((_, row)) = self.row_of(info)? else {
            return Err(Errno::NO_XATTR);
        };
        if name == XATTR_SHA {
            return Ok(hex::encode(row.sha).into_bytes());
        }
        if name == XATTR_BATCH {
            return Ok(row.batch.to_string().into_bytes());
        }
        if let Some(k) = name.strip_prefix(XATTR_ATTR_PREFIX) {
            if let Some((_, v)) = row.attrs.iter().find(|(kk, _)| kk == k) {
                return Ok(v.clone().into_bytes());
            }
        }
        Err(Errno::NO_XATTR)
    }
}

fn reply_xattr(reply: ReplyXattr, size: u32, value: &[u8]) {
    if size == 0 {
        reply.size(value.len() as u32);
    } else if value.len() as u32 > size {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(value);
    }
}

impl Filesystem for TrajFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // data cache dropped when mtime changes (a path re-recorded by a batch); readdirplus when the kernel can
        let _ = config.add_capabilities(fuser::InitFlags::FUSE_AUTO_INVAL_DATA);
        let _ = config.add_capabilities(fuser::InitFlags::FUSE_DO_READDIRPLUS);
        Ok(())
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        if self.opts.debug {
            eprintln!("traj mount: forget({}, {nlookup})", ino.0);
        }
        self.forget_inode(ino.0, nlookup);
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.trim();
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(pinfo) = self.inode(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let ttl = self.ttl();
        match pinfo.store {
            None => {
                self.rescan_root();
                match self.store_by_id(name) {
                    Some((i, h)) if !h.gone.load(Ordering::Relaxed) => {
                        let info = self.intern(1, name, Some(i), [0u8; 32], None);
                        self.mark_served(&info);
                        info.nlookup.fetch_add(1, Ordering::Relaxed);
                        reply.entry(
                            &ttl,
                            &self.dir_attr(info.ino, h.batch_time(), 2),
                            Generation(0),
                        );
                    }
                    _ => reply.error(Errno::ENOENT),
                }
            }
            Some(i) => {
                let h = match self.live_store(i) {
                    Ok(h) => h,
                    Err(e) => {
                        reply.error(e);
                        return;
                    }
                };
                let ppath = self.path_of(&pinfo);
                let listing = match h.listing(&ppath) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("traj mount: {ppath}: {e:#}");
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                let Some(e) = listing.get(name) else {
                    reply.error(Errno::ENOENT);
                    return;
                };
                let info = self.intern(parent.0, name, Some(i), e.sha, snap_of(e));
                self.mark_served(&info);
                info.nlookup.fetch_add(1, Ordering::Relaxed);
                reply.entry(&ttl, &self.entry_attr(info.ino, e, &h), Generation(0));
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.attr_of(&info) {
            Ok(a) => reply.attr(&self.ttl(), &a),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.node(&info) {
            Ok(Node::Child(h, e)) if e.kind == EKind::Symlink => match self.entry_bytes(&h, &e) {
                Ok(b) => reply.data(&b),
                Err(err) => {
                    eprintln!("traj mount: {}: {err:#}", self.path_of(&info));
                    reply.error(Errno::EIO);
                }
            },
            Ok(_) => reply.error(Errno::EINVAL),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.trim();
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let bytes = match self.node(&info) {
            Ok(Node::Child(h, e)) => match e.kind {
                EKind::File | EKind::Empty => match self.entry_bytes(&h, &e) {
                    Ok(b) => b,
                    Err(err) => {
                        eprintln!("traj mount: {}: {err:#}", self.path_of(&info));
                        reply.error(Errno::EIO);
                        return;
                    }
                },
                EKind::Dir => {
                    reply.error(Errno::EISDIR);
                    return;
                }
                EKind::Symlink => {
                    reply.error(Errno::EINVAL);
                    return;
                }
            },
            Ok(_) => {
                reply.error(Errno::EISDIR);
                return;
            }
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let fh = self.open_handle(bytes);
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(b) = self.handle(fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let start = (offset as usize).min(b.len());
        let end = (start + size as usize).min(b.len());
        reply.data(&b[start..end]);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.close_handle(fh.0);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.open_dir(&info) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(items) = self.dir_handle(fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        for (i, it) in items.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(it.info.ino), (i + 1) as u64, it.kind, &it.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let Some(items) = self.dir_handle(fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let ttl = self.ttl();
        for (i, it) in items.iter().enumerate().skip(offset as usize) {
            if reply.add(
                INodeNo(it.info.ino),
                (i + 1) as u64,
                &it.name,
                &ttl,
                &it.attr,
                Generation(0),
            ) {
                break;
            }
            self.mark_served(&it.info);
            if i >= 2 {
                // the kernel counts one lookup per readdirplus entry, `.` and `..` excepted
                it.info.nlookup.fetch_add(1, Ordering::Relaxed);
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.dir_handles.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let (paths, bytes) = self.filesystem_totals();
        reply.statfs(bytes.div_ceil(4096), 0, 0, paths, 0, 4096, 255, 4096);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::NO_XATTR);
            return;
        };
        match self.xattr_value(&info, name) {
            Ok(v) => reply_xattr(reply, size, &v),
            Err(e) => reply.error(e),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.xattr_names(&info) {
            Ok(names) => {
                let mut buf = Vec::new();
                for n in names {
                    buf.extend_from_slice(n.as_bytes());
                    buf.push(0);
                }
                reply_xattr(reply, size, &buf);
            }
            Err(e) => reply.error(e),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::tests::{options, refresh_due, Fixture};

    #[test]
    fn review_negative_mtime_is_preserved() {
        assert_eq!(ns_to_time(-1), UNIX_EPOCH - Duration::from_nanos(1));
    }

    #[test]
    fn review_kind_change_with_identical_bytes_gets_a_new_inode() {
        let fs = TrajFs::multi(Vec::new(), None, options(), 0, 0);
        let mut snap = Snap {
            kind: EKind::File,
            mode: 0o644,
            size: 5,
            mtime_ns: 0,
            batch: 1,
        };
        let file = fs.intern(1, "same", Some(0), [1; 32], Some(snap));
        snap.kind = EKind::Symlink;
        let link = fs.intern(1, "same", Some(0), [1; 32], Some(snap));
        assert_ne!(file.ino, link.ino);
    }

    #[test]
    fn review_old_inode_xattrs_describe_its_original_contents() {
        let fixture = Fixture::new(&[("file", b"original")]);
        let handle = fixture.handle();
        let fs = TrajFs::single(handle.clone(), options(), 0, 0);
        let root = fs.inode(1).unwrap();
        let items = fs.dir_items(&root).unwrap();
        let info = items
            .iter()
            .find(|item| item.name == "file")
            .unwrap()
            .info
            .clone();
        let old_sha = fs.xattr_value(&info, XATTR_SHA).unwrap();
        let old_batch = fs.xattr_value(&info, XATTR_BATCH).unwrap();
        std::fs::write(fixture.src.join("file"), b"replacement contents").unwrap();
        fixture.pack();
        refresh_due(&handle);
        fs.live_store(0).unwrap();
        assert_eq!(fs.xattr_value(&info, XATTR_SHA).unwrap(), old_sha);
        assert_eq!(fs.xattr_value(&info, XATTR_BATCH).unwrap(), old_batch);
    }

    #[test]
    fn review_failed_manifest_refresh_returns_an_error_until_recovery() {
        let fixture = Fixture::new(&[("file", b"bytes")]);
        let handle = fixture.handle();
        let fs = TrajFs::single(handle.clone(), options(), 0, 0);
        let manifest = fixture.store.join("MANIFEST.json");
        let original = std::fs::read(&manifest).unwrap();
        std::fs::write(&manifest, b"invalid manifest").unwrap();
        refresh_due(&handle);
        assert!(fs.live_store(0).is_err());
        assert!(fs.live_store(0).is_err());
        std::fs::write(&manifest, original).unwrap();
        refresh_due(&handle);
        assert!(fs.live_store(0).is_ok());
    }

    #[test]
    fn review_directory_handles_keep_offsets_stable_across_append() {
        let fixture = Fixture::new(&[("b", b"second"), ("c", b"third")]);
        let handle = fixture.handle();
        let fs = TrajFs::single(handle.clone(), options(), 0, 0);
        let root = fs.inode(1).unwrap();
        let fh = fs.open_dir(&root).unwrap();
        std::fs::write(fixture.src.join("a"), b"first").unwrap();
        fixture.pack();
        refresh_due(&handle);
        fs.live_store(0).unwrap();
        let old = fs.dir_handle(fh).unwrap();
        let tail: Vec<_> = old
            .iter()
            .skip(3)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(tail, ["c"]);
        let fresh = fs.dir_items(&root).unwrap();
        let names: Vec<_> = fresh
            .iter()
            .skip(2)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
        for entry in old.iter() {
            assert!(!fs
                .inodes
                .write()
                .unwrap()
                .remove_unreferenced(entry.info.ino));
        }
        fs.dir_handles.lock().unwrap().remove(&fh);
        assert!(fs.dir_handle(fh).is_none());
    }

    #[test]
    fn review_mount_checks_catalog_size_even_for_a_cached_blob() {
        let fixture = Fixture::new(&[("file", b"bytes"), ("empty", b"")]);
        let handle = fixture.handle();
        let fs = TrajFs::single(handle.clone(), options(), 0, 0);
        let listing = handle.listing("").unwrap();
        let mut entry = listing.get("file").unwrap().clone();
        assert_eq!(
            fs.entry_bytes(&handle, &entry).unwrap().as_slice(),
            b"bytes"
        );
        entry.size += 1;
        assert!(fs.entry_bytes(&handle, &entry).is_err());
        assert!(fs
            .entry_bytes(&handle, listing.get("empty").unwrap())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn review_removed_stores_are_excluded_from_filesystem_statistics() {
        let first = Fixture::new(&[("a", b"a")]);
        let second = Fixture::new(&[("b", b"bb")]);
        let gone = first.handle();
        let remaining = StoreHandle::open("other", &second.store, 100).unwrap();
        let fs = TrajFs::multi(vec![gone.clone(), remaining.clone()], None, options(), 0, 0);
        gone.gone.store(true, Ordering::Relaxed);
        assert_eq!(fs.filesystem_totals(), remaining.totals());
        assert_eq!(fs.store_ids(), [(1, "other".into())]);
    }
}
