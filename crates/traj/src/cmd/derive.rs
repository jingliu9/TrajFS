use crate::config::{resolve_store, Config};
use anyhow::{Context, Result};
use clap::Args;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use trajfs_core::events::SegmentedEventsWriter;
use trajfs_core::manifest::validate_adapter_name;

#[derive(Args, Debug)]
pub struct DeriveArgs {
    /// Adapter spec (default: trajfs.toml's, else the built-in named in the store manifest)
    #[arg(long)]
    pub adapter: Option<String>,
}

/// Full rebuild of `derived/<adapter>/events-*.parquet` from the packs.
pub fn run(stores: &[String], a: DeriveArgs) -> Result<i32> {
    let cfg = Config::try_find()?;
    let store_arg = match stores {
        [] => anyhow::bail!("no store given: pass -S <store-dir|store-id> or set TRAJ_STORE"),
        [store] => store,
        _ => anyhow::bail!("this verb takes exactly one store"),
    };
    let root = resolve_store(store_arg, cfg.as_ref())?.canonicalize()?;
    let _lock = trajfs_core::store::lock_store_exclusive(&root).with_context(|| {
        format!(
            "another traj command holds {}",
            root.join(".lock").display()
        )
    })?;
    let st = trajfs_core::Store::open_for_derive_unlocked(&root)?;
    if st.manifest.batches.is_empty() {
        anyhow::bail!("store has no batches");
    }
    let spec = a
        .adapter
        .clone()
        .or_else(|| cfg.as_ref().map(|c| c.file.adapter.clone()))
        .unwrap_or_else(|| st.manifest.adapter.name.clone());
    let ad = trajfs_adapters::resolve(&spec, cfg.as_ref().map(|c| c.dir.as_path()))?;
    validate_adapter_name(ad.name())?;
    ensure_store_dir(&st.root, Path::new("derived"))?;
    let dir = ensure_store_dir(&st.root, &Path::new("derived").join(ad.name()))?;
    let old_events: BTreeSet<PathBuf> = st
        .derived_segments()
        .iter()
        .filter(|path| is_event_artifact(path))
        .cloned()
        .collect();
    remove_unpublished_events(&st.root.join("derived"), &old_events)?;
    let generation = next_generation(&dir)?;
    let stem = format!("events-rebuild-{generation:04}");
    let build = (|| -> Result<(usize, usize, Vec<PathBuf>)> {
        let rows = st.files_under("", false)?;
        let mut reader = st.reader();
        let mut ew = SegmentedEventsWriter::create(
            &dir,
            &stem,
            ad.version(),
            crate::config::artifact_target_bytes(cfg.as_ref()),
        )?;
        let mut n_traj = 0;
        for r in rows
            .iter()
            .filter(|r| r.kind == trajfs_core::Kind::File && ad.is_trajectory(&r.path))
        {
            let bytes = st.read_row(&mut reader, r, false)?;
            ew.push(&r.path, ad.parse_events(&r.path, &bytes))?;
            n_traj += 1;
        }
        let (events, paths) = ew.finish()?;
        Ok((events, n_traj, paths))
    })();
    let (n, n_traj, paths) = match build {
        Ok(result) => result,
        Err(error) => {
            remove_generation(&dir, &stem);
            return Err(error);
        }
    };
    let derived: Vec<String> = paths
        .iter()
        .map(|path| {
            format!(
                "{}/{}",
                ad.name(),
                path.file_stem().unwrap().to_string_lossy()
            )
        })
        .collect();
    let mut manifest = st.manifest.clone();
    if let Err(error) = manifest.upgrade(&st.root) {
        remove_generation(&dir, &stem);
        return Err(error);
    }
    for batch in &mut manifest.batches {
        batch
            .derived
            .retain(|entry| !is_event_artifact(Path::new(entry)));
    }
    let last = manifest.batches.last_mut().unwrap();
    last.derived.extend(derived);
    if let Err(error) =
        manifest.save_with_limit(&st.root, crate::config::artifact_target_bytes(cfg.as_ref()))
    {
        remove_generation(&dir, &stem);
        return Err(error);
    }
    for path in old_events {
        if !paths.contains(&path) {
            let _ = std::fs::remove_file(path);
        }
    }
    let active: BTreeSet<PathBuf> = paths.iter().cloned().collect();
    if let Err(error) = remove_unpublished_events(&st.root.join("derived"), &active) {
        eprintln!(
            "warning: derived generation was published, but old artifacts could not be removed: {error:#}"
        );
    }
    println!(
        "{n} events from {n_traj} trajectories -> {} segment(s) under {}",
        paths.len(),
        dir.display()
    );
    Ok(0)
}

fn is_event_artifact(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("events-"))
}

fn ensure_store_dir(store: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        anyhow::bail!("unsafe store artifact directory {}", relative.display());
    }
    let path = store.join(relative);
    if !path.exists() {
        let parent = path.parent().context("store directory has no parent")?;
        let resolved_parent = parent.canonicalize()?;
        if !resolved_parent.starts_with(store) {
            anyhow::bail!(
                "store artifact directory {} has a parent outside {}",
                path.display(),
                store.display()
            );
        }
        std::fs::create_dir(&path)?;
    }
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(store) || !resolved.is_dir() {
        anyhow::bail!(
            "store artifact directory {} is not a directory within {}",
            path.display(),
            store.display()
        );
    }
    Ok(resolved)
}

fn sorted_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(|left, right| {
        trajfs_core::manifest::artifact_path_cmp(&left.path(), &right.path())
    });
    Ok(entries)
}

fn next_generation(dir: &Path) -> Result<u64> {
    let mut max_generation = 0;
    for entry in sorted_entries(dir)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("events-rebuild-") else {
            continue;
        };
        let Some(stem) = rest.strip_suffix(".parquet") else {
            continue;
        };
        if let Some(generation) = stem
            .split('-')
            .next()
            .and_then(|part| part.parse::<u64>().ok())
        {
            max_generation = max_generation.max(generation);
        }
    }
    max_generation
        .checked_add(1)
        .context("derived generation id overflow")
}

fn remove_generation(dir: &Path, stem: &str) {
    let Ok(entries) = sorted_entries(dir) else {
        return;
    };
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == format!("{stem}.parquet")
            || name.starts_with(&format!("{stem}-"))
            || name.starts_with(&format!("{stem}.parquet."))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn remove_unpublished_events(derived: &Path, active: &BTreeSet<PathBuf>) -> Result<()> {
    if !derived.is_dir() {
        return Ok(());
    }
    for adapter in sorted_entries(derived)? {
        if !adapter.file_type()?.is_dir() {
            continue;
        }
        for entry in sorted_entries(&adapter.path())? {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_event_artifact(&path)
                && (name.ends_with(".tmp")
                    || (name.ends_with(".parquet") && !active.contains(&path)))
            {
                std::fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}
