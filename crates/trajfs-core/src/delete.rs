//! `traj delete`: rebuild a store without one path, then swap it in atomically
//! (docs/PLAN-deletion.md §4). Nothing inside the store is modified in place:
//! the replacement is built in a sibling directory `<store>.deleting`, deep-verified,
//! and exchanged with the store in one `renameat2(RENAME_EXCHANGE)` call.

use crate::catalog::{self, arrow_writer};
use crate::manifest::{sync_dir, Deletion, Manifest};
use crate::pack::PackWriter;
use crate::store::{lock_store_exclusive, Store};
use crate::walk::{is_under, Excluded};
use crate::{Batch, FileRow, Kind, Sha, ROW_GROUP};
use anyhow::{bail, ensure, Context, Result};
use arrow::array::{AsArray, BooleanArray};
use arrow::datatypes::DataType;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ProjectionMask;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Suffix of the sibling work directory.
pub const WORK_SUFFIX: &str = ".deleting";

/// The sibling directory a deletion of `store` builds into (or leaves behind).
pub fn work_dir(store: &Path) -> PathBuf {
    let mut name = store
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(WORK_SUFFIX);
    store.with_file_name(name)
}

/// `Some(store)` when `path` is itself the work directory of a store (its name ends in `.deleting`).
pub fn pending_sibling(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(WORK_SUFFIX)?;
    Some(path.with_file_name(stem))
}

/// `Some(work dir)` when a deletion of `store` was interrupted and its sibling still exists.
pub fn pending(store: &Path) -> Option<PathBuf> {
    let work = work_dir(store);
    std::fs::symlink_metadata(&work).ok().map(|_| work)
}

/// What one deletion removes, computed from the published store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub path: String,
    /// Batches that lose at least one row, exclusion record, or event row.
    pub batches: Vec<u32>,
    pub paths: u64,
    pub bytes: u64,
    pub events: u64,
    pub excluded: u64,
    /// Blobs referenced by removed rows only, and their raw bytes.
    pub blobs: u64,
    pub blob_bytes: u64,
    /// Catalog rows that remain, over every batch.
    pub survivors: u64,
    /// Paths that disappear from the latest visible namespace.
    pub latest_paths: u64,
}

/// What `recover` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovered {
    /// The sibling was an unfinished candidate; the store is unchanged.
    NotApplied,
    /// The sibling was the pre-deletion store; the store is the verified replacement.
    Applied,
}

fn validate_target(store: &Store, path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("a path is required: one file or one subtree of the store");
    }
    catalog::validate_catalog_path(path, false)?;
    if let Some(d) = store
        .manifest
        .deleted
        .iter()
        .find(|d| is_under(path, &d.path))
    {
        bail!(
            "{path} is already deleted (covered by {} on {})",
            d.path,
            d.created
        );
    }
    Ok(())
}

/// Per-batch slices of the store's physical segments, in batch id order (the layout `verify` relies on).
fn batch_ranges(manifest: &Manifest) -> Vec<(Batch, std::ops::Range<usize>)> {
    let mut batches: Vec<Batch> = manifest.batches.clone();
    batches.sort_by_key(|b| b.id);
    let mut first = 0;
    batches
        .into_iter()
        .map(|b| {
            let n = b.segments.len().max(1);
            let range = first..first + n;
            first += n;
            (b, range)
        })
        .collect()
}

/// The `trajectory` column of a derived table, when it has one.
fn trajectory_column(builder: &ParquetRecordBatchReaderBuilder<File>) -> Option<usize> {
    let schema = builder.schema();
    let i = schema.index_of("trajectory").ok()?;
    (schema.field(i).data_type() == &DataType::Utf8).then_some(i)
}

fn count_events_under(segment: &Path, path: &str) -> Result<u64> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(segment)?)?;
    let Some(i) = trajectory_column(&builder) else {
        return Ok(0);
    };
    let mask = ProjectionMask::leaves(builder.parquet_schema(), [i]);
    let reader = builder
        .with_projection(mask)
        .with_batch_size(ROW_GROUP)
        .build()?;
    let mut n = 0;
    for batch in reader {
        let batch = batch?;
        let col = batch.column(0).as_string::<i32>();
        n += (0..batch.num_rows())
            .filter(|&r| is_under(col.value(r), path))
            .count() as u64;
    }
    Ok(n)
}

