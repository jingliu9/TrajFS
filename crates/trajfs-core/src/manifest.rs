//! MANIFEST.json (docs/PLAN.md §3.4).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

static MANIFEST_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    /// `files-*` stems. Each has matching `dirs-*`, `excluded-*`, and
    /// `index-*` Parquet files with the same suffix.
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

#[derive(Clone, Debug, Default)]
pub struct ArtifactInventory {
    pub files: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
    pub excluded: Vec<PathBuf>,
    pub indexes: Vec<PathBuf>,
    pub packs: Vec<PathBuf>,
    pub derived: Vec<PathBuf>,
}

impl ArtifactInventory {
    pub fn all(&self) -> impl Iterator<Item = &PathBuf> {
        self.files
            .iter()
            .chain(&self.dirs)
            .chain(&self.excluded)
            .chain(&self.indexes)
            .chain(&self.packs)
            .chain(&self.derived)
    }

    pub fn paths(&self) -> BTreeSet<PathBuf> {
        self.all().cloned().collect()
    }
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub fn validate_adapter_name(name: &str) -> Result<()> {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(part)), None) if !part.is_empty() => Ok(()),
        _ => bail!("adapter name {name:?} must be exactly one non-empty path component"),
    }
}

pub fn resolve_store_artifact(store: &Path, relative: &Path) -> Result<PathBuf> {
    if !safe_relative(relative) {
        bail!(
            "manifest contains unsafe artifact path {}",
            relative.display()
        );
    }
    let store = store.canonicalize()?;
    let path = store.join(relative);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("manifest declares missing artifact {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("manifest artifact {} is not a regular file", path.display());
    }
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(&store) {
        bail!(
            "manifest artifact {} resolves outside store {}",
            path.display(),
            store.display()
        );
    }
    Ok(resolved)
}

fn parquet_path(dir: &str, stem: &str) -> Result<PathBuf> {
    let stem = Path::new(stem);
    if !safe_relative(stem) {
        bail!("manifest contains unsafe artifact name {}", stem.display());
    }
    let mut path = PathBuf::from(dir);
    path.push(stem);
    let mut name = path.into_os_string();
    name.push(".parquet");
    Ok(PathBuf::from(name))
}

fn is_manifest_temp(name: &str) -> bool {
    name == "MANIFEST.json.tmp" || name.starts_with(".MANIFEST.json.tmp-")
}

pub fn remove_manifest_temps(store: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for entry in std::fs::read_dir(store)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_manifest_temp(&name) {
            continue;
        }
        let kind = entry.file_type()?;
        if !kind.is_file() && !kind.is_symlink() {
            bail!(
                "refusing to remove non-file manifest temporary {}",
                entry.path().display()
            );
        }
        std::fs::remove_file(entry.path())?;
        removed.push(name);
    }
    Ok(removed)
}

