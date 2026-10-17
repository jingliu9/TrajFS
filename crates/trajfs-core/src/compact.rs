//! Lossless storage auditing and compaction. Catalog history is copied intact;
//! only physical frame locations and compression may change.

use crate::catalog;
use crate::hash::sha_of_bytes;
use crate::manifest::{resolve_store_artifact, sync_dir};
use crate::pack::{Loc, PackReader, PackWriter, MAGIC};
use crate::{Sha, Store, ARTIFACT_TARGET_BYTES};
use anyhow::{ensure, Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

fn auxiliary_paths(store: &Store) -> Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    // Written by `pack` outside MANIFEST.json, but still part of a portable store.
    {
        let name = ".gitattributes";
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
fn layout(store: &Store, jobs: usize) -> Result<StorageLayout> {
    ensure!(
        (1..=128).contains(&jobs),
        "frame audit workers must be between 1 and 128"
    );
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
    let mut frame_bytes = 0u64;
    for key in digests.keys() {
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
        frame_bytes += key.len as u64;
    }
    let keys: Vec<_> = digests.keys().copied().collect();
    let workers = jobs.min(8);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?;
    let chunk_size = keys.len().div_ceil(workers).max(1);
    let chunks: Vec<Result<Vec<_>>> = pool.install(|| {
        keys.par_chunks(chunk_size)
            .map(|chunk| {
                let mut reader =
                    PackReader::with_open_pack_limit(&store.root.join("packs"), 16 / workers);
                let mut hashes = Vec::with_capacity(chunk.len());
                for key in chunk {
                    let raw = reader.frame(key.pack, key.offset, key.len)?;
                    hashes.push((*key, sha_of_bytes(raw), raw.len() as u64));
                }
                Ok(hashes)
            })
            .collect()
    });
    for chunk in chunks {
        for (key, digest, raw_len) in chunk? {
            *digests.get_mut(&key).context("frame audit lost a key")? = digest;
            if let Some(previous) = best.get_mut(&digest) {
                if key.len < previous.len {
                    *previous = key;
                }
            } else {
                report.unique_frame_bytes += raw_len;
                best.insert(digest, key);
            }
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
    storage_report_with_jobs(store, 1)
}

pub fn storage_report_with_jobs(store: &Store, jobs: usize) -> Result<StorageReport> {
    Ok(layout(store, jobs)?.report)
}

#[derive(Debug, Serialize)]
pub struct CompactionReport {
    pub source: String,
    pub destination: String,
    pub compression_level: Option<i32>,
    pub compression_workers: usize,
    pub before: StorageReport,
    pub after: StorageReport,
    pub saved_bytes: i128,
    pub source_deep_verified: bool,
    pub destination_deep_verified: bool,
    pub catalog_history_preserved: bool,
}

/// Progress counts unique uncompressed frames, independently of output compression.
#[derive(Debug, Serialize)]
pub struct CompactionProgress {
    pub phase: &'static str,
    pub frames_done: u64,
    pub frames_total: u64,
    pub raw_bytes_done: u64,
    pub raw_bytes_total: u64,
}

/// Write a new store, preserving every catalog row, blob, batch, exclusion,
/// deletion marker and derived table. The source and any existing destination
/// are never overwritten or removed, including when validation fails.
pub fn compact(
    store: &Store,
    destination: &Path,
    compression_level: Option<i32>,
) -> Result<CompactionReport> {
    compact_with_jobs(store, destination, compression_level, 1)
}

/// Compress bounded groups concurrently and commit frames in their original order.
/// Catalogs, frame reuse, verification and the existing-encoding fallback are unchanged.
pub fn compact_with_jobs(
    store: &Store,
    destination: &Path,
    compression_level: Option<i32>,
    jobs: usize,
) -> Result<CompactionReport> {
    compact_with_progress(store, destination, compression_level, jobs, |_| {})
}

pub fn compact_with_progress(
    store: &Store,
    destination: &Path,
    compression_level: Option<i32>,
    jobs: usize,
    mut progress: impl FnMut(&CompactionProgress),
) -> Result<CompactionReport> {
    ensure!(
        (1..=128).contains(&jobs),
        "compression workers must be between 1 and 128"
    );
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
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .thread_name(|n| format!("traj-compact-{n}"))
        .build()?;
    let mut current = CompactionProgress {
        phase: "verify-source",
        frames_done: 0,
        frames_total: 0,
        raw_bytes_done: 0,
        raw_bytes_total: 0,
    };
    progress(&current);
    let verified = store.verify_with_jobs(true, jobs)?;
    ensure!(
        verified.ok(),
        "source store fails deep verification: {verified:?}"
    );
    current.phase = "audit-source-frames";
    progress(&current);
    let source_layout = layout(store, jobs)?;
    current.frames_total = source_layout.report.unique_frames;
    current.raw_bytes_total = source_layout.report.unique_frame_bytes;
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
    let mut seen_frames = HashSet::new();
    let mut next_pack = 1;
    current.phase = "rewrite-frames";
    progress(&current);
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
            let mut groups = groups.into_iter();
            loop {
                // Two frames per worker bounds live payload memory. Reading uses the
                // same bounded PackReader; only independent compression runs in parallel.
                let mut pending = Vec::with_capacity(jobs * 2);
                for (key, members) in groups.by_ref().take(jobs * 2) {
                    let digest = source_layout
                        .digests
                        .get(&key)
                        .context("unrecognised source frame")?;
                    let best = source_layout.best[digest];
                    let raw = reader.frame(best.pack, best.offset, best.len)?.to_vec();
                    let encoded = reader.encoded_frame(best.pack, best.offset, best.len)?;
                    let first = seen_frames.insert(*digest);
                    if first {
                        current.frames_done += 1;
                        current.raw_bytes_done += raw.len() as u64;
                    }
                    pending.push((raw, encoded, members, first));
                }
                if pending.is_empty() {
                    break;
                }
                let prepared: Vec<Result<_>> = pool.install(|| {
                    pending
                        .into_par_iter()
                        .map(|(raw, mut encoded, members, first)| {
                            if first {
                                if let Some(level) = compression_level {
                                    let candidate = zstd::bulk::compress(&raw, level)?;
                                    if candidate.len() < encoded.len() {
                                        encoded = candidate;
                                    }
                                }
                            }
                            Ok((raw, encoded, members))
                        })
                        .collect()
                });
                // Indexed parallel collection preserves frame order. The existing
                // writer rechecks ranges and hashes and shares frames across all batches.
                for prepared in prepared {
                    let (raw, encoded, members) = prepared?;
                    writer.add_existing_frame(raw, &encoded, &members, None)?;
                }
                progress(&current);
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
    drop(reader);
    let output = Store::open_unlocked(&destination)?;
    current.phase = "verify-destination";
    progress(&current);
    let verified = output.verify_with_jobs(true, jobs)?;
    ensure!(
        verified.ok(),
        "compacted store fails deep verification: {verified:?}"
    );
    current.phase = "audit-destination-frames";
    progress(&current);
    let after = storage_report_with_jobs(&output, jobs)?;
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
    current.phase = "complete";
    progress(&current);
    Ok(CompactionReport {
        source: store.root.display().to_string(),
        destination: destination.display().to_string(),
        compression_level,
        compression_workers: jobs,
        before: source_layout.report,
        after,
        saved_bytes,
        source_deep_verified: true,
        destination_deep_verified: true,
        catalog_history_preserved: true,
    })
}