/// Copy a derived table without the rows whose `trajectory` is at or below `path`.
/// Returns the number of rows dropped. Tables without that column are copied unchanged.
fn filter_table(src: &Path, dst: &Path, path: &str, max_bytes: u64) -> Result<u64> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(src)?)?;
    let schema = builder.schema().clone();
    let column = trajectory_column(&builder);
    let reader = builder.with_batch_size(ROW_GROUP).build()?;
    let mut writer = arrow_writer(dst, schema)?;
    let mut removed = 0u64;
    for batch in reader {
        let batch = batch?;
        let kept = match column {
            None => batch,
            Some(i) => {
                let col = batch.column(i).as_string::<i32>();
                let mask: BooleanArray = (0..batch.num_rows())
                    .map(|r| Some(!is_under(col.value(r), path)))
                    .collect();
                let kept = arrow::compute::filter_record_batch(&batch, &mask)?;
                removed += (batch.num_rows() - kept.num_rows()) as u64;
                kept
            }
        };
        if kept.num_rows() > 0 {
            writer.write(&kept)?;
        }
    }
    writer.close()?;
    let size = std::fs::metadata(dst)?.len();
    ensure!(
        size <= max_bytes,
        "{} would be {size} bytes (artifact limit {max_bytes})",
        dst.display()
    );
    Ok(removed)
}

/// Read one batch's file rows (with attributes) and exclusion records.
fn read_batch(
    store: &Store,
    range: &std::ops::Range<usize>,
) -> Result<(Vec<FileRow>, Vec<Excluded>)> {
    let mut rows = Vec::new();
    for seg in &store.files_segments()[range.clone()] {
        catalog::scan_files(seg, None, true, |r| rows.push(r))?;
    }
    let mut excluded = Vec::new();
    for seg in &store.excluded_segments()[range.clone()] {
        catalog::read_excluded(seg, |e| excluded.push(e))?;
    }
    Ok((rows, excluded))
}

fn derived_paths(root: &Path, batch: &Batch) -> Vec<(String, PathBuf)> {
    batch
        .derived
        .iter()
        .map(|entry| {
            (
                entry.clone(),
                root.join("derived").join(format!("{entry}.parquet")),
            )
        })
        .collect()
}

/// Compute what deleting `path` removes. Read-only.
///
/// # Examples
///
/// The full protocol: plan, rebuild the candidate under the exclusive lock, swap it in.
///
/// ```
/// use trajfs_core::delete::{apply, plan, rebuild};
/// use trajfs_core::store::lock_store_exclusive;
/// use trajfs_core::Store;
/// # fn main() -> anyhow::Result<()> {
/// # let tmp = tempfile::tempdir()?;
/// # let src = tmp.path().join("run");
/// # std::fs::create_dir_all(src.join("rounds/round-0001"))?;
/// # std::fs::write(src.join("rounds/round-0001/review.json"), "{\"ok\":true}\n")?;
/// # std::fs::write(src.join("README"), "hello\n")?;
/// # let store_dir = tmp.path().join("run.trajstore");
/// # trajfs_core::ingest::ingest(&src, &store_dir, trajfs_core::ingest::IngestOptions {
/// #     rules: trajfs_core::rules::Rules::resolve("none")?,
/// #     rules_name: "none".into(),
/// #     adapter: &trajfs_core::NoAdapter,
/// #     label: "round-0001".into(),
/// #     jobs: 1,
/// #     derive: false,
/// #     store_id: None,
/// # })?;
/// let lock = lock_store_exclusive(&store_dir)?;
/// let store = Store::open_unlocked(&store_dir)?;
/// let plan = plan(&store, "rounds/round-0001")?;
/// assert_eq!((plan.paths, plan.survivors, plan.blobs), (1, 1, 1));
/// let work = rebuild(&store, &plan, trajfs_core::ARTIFACT_TARGET_BYTES)?;
/// drop(store);
/// apply(&store_dir, &work)?;
/// drop(lock);
///
/// let after = Store::open(&store_dir)?;
/// assert!(after.stat("rounds/round-0001/review.json")?.is_none());
/// assert!(after.stat("README")?.is_some());
/// assert_eq!(after.manifest.deleted[0].path, "rounds/round-0001");
/// assert!(after.verify(true)?.ok());
/// # Ok(())
/// # }
/// ```
pub fn plan(store: &Store, path: &str) -> Result<Plan> {
    validate_target(store, path)?;
    let mut manifest = store.manifest.clone();
    manifest.upgrade(&store.root)?;
    let index = store.index()?;
    let mut plan = Plan {
        path: path.to_string(),
        ..Default::default()
    };
    let mut kept_shas: HashSet<Sha> = HashSet::new();
    let mut removed_shas: HashSet<Sha> = HashSet::new();
    for (batch, range) in batch_ranges(&manifest) {
        let (rows, excluded) = read_batch(store, &range)?;
        let mut touched = false;
        for r in rows {
            if is_under(&r.path, path) {
                touched = true;
                plan.paths += 1;
                plan.bytes += r.size as u64;
                if r.kind != Kind::Empty {
                    removed_shas.insert(r.sha);
                }
            } else {
                plan.survivors += 1;
                if r.kind != Kind::Empty {
                    kept_shas.insert(r.sha);
                }
            }
        }
        let n = excluded.iter().filter(|e| is_under(&e.rel, path)).count() as u64;
        touched |= n > 0;
        plan.excluded += n;
        for (_, src) in derived_paths(&store.root, &batch) {
            let n = count_events_under(&src, path)?;
            touched |= n > 0;
            plan.events += n;
        }
        if touched {
            plan.batches.push(batch.id);
        }
    }
    for sha in removed_shas.difference(&kept_shas) {
        plan.blobs += 1;
        plan.blob_bytes += index
            .get(sha)
            .map(|parts| parts.iter().map(|l| l.size as u64).sum::<u64>())
            .unwrap_or(0);
    }
    if plan.paths == 0 && plan.events == 0 {
        bail!("nothing at or below {path} in any batch of the store");
    }
    plan.latest_paths =
        store.stat(path)?.is_some() as u64 + store.files_under(path, false)?.len() as u64;
    Ok(plan)
}