fn create_manifest_temp(store: &Path) -> Result<(PathBuf, std::fs::File)> {
    for _ in 0..128 {
        let counter = MANIFEST_TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = store.join(format!(
            ".MANIFEST.json.tmp-{}-{nanos}-{counter}",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!(
        "could not create a unique temporary manifest in {}",
        store.display()
    )
}

fn batch_part(stem: &str, prefix: &str) -> Option<(u64, u64)> {
    let rest = stem.strip_prefix(prefix)?;
    let mut parts = rest.split('-');
    let batch = parts.next()?.parse().ok()?;
    let part = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((batch, part))
}

fn trailing_numbers(path: &Path) -> Option<(u64, u64)> {
    let stem = path.file_stem()?.to_str()?;
    let mut parts = stem.rsplit('-');
    let last = parts.next()?.parse().ok()?;
    match parts.next().and_then(|part| part.parse().ok()) {
        Some(previous) => Some((previous, last)),
        None => Some((last, 0)),
    }
}

pub fn artifact_path_cmp(left: &Path, right: &Path) -> Ordering {
    left.parent()
        .cmp(&right.parent())
        .then_with(|| match (trailing_numbers(left), trailing_numbers(right)) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => left.file_name().cmp(&right.file_name()),
        })
        .then_with(|| left.file_name().cmp(&right.file_name()))
}

impl Manifest {
    pub fn path(store: &Path) -> std::path::PathBuf {
        store.join("MANIFEST.json")
    }

    pub fn load(store: &Path) -> Result<Manifest> {
        let p = Self::path(store);
        let metadata = std::fs::symlink_metadata(&p)
            .with_context(|| format!("{} (not a trajfs store?)", p.display()))?;
        if !metadata.file_type().is_file() {
            bail!("{} is not a regular manifest file", p.display());
        }
        let root = store.canonicalize()?;
        if !p.canonicalize()?.starts_with(&root) {
            bail!("{} resolves outside store {}", p.display(), root.display());
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options
            .open(&p)
            .with_context(|| format!("{} (not a trajfs store?)", p.display()))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Self::parse(&text).with_context(|| format!("parse {}", p.display()))
    }

    pub fn parse(text: &str) -> Result<Manifest> {
        let m: Manifest = serde_json::from_str(text)?;
        if !(1..=crate::FORMAT_VERSION).contains(&m.format) {
            anyhow::bail!(
                "store format {} is not supported (this build reads 1 through {})",
                m.format,
                crate::FORMAT_VERSION
            );
        }
        validate_adapter_name(&m.adapter.name)?;
        Ok(m)
    }

    pub fn artifacts(&self) -> Result<ArtifactInventory> {
        validate_adapter_name(&self.adapter.name)?;
        let mut inventory = ArtifactInventory::default();
        let mut batches: Vec<&Batch> = self.batches.iter().collect();
        batches.sort_by_key(|batch| batch.id);
        for batch in batches {
            let mut segments = if batch.segments.is_empty() {
                if self.format == 1 {
                    vec![format!("files-{:04}", batch.id)]
                } else {
                    bail!("manifest batch {} declares no catalog segments", batch.id);
                }
            } else {
                batch.segments.clone()
            };
            segments.sort_by(|left, right| {
                match (batch_part(left, "files-"), batch_part(right, "files-")) {
                    (Some(a), Some(b)) => a.cmp(&b),
                    _ => left.cmp(right),
                }
            });
            for segment in segments {
                let segment_path = Path::new(&segment);
                if segment_path.components().count() != 1 || !safe_relative(segment_path) {
                    bail!("manifest contains unsafe segment name {segment}");
                }
                let suffix = segment.strip_prefix("files-").unwrap_or(&segment);
                inventory.files.push(parquet_path("catalog", &segment)?);
                inventory
                    .dirs
                    .push(parquet_path("catalog", &format!("dirs-{suffix}"))?);
                inventory
                    .excluded
                    .push(parquet_path("catalog", &format!("excluded-{suffix}"))?);
                inventory
                    .indexes
                    .push(parquet_path("packs", &format!("index-{suffix}"))?);
            }
            for pack in &batch.packs {
                inventory
                    .packs
                    .push(PathBuf::from("packs").join(format!("{pack:04}.pack")));
            }
            for derived in &batch.derived {
                if Path::new(derived).components().count() != 2 {
                    bail!("manifest contains invalid derived artifact name {derived}");
                }
                inventory.derived.push(parquet_path("derived", derived)?);
            }
        }
        inventory.packs.sort_by(|left, right| {
            let id = |path: &Path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<u64>().ok())
            };
            match (id(left), id(right)) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => left.cmp(right),
            }
        });
        inventory
            .derived
            .sort_by(|left, right| artifact_path_cmp(left, right));

        let all: Vec<PathBuf> = inventory.all().cloned().collect();
        let unique: BTreeSet<PathBuf> = all.iter().cloned().collect();
        if unique.len() != all.len() {
            bail!("manifest declares the same artifact more than once");
        }
        Ok(inventory)
    }

    /// Older `traj derive` versions replaced batch-derived files with one
    /// unlisted `events-0000.parquet`. Treat that conventional file, plus any
    /// still-present batch-derived files, as the legacy publication.
    pub fn artifacts_with_legacy_derived(
        &self,
        exists: impl Fn(&Path) -> bool,
    ) -> Result<ArtifactInventory> {
        let mut inventory = self.artifacts()?;
        let missing_declared = inventory.derived.iter().any(|path| !exists(path));
        let legacy = PathBuf::from("derived")
            .join(&self.adapter.name)
            .join("events-0000.parquet");
        if self.format == 1 && exists(&legacy) && (inventory.derived.is_empty() || missing_declared)
        {
            inventory.derived.retain(|path| exists(path));
            if !inventory.derived.contains(&legacy) {
                inventory.derived.push(legacy);
            }
            inventory
                .derived
                .sort_by(|left, right| artifact_path_cmp(left, right));
        }
        Ok(inventory)
    }

    /// Upgrade a V1 manifest to explicit derived-artifact publication.
    pub fn upgrade(&mut self, store: &Path) -> Result<()> {
        if self.format == crate::FORMAT_VERSION {
            return Ok(());
        }
        let inventory = self.artifacts_with_legacy_derived(|relative| {
            resolve_store_artifact(store, relative).is_ok()
        })?;
        let derived = inventory
            .derived
            .iter()
            .map(|path| {
                let relative = path.strip_prefix("derived").unwrap_or(path);
                let parent = relative.parent().unwrap_or_else(|| Path::new(""));
                let stem = relative
                    .file_stem()
                    .context("derived artifact has no stem")?;
                Ok(parent.join(stem).to_string_lossy().into_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        for batch in &mut self.batches {
            if batch.segments.is_empty() {
                batch.segments.push(format!("files-{:04}", batch.id));
            }
            batch.derived.clear();
        }
        if !derived.is_empty() {
            self.batches
                .last_mut()
                .context("legacy store has derived artifacts but no batches")?
                .derived = derived;
        }
        self.format = crate::FORMAT_VERSION;
        Ok(())
    }

    /// Atomic write: temp file + rename.
    pub fn save(&self, store: &Path) -> Result<()> {
        self.save_with_limit(store, crate::ARTIFACT_TARGET_BYTES)
    }

    pub fn save_with_limit(&self, store: &Path, max_bytes: u64) -> Result<()> {
        let store = store.canonicalize()?;
        let p = Self::path(&store);
        remove_manifest_temps(&store)?;
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > max_bytes {
            bail!(
                "MANIFEST.json would be {} bytes (artifact limit {max_bytes})",
                bytes.len()
            );
        }
        let (tmp, mut file) = create_manifest_temp(&store)?;
        if let Err(error) = file
            .write_all(&bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
        {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
        drop(file);
        if let Err(error) = std::fs::rename(&tmp, &p) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn manifest_with_segments(segments: Vec<String>) -> Manifest {
        Manifest {
            format: crate::FORMAT_VERSION,
            store_id: "test".into(),
            source: "/source".into(),
            adapter: AdapterInfo {
                name: "none".into(),
                version: 1,
            },
            rules: RulesInfo {
                name: "none".into(),
                version: 1,
            },
            batches: vec![Batch {
                id: 1,
                created: "2026-09-05T00:00:00Z".into(),
                label: String::new(),
                paths: 0,
                bytes: 0,
                new_blobs: 0,
                new_blob_bytes: 0,
                packed_bytes: 0,
                packs: vec![],
                segments,
                derived: vec![],
                excluded: 0,
                errors: vec![],
                elapsed_ms: 0,
            }],
        }
    }

    #[test]
    fn artifact_parts_are_sorted_numerically() {
        let manifest = manifest_with_segments(vec![
            "files-0001-10000".into(),
            "files-0001-9999".into(),
            "files-0001-0002".into(),
        ]);
        let inventory = manifest.artifacts().unwrap();
        let names: Vec<String> = inventory
            .files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "files-0001-0002.parquet",
                "files-0001-9999.parquet",
                "files-0001-10000.parquet"
            ]
        );
    }

    #[test]
    fn missing_legacy_segments_fall_back_to_batch_name() {
        let mut manifest = manifest_with_segments(vec![]);
        manifest.format = 1;
        let inventory = manifest.artifacts().unwrap();
        assert_eq!(
            inventory.files,
            [PathBuf::from("catalog/files-0001.parquet")]
        );
    }

    #[test]
    fn legacy_rederived_events_are_recognized() {
        let mut manifest = manifest_with_segments(vec!["files-0001".into()]);
        manifest.format = 1;
        manifest.batches[0].derived = vec!["none/events-0001".into()];
        let legacy = PathBuf::from("derived/none/events-0000.parquet");
        let inventory = manifest
            .artifacts_with_legacy_derived(|path| path == legacy)
            .unwrap();
        assert_eq!(inventory.derived, [legacy]);
    }

    #[test]
    fn upgrading_v1_publishes_legacy_rederived_events() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("derived/none/events-0000.parquet");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"legacy").unwrap();
        let mut manifest = manifest_with_segments(vec!["files-0001".into()]);
        manifest.format = 1;
        manifest.batches[0].derived = vec!["none/events-0001".into()];
        manifest.upgrade(tmp.path()).unwrap();
        assert_eq!(manifest.format, crate::FORMAT_VERSION);
        assert_eq!(manifest.batches[0].derived, ["none/events-0000"]);
    }

    #[test]
    fn format_two_does_not_publish_unlisted_legacy_names() {
        let manifest = manifest_with_segments(vec!["files-0001".into()]);
        let inventory = manifest.artifacts_with_legacy_derived(|_| true).unwrap();
        assert!(inventory.derived.is_empty());
    }

    #[test]
    fn adapter_names_are_single_safe_components() {
        for invalid in ["", ".", "..", "/absolute", "nested/name"] {
            assert!(validate_adapter_name(invalid).is_err(), "{invalid:?}");
        }
        validate_adapter_name("safe-name").unwrap();

        let mut manifest = manifest_with_segments(vec!["files-0001".into()]);
        manifest.format = 1;
        manifest.adapter.name = "../escape".into();
        let called = Cell::new(false);
        assert!(manifest
            .artifacts_with_legacy_derived(|_| {
                called.set(true);
                false
            })
            .is_err());
        assert!(!called.get());
    }

    #[test]
    fn symlinked_artifacts_cannot_escape_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let outside = tmp.path().join("outside.parquet");
        std::fs::create_dir_all(store.join("catalog")).unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, store.join("catalog/files-0001.parquet")).unwrap();
        let store = store.canonicalize().unwrap();
        assert!(resolve_store_artifact(&store, Path::new("catalog/files-0001.parquet")).is_err());
    }

    #[test]
    fn oversized_manifest_does_not_replace_published_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = manifest_with_segments(vec!["files-0001".into()]);
        manifest.save_with_limit(tmp.path(), 64 << 10).unwrap();
        let published = std::fs::read(Manifest::path(tmp.path())).unwrap();
        manifest.batches[0].label = "x".repeat(20_000);
        assert!(manifest.save_with_limit(tmp.path(), 4096).is_err());
        assert_eq!(
            std::fs::read(Manifest::path(tmp.path())).unwrap(),
            published
        );
        assert!(!tmp.path().join("MANIFEST.json.tmp").exists());
    }

    #[test]
    fn manifest_save_does_not_follow_temporary_or_destination_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        std::fs::create_dir(&store).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, b"sentinel").unwrap();
        std::os::unix::fs::symlink(&outside, Manifest::path(&store)).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), store.join("MANIFEST.json.tmp"))
            .unwrap();
        std::os::unix::fs::symlink(&outside, store.join(".MANIFEST.json.tmp-attacker")).unwrap();

        let manifest = manifest_with_segments(vec!["files-0001".into()]);
        manifest.save_with_limit(&store, 64 << 10).unwrap();

        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");
        assert!(std::fs::symlink_metadata(Manifest::path(&store))
            .unwrap()
            .file_type()
            .is_file());
        assert!(std::fs::symlink_metadata(store.join("MANIFEST.json.tmp")).is_err());
        assert!(!store.join(".MANIFEST.json.tmp-attacker").exists());
        assert!(!std::fs::read_dir(&store).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".MANIFEST.json.tmp-")
        }));
    }
}
