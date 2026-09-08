//! trajfs-core: content-addressed store for agent run trees.
//!
//! Layout of a store (see docs/PLAN.md §3):
//! `MANIFEST.json`, `catalog/{files,dirs,excluded}-B[-P].parquet`,
//! `packs/NNNN.pack`, `packs/index-B[-P].parquet`,
//! `derived/<adapter>/<table>-B[-P].parquet`.

pub mod adapter;
pub mod catalog;
pub mod delete;
pub mod events;
pub mod hash;
pub mod ingest;
pub mod manifest;
pub mod pack;
pub mod rules;
pub mod store;
pub mod walk;

pub use adapter::{Adapter, NoAdapter};
pub use manifest::{Batch, Deletion, Manifest};
pub use store::Store;

/// sha256 digest.
pub type Sha = [u8; 32];

/// What a catalog row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Symlink = 1,
    Empty = 2,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        match v {
            0 => Some(Kind::File),
            1 => Some(Kind::Symlink),
            2 => Some(Kind::Empty),
            _ => None,
        }
    }
}

/// One catalog row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    pub path: String,
    pub kind: Kind,
    pub mode: u16,
    pub size: i64,
    pub sha: Sha,
    pub mtime_ns: i64,
    pub batch: u32,
    pub attrs: Vec<(String, String)>,
}

impl FileRow {
    pub fn dir(&self) -> &str {
        parent_of(&self.path)
    }
    pub fn name(&self) -> &str {
        basename_of(&self.path)
    }
}

pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

pub fn basename_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Maximum uncompressed bytes per zstd frame (a "chunk").
pub const CHUNK_BYTES: usize = 1 << 20;
/// Conservative maximum for generated packs and Parquet segments.
pub const ARTIFACT_TARGET_BYTES: u64 = 60 << 20;
/// Default maximum pack size.
pub const PACK_SEAL_BYTES: u64 = ARTIFACT_TARGET_BYTES;
/// Row group size for catalog Parquet files.
pub const ROW_GROUP: usize = 16_384;
/// Store format version written to MANIFEST.json.
pub const FORMAT_VERSION: u32 = 3;
