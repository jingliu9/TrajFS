//! `fuser::Filesystem` for `TrajFs` (docs/PLAN-fuse.md §4–§6). Every handler resolves an inode to its store and
//! its parent's listing; nothing here touches the catalog per path except `getxattr` (adapter attrs).

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

struct Item {
    ino: u64,
    kind: FileType,
    name: String,
    attr: FileAttr,
}

fn ns_to_time(ns: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns.max(0) as u64)
}

fn snap_of(e: &Entry) -> Option<Snap> {
    match e.kind {
        EKind::Dir => None,
        kind => Some(Snap {
            kind,
            mode: e.mode,
            size: e.size,
            mtime_ns: e.mtime_ns,
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
            .map(|s| s.batch_time())
            .max()
            .unwrap_or(UNIX_EPOCH)
    }

    /// The store, after a freshness check; ENOENT when the store index is gone.
    fn live_store(&self, i: usize) -> Result<Arc<StoreHandle>, Errno> {
        let Some(h) = self.store(i) else {
            return Err(Errno::ENOENT);
        };
        if h.gone.load(Ordering::Relaxed) {
            return Err(Errno::ENOENT);
        }
        self.refresh_store(i, &h);
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
            (Some(e), Some(snap)) if e.sha != info.sha => Ok(Node::Child(h, snap.entry(info))),
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
            ino: info.ino,
            kind: FileType::Directory,
            name: ".".into(),
            attr: self_attr,
        });
        let parent = self.inode(info.parent).unwrap_or_else(|| info.clone());
        let parent_attr = self.attr_of(&parent)?;
        items.push(Item {
            ino: parent.ino,
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
                        ino: child.ino,
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
                        ino: child.ino,
                        kind,
                        name: e.name.clone(),
                        attr: self.entry_attr(child.ino, e, &h),
                    });
                }
            }
        }
        Ok(items)
    }

    /// The catalog row of a file inode (for xattrs); `None` for directories and roots.
    fn row_of(
        &self,
        info: &InodeInfo,
    ) -> Result<Option<(Arc<StoreHandle>, trajfs_core::FileRow)>, Errno> {
        let h = match self.node(info)? {
            Node::Child(h, e) if e.kind != EKind::Dir => h,
            _ => return Ok(None),
        };
        let path = self.path_of(info);
        let row = h.stat(&path).map_err(|_| Errno::EIO)?;
        Ok(row.map(|r| (h, r)))
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
            Ok(Node::Child(h, e)) if e.kind == EKind::Symlink => {
                match h.blob(&e.sha, self.opts.verify) {
                    Ok(b) => reply.data(&b),
                    Err(err) => {
                        eprintln!("traj mount: {}: {err:#}", self.path_of(&info));
                        reply.error(Errno::EIO);
                    }
                }
            }
            Ok(_) => reply.error(Errno::EINVAL),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.trim();
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let bytes = match self.node(&info) {
            Ok(Node::Child(h, e)) => match e.kind {
                EKind::File => match self.blob(&h, &e.sha) {
                    Ok(b) => b,
                    Err(err) => {
                        eprintln!("traj mount: {}: {err:#}", self.path_of(&info));
                        reply.error(Errno::EIO);
                        return;
                    }
                },
                EKind::Empty => Arc::new(Vec::new()),
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
        match self.attr_of(&info) {
            Ok(a) if a.kind == FileType::Directory => {
                reply.opened(FileHandle(0), FopenFlags::empty())
            }
            Ok(_) => reply.error(Errno::ENOTDIR),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let items = match self.dir_items(&info) {
            Ok(v) => v,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        for (i, it) in items.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(it.ino), (i + 1) as u64, it.kind, &it.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let Some(info) = self.inode(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let items = match self.dir_items(&info) {
            Ok(v) => v,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let ttl = self.ttl();
        for (i, it) in items.iter().enumerate().skip(offset as usize) {
            if reply.add(
                INodeNo(it.ino),
                (i + 1) as u64,
                &it.name,
                &ttl,
                &it.attr,
                Generation(0),
            ) {
                break;
            }
            if let Some(child) = self.inode(it.ino) {
                self.mark_served(&child);
                if i >= 2 {
                    // the kernel counts one lookup per readdirplus entry, `.` and `..` excepted
                    child.nlookup.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let (paths, bytes) = self
            .all_stores()
            .iter()
            .map(|s| s.totals())
            .fold((0u64, 0u64), |(p, b), (x, y)| (p + x, b + y));
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
