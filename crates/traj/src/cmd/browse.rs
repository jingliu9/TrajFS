use crate::{human, norm, one_store};
use anyhow::{bail, Result};
use clap::Args;
use globset::Glob;
use std::collections::HashSet;
use trajfs_core::{FileRow, Kind};

fn mode_str(r: &FileRow) -> String {
    match r.kind {
        Kind::Symlink => "lrwxrwxrwx".into(),
        _ => {
            let m = r.mode;
            let b = |bit: u16, c: char| if m & bit != 0 { c } else { '-' };
            format!("-{}{}{}{}{}{}{}{}{}", b(0o400, 'r'), b(0o200, 'w'), b(0o100, 'x'), b(0o40, 'r'), b(0o20, 'w'), b(0o10, 'x'), b(0o4, 'r'), b(0o2, 'w'), b(0o1, 'x'))
        }
    }
}

fn attrs_str(r: &FileRow) -> String {
    r.attrs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}

#[derive(Args, Debug)]
pub struct LsArgs {
    #[arg(default_value = "")]
    pub dir: String,
    /// Long format: kind/mode, size, attrs
    #[arg(short = 'l')]
    pub long: bool,
    /// Recursive
    #[arg(short = 'R')]
    pub recursive: bool,
}

pub fn ls(stores: &[String], a: LsArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let dir = norm(&a.dir);
    if !dir.is_empty() && st.dir_info(&dir)?.is_none() {
        if let Some(r) = st.stat(&dir)? {
            print_file(&r, a.long, true);
            return Ok(0);
        }
        bail!("{dir}: no such directory in the store");
    }
    if a.recursive {
        let mut dirs = vec![dir.clone()];
        dirs.extend(st.dirs_under(&dir)?.into_iter().map(|d| d.dir));
        for d in dirs {
            println!("{}:", if d.is_empty() { "." } else { &d });
            let (subs, files) = st.children(&d)?;
            print_children(&subs, &files, a.long);
            println!();
        }
    } else {
        let (subs, files) = st.children(&dir)?;
        print_children(&subs, &files, a.long);
    }
    Ok(0)
}

fn print_children(subs: &[trajfs_core::store::DirEntry], files: &[FileRow], long: bool) {
    for d in subs {
        if long {
            println!("d---------  {:>10}  {:>6} files  {}/", human(d.bytes), d.n_files, d.name);
        } else {
            println!("{}/", d.name);
        }
    }
    for f in files {
        print_file(f, long, false);
    }
}

fn print_file(f: &FileRow, long: bool, full: bool) {
    let name = if full { f.path.as_str() } else { f.name() };
    if long {
        let a = attrs_str(f);
        println!("{}  {:>10}  {}{}", mode_str(f), human(f.size), name, if a.is_empty() { String::new() } else { format!("  [{a}]") });
    } else {
        println!("{name}");
    }
}

#[derive(Args, Debug)]
pub struct TreeArgs {
    #[arg(default_value = "")]
    pub dir: String,
    #[arg(long, default_value_t = 3)]
    pub depth: usize,
    /// Directories only
    #[arg(short = 'd')]
    pub dirs_only: bool,
}

pub fn tree(stores: &[String], a: TreeArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let dir = norm(&a.dir);
    println!("{}", if dir.is_empty() { "." } else { &dir });
    walk_tree(&st, &dir, 1, a.depth, a.dirs_only)?;
    Ok(0)
}