fn ts_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Build the replacement store in the sibling work directory and deep-verify it.
/// The caller holds the store's exclusive lock and has checked the preconditions.
/// Returns the work directory; the store itself is untouched.
pub fn rebuild(store: &Store, plan: &Plan, max_artifact_bytes: u64) -> Result<PathBuf> {
    let path = plan.path.as_str();
    validate_target(store, path)?;
    let mut manifest = store.manifest.clone();
    manifest.upgrade(&store.root)?;
    let work = work_dir(&store.root);
    std::fs::create_dir(&work)
        .with_context(|| format!("create work directory {}", work.display()))?;
    let result = build_into(store, &manifest, plan, &work, max_artifact_bytes);
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&work);
        return Err(error);
    }
    Ok(work)
}

fn build_into(
    store: &Store,
    manifest: &Manifest,
    plan: &Plan,
    work: &Path,
    max_artifact_bytes: u64,
) -> Result<()> {
    let path = plan.path.as_str();
    std::fs::create_dir(work.join("catalog"))?;
    let packs_dir = work.join("packs");
    std::fs::create_dir(&packs_dir)?;
    let index = store.index()?;
    let mut reader = store.reader();
    let mut written: HashSet<Sha> = HashSet::new();
    let mut next_pack = 1u32;
    let mut batches = Vec::new();
    let mut kept_rows: Vec<Vec<FileRow>> = Vec::new();
    let mut kept_excluded: Vec<Vec<Excluded>> = Vec::new();
    let mut found = Plan {
        path: path.to_string(),
        ..Default::default()
    };

    for (batch, range) in batch_ranges(manifest) {
        let (rows, excluded) = read_batch(store, &range)?;
        let mut kept = Vec::with_capacity(rows.len());
        let mut touched = false;
        for r in rows {
            if is_under(&r.path, path) {
                touched = true;
                found.paths += 1;
                found.bytes += r.size as u64;
            } else {
                kept.push(r);
            }
        }
        let excluded: Vec<Excluded> = excluded
            .into_iter()
            .filter(|e| {
                let gone = is_under(&e.rel, path);
                found.excluded += gone as u64;
                touched |= gone;
                !gone
            })
            .collect();

        // content first referenced by this batch, read in pack order
        let mut fresh: Vec<Sha> = Vec::new();
        for r in &kept {
            if r.kind != Kind::Empty && written.insert(r.sha) {
                fresh.push(r.sha);
            }
        }
        fresh.sort_by_key(|sha| {
            let l = index.get(sha).and_then(|p| p.first()).copied();
            l.map(|l| (l.pack, l.chunk_offset, l.offset))
        });
        let mut pw = PackWriter::with_max_bytes(&packs_dir, next_pack, max_artifact_bytes)?;
        let mut new_blob_bytes = 0u64;
        for sha in &fresh {
            let bytes = store.read_blob(&mut reader, sha, true)?;
            new_blob_bytes += bytes.len() as u64;
            pw.add_bytes(*sha, &bytes)?;
        }
        let (index_rows, packs, _, packed_bytes) = pw.finish()?;
        if let Some(last) = packs.last() {
            next_pack = last + 1;
        }

        let dirs = catalog::dirs_from_files(kept.iter(), batch.id);
        let segments = catalog::write_batch_segments(
            work,
            batch.id,
            &kept,
            &dirs,
            &excluded,
            &index_rows,
            max_artifact_bytes,
        )?;
        for (entry, src) in derived_paths(&store.root, &batch) {
            let dst = work.join("derived").join(format!("{entry}.parquet"));
            std::fs::create_dir_all(dst.parent().context("derived artifact has no parent")?)?;
            let n = filter_table(&src, &dst, path, max_artifact_bytes)?;
            touched |= n > 0;
            found.events += n;
        }
        if touched {
            found.batches.push(batch.id);
        }
        batches.push(Batch {
            id: batch.id,
            created: batch.created.clone(),
            label: batch.label.clone(),
            paths: kept.len() as u64,
            bytes: kept.iter().map(|r| r.size as u64).sum(),
            new_blobs: fresh.len() as u64,
            new_blob_bytes,
            packed_bytes,
            packs,
            segments,
            derived: batch.derived.clone(),
            excluded: excluded.len() as u64,
            errors: batch.errors.clone(),
            elapsed_ms: batch.elapsed_ms,
        });
        found.survivors += kept.len() as u64;
        kept_rows.push(kept);
        kept_excluded.push(excluded);
    }

    // The store is locked, so the plan and the rebuild must have seen the same data.
    ensure!(
        found.paths == plan.paths
            && found.bytes == plan.bytes
            && found.events == plan.events
            && found.excluded == plan.excluded
            && found.survivors == plan.survivors
            && found.batches == plan.batches,
        "the store changed between planning and rebuilding; run the command again"
    );
    let mut new_manifest = Manifest {
        format: crate::FORMAT_VERSION,
        store_id: manifest.store_id.clone(),
        source: manifest.source.clone(),
        adapter: manifest.adapter.clone(),
        rules: manifest.rules.clone(),
        batches,
        deleted: manifest.deleted.clone(),
    };
    new_manifest.deleted.push(Deletion {
        path: path.to_string(),
        created: ts_now(),
        paths: plan.paths,
        bytes: plan.bytes,
        events: plan.events,
        excluded: plan.excluded,
        blobs: plan.blobs,
        blob_bytes: plan.blob_bytes,
    });
    crate::ingest::write_gitattributes(work)?;
    File::create(work.join(".lock"))?;
    new_manifest.save_with_limit(work, max_artifact_bytes)?;

    verify_candidate(work, manifest, &kept_rows, &kept_excluded, path)?;
    sync_tree(work)?;
    Ok(())
}

