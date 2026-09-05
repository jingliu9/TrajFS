//! sha256 helpers.

use crate::Sha;
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub fn sha_of_bytes(b: &[u8]) -> Sha {
    let mut h = Sha256::new();
    h.update(b);
    h.finalize().into()
}

/// Streaming sha256 of a file's captured prefix; returns (sha, size).
pub fn sha_of_file(path: &Path) -> Result<(Sha, u64)> {
    let snapshot = FileSnapshot::capture(path)?;
    Ok((snapshot.sha, snapshot.size()))
}

/// A regular file's fixed-length prefix, bound to its inode and content hash.
/// No file descriptor or file contents are retained between hashing and packing.
#[derive(Clone, Debug)]
pub struct FileSnapshot {
    path: PathBuf,
    metadata: Metadata,
    pub sha: Sha,
}

impl FileSnapshot {
    pub fn capture(path: &Path) -> Result<Self> {
        let file = open_regular(path)?;
        let metadata = file.metadata()?;
        Self::capture_open_file(path, file, metadata)
    }

    fn capture_open_file(path: &Path, mut file: File, metadata: Metadata) -> Result<Self> {
        let mut hash = Sha256::new();
        read_prefix(&mut file, metadata.len(), crate::CHUNK_BYTES, |bytes| {
            hash.update(bytes);
            Ok(())
        })
        .with_context(|| format!("hash captured prefix of {}", path.display()))?;
        let snapshot = Self {
            path: path.to_path_buf(),
            metadata,
            sha: hash.finalize().into(),
        };
        snapshot.check_source(&file)?;
        Ok(snapshot)
    }

    pub fn size(&self) -> u64 {
        self.metadata.len()
    }

    pub fn mtime_ns(&self) -> i64 {
        self.metadata.mtime() * 1_000_000_000 + self.metadata.mtime_nsec()
    }

    pub fn mode(&self) -> u16 {
        if self.metadata.mode() & 0o111 != 0 {
            0o755
        } else {
            0o644
        }
    }

    /// Read exactly the captured prefix in bounded chunks, checking the hash of
    /// the bytes actually consumed. Appended bytes never enter the snapshot.
    pub(crate) fn read_chunks(
        &self,
        chunk_bytes: usize,
        mut consume: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut file = open_regular(&self.path)?;
        self.check_source(&file)?;
        let before = file.metadata()?;
        let mut hash = Sha256::new();
        read_prefix(&mut file, self.size(), chunk_bytes, |bytes| {
            hash.update(bytes);
            consume(bytes)
        })
        .with_context(|| format!("read captured prefix of {}", self.path.display()))?;
        let actual: Sha = hash.finalize().into();
        ensure!(
            actual == self.sha,
            "source prefix changed after hashing: {}",
            self.path.display()
        );
        check_metadata(&before, &file.metadata()?, &self.path)?;
        self.check_source(&file)
    }

    /// For small pack members and the existing whole-trajectory adapter API.
    pub(crate) fn read_bytes(&self) -> Result<Vec<u8>> {
        let size = usize::try_from(self.size()).context("snapshot is too large for memory")?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .context("allocate snapshot bytes")?;
        self.read_chunks(crate::CHUNK_BYTES, |part| {
            bytes.extend_from_slice(part);
            Ok(())
        })?;
        Ok(bytes)
    }

    pub(crate) fn verify(&self) -> Result<()> {
        self.read_chunks(crate::CHUNK_BYTES, |_| Ok(()))
    }

    fn check_source(&self, file: &File) -> Result<()> {
        check_metadata(&self.metadata, &file.metadata()?, &self.path)?;
        let named = std::fs::symlink_metadata(&self.path)
            .with_context(|| format!("source disappeared: {}", self.path.display()))?;
        check_metadata(&self.metadata, &named, &self.path)
    }
}

fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open source {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "source is not a regular file: {}",
        path.display()
    );
    Ok(file)
}