fn walk_tree(st: &trajfs_core::Store, dir: &str, level: usize, max: usize, dirs_only: bool) -> Result<()> {
    if level > max {
        return Ok(());
    }
    let (subs, files) = st.children(dir)?;
    let pad = "  ".repeat(level);
    for d in &subs {
        println!("{pad}{}/  ({} files, {})", d.name, d.n_files, human(d.bytes));
        walk_tree(st, &d.dir, level + 1, max, dirs_only)?;
    }
    if !dirs_only {
        for f in &files {
            println!("{pad}{}", f.name());
        }
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct FindArgs {
    #[arg(default_value = "")]
    pub dir: String,
    /// Glob on the basename
    #[arg(long)]
    pub name: Option<String>,
    /// Glob on the full path
    #[arg(long)]
    pub path: Option<String>,
    /// f (file) | l (symlink) | e (empty)
    #[arg(long)]
    pub kind: Option<String>,
    /// +N (larger than) or -N (smaller than) bytes
    #[arg(long)]
    pub size: Option<String>,
    /// key=value, all must match
    #[arg(long = "attr")]
    pub attrs: Vec<String>,
    /// Print size and sha too
    #[arg(short = 'l')]
    pub long: bool,
}

pub fn find(stores: &[String], a: FindArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let dir = norm(&a.dir);
    let name_g = a.name.as_deref().map(|g| Glob::new(g)).transpose()?.map(|g| g.compile_matcher());
    let path_g = a.path.as_deref().map(|g| Glob::new(g)).transpose()?.map(|g| g.compile_matcher());
    let kind = match a.kind.as_deref() {
        None => None,
        Some("f") => Some(Kind::File),
        Some("l") => Some(Kind::Symlink),
        Some("e") => Some(Kind::Empty),
        Some(k) => bail!("--kind {k}: use f, l or e"),
    };
    let size = match a.size.as_deref() {
        None => None,
        Some(s) if s.starts_with('+') => Some((true, s[1..].parse::<i64>()?)),
        Some(s) if s.starts_with('-') => Some((false, s[1..].parse::<i64>()?)),
        Some(s) => bail!("--size {s}: use +N or -N"),
    };
    let attrs: Vec<(String, String)> = a.attrs.iter().map(|s| match s.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => bail!("--attr {s}: use key=value"),
    }).collect::<Result<_>>()?;
    let mut n = 0;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    use std::io::Write;
    let pre = |p: &str| {
        let name = trajfs_core::basename_of(p);
        name_g.as_ref().map(|g| g.is_match(name)).unwrap_or(true) && path_g.as_ref().map(|g| g.is_match(p)).unwrap_or(true)
    };
    st.scan_under(&dir, !attrs.is_empty(), pre, |r| {
        if let Some(k) = kind {
            if r.kind != k {
                return;
            }
        }
        if let Some((larger, n)) = size {
            if larger && r.size <= n || !larger && r.size >= n {
                return;
            }
        }
        if !attrs.iter().all(|(k, v)| r.attrs.iter().any(|(rk, rv)| rk == k && rv == v)) {
            return;
        }
        if a.long {
            let _ = writeln!(out, "{:>12} {} {}", r.size, hex::encode(r.sha), r.path);
        } else {
            let _ = writeln!(out, "{}", r.path);
        }
        n += 1;
    })?;
    let _ = out.flush();
    eprintln!("{n} paths");
    Ok(0)
}

#[derive(Args, Debug)]
pub struct DuArgs {
    #[arg(default_value = "")]
    pub dir: String,
    /// Also list subdirectories down to this depth
    #[arg(long, default_value_t = 0)]
    pub depth: usize,
}

pub fn du(stores: &[String], a: DuArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let dir = norm(&a.dir);
    let info = st.dir_info(&dir)?;
    let Some(info) = info else { bail!("{dir}: no such directory in the store") };
    let mut distinct: HashSet<[u8; 32]> = HashSet::new();
    let mut dedup = 0i64;
    let mut n_rows = 0usize;
    st.scan_under(&dir, false, |_| true, |r| {
        n_rows += 1;
        if r.kind != Kind::Empty && distinct.insert(r.sha) {
            dedup += r.size;
        }
    })?;
    println!("{:>12} raw   {:>12} deduplicated   {:>9} files  {:>6} blobs  {}", human(info.bytes), human(dedup), n_rows, distinct.len(), if dir.is_empty() { "." } else { &dir });
    if a.depth > 0 {
        let base_depth = if dir.is_empty() { 0 } else { dir.matches('/').count() + 1 };
        for d in st.dirs_under(&dir)? {
            if !d.dir.is_empty() && (d.depth as usize) <= base_depth + a.depth {
                println!("{:>12} raw   {:>9} files  {}", human(d.bytes), d.n_files, d.dir);
            }
        }
    }
    Ok(0)
}

#[derive(Args, Debug)]
pub struct StatArgs {
    pub path: String,
}

pub fn stat(stores: &[String], a: StatArgs) -> Result<i32> {
    let st = one_store(stores)?;
    let p = norm(&a.path);
    let Some(r) = st.stat(&p)? else {
        if let Some(d) = st.dir_info(&p)? {
            println!("path:     {}\nkind:     directory\nfiles:    {}\nsubdirs:  {}\nbytes:    {}", p, d.n_files, d.n_dirs, d.bytes);
            return Ok(0);
        }
        bail!("{p}: not in the store");
    };
    println!("path:     {}", r.path);
    println!("kind:     {:?}", r.kind);
    println!("mode:     {:o}", r.mode);
    println!("size:     {}", r.size);
    println!("sha256:   {}", hex::encode(r.sha));
    println!("mtime:    {}", chrono::DateTime::from_timestamp_nanos(r.mtime_ns).to_rfc3339());
    println!("batch:    {}", r.batch);
    println!("attrs:    {}", attrs_str(&r));
    if r.kind != Kind::Empty {
        for l in st.parts(&r.sha)? {
            println!("part {:>3}: pack {:04} frame@{} len {} offset {} size {}", l.part, l.pack, l.chunk_offset, l.chunk_len, l.offset, l.size);
        }
    }
    Ok(0)
}
