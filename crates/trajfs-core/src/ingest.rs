//! `traj pack`: walk → filter → hash → dedupe → pack → catalog → derived → manifest (docs/PLAN.md §4).

use crate::adapter::Adapter;
use crate::catalog;
use crate::events::SegmentedEventsWriter;
use crate::hash::{sha_of_bytes, sha_of_file};
use crate::manifest::{
    artifact_path_cmp, remove_manifest_temps, resolve_store_artifact, validate_adapter_name,
    AdapterInfo, Batch, Manifest, RulesInfo,
};
use crate::pack::PackWriter;
use crate::rules::Rules;
use crate::walk::{walk, Candidate};
use crate::{FileRow, Kind, Sha, ARTIFACT_TARGET_BYTES, FORMAT_VERSION};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct IngestOptions<'a> {
    pub rules: Rules,
    pub rules_name: String,
    pub adapter: &'a dyn Adapter,
    pub label: String,
    pub jobs: usize,
    pub derive: bool,
    pub store_id: Option<String>,
}

pub struct IngestSummary {
    pub batch: Batch,
    pub skipped_unchanged: u64,
    pub store: PathBuf,
}

fn ts_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn sorted_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(|left, right| artifact_path_cmp(&left.path(), &right.path()));
    Ok(entries)
}

fn ensure_store_dir(store: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        bail!("unsafe store artifact directory {}", relative.display());
    }
    let path = store.join(relative);
    if !path.exists() {
        let parent = path.parent().context("store directory has no parent")?;
        let resolved_parent = parent.canonicalize()?;
        if !resolved_parent.starts_with(store) {
            bail!(
                "store artifact directory {} has a parent outside {}",
                path.display(),
                store.display()
            );
        }
        std::fs::create_dir(&path)?;
    }
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(store) || !resolved.is_dir() {
        bail!(
            "store artifact directory {} is not a directory within {}",
            path.display(),
            store.display()
        );
    }
    Ok(resolved)
}

/// Remove temporary and finalized artifacts not published by the manifest.
fn remove_orphans(store: &Path, m: &Manifest) -> Result<Vec<String>> {
    let inventory = m.artifacts_with_legacy_derived(|relative| {
        resolve_store_artifact(store, relative).is_ok()
    })?;
    let active = inventory.paths();
    let mut removed = remove_manifest_temps(store)?;
    for (dir, prefixes) in [
        ("catalog", vec!["files-", "dirs-", "excluded-"]),
        ("packs", vec!["index-"]),
    ] {
        let d = store.join(dir);
        if !d.is_dir() {
            continue;
        }
        for e in sorted_entries(&d)? {
            let n = e.file_name().to_string_lossy().to_string();
            let mut orphan = n.ends_with(".tmp");
            let relative = PathBuf::from(dir).join(&n);
            let generated_parquet = prefixes
                .iter()
                .any(|prefix| n.starts_with(prefix) && n.ends_with(".parquet"));
            orphan |= generated_parquet && !active.contains(&relative);
            if dir == "packs" {
                let generated_pack = n
                    .strip_suffix(".pack")
                    .is_some_and(|stem| stem.parse::<u64>().is_ok());
                orphan |= generated_pack && !active.contains(&relative);
            }
            if orphan {
                std::fs::remove_file(e.path())?;
                removed.push(format!("{dir}/{n}"));
            }
        }
    }
    let derived = store.join("derived");
    if derived.is_dir() {
        for a in sorted_entries(&derived)? {
            if !a.file_type()?.is_dir() {
                continue;
            }
            for e in sorted_entries(&a.path())? {
                let n = e.file_name().to_string_lossy().to_string();
                let relative = PathBuf::from("derived").join(a.file_name()).join(&n);
                if n.ends_with(".tmp") || (n.ends_with(".parquet") && !active.contains(&relative)) {
                    std::fs::remove_file(e.path())?;
                    removed.push(format!("derived/{}/{n}", a.file_name().to_string_lossy()));
                }
            }
        }
    }
    Ok(removed)
}

pub fn ingest(src: &Path, store: &Path, opts: IngestOptions) -> Result<IngestSummary> {
    ingest_with_max_artifact_bytes(src, store, opts, ARTIFACT_TARGET_BYTES)
}

