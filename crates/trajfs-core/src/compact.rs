//! Lossless storage auditing and compaction. Catalog history is copied intact;
//! only physical frame locations and compression may change.

use crate::catalog;
use crate::hash::sha_of_bytes;
use crate::manifest::{resolve_store_artifact, sync_dir};
use crate::pack::{Loc, PackWriter, MAGIC};
use crate::{Sha, Store, ARTIFACT_TARGET_BYTES};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

fn auxiliary_paths(store: &Store) -> Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    // Written by `pack` outside MANIFEST.json, but still part of a portable store.
    for name in [".gitattributes"] {
        match std::fs::symlink_metadata(store.root.join(name)) {
            Ok(_) => {
                resolve_store_artifact(&store.root, Path::new(name))?;
                paths.push(std::path::PathBuf::from(name));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(paths)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StorageReport {
    /// Includes retained historical catalog rows, not only currently visible paths.
    pub catalog_rows: u64,
    pub logical_bytes: u64,
    pub blobs: u64,
    pub unique_blob_bytes: u64,
    pub frames: u64,
    pub unique_frames: u64,
    pub unique_frame_bytes: u64,
    pub duplicate_frames: u64,
    /// Bytes recoverable by keeping the smallest existing encoding of each frame.
    pub duplicate_frame_bytes: u64,
    pub unreferenced_pack_bytes: u64,
    pub pack_bytes: u64,
    pub metadata_bytes: u64,
    pub stored_bytes: u64,
}

impl StorageReport {
    pub fn compact(&self) -> bool {
        self.duplicate_frames == 0 && self.unreferenced_pack_bytes == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct FrameKey {
    pack: u32,
    offset: i64,
    len: i32,
}

impl From<Loc> for FrameKey {
    fn from(loc: Loc) -> Self {
        Self {
            pack: loc.pack,
            offset: loc.chunk_offset,
            len: loc.chunk_len,
        }
    }
}

struct StorageLayout {
    report: StorageReport,
    digests: BTreeMap<FrameKey, Sha>,
    best: HashMap<Sha, FrameKey>,
}

/// Audit actual encoded frames, not just distinct catalog hashes. Memory holds
/// frame identities and locations; file payloads are read one bounded frame at a time.
fn layout(store: &Store) -> Result<StorageLayout> {
    let mut report = StorageReport::default();
    store.for_each_file(false, |row| {
        report.catalog_rows += 1;
        report.logical_bytes += row.size as u64;
    })?;
    let index = store.index()?;
    report.blobs = index.len() as u64;
    let mut digests = BTreeMap::new();
    for parts in index.values() {
        for loc in parts {
            report.unique_blob_bytes += u64::try_from(loc.size).context("negative blob size")?;
            digests.entry(FrameKey::from(*loc)).or_insert([0; 32]);
        }
    }
    let inventory = store.manifest.artifacts_with_legacy_derived(|relative| {
        resolve_store_artifact(&store.root, relative).is_ok()
    })?;
    let mut pack_lengths = BTreeMap::new();
    for relative in &inventory.packs {
        let id = relative
            .file_stem()
            .and_then(|v| v.to_str())
            .context("invalid pack name")?
            .parse::<u32>()?;
        let len = std::fs::metadata(resolve_store_artifact(&store.root, relative)?)?.len();
        ensure!(len >= MAGIC.len() as u64, "truncated pack {id}");
        pack_lengths.insert(id, len);
        report.pack_bytes += len;
    }
    report.metadata_bytes = std::fs::metadata(store.root.join("MANIFEST.json"))?.len();
    for relative in inventory
        .files
        .iter()
        .chain(&inventory.dirs)
        .chain(&inventory.excluded)
        .chain(&inventory.indexes)
        .chain(&inventory.derived)
    {
        report.metadata_bytes +=
            std::fs::metadata(resolve_store_artifact(&store.root, relative)?)?.len();
    }
    for relative in auxiliary_paths(store)? {
        report.metadata_bytes += std::fs::metadata(store.root.join(relative))?.len();
    }
    report.stored_bytes = report.pack_bytes + report.metadata_bytes;
    let mut ends: BTreeMap<u32, u64> = pack_lengths
        .keys()
        .map(|id| (*id, MAGIC.len() as u64))
        .collect();
    let mut best: HashMap<Sha, FrameKey> = HashMap::new();
    let mut reader = store.reader();
    let mut frame_bytes = 0u64;
    for (key, digest) in &mut digests {
        let end = ends
            .get_mut(&key.pack)
            .context("frame references an undeclared pack")?;
        let offset = u64::try_from(key.offset).context("negative frame offset")?;
        ensure!(offset >= *end, "overlapping frames in pack {}", key.pack);
        report.unreferenced_pack_bytes += offset - *end;
        *end = offset
            .checked_add(u64::try_from(key.len).context("negative frame length")?)
            .context("frame extent overflow")?;
        ensure!(
            *end <= pack_lengths[&key.pack],
            "frame extends beyond its pack"
        );
        let raw = reader.frame(key.pack, key.offset, key.len)?;
        *digest = sha_of_bytes(raw);
        frame_bytes += key.len as u64;
        if let Some(previous) = best.get_mut(digest) {
            if key.len < previous.len {
                *previous = *key;
            }
        } else {
            report.unique_frame_bytes += raw.len() as u64;
            best.insert(*digest, *key);
        }
    }
    for (id, len) in pack_lengths {
        report.unreferenced_pack_bytes += len - ends[&id];
    }
    report.frames = digests.len() as u64;
    report.unique_frames = best.len() as u64;
    report.duplicate_frames = report.frames - report.unique_frames;
    report.duplicate_frame_bytes =
        frame_bytes - best.values().map(|key| key.len as u64).sum::<u64>();
    Ok(StorageLayout {
        report,
        digests,
        best,
    })
}

pub fn storage_report(store: &Store) -> Result<StorageReport> {
    Ok(layout(store)?.report)
}

#[derive(Debug, Serialize)]
pub struct CompactionReport {
    pub source: String,
    pub destination: String,
    pub compression_level: Option<i32>,
    pub before: StorageReport,
    pub after: StorageReport,
    pub saved_bytes: i128,
    pub source_deep_verified: bool,
    pub destination_deep_verified: bool,
    pub catalog_history_preserved: bool,
}

/// Write a new store, preserving every catalog row, blob, batch, exclusion,
/// deletion marker and derived table. The source and any existing destination
/// are never overwritten or removed, including when validation fails.
pub fn compact(
    store: &Store,
    destination: &Path,
    compression_level: Option<i32>,
) -> Result<CompactionReport> {
    if let Some(level) = compression_level {
        ensure!(
            (1..=22).contains(&level),
            "compression level must be between 1 and 22"
        );
    }
    ensure!(
        std::fs::symlink_metadata(destination)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "compaction requires a new destination: {}",
        destination.display()
    );
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    ensure!(
        !parent.starts_with(&store.root),
        "compaction destination must be outside its source store"
    );
    let verified = store.verify(true)?;
    ensure!(
        verified.ok(),
        "source store fails deep verification: {verified:?}"
    );
    let source_layout = layout(store)?;
    let mut manifest = store.manifest.clone();
    manifest.upgrade(&store.root)?;
    let inventory = manifest.artifacts()?;
    let auxiliary = auxiliary_paths(store)?;
    let mut declared = inventory.paths();
    declared.extend(auxiliary.iter().cloned());
    for entry in walkdir::WalkDir::new(&store.root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry.path().strip_prefix(&store.root)?;
        ensure!(
            relative == Path::new("MANIFEST.json")
                || relative == Path::new(".lock")
                || declared.contains(relative),
            "unpublished artifact {} must be accounted for before compaction; source preserved",
            relative.display()
        );
    }
    std::fs::create_dir(destination)?;
    let destination = destination.canonicalize()?;
    let _lock = crate::store::lock_store_exclusive(&destination)?;
    std::fs::create_dir(destination.join("packs"))?;
    std::fs::create_dir(destination.join("catalog"))?;
    for relative in inventory
        .files
        .iter()
        .chain(&inventory.dirs)
        .chain(&inventory.excluded)
        .chain(&inventory.derived)
        .chain(&auxiliary)
    {
        let source = resolve_store_artifact(&store.root, relative)?;
        let target = destination.join(relative);
        std::fs::create_dir_all(target.parent().context("artifact has no parent")?)?;
        std::fs::copy(&source, &target)?;
        ensure!(
            crate::hash::sha_of_file(&source)? == crate::hash::sha_of_file(&target)?,
            "metadata changed during compaction: {}",
            relative.display()
        );
        std::fs::File::open(&target)?.sync_all()?;
    }
    let mut reader = store.reader();
    let mut frames = HashMap::new();
    let mut next_pack = 1;
    manifest.batches.sort_by_key(|batch| batch.id);
    for batch in &mut manifest.batches {
        let mut writer = PackWriter::with_max_bytes(
            &destination.join("packs"),
            next_pack,
            ARTIFACT_TARGET_BYTES,
        )?;
        writer.reuse_frames(frames);
        // An index segment is the unit of bounded catalog memory. A frame can
        // be referenced by several segments/batches; the cache reuses its bytes.
        for segment in &batch.segments {
            let suffix = segment.strip_prefix("files-").unwrap_or(segment);
            let relative = std::path::PathBuf::from(format!("packs/index-{suffix}.parquet"));
            let source = resolve_store_artifact(&store.root, &relative)?;
            let mut groups: BTreeMap<FrameKey, Vec<(Sha, Loc)>> = BTreeMap::new();
            catalog::read_index(&source, |sha, loc| {
                groups
                    .entry(FrameKey::from(loc))
                    .or_default()
                    .push((sha, loc));
            })?;
            for (key, members) in groups {
                let digest = source_layout
                    .digests
                    .get(&key)
                    .context("unrecognised source frame")?;
                let best = source_layout.best[digest];
                let raw = reader.frame(best.pack, best.offset, best.len)?.to_vec();
                let encoded = reader.encoded_frame(best.pack, best.offset, best.len)?;
                writer.add_existing_frame(raw, &encoded, &members, compression_level)?;
            }
            let rows = std::mem::take(&mut writer.rows);
            let target = destination.join(&relative);
            catalog::write_index(&target, &rows)?;
            ensure!(
                std::fs::metadata(&target)?.len()
                    <= ARTIFACT_TARGET_BYTES.max(std::fs::metadata(source)?.len()),
                "compacted index exceeds its artifact size bound"
            );
            std::fs::File::open(&target)?.sync_all()?;
        }
        let finished = writer.finish_with_frames()?;
        ensure!(finished.rows.is_empty(), "unwritten compacted index rows");
        if let Some(last) = finished.packs.last() {
            next_pack = last.checked_add(1).context("pack identifiers exhausted")?;
        }
        batch.packs = finished.packs;
        batch.packed_bytes = finished.bytes_out;
        frames = finished.frames;
    }
    sync_dir(&destination.join("packs"))?;
    sync_dir(&destination.join("catalog"))?;
    manifest.save(&destination)?;
    let output = Store::open_unlocked(&destination)?;
    let verified = output.verify(true)?;
    ensure!(
        verified.ok(),
        "compacted store fails deep verification: {verified:?}"
    );
    let after = storage_report(&output)?;
    ensure!(
        after.compact(),
        "compacted store still contains redundant frame data"
    );
    ensure!(
        after.catalog_rows == source_layout.report.catalog_rows
            && after.logical_bytes == source_layout.report.logical_bytes
            && after.blobs == source_layout.report.blobs
            && after.unique_blob_bytes == source_layout.report.unique_blob_bytes,
        "compaction changed the retained content"
    );
    ensure!(
        store
            .index()?
            .keys()
            .all(|sha| output.index().is_ok_and(|index| index.contains_key(sha))),
        "compaction changed the blob identity set"
    );
    let saved_bytes = source_layout.report.stored_bytes as i128 - after.stored_bytes as i128;
    Ok(CompactionReport {
        source: store.root.display().to_string(),
        destination: destination.display().to_string(),
        compression_level,
        before: source_layout.report,
        after,
        saved_bytes,
        source_deep_verified: true,
        destination_deep_verified: true,
        catalog_history_preserved: true,
    })
}
