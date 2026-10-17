use crate::one_store;
use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Re-hash every blob
    #[arg(long)]
    pub deep: bool,
    /// Parallel hashing workers (at most eight readers and sixteen total pack handles)
    #[arg(long, default_value_t = super::pack::default_jobs().min(8))]
    pub jobs: usize,
    /// Audit logical, unique and stored bytes, including duplicate physical frames
    #[arg(long)]
    pub storage: bool,
    /// Fail if identical frames or unreferenced pack bytes remain
    #[arg(long, requires = "deep")]
    pub require_compact: bool,
    /// Print a machine-readable verification and optional storage report
    #[arg(long)]
    pub json: bool,
}

pub fn run(stores: &[String], a: VerifyArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let rep = st.verify_with_jobs(a.deep, a.jobs)?;
    let storage = if a.storage || a.require_compact {
        Some(trajfs_core::compact::storage_report_with_jobs(&st, a.jobs)?)
    } else {
        None
    };
    let compact = !a.require_compact || storage.as_ref().is_some_and(|s| s.compact());
    let ok = rep.ok() && compact;
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": ok,
                "deep": a.deep,
                "require_compact": a.require_compact,
                "verification": rep,
                "storage": storage,
            }))?
        );
        return Ok(if ok { 0 } else { 1 });
    }
    println!(
        "files: {}  blobs: {}  shadowed paths: {}",
        rep.files, rep.blobs, rep.shadowed_paths
    );
    for (label, v) in [
        ("missing packs", &rep.missing_packs),
        ("catalog rows without a blob", &rep.missing_blobs),
        ("bad parts", &rep.bad_parts),
        ("corrupt blobs", &rep.corrupt),
    ] {
        if !v.is_empty() {
            println!("{label}: {}", v.len());
            for x in v.iter().take(20) {
                println!("  {x}");
            }
        }
    }
    if let Some(storage) = &storage {
        println!(
            "bytes: {} logical, {} unique blobs, {} packs, {} metadata, {} stored",
            storage.logical_bytes,
            storage.unique_blob_bytes,
            storage.pack_bytes,
            storage.metadata_bytes,
            storage.stored_bytes,
        );
        println!(
            "duplicate frames: {} ({} bytes); unreferenced pack bytes: {}",
            storage.duplicate_frames,
            storage.duplicate_frame_bytes,
            storage.unreferenced_pack_bytes,
        );
    }
    if ok {
        println!("OK{}", if a.deep { " (deep)" } else { "" });
        Ok(0)
    } else {
        println!("FAILED");
        Ok(1)
    }
}
