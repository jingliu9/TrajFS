use crate::{norm, one_store};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::Write;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct CatArgs {
    #[arg(required = true)]
    pub paths: Vec<String>,
    /// Skip the sha check
    #[arg(long)]
    pub no_verify: bool,
}

pub fn cat(stores: &[String], a: CatArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let mut reader = st.reader();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for p in &a.paths {
        let p = norm(p);
        let Some(r) = st.stat(&p)? else { bail!("{p}: not in the store") };
        let bytes = st.read_row(&mut reader, &r, !a.no_verify)?;
        out.write_all(&bytes)?;
    }
    Ok(0)
}

#[derive(Args, Debug)]
pub struct ExtractArgs {
    /// File or directory in the store ("" or "." for everything)
    pub path: String,
    pub dst: PathBuf,
    /// Hard-link files with identical content
    #[arg(long)]
    pub hardlink_dedupe: bool,
    /// Restore recorded mtimes
    #[arg(long)]
    pub mtime: bool,
    #[arg(long)]
    pub no_verify: bool,
}

pub fn extract(stores: &[String], a: ExtractArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let p = norm(&a.path);
    let n = st.extract(&p, &a.dst, a.hardlink_dedupe, a.mtime, !a.no_verify)?;
    eprintln!("{n} entries written to {}", a.dst.display());
    Ok(0)
}

#[derive(Args, Debug)]
pub struct EditArgs {
    pub path: String,
}

/// Extract to a temp file, run $EDITOR, print a unified diff; the store is never modified.
pub fn edit(stores: &[String], a: EditArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let p = norm(&a.path);
    let Some(r) = st.stat(&p)? else { bail!("{p}: not in the store") };
    let mut reader = st.reader();
    let original = st.read_row(&mut reader, &r, true)?;
    let tmpdir = std::env::temp_dir().join(format!("traj-edit-{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir)?;
    let tmp = tmpdir.join(r.name());
    std::fs::write(&tmp, &original)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&tmp).status().with_context(|| format!("run {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}");
    }
    let edited = std::fs::read(&tmp)?;
    if edited == original {
        eprintln!("unchanged");
    } else {
        let a_s = String::from_utf8_lossy(&original);
        let b_s = String::from_utf8_lossy(&edited);
        let diff = similar::TextDiff::from_lines(&a_s, &b_s);
        print!("{}", diff.unified_diff().context_radius(3).header(&format!("store/{p}"), &tmp.display().to_string()));
        eprintln!("edited copy left at {} (the store is read-only)", tmp.display());
    }
    Ok(0)
}