/// Deep-verify the candidate as an ordinary store, then check it against what was kept.
fn verify_candidate(
    work: &Path,
    old: &Manifest,
    kept_rows: &[Vec<FileRow>],
    kept_excluded: &[Vec<Excluded>],
    path: &str,
) -> Result<()> {
    let candidate = Store::open_unlocked(work)?;
    let report = candidate.verify(true)?;
    if !report.ok() {
        bail!(
            "the rebuilt store fails deep verification ({} missing packs, {} rows without a blob, {} bad parts, {} corrupt); nothing was changed",
            report.missing_packs.len(),
            report.missing_blobs.len(),
            report.bad_parts.len(),
            report.corrupt.len()
        );
    }
    let ranges = batch_ranges(&candidate.manifest);
    ensure!(
        ranges.len() == kept_rows.len() && ranges.len() == old.batches.len(),
        "the rebuilt store has {} batches, expected {}",
        ranges.len(),
        old.batches.len()
    );
    for ((batch, range), (expected_rows, expected_excluded)) in
        ranges.iter().zip(kept_rows.iter().zip(kept_excluded))
    {
        let (rows, excluded) = read_batch(&candidate, range)?;
        ensure!(
            rows == *expected_rows,
            "batch {}: rebuilt rows differ from the rows to keep",
            batch.id
        );
        ensure!(
            excluded == *expected_excluded,
            "batch {}: rebuilt exclusion records differ from the records to keep",
            batch.id
        );
        ensure!(
            !rows.iter().any(|r| is_under(&r.path, path)),
            "batch {}: a row at or below {path} survived",
            batch.id
        );
        for (_, src) in derived_paths(work, batch) {
            ensure!(
                count_events_under(&src, path)? == 0,
                "{}: an event at or below {path} survived",
                src.display()
            );
        }
    }
    Ok(())
}

