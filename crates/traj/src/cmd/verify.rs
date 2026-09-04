use crate::one_store;
use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Re-hash every blob
    #[arg(long)]
    pub deep: bool,
}

pub fn run(stores: &[String], a: VerifyArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let rep = st.verify(a.deep)?;
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
    if rep.ok() {
        println!("OK{}", if a.deep { " (deep)" } else { "" });
        Ok(0)
    } else {
        println!("FAILED");
        Ok(1)
    }
}
