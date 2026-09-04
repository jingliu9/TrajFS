use crate::{norm, one_store};
use anyhow::Result;
use clap::Args;
use globset::Glob;
use regex::bytes::RegexSet;
use std::collections::HashMap;
use std::io::Write;
use trajfs_core::Sha;

#[derive(Args, Debug)]
pub struct GrepArgs {
    /// Pattern (repeatable, any match)
    #[arg(short = 'e', long = "regexp", required = true)]
    pub patterns: Vec<String>,
    /// Restrict to paths below this directory
    #[arg(long, default_value = "")]
    pub path: String,
    /// Glob on the basename
    #[arg(long)]
    pub name: Option<String>,
    /// Print matching paths only
    #[arg(short = 'l')]
    pub files_only: bool,
    /// Print match counts per path
    #[arg(short = 'c')]
    pub count: bool,
    /// Also search blobs that look binary
    #[arg(short = 'a')]
    pub binary: bool,
    /// Case-insensitive
    #[arg(short = 'i')]
    pub ignore_case: bool,
}

fn looks_binary(b: &[u8]) -> bool {
    b.iter().take(8192).any(|&c| c == 0)
}

pub fn run(stores: &[String], a: GrepArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let pats: Vec<String> = a.patterns.iter().map(|p| if a.ignore_case { format!("(?i){p}") } else { p.clone() }).collect();
    let set = RegexSet::new(&pats)?;
    let name_g = a.name.as_deref().map(|g| Glob::new(g)).transpose()?.map(|g| g.compile_matcher());
    let dir = norm(&a.path);
    let mut by_sha: HashMap<Sha, Vec<String>> = HashMap::new();
    st.scan_under(&dir, false, |p| name_g.as_ref().map(|g| g.is_match(trajfs_core::basename_of(p))).unwrap_or(true), |r| {
        if r.kind != trajfs_core::Kind::Empty {
            by_sha.entry(r.sha).or_default().push(r.path);
        }
    })?;
    // read blobs in pack order for locality
    let index = st.index()?;
    let mut shas: Vec<&Sha> = by_sha.keys().collect();
    shas.sort_by_key(|s| index.get(*s).map(|l| (l[0].pack, l[0].chunk_offset, l[0].offset)).unwrap_or((u32::MAX, 0, 0)));
    let mut reader = st.reader();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut hits = 0u64;
    for sha in shas {
        let bytes = match st.read_sha(&mut reader, sha) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{}: {e:#}", hex::encode(sha));
                continue;
            }
        };
        if !a.binary && looks_binary(&bytes) {
            continue;
        }
        if !set.is_match(&bytes) {
            continue;
        }
        let mut lines: Vec<(usize, &[u8])> = Vec::new();
        for (i, line) in bytes.split(|&c| c == b'\n').enumerate() {
            if set.is_match(line) {
                lines.push((i + 1, line));
            }
        }
        if lines.is_empty() {
            continue;
        }
        let mut paths = by_sha[sha].clone();
        paths.sort();
        for p in paths {
            hits += 1;
            if a.files_only {
                writeln!(out, "{p}")?;
            } else if a.count {
                writeln!(out, "{p}:{}", lines.len())?;
            } else {
                for (n, l) in &lines {
                    write!(out, "{p}:{n}:")?;
                    out.write_all(l)?;
                    out.write_all(b"\n")?;
                }
            }
        }
    }
    Ok(if hits > 0 { 0 } else { 1 })
}