/// fsync every file and directory below `root`, then `root` itself.
pub fn sync_tree(root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let kind = entry.file_type();
        if kind.is_file() || kind.is_dir() {
            File::open(entry.path())
                .and_then(|f| f.sync_all())
                .with_context(|| format!("sync {}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains a NUL byte")
}

/// Atomically exchange two directory entries (`renameat2` with `RENAME_EXCHANGE`).
pub fn exchange(a: &Path, b: &Path) -> Result<()> {
    let (ca, cb) = (c_path(a)?, c_path(b)?);
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            ca.as_ptr(),
            libc::AT_FDCWD,
            cb.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::ENOTSUP) => bail!(
                "the filesystem holding {} does not support atomic directory exchange (renameat2 RENAME_EXCHANGE): {error}",
                a.display()
            ),
            _ => {
                return Err(error).with_context(|| {
                    format!("exchange {} and {}", a.display(), b.display())
                })
            }
        }
    }
    Ok(())
}

/// Prove that `parent` supports the atomic exchange before anything is built.
pub fn check_exchange_support(parent: &Path) -> Result<()> {
    let tag = format!(".traj-exchange-probe-{}", std::process::id());
    let a = parent.join(format!("{tag}-a"));
    let b = parent.join(format!("{tag}-b"));
    std::fs::create_dir(&a)?;
    if let Err(error) = std::fs::create_dir(&b) {
        let _ = std::fs::remove_dir(&a);
        return Err(error.into());
    }
    let result = exchange(&a, &b);
    let _ = std::fs::remove_dir(&a);
    let _ = std::fs::remove_dir(&b);
    result
}

fn parent_of_store(store: &Path) -> Result<PathBuf> {
    store
        .parent()
        .map(Path::to_path_buf)
        .context("store has no parent directory")
}

fn remove_sibling(work: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(work)?;
    ensure!(
        md.file_type().is_dir(),
        "{} is not a directory; refusing to remove it",
        work.display()
    );
    std::fs::remove_dir_all(work).with_context(|| format!("remove {}", work.display()))?;
    sync_dir(&parent_of_store(work)?)
}

/// D1–D3 of the protocol: exchange the verified candidate with the store, then remove the old copy.
/// The caller still holds the store's exclusive lock (whose file moves along with the old directory).
pub fn apply(store: &Path, work: &Path) -> Result<()> {
    let store = store.canonicalize()?;
    ensure!(
        work_dir(&store) == work.canonicalize()?,
        "{} is not the work directory of {}",
        work.display(),
        store.display()
    );
    // Held across the exchange: afterwards this is the new store's `.lock`.
    let _candidate_lock = lock_store_exclusive(work)?;
    exchange(&store, work)?;
    sync_dir(&parent_of_store(&store)?)?;
    remove_sibling(work)
}

/// Resolve an interrupted deletion: the sibling is always discarded, because the store
/// directory is either untouched (exchange not reached) or the verified replacement.
pub fn recover(store: &Path) -> Result<Option<Recovered>> {
    let store = store.canonicalize()?;
    let Some(work) = pending(&store) else {
        return Ok(None);
    };
    let _lock = lock_store_exclusive(&store)?;
    let current = Store::open_unlocked(&store).with_context(|| {
        format!(
            "{} does not open as a store; restore it from Git",
            store.display()
        )
    })?;
    if !current.verify(false)?.ok() {
        bail!(
            "{} fails verification; restore it from Git before recovering",
            store.display()
        );
    }
    let outcome = match Manifest::load(&work) {
        Ok(m) if m.deleted.len() > current.manifest.deleted.len() => Recovered::NotApplied,
        Ok(_) => Recovered::Applied,
        Err(_) => Recovered::NotApplied,
    };
    drop(current);
    remove_sibling(&work)?;
    Ok(Some(outcome))
}

