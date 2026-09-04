//! sha256 helpers.

use crate::Sha;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

pub fn sha_of_bytes(b: &[u8]) -> Sha {
    let mut h = Sha256::new();
    h.update(b);
    h.finalize().into()
}

/// Streaming sha256 of a file; returns (sha, size).
pub fn sha_of_file(path: &Path) -> Result<(Sha, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf).with_context(|| format!("read {}", path.display()))?;
        if r == 0 {
            break;
        }
        h.update(&buf[..r]);
        n += r as u64;
    }
    Ok((h.finalize().into(), n))
}

pub fn hex(sha: &Sha) -> String {
    hex::encode(sha)
}

pub fn parse_hex(s: &str) -> Result<Sha> {
    let v = hex::decode(s.trim()).context("sha is not hex")?;
    let arr: Sha = v.as_slice().try_into().context("sha must be 32 bytes")?;
    Ok(arr)
}