fn check_metadata(before: &Metadata, after: &Metadata, path: &Path) -> Result<()> {
    ensure!(
        after.is_file() && before.dev() == after.dev() && before.ino() == after.ino(),
        "source was replaced: {}",
        path.display()
    );
    ensure!(
        after.len() >= before.len(),
        "source was truncated: {}",
        path.display()
    );
    // Appending legitimately changes timestamps. For growth, the second-pass
    // prefix hash detects overwrites; with no growth, timestamp changes also
    // reject same-size rewrites and truncate/regrow cycles.
    ensure!(
        after.len() > before.len()
            || (
                before.mtime(),
                before.mtime_nsec(),
                before.ctime(),
                before.ctime_nsec()
            ) == (
                after.mtime(),
                after.mtime_nsec(),
                after.ctime(),
                after.ctime_nsec()
            ),
        "source changed without append-only growth: {}",
        path.display()
    );
    Ok(())
}

fn read_prefix(
    file: &mut File,
    size: u64,
    chunk_bytes: usize,
    mut consume: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    ensure!(chunk_bytes > 0, "snapshot read chunk must be nonzero");
    let mut remaining = size;
    let mut buffer = vec![0; chunk_bytes.min(size.try_into().unwrap_or(usize::MAX))];
    while remaining > 0 {
        let n = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..n])
            .context("source was truncated while reading its captured prefix")?;
        consume(&buffer[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

pub fn hex(sha: &Sha) -> String {
    hex::encode(sha)
}

pub fn parse_hex(s: &str) -> Result<Sha> {
    let v = hex::decode(s.trim()).context("sha is not hex")?;
    let arr: Sha = v.as_slice().try_into().context("sha must be 32 bytes")?;
    Ok(arr)
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn growth_after_metadata_capture_has_one_fixed_size_and_hash() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        std::fs::write(&path, b"first\n").unwrap();
        let file = open_regular(&path).unwrap();
        let metadata = file.metadata().unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"second\n")
            .unwrap();
        let snapshot = FileSnapshot::capture_open_file(&path, file, metadata).unwrap();
        assert_eq!(snapshot.size(), 6);
        assert_eq!(snapshot.sha, sha_of_bytes(b"first\n"));
        assert_eq!(snapshot.read_bytes().unwrap(), b"first\n");
    }

    #[test]
    fn appended_bytes_never_enter_empty_small_or_large_snapshots() {
        for size in [0, 7, crate::CHUNK_BYTES * 2 + 13] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("log");
            let original = vec![b'a'; size];
            std::fs::write(&path, &original).unwrap();
            let snapshot = FileSnapshot::capture(&path).unwrap();
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"new bytes")
                .unwrap();
            assert_eq!(snapshot.read_bytes().unwrap(), original);
            assert_eq!(snapshot.size(), size as u64);
        }
    }

    #[test]
    fn mutation_truncation_replacement_and_link_swaps_are_rejected() {
        for action in [
            "overwrite",
            "truncate",
            "replace",
            "link",
            "overwrite-and-grow",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("log");
            std::fs::write(&path, b"original").unwrap();
            let snapshot = FileSnapshot::capture(&path).unwrap();
            match action {
                "overwrite" => std::fs::write(&path, b"modified").unwrap(),
                "truncate" => std::fs::write(&path, b"short").unwrap(),
                "overwrite-and-grow" => std::fs::write(&path, b"modified and longer").unwrap(),
                "replace" => {
                    std::fs::rename(&path, temp.path().join("old")).unwrap();
                    std::fs::write(&path, b"original").unwrap();
                }
                "link" => {
                    let old = temp.path().join("old");
                    std::fs::rename(&path, &old).unwrap();
                    std::os::unix::fs::symlink(&old, &path).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(snapshot.read_bytes().is_err(), "{action}");
        }
    }

    #[test]
    fn mutation_of_unread_bytes_during_packing_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("log");
        std::fs::write(&path, b"abcdefgh").unwrap();
        let snapshot = FileSnapshot::capture(&path).unwrap();
        let mut changed = false;
        let result = snapshot.read_chunks(2, |_| {
            if !changed {
                changed = true;
                let mut writer = OpenOptions::new().write(true).open(&path)?;
                writer.seek(SeekFrom::Start(4))?;
                writer.write_all(b"XXgh-extra")?;
            }
            Ok(())
        });
        assert!(result.is_err());
    }
}