pub fn ingest_with_max_artifact_bytes(
    src: &Path,
    store: &Path,
    opts: IngestOptions,
    max_artifact_bytes: u64,
) -> Result<IngestSummary> {
    validate_adapter_name(opts.adapter.name())?;
    let t0 = Instant::now();
    let src = src
        .canonicalize()
        .with_context(|| format!("source {}", src.display()))?;
    std::fs::create_dir_all(store)?;
    let store = store.canonicalize()?;
    if store.starts_with(&src) && store != src {
        // allowed, but the store must not be walked
    }
    if src.starts_with(&store) {
        bail!(
            "source {} is inside the store {}",
            src.display(),
            store.display()
        );
    }
    let _lock = crate::store::lock_store_exclusive(&store).with_context(|| {
        format!(
            "another traj command holds {}",
            store.join(".lock").display()
        )
    })?;

    let mut manifest = match Manifest::load(&store) {
        Ok(m) => m,
        Err(_) if !Manifest::path(&store).exists() => Manifest {
            format: FORMAT_VERSION,
            store_id: opts.store_id.clone().unwrap_or_else(|| {
                src.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "store".into())
            }),
            source: src.display().to_string(),
            adapter: AdapterInfo {
                name: opts.adapter.name().to_string(),
                version: opts.adapter.version(),
            },
            rules: RulesInfo {
                name: opts.rules.file.name.clone(),
                version: opts.rules.file.version,
            },
            batches: Vec::new(),
        },
        Err(e) => return Err(e),
    };
    validate_adapter_name(&manifest.adapter.name)?;
    if manifest.adapter.name != opts.adapter.name() {
        bail!(
            "store was created with adapter '{}' but '{}' was requested; derived tables would disagree",
            manifest.adapter.name,
            opts.adapter.name()
        );
    }
    ensure_store_dir(&store, Path::new("catalog"))?;
    let packs_dir = ensure_store_dir(&store, Path::new("packs"))?;
    if store.join("derived").exists() {
        ensure_store_dir(&store, Path::new("derived"))?;
    }
    let orphans = remove_orphans(&store, &manifest)?;
    for o in &orphans {
        eprintln!("removed orphan {o}");
    }
    manifest.upgrade(&store)?;
    let batch_id = manifest.next_batch_id();
    let first_pack = manifest.next_pack_id();

    // 1. walk
    let skip = if store.starts_with(&src) {
        vec![store.clone()]
    } else {
        vec![]
    };
    let w = walk(&src, &opts.rules, &skip)?;
    let mut errors = w.errors.clone();

    // 2. skip unchanged paths (same size and mtime as an existing row)
    let mut existing: HashMap<String, (i64, i64, Sha)> = HashMap::new();
    let mut known_shas: HashSet<Sha> = HashSet::new();
    if !manifest.batches.is_empty() {
        let st = crate::store::Store::open_unlocked(&store)?;
        st.for_each_file(false, |r| {
            existing.insert(r.path, (r.size, r.mtime_ns, r.sha));
        })?;
        known_shas = st.index()?.keys().copied().collect();
    }
    let mut skipped = 0u64;
    let mut todo: Vec<&Candidate> = Vec::new();
    for c in &w.kept {
        if let Some((size, mtime, _)) = existing.get(&c.rel) {
            if *size == c.size as i64 && *mtime == c.mtime_ns {
                skipped += 1;
                continue;
            }
        }
        todo.push(c);
    }

    // 3. hash
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.jobs.max(1))
        .build()?;
    let hashed: Vec<Result<(usize, Sha, Option<Vec<u8>>)>> = pool.install(|| {
        todo.par_iter()
            .enumerate()
            .map(|(i, c)| match c.kind {
                Kind::Empty => Ok((i, sha_of_bytes(b""), None)),
                Kind::Symlink => {
                    let t = std::fs::read_link(&c.abs)?;
                    let b = t.as_os_str().as_encoded_bytes().to_vec();
                    Ok((i, sha_of_bytes(&b), Some(b)))
                }
                Kind::File => {
                    let (sha, _) = sha_of_file(&c.abs)?;
                    Ok((i, sha, None))
                }
            })
            .collect()
    });
    let mut rows: Vec<FileRow> = Vec::with_capacity(todo.len());
    let mut link_targets: HashMap<Sha, Vec<u8>> = HashMap::new();
    let mut ok_idx: Vec<usize> = Vec::new();
    for h in hashed {
        match h {
            Ok((i, sha, target)) => {
                let c = todo[i];
                if let Some(t) = target {
                    link_targets.insert(sha, t);
                }
                rows.push(FileRow {
                    path: c.rel.clone(),
                    kind: c.kind,
                    mode: c.mode,
                    size: c.size as i64,
                    sha,
                    mtime_ns: c.mtime_ns,
                    batch: batch_id,
                    attrs: opts.adapter.attrs(&c.rel),
                });
                ok_idx.push(i);
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    // rows are in walk order (sorted by path) because par_iter preserves order in collect
    let bytes_total: u64 = rows.iter().map(|r| r.size as u64).sum();

    // 4. pack new blobs
    let mut pw = PackWriter::with_max_bytes(&packs_dir, first_pack, max_artifact_bytes)?;
    let mut new: HashSet<Sha> = HashSet::new();
    let mut small: Vec<(Sha, PathBuf, u64)> = Vec::new();
    let mut large: Vec<(Sha, PathBuf)> = Vec::new();
    let mut new_blob_bytes = 0u64;
    for (r, &i) in rows.iter().zip(&ok_idx) {
        if r.kind == Kind::Empty || known_shas.contains(&r.sha) || !new.insert(r.sha) {
            continue;
        }
        new_blob_bytes += r.size as u64;
        let c = todo[i];
        match r.kind {
            Kind::Symlink => {} // handled below from link_targets
            Kind::File if c.size as usize <= pw.chunk_bytes() => {
                small.push((r.sha, c.abs.clone(), c.size))
            }
            Kind::File => large.push((r.sha, c.abs.clone())),
            Kind::Empty => {}
        }
    }
    for (sha, t) in &link_targets {
        if new.contains(sha) {
            pw.add_bytes(*sha, t)?;
        }
    }
    pool.install(|| pw.add_many_parallel(&small))?;
    for (sha, p) in &large {
        pw.add_file(*sha, p)?;
    }
    let (index_rows, packs, _bin, bout) = pw.finish()?;

    // 5. catalog
    let dirs = catalog::dirs_from_files(rows.iter(), batch_id);
    let segments = catalog::write_batch_segments(
        &store,
        batch_id,
        &rows,
        &dirs,
        &w.excluded,
        &index_rows,
        max_artifact_bytes,
    )?;

    // 6. derived tables
    let mut derived = Vec::new();
    if opts.derive {
        let traj: Vec<(&FileRow, &Candidate)> = rows
            .iter()
            .zip(&ok_idx)
            .filter(|(r, _)| r.kind == Kind::File && opts.adapter.is_trajectory(&r.path))
            .map(|(r, &i)| (r, todo[i]))
            .collect();
        if !traj.is_empty() {
            ensure_store_dir(&store, Path::new("derived"))?;
            let dir = ensure_store_dir(&store, &Path::new("derived").join(opts.adapter.name()))?;
            let mut ew = SegmentedEventsWriter::create(
                &dir,
                format!("events-{batch_id:04}"),
                opts.adapter.version(),
                max_artifact_bytes,
            )?;
            for (r, c) in traj {
                let bytes = std::fs::read(&c.abs)?;
                let evs = opts.adapter.parse_events(&r.path, &bytes);
                ew.push(&r.path, evs)?;
            }
            let (_, paths) = ew.finish()?;
            derived.extend(paths.into_iter().map(|path| {
                format!(
                    "{}/{}",
                    opts.adapter.name(),
                    path.file_stem().unwrap().to_string_lossy()
                )
            }));
        }
    }

    // 7. manifest
    let batch = Batch {
        id: batch_id,
        created: ts_now(),
        label: opts.label.clone(),
        paths: rows.len() as u64,
        bytes: bytes_total,
        new_blobs: new.len() as u64,
        new_blob_bytes,
        packed_bytes: bout,
        packs,
        segments,
        derived,
        excluded: w.excluded.len() as u64,
        errors,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    };
    manifest.batches.push(batch.clone());
    write_gitattributes(&store)?;
    manifest.save_with_limit(&store, max_artifact_bytes)?;
    Ok(IngestSummary {
        batch,
        skipped_unchanged: skipped,
        store,
    })
}

fn write_gitattributes(store: &Path) -> Result<()> {
    let p = store.join(".gitattributes");
    if !p.exists() {
        std::fs::write(
            p,
            "*.pack -diff -delta binary\n*.parquet -diff -delta binary\n",
        )?;
    }
    Ok(())
}
