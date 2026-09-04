//! Pack files: `TRAJPACK\x01` magic followed by independent zstd frames ("chunks").
//! `packs/index-B.parquet` maps sha → (pack, chunk_offset, chunk_len, offset, size, part).

use crate::{Sha, CHUNK_BYTES, PACK_SEAL_BYTES};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 9] = b"TRAJPACK\x01";
pub const ZSTD_LEVEL: i32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Loc {
    pub pack: u32,
    pub chunk_offset: i64,
    pub chunk_len: i32,
    pub offset: i32,
    pub size: i64,
    pub part: u16,
}

#[derive(Clone, Debug)]
pub struct IndexRow {
    pub sha: Sha,
    pub loc: Loc,
}

pub fn pack_name(id: u32) -> String {
    format!("{id:04}.pack")
}

/// One item to store: (sha, part number, bytes of that part).
struct Item {
    sha: Sha,
    part: u16,
    bytes: Vec<u8>,
}

/// Builds chunks from blobs, compresses chunks in parallel, appends them to packs sealed at `PACK_SEAL_BYTES`.
pub struct PackWriter {
    packs_dir: PathBuf,
    next_pack: u32,
    current: Option<(u32, File, u64)>,
    pending: Vec<Item>,
    pending_bytes: usize,
    pub rows: Vec<IndexRow>,
    pub packs_written: Vec<u32>,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl PackWriter {
    pub fn new(packs_dir: &Path, first_pack_id: u32) -> Result<Self> {
        std::fs::create_dir_all(packs_dir)?;
        Ok(Self {
            packs_dir: packs_dir.to_path_buf(),
            next_pack: first_pack_id,
            current: None,
            pending: Vec::new(),
            pending_bytes: 0,
            rows: Vec::new(),
            packs_written: Vec::new(),
            bytes_in: 0,
            bytes_out: 0,
        })
    }

    /// Add a whole blob (already in memory).
    pub fn add_bytes(&mut self, sha: Sha, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() <= CHUNK_BYTES {
            self.push_item(Item { sha, part: 0, bytes: bytes.to_vec() })?;
        } else {
            for (i, part) in bytes.chunks(CHUNK_BYTES).enumerate() {
                self.push_item(Item { sha, part: i as u16, bytes: part.to_vec() })?;
            }
        }
        Ok(())
    }

    /// Add a blob by streaming a file in CHUNK_BYTES parts (memory stays bounded for large files).
    pub fn add_file(&mut self, sha: Sha, path: &Path) -> Result<()> {
        let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut part = 0u16;
        loop {
            let mut buf = vec![0u8; CHUNK_BYTES];
            let mut n = 0;
            while n < CHUNK_BYTES {
                let r = f.read(&mut buf[n..])?;
                if r == 0 {
                    break;
                }
                n += r;
            }
            if n == 0 {
                break;
            }
            buf.truncate(n);
            self.push_item(Item { sha, part, bytes: buf })?;
            part = part.checked_add(1).context("blob has more than 65535 parts")?;
            if n < CHUNK_BYTES {
                break;
            }
        }
        Ok(())
    }

    fn push_item(&mut self, item: Item) -> Result<()> {
        if self.pending_bytes + item.bytes.len() > CHUNK_BYTES && !self.pending.is_empty() {
            self.flush_pending()?;
        }
        self.pending_bytes += item.bytes.len();
        self.pending.push(item);
        if self.pending_bytes >= CHUNK_BYTES {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Compress the pending chunk and write it. Parallelism comes from `flush_many` below when many chunks are
    /// queued; a single chunk is compressed inline.
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let chunk = build_chunk(&items);
        let frame = zstd::bulk::compress(&chunk.raw, ZSTD_LEVEL).context("zstd compress")?;
        self.write_frame(&chunk, &frame)
    }

    fn write_frame(&mut self, chunk: &Chunk, frame: &[u8]) -> Result<()> {
        let (pack_id, chunk_offset, written) = {
            let (id, file, written) = self.open_current()?;
            let off = *written as i64;
            file.write_all(frame)?;
            *written += frame.len() as u64;
            (id, off, *written)
        };
        self.bytes_in += chunk.raw.len() as u64;
        self.bytes_out += frame.len() as u64;
        for (sha, part, offset, size) in &chunk.members {
            self.rows.push(IndexRow {
                sha: *sha,
                loc: Loc {
                    pack: pack_id,
                    chunk_offset,
                    chunk_len: frame.len() as i32,
                    offset: *offset as i32,
                    size: *size as i64,
                    part: *part,
                },
            });
        }
        if written >= PACK_SEAL_BYTES {
            self.seal()?;
        }
        Ok(())
    }

    fn open_current(&mut self) -> Result<(u32, &mut File, &mut u64)> {
        if self.current.is_none() {
            let id = self.next_pack;
            self.next_pack += 1;
            let tmp = self.packs_dir.join(format!("{}.tmp", pack_name(id)));
            let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(MAGIC)?;
            self.current = Some((id, f, MAGIC.len() as u64));
        }
        let (id, f, w) = self.current.as_mut().unwrap();
        Ok((*id, f, w))
    }

    fn seal(&mut self) -> Result<()> {
        if let Some((id, f, _)) = self.current.take() {
            f.sync_all()?;
            drop(f);
            let tmp = self.packs_dir.join(format!("{}.tmp", pack_name(id)));
            std::fs::rename(&tmp, self.packs_dir.join(pack_name(id)))?;
            self.packs_written.push(id);
        }
        Ok(())
    }

    /// Compress many whole blobs in parallel: groups them into chunks first, compresses the chunks with rayon, then
    /// writes sequentially. Blobs larger than CHUNK_BYTES are streamed through `add_file` instead.
    pub fn add_many_parallel(&mut self, blobs: &[(Sha, PathBuf, u64)]) -> Result<()> {
        // group into chunks by cumulative size, in the given order
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut cur: Vec<usize> = Vec::new();
        let mut acc = 0u64;
        for (i, (_, _, size)) in blobs.iter().enumerate() {
            if acc + size > CHUNK_BYTES as u64 && !cur.is_empty() {
                groups.push(std::mem::take(&mut cur));
                acc = 0;
            }
            cur.push(i);
            acc += size;
        }
        if !cur.is_empty() {
            groups.push(cur);
        }
        // read + compress in parallel, in batches to bound memory
        for batch in groups.chunks(256) {
            let compressed: Vec<Result<(Chunk, Vec<u8>)>> = batch
                .par_iter()
                .map(|g| {
                    let mut items = Vec::with_capacity(g.len());
                    for &i in g {
                        let (sha, path, _) = &blobs[i];
                        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
                        items.push(Item { sha: *sha, part: 0, bytes });
                    }
                    let chunk = build_chunk(&items);
                    let frame = zstd::bulk::compress(&chunk.raw, ZSTD_LEVEL)?;
                    Ok((chunk, frame))
                })
                .collect();
            for r in compressed {
                let (chunk, frame) = r?;
                self.write_frame(&chunk, &frame)?;
            }
        }
        Ok(())
    }

    /// Flush pending data, seal the open pack, and return index rows.
    pub fn finish(mut self) -> Result<(Vec<IndexRow>, Vec<u32>, u64, u64)> {
        self.flush_pending()?;
        self.seal()?;
        Ok((self.rows, self.packs_written, self.bytes_in, self.bytes_out))
    }
}

struct Chunk {
    raw: Vec<u8>,
    /// (sha, part, offset, size)
    members: Vec<(Sha, u16, usize, usize)>,
}

fn build_chunk(items: &[Item]) -> Chunk {
    let total: usize = items.iter().map(|i| i.bytes.len()).sum();
    let mut raw = Vec::with_capacity(total);
    let mut members = Vec::with_capacity(items.len());
    for it in items {
        members.push((it.sha, it.part, raw.len(), it.bytes.len()));
        raw.extend_from_slice(&it.bytes);
    }
    Chunk { raw, members }
}

/// Reads blobs back. Caches the most recently decompressed frames (locality: neighbours share a chunk).
pub struct PackReader {
    packs_dir: PathBuf,
    files: HashMap<u32, File>,
    cache: Vec<((u32, i64), Vec<u8>)>,
    cache_max: usize,
}

impl PackReader {
    pub fn new(packs_dir: &Path) -> Self {
        Self { packs_dir: packs_dir.to_path_buf(), files: HashMap::new(), cache: Vec::new(), cache_max: 8 }
    }

    fn file(&mut self, pack: u32) -> Result<&File> {
        if !self.files.contains_key(&pack) {
            let p = self.packs_dir.join(pack_name(pack));
            let mut f = File::open(&p).with_context(|| format!("open pack {}", p.display()))?;
            let mut magic = [0u8; 9];
            f.seek(SeekFrom::Start(0))?;
            f.read_exact(&mut magic).with_context(|| format!("pack {} is too short", p.display()))?;
            if &magic != MAGIC {
                bail!("pack {} has a bad magic", p.display());
            }
            self.files.insert(pack, f);
        }
        Ok(self.files.get(&pack).unwrap())
    }

    /// Decompressed frame at (pack, chunk_offset).
    pub fn frame(&mut self, pack: u32, chunk_offset: i64, chunk_len: i32) -> Result<&[u8]> {
        let key = (pack, chunk_offset);
        if let Some(pos) = self.cache.iter().position(|(k, _)| *k == key) {
            let entry = self.cache.remove(pos);
            self.cache.push(entry);
            return Ok(&self.cache.last().unwrap().1);
        }
        let mut buf = vec![0u8; chunk_len as usize];
        self.file(pack)?.read_exact_at(&mut buf, chunk_offset as u64).with_context(|| {
            format!("read frame at {}+{} of pack {}", chunk_offset, chunk_len, pack)
        })?;
        let raw = zstd::bulk::decompress(&buf, CHUNK_BYTES * 2 + 1024)
            .with_context(|| format!("decompress frame at {} of pack {}", chunk_offset, pack))?;
        if self.cache.len() >= self.cache_max {
            self.cache.remove(0);
        }
        self.cache.push((key, raw));
        Ok(&self.cache.last().unwrap().1)
    }

    /// Whole blob from its (ordered) parts.
    pub fn blob(&mut self, parts: &[Loc]) -> Result<Vec<u8>> {
        let total: usize = parts.iter().map(|l| l.size as usize).sum();
        let mut out = Vec::with_capacity(total);
        for l in parts {
            let raw = self.frame(l.pack, l.chunk_offset, l.chunk_len)?;
            let start = l.offset as usize;
            let end = start + l.size as usize;
            if end > raw.len() {
                bail!("index points outside frame ({}+{} > {}) in pack {}", start, l.size, raw.len(), l.pack);
            }
            out.extend_from_slice(&raw[start..end]);
        }
        Ok(out)
    }

    /// Stream a blob into a writer.
    pub fn copy_blob(&mut self, parts: &[Loc], w: &mut dyn Write) -> Result<u64> {
        let mut n = 0u64;
        for l in parts {
            let raw = self.frame(l.pack, l.chunk_offset, l.chunk_len)?;
            let start = l.offset as usize;
            let end = start + l.size as usize;
            if end > raw.len() {
                bail!("index points outside frame in pack {}", l.pack);
            }
            w.write_all(&raw[start..end])?;
            n += l.size as u64;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha_of_bytes;

    #[test]
    fn roundtrip_small_and_large() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path(), 1).unwrap();
        let blobs: Vec<Vec<u8>> = vec![
            b"hello".to_vec(),
            vec![7u8; CHUNK_BYTES],
            vec![9u8; CHUNK_BYTES + 1],
            (0..3_000_000u32).map(|i| (i % 251) as u8).collect(),
            b"x".to_vec(),
        ];
        let shas: Vec<Sha> = blobs.iter().map(|b| sha_of_bytes(b)).collect();
        for (s, b) in shas.iter().zip(&blobs) {
            w.add_bytes(*s, b).unwrap();
        }
        let (rows, packs, bin, _bout) = w.finish().unwrap();
        assert_eq!(packs, vec![1]);
        assert_eq!(bin as usize, blobs.iter().map(|b| b.len()).sum::<usize>());
        let mut by_sha: HashMap<Sha, Vec<Loc>> = HashMap::new();
        for r in rows {
            by_sha.entry(r.sha).or_default().push(r.loc);
        }
        for v in by_sha.values_mut() {
            v.sort_by_key(|l| l.part);
        }
        assert_eq!(by_sha[&shas[2]].len(), 2);
        assert_eq!(by_sha[&shas[3]].len(), 3);
        let mut r = PackReader::new(dir.path());
        for (s, b) in shas.iter().zip(&blobs) {
            assert_eq!(&r.blob(&by_sha[s]).unwrap(), b);
        }
        // frames are plain zstd: decode the first one independently
        let l = by_sha[&shas[0]][0];
        let data = std::fs::read(dir.path().join(pack_name(1))).unwrap();
        assert_eq!(&data[..9], MAGIC);
        let raw = zstd::bulk::decompress(&data[l.chunk_offset as usize..(l.chunk_offset + l.chunk_len as i64) as usize], 1 << 22).unwrap();
        assert_eq!(&raw[l.offset as usize..l.offset as usize + 5], b"hello");
    }

    /// Deterministic incompressible bytes (xorshift), so pack sealing is reached with modest input.
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.truncate(n);
        v
    }

    #[test]
    fn t1_pack_seals_at_64mib_and_large_blobs_split_into_parts() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path(), 1).unwrap();
        // 100 MiB incompressible blob -> 100 parts, spanning two packs
        let big = noise(100 << 20, 7);
        let big_sha = sha_of_bytes(&big);
        w.add_bytes(big_sha, &big).unwrap();
        // exact boundaries
        for n in [CHUNK_BYTES, CHUNK_BYTES + 1, 0] {
            let b = noise(n, n as u64 + 1);
            w.add_bytes(sha_of_bytes(&b), &b).unwrap();
        }
        let (rows, packs, _, _) = w.finish().unwrap();
        assert_eq!(packs, vec![1, 2], "100 MiB of noise must seal pack 1 at 64 MiB and continue in pack 2");
        let len1 = std::fs::metadata(dir.path().join(pack_name(1))).unwrap().len();
        assert!(len1 >= PACK_SEAL_BYTES && len1 < PACK_SEAL_BYTES + (CHUNK_BYTES as u64) + 1024, "{len1}");
        let parts: Vec<&IndexRow> = rows.iter().filter(|r| r.sha == big_sha).collect();
        assert_eq!(parts.len(), 100);
        assert!(parts.iter().any(|r| r.loc.pack == 1) && parts.iter().any(|r| r.loc.pack == 2));
        // the 0-byte blob leaves no index row
        assert!(!rows.iter().any(|r| r.sha == sha_of_bytes(b"")));
        // read back
        let mut by: HashMap<Sha, Vec<Loc>> = HashMap::new();
        for r in &rows {
            by.entry(r.sha).or_default().push(r.loc);
        }
        for v in by.values_mut() {
            v.sort_by_key(|l| l.part);
        }
        let mut rd = PackReader::new(dir.path());
        assert_eq!(rd.blob(&by[&big_sha]).unwrap(), big);
        // every frame is a plain zstd frame: the zstd CLI decodes one cut out by offset
        let l = by[&big_sha][3];
        let data = std::fs::read(dir.path().join(pack_name(l.pack))).unwrap();
        let frame = &data[l.chunk_offset as usize..(l.chunk_offset + l.chunk_len as i64) as usize];
        let fpath = dir.path().join("frame.zst");
        std::fs::write(&fpath, frame).unwrap();
        if let Ok(out) = std::process::Command::new("zstd").arg("-dc").arg(&fpath).output() {
            assert!(out.status.success());
            assert_eq!(&out.stdout[l.offset as usize..l.offset as usize + l.size as usize], &big[3 * CHUNK_BYTES..4 * CHUNK_BYTES]);
        }
    }
}
