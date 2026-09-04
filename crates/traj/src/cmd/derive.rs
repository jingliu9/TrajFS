use crate::one_store;
use anyhow::Result;
use clap::Args;
use trajfs_core::events::EventsWriter;

#[derive(Args, Debug)]
pub struct DeriveArgs {
    /// Adapter spec (default: trajfs.toml's, else the built-in named in the store manifest)
    #[arg(long)]
    pub adapter: Option<String>,
}

/// Full rebuild of `derived/<adapter>/events-*.parquet` from the packs.
pub fn run(stores: &[String], a: DeriveArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let cfg = crate::config::Config::find();
    let spec = a.adapter.clone().or_else(|| cfg.as_ref().map(|c| c.file.adapter.clone())).unwrap_or_else(|| st.manifest.adapter.name.clone());
    let ad = trajfs_adapters::resolve(&spec, cfg.as_ref().map(|c| c.dir.as_path()))?;
    let dir = st.root.join("derived").join(ad.name());
    std::fs::create_dir_all(&dir)?;
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        if e.file_name().to_string_lossy().starts_with("events-") {
            std::fs::remove_file(e.path())?;
        }
    }
    let rows = st.files_under("", false)?;
    let mut reader = st.reader();
    let out = dir.join("events-0000.parquet");
    let mut ew = EventsWriter::create(&out, ad.version())?;
    let mut n_traj = 0;
    for r in rows.iter().filter(|r| r.kind == trajfs_core::Kind::File && ad.is_trajectory(&r.path)) {
        let bytes = st.read_row(&mut reader, r, false)?;
        ew.push(&r.path, &ad.parse_events(&r.path, &bytes))?;
        n_traj += 1;
    }
    let n = ew.finish()?;
    println!("{n} events from {n_traj} trajectories -> {}", out.display());
    Ok(0)
}
