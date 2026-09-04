//! MANIFEST.json (docs/PLAN.md §3.4).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub store_id: String,
    pub source: String,
    pub adapter: AdapterInfo,
    pub rules: RulesInfo,
    #[serde(default)]
    pub batches: Vec<Batch>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AdapterInfo {
    pub name: String,
    pub version: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RulesInfo {
    pub name: String,
    pub version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub id: u32,
    pub created: String,
    #[serde(default)]
    pub label: String,
    pub paths: u64,
    pub bytes: u64,
    pub new_blobs: u64,
    pub new_blob_bytes: u64,
    pub packed_bytes: u64,
    /// Inclusive range of pack ids written by this batch, empty when none.
    #[serde(default)]
    pub packs: Vec<u32>,
    #[serde(default)]
    pub segments: Vec<String>,
    #[serde(default)]
    pub derived: Vec<String>,
    #[serde(default)]
    pub excluded: u64,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub elapsed_ms: u64,
}

impl Manifest {
    pub fn path(store: &Path) -> std::path::PathBuf {
        store.join("MANIFEST.json")
    }

    pub fn load(store: &Path) -> Result<Manifest> {
        let p = Self::path(store);
        let text = std::fs::read_to_string(&p)
            .with_context(|| format!("{} (not a trajfs store?)", p.display()))?;
        let m: Manifest =
            serde_json::from_str(&text).with_context(|| format!("parse {}", p.display()))?;
        if m.format != crate::FORMAT_VERSION {
            anyhow::bail!(
                "store format {} is not supported (this build reads {})",
                m.format,
                crate::FORMAT_VERSION
            );
        }
        Ok(m)
    }

    /// Atomic write: temp file + rename.
    pub fn save(&self, store: &Path) -> Result<()> {
        let p = Self::path(store);
        let tmp = store.join("MANIFEST.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    pub fn next_batch_id(&self) -> u32 {
        self.batches.iter().map(|b| b.id).max().unwrap_or(0) + 1
    }

    pub fn next_pack_id(&self) -> u32 {
        self.batches
            .iter()
            .flat_map(|b| b.packs.iter().copied())
            .max()
            .unwrap_or(0)
            + 1
    }
}