/// Blob hashes referenced by any row of the store, for tests and reporting.
pub fn referenced_shas(store: &Store) -> Result<HashMap<Sha, u64>> {
    let mut m = HashMap::new();
    store.for_each_file(false, |r| {
        if r.kind != Kind::Empty {
            *m.entry(r.sha).or_insert(0) += 1;
        }
    })?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::NoAdapter;
    use crate::ingest::{ingest_with_max_artifact_bytes, IngestOptions};
    use crate::rules::Rules;

    fn write(p: &Path, content: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn pack(src: &Path, store: &Path, label: &str) {
        ingest_with_max_artifact_bytes(
            src,
            store,
            IngestOptions {
                rules: Rules::resolve("none").unwrap(),
                rules_name: "none".into(),
                adapter: &NoAdapter,
                label: label.into(),
                jobs: 2,
                derive: false,
                store_id: Some("t".into()),
            },
            64 << 10,
        )
        .unwrap();
    }

    fn rows_of(store: &Store) -> Vec<FileRow> {
        let mut v = Vec::new();
        store.for_each_file(true, |r| v.push(r)).unwrap();
        v
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let store = tmp.path().join("store");
        write(&src.join("rounds/round-0001/a.log"), b"shared\n");
        write(
            &src.join("rounds/round-0001/only.log"),
            b"only in round one\n",
        );
        write(&src.join("rounds/round-0002/a.log"), b"shared\n");
        write(&src.join("rounds/round-0002/b.log"), b"round two\n");
        write(&src.join("rounds/round-0002/empty"), b"");
        write(&src.join("README"), b"top\n");
        pack(&src, &store, "b1");
        write(&src.join("rounds/round-0002/b.log"), b"round two, edited\n");
        write(&src.join("rounds/round-0003/c.log"), b"round three\n");
        pack(&src, &store, "b2");
        (tmp, src, store)
    }

    #[test]
    fn deleting_a_round_keeps_every_other_row_and_shared_content() {
        let (_tmp, src, store) = fixture();
        let before = Store::open(&store).unwrap();
        let expected: Vec<FileRow> = rows_of(&before)
            .into_iter()
            .filter(|r| !is_under(&r.path, "rounds/round-0002"))
            .collect();
        let p = plan(&before, "rounds/round-0002").unwrap();
        assert_eq!(p.paths, 4, "{p:?}"); // a, b, empty in batch 1; b in batch 2
        assert_eq!(p.batches, vec![1, 2]);
        assert_eq!(p.blobs, 2, "the two versions of b.log; a.log is shared");
        assert_eq!(p.survivors, expected.len() as u64);
        drop(before);
        let lock = lock_store_exclusive(&store).unwrap();
        let before = Store::open_unlocked(&store).unwrap();
        let work = rebuild(&before, &p, 64 << 10).unwrap();
        assert!(work.ends_with("store.deleting"));
        apply(&store, &work).unwrap();
        drop(lock);
        assert!(!work.exists());

        let after = Store::open(&store).unwrap();
        assert!(after.verify(true).unwrap().ok());
        assert_eq!(after.manifest.format, crate::FORMAT_VERSION);
        assert_eq!(rows_of(&after), expected);
        assert_eq!(after.manifest.deleted.len(), 1);
        assert_eq!(after.manifest.deleted[0].path, "rounds/round-0002");
        assert_eq!(after.manifest.batches.len(), 2);
        assert!(after.stat("rounds/round-0002/b.log").unwrap().is_none());
        let mut reader = after.reader();
        let row = after.stat("rounds/round-0001/a.log").unwrap().unwrap();
        assert_eq!(
            after.read_row(&mut reader, &row, true).unwrap(),
            b"shared\n"
        );
        assert!(!after
            .index()
            .unwrap()
            .contains_key(&crate::hash::sha_of_bytes(b"round two\n")));

        drop(reader);
        drop(after);
        // the source still has the round: the next pack skips it and records why
        pack(&src, &store, "b3");
        let again = Store::open(&store).unwrap();
        assert!(again.stat("rounds/round-0002/b.log").unwrap().is_none());
        assert_eq!(again.manifest.batches.len(), 3);
        let mut deleted_rule = 0;
        for seg in again.excluded_segments() {
            catalog::read_excluded(seg, |e| {
                if e.rule == crate::walk::DELETED_RULE {
                    deleted_rule += 1;
                }
            })
            .unwrap();
        }
        assert_eq!(deleted_rule, 1, "one pruned directory record");
        assert!(again.verify(true).unwrap().ok());
        assert!(plan(&again, "rounds/round-0002/b.log")
            .unwrap_err()
            .to_string()
            .contains("already deleted"));
    }

    #[test]
    fn deleting_the_last_path_leaves_a_valid_empty_store() {
        let (_tmp, _src, store) = fixture();
        let lock = lock_store_exclusive(&store).unwrap();
        let st = Store::open_unlocked(&store).unwrap();
        let p = plan(&st, "rounds").unwrap();
        let work = rebuild(&st, &p, 64 << 10).unwrap();
        apply(&store, &work).unwrap();
        drop(lock);
        let after = Store::open(&store).unwrap();
        assert!(after.verify(true).unwrap().ok());
        assert_eq!(rows_of(&after).len(), 1);
        let (dirs, files) = after.children("").unwrap();
        assert!(dirs.is_empty());
        assert_eq!(files.len(), 1);
        drop(after);
        let lock = lock_store_exclusive(&store).unwrap();
        let st = Store::open_unlocked(&store).unwrap();
        let p = plan(&st, "README").unwrap();
        let work = rebuild(&st, &p, 64 << 10).unwrap();
        apply(&store, &work).unwrap();
        drop(lock);
        let after = Store::open(&store).unwrap();
        assert!(after.verify(true).unwrap().ok());
        assert!(rows_of(&after).is_empty());
        assert!(after.index().unwrap().is_empty());
        assert_eq!(after.manifest.deleted.len(), 2);
    }

    #[test]
    fn unknown_paths_and_bad_paths_are_refused() {
        let (_tmp, _src, store) = fixture();
        let st = Store::open(&store).unwrap();
        for bad in ["", "nope", "rounds/../rounds", "rounds/round-0002/"] {
            assert!(plan(&st, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn recovery_discards_the_sibling_in_both_states() {
        let (_tmp, _src, store) = fixture();
        assert_eq!(recover(&store).unwrap(), None);
        // before the exchange: a candidate with one more deletion record
        let st = Store::open(&store).unwrap();
        let p = plan(&st, "rounds/round-0001").unwrap();
        let rows_before = rows_of(&st);
        drop(st);
        let lock = lock_store_exclusive(&store).unwrap();
        let st = Store::open_unlocked(&store).unwrap();
        let work = rebuild(&st, &p, 64 << 10).unwrap();
        drop(st);
        drop(lock);
        assert_eq!(pending(&store), Some(work.clone()));
        assert_eq!(recover(&store).unwrap(), Some(Recovered::NotApplied));
        assert!(!work.exists());
        assert_eq!(rows_of(&Store::open(&store).unwrap()), rows_before);
        // after the exchange: the sibling is the old store
        let lock = lock_store_exclusive(&store).unwrap();
        let st = Store::open_unlocked(&store).unwrap();
        let work = rebuild(&st, &p, 64 << 10).unwrap();
        drop(st);
        exchange(&store, &work).unwrap();
        drop(lock);
        assert_eq!(recover(&store).unwrap(), Some(Recovered::Applied));
        assert!(!work.exists());
        let after = Store::open(&store).unwrap();
        assert_eq!(after.manifest.deleted.len(), 1);
        assert!(after.stat("rounds/round-0001/only.log").unwrap().is_none());
        assert!(after.verify(true).unwrap().ok());
    }

    #[test]
    fn a_pending_deletion_blocks_pack() {
        let (_tmp, src, store) = fixture();
        std::fs::create_dir(work_dir(&store)).unwrap();
        let err = match ingest_with_max_artifact_bytes(
            &src,
            &store,
            IngestOptions {
                rules: Rules::resolve("none").unwrap(),
                rules_name: "none".into(),
                adapter: &NoAdapter,
                label: String::new(),
                jobs: 1,
                derive: false,
                store_id: None,
            },
            64 << 10,
        ) {
            Ok(_) => panic!("pack must refuse a store with a pending deletion"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("--recover"), "{err:#}");
    }

    #[test]
    fn exchange_is_supported_in_the_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        check_exchange_support(tmp.path()).unwrap();
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[test]
    fn is_under_matches_exact_and_descendant_paths_only() {
        assert!(is_under("a/b", "a/b"));
        assert!(is_under("a/b/c", "a/b"));
        assert!(!is_under("a/bc", "a/b"));
        assert!(!is_under("a", "a/b"));
        assert!(is_under("anything", ""));
    }
}
