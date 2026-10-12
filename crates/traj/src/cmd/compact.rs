use crate::{human, one_store};
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct CompactArgs {
    /// New store directory; existing stores are never overwritten
    #[arg(long)]
    pub out: PathBuf,
    /// Zstandard level; keep the existing encoding when it is smaller
    #[arg(long, default_value_t = 19, value_parser = clap::value_parser!(i32).range(1..=22))]
    pub level: i32,
    /// Share repeated frames without recompressing their existing encodings
    #[arg(long, conflicts_with = "level")]
    pub no_recompress: bool,
    /// Print the verified before/after report as JSON
    #[arg(long)]
    pub json: bool,
}

pub fn run(stores: &[String], args: CompactArgs) -> Result<i32> {
    let store = one_store(stores)?;
    let report = trajfs_core::compact::compact(
        &store,
        &args.out,
        (!args.no_recompress).then_some(args.level),
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", report.destination);
        println!(
            "{} logical, {} unique blob bytes, {} stored -> {} stored",
            human(report.before.logical_bytes as i64),
            human(report.before.unique_blob_bytes as i64),
            human(report.before.stored_bytes as i64),
            human(report.after.stored_bytes as i64),
        );
        println!(
            "{} duplicate frames removed; all {} catalog rows and {} blobs preserved",
            report.before.duplicate_frames, report.after.catalog_rows, report.after.blobs,
        );
        println!("OK (source and destination deep-verified; source preserved)");
    }
    Ok(0)
}
