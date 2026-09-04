//! `traj pack`: walk → filter → hash → dedupe → pack → catalog → derived → manifest (PLAN.md §4).

use crate::adapter::Adapter;
use crate::catalog;
use crate::events::EventsWriter;
use crate::hash::{sha_of_bytes, sha_of_file};
use crate::manifest::{AdapterInfo, Batch, Manifest, RulesInfo};
use crate::pack::PackWriter;
use crate::rules::Rules;
use crate::walk::{walk, Candidate};
use crate::{FileRow, Kind, Sha, CHUNK_BYTES, FORMAT_VERSION};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
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

/// Remove files a crashed batch left behind: anything newer than the manifest knows about.
fn remove_orphans(store: &Path, m: &Manifest) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    let next_batch = m.next_batch_id();
    let next_pack = m.next_pack_id();
    for (dir, prefixes) in [("catalog", vec!["files-", "dirs-", "excluded-"]), ("packs", vec!["index-"])] {
        let d = store.join(dir);
        if !d.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let n = e.file_name().to_string_lossy().to_string();
            let mut orphan = n.ends_with(".tmp");
            for p in &prefixes {
                if let Some(rest) = n.strip_prefix(p) {
                    if let Some(id) = rest.strip_suffix(".parquet").and_then(|s| s.parse::<u32>().ok()) {
                        if id >= next_batch {
                            orphan = true;
                        }
                    }
                }
            }
            if dir == "packs" {
                if let Some(id) = n.strip_suffix(".pack").and_then(|s| s.parse::<u32>().ok()) {
                    if id >= next_pack {
                        orphan = true;
                    }
                }
            }
            if orphan {
                std::fs::remove_file(e.path())?;
                removed.push(format!("{dir}/{n}"));
            }
        }
    }
    let derived = store.join("derived");
    if derived.is_dir() {
        for a in std::fs::read_dir(&derived)? {
            let a = a?;
            if !a.path().is_dir() {
                continue;
            }
            for e in std::fs::read_dir(a.path())? {
                let e = e?;
                let n = e.file_name().to_string_lossy().to_string();
                let id = n.rsplit_once('-').and_then(|(_, r)| r.strip_suffix(".parquet")).and_then(|s| s.parse::<u32>().ok());
                if n.ends_with(".tmp") || id.map(|i| i >= next_batch).unwrap_or(false) {
                    std::fs::remove_file(e.path())?;
                    removed.push(format!("derived/{}/{n}", a.file_name().to_string_lossy()));
                }
            }
        }
    }
    Ok(removed)
}

pub fn ingest(src: &Path, store: &Path, opts: IngestOptions) -> Result<IngestSummary> {
    let t0 = Instant::now();
    let src = src.canonicalize().with_context(|| format!("source {}", src.display()))?;
    std::fs::create_dir_all(store)?;
    let store = store.canonicalize()?;
    if store.starts_with(&src) && store != src {
        // allowed, but the store must not be walked
    }
    if src.starts_with(&store) {
        bail!("source {} is inside the store {}", src.display(), store.display());
    }
    let lock = std::fs::File::create(store.join(".lock"))?;
    lock.try_lock_exclusive().with_context(|| format!("another traj pack holds {}", store.join(".lock").display()))?;

    let mut manifest = match Manifest::load(&store) {
        Ok(m) => m,
        Err(_) if !Manifest::path(&store).exists() => Manifest {
            format: FORMAT_VERSION,
            store_id: opts.store_id.clone().unwrap_or_else(|| {
                src.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "store".into())
            }),
            source: src.display().to_string(),
            adapter: AdapterInfo { name: opts.adapter.name().to_string(), version: opts.adapter.version() },
            rules: RulesInfo { name: opts.rules.file.name.clone(), version: opts.rules.file.version },
            batches: Vec::new(),
        },
        Err(e) => return Err(e),
    };
    if manifest.adapter.name != opts.adapter.name() {
        bail!(
            "store was created with adapter '{}' but '{}' was requested; derived tables would disagree",
            manifest.adapter.name,
            opts.adapter.name()
        );
    }
    let orphans = remove_orphans(&store, &manifest)?;
    for o in &orphans {
        eprintln!("removed orphan {o}");
    }
    let batch_id = manifest.next_batch_id();
    let first_pack = manifest.next_pack_id();
    std::fs::create_dir_all(store.join("catalog"))?;
    std::fs::create_dir_all(store.join("packs"))?;

    // 1. walk
    let skip = if store.starts_with(&src) { vec![store.clone()] } else { vec![] };
    let w = walk(&src, &opts.rules, &skip)?;
    let mut errors = w.errors.clone();

    // 2. skip unchanged paths (same size and mtime as an existing row)
    let mut existing: HashMap<String, (i64, i64, Sha)> = HashMap::new();
    let mut known_shas: HashSet<Sha> = HashSet::new();
    if !manifest.batches.is_empty() {
        let st = crate::store::Store::open(&store)?;
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
    let pool = rayon::ThreadPoolBuilder::new().num_threads(opts.jobs.max(1)).build()?;
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
            Kind::File if c.size as usize <= CHUNK_BYTES => small.push((r.sha, c.abs.clone(), c.size)),
            Kind::File => large.push((r.sha, c.abs.clone())),
            Kind::Empty => {}
        }
    }
    let mut pw = PackWriter::new(&store.join("packs"), first_pack)?;
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
    let index_path = store.join("packs").join(format!("index-{batch_id:04}.parquet"));
    catalog::write_index(&index_path, &index_rows)?;

    // 5. catalog
    let files_path = store.join("catalog").join(format!("files-{batch_id:04}.parquet"));
    let mut fw = catalog::FilesWriter::create(&files_path);
    for r in &rows {
        fw.push(r)?;
    }
    fw.finish()?;
    let dirs = catalog::dirs_from_files(rows.iter(), batch_id);
    catalog::write_dirs(&store.join("catalog").join(format!("dirs-{batch_id:04}.parquet")), &dirs)?;
    catalog::write_excluded(&store.join("catalog").join(format!("excluded-{batch_id:04}.parquet")), &w.excluded, batch_id)?;

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
            let dir = store.join("derived").join(opts.adapter.name());
            std::fs::create_dir_all(&dir)?;
            let p = dir.join(format!("events-{batch_id:04}.parquet"));
            let mut ew = EventsWriter::create(&p, opts.adapter.version())?;
            for (r, c) in traj {
                let bytes = std::fs::read(&c.abs)?;
                let evs = opts.adapter.parse_events(&r.path, &bytes);
                ew.push(&r.path, &evs)?;
            }
            ew.finish()?;
            derived.push(format!("{}/events-{batch_id:04}", opts.adapter.name()));
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
        segments: vec![format!("files-{batch_id:04}")],
        derived,
        excluded: w.excluded.len() as u64,
        errors,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    };
    manifest.batches.push(batch.clone());
    manifest.save(&store)?;
    write_gitattributes(&store)?;
    Ok(IngestSummary { batch, skipped_unchanged: skipped, store })
}

fn write_gitattributes(store: &Path) -> Result<()> {
    let p = store.join(".gitattributes");
    if !p.exists() {
        std::fs::write(p, "*.pack -diff -delta binary\n*.parquet -diff -delta binary\n")?;
    }
    Ok(())
}
