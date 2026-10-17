//! Read-only, stratified compression sample; this is not a store verifier.
use anyhow::{ensure, Context, Result};
use rayon::prelude::*;
use std::{collections::BTreeSet, env, path::Path, time::Instant};
use trajfs_core::Store;

fn main() -> Result<()> {
    let args: Vec<_> = env::args().collect();
    ensure!(
        args.len() == 3,
        "usage: compaction_bench STORE SAMPLE_FRAMES"
    );
    let count: usize = args[2].parse()?;
    ensure!(
        (1..=256).contains(&count),
        "sample must contain 1–256 frames"
    );
    let store = Store::open(Path::new(&args[1]))?;
    let mut keys = BTreeSet::new();
    for parts in store.index()?.values() {
        for loc in parts {
            keys.insert((loc.pack, loc.chunk_offset, loc.chunk_len));
        }
    }
    let keys: Vec<_> = keys.into_iter().collect();
    ensure!(!keys.is_empty(), "store has no frames");
    let mut reader = store.reader();
    let mut sample = Vec::new();
    for n in 0..count.min(keys.len()) {
        let key = keys[n * keys.len() / count.min(keys.len())];
        let raw = reader.frame(key.0, key.1, key.2)?.to_vec();
        sample.push((raw, usize::try_from(key.2)?));
    }
    let raw_bytes: usize = sample.iter().map(|x| x.0.len()).sum();
    let existing_bytes: usize = sample.iter().map(|x| x.1).sum();
    println!(
        "{}",
        serde_json::json!({"phase":"sample-ready", "frames":sample.len(),
        "source_frames":keys.len(), "raw_bytes":raw_bytes, "existing_encoded_bytes":existing_bytes})
    );
    for level in [3, 9, 19] {
        for jobs in [1, 8, 24] {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;
            let started = Instant::now();
            let encoded: Vec<Result<Vec<u8>>> = pool.install(|| {
                sample
                    .par_iter()
                    .map(|(raw, _)| zstd::bulk::compress(raw, level).context("sample compression"))
                    .collect()
            });
            let seconds = started.elapsed().as_secs_f64();
            let mut kept_bytes = 0;
            for ((raw, original_len), candidate) in sample.iter().zip(encoded) {
                let candidate = candidate?;
                ensure!(
                    zstd::bulk::decompress(&candidate, raw.len())? == *raw,
                    "sample compression changed content"
                );
                kept_bytes += candidate.len().min(*original_len);
            }
            println!(
                "{}",
                serde_json::json!({"phase":"compression-sample", "level":level,
                "jobs":jobs, "frames":sample.len(), "raw_bytes":raw_bytes,
                "existing_encoded_bytes":existing_bytes, "kept_encoded_bytes":kept_bytes,
                "compression_seconds":seconds, "raw_mib_per_second":raw_bytes as f64 / 1048576.0 / seconds,
                "retained_fraction":kept_bytes as f64 / existing_bytes as f64,
                "sample_roundtrip_passed":true,
                "scope":"compression only; full archive verification, catalog work and transport excluded"})
            );
        }
    }
    Ok(())
}
