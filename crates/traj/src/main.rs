//! `traj`: the trajfs command line (docs/PLAN.md §5, §6.1, §12, §13).

mod cmd;
mod config;
#[cfg(all(feature = "mount", target_os = "linux"))]
mod mount;

use anyhow::Result;
use clap::{Parser, Subcommand};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "traj",
    version,
    about = "content-addressed store for agent run trees"
)]
struct Cli {
    /// Store directory or store id (repeatable for `sql`). Defaults to TRAJ_STORE.
    #[arg(short = 'S', long = "store", global = true, env = "TRAJ_STORE")]
    store: Vec<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set up a repo: trajfs.toml, roots, .gitattributes, pre-commit hook, agent skill
    Init(cmd::init::InitArgs),
    /// Check roots, hook, skill and tracked raw-run paths
    Doctor,
    /// Pack a source tree into a store (new batch)
    Pack(cmd::pack::PackArgs),
    /// Pack automatically whenever the adapter reports a batch ready
    Watch(cmd::pack::WatchArgs),
    /// List a directory
    Ls(cmd::browse::LsArgs),
    /// Print a directory tree
    Tree(cmd::browse::TreeArgs),
    /// Find paths by name, glob, kind, size or attribute
    Find(cmd::browse::FindArgs),
    /// Bytes and counts, raw and deduplicated
    Du(cmd::browse::DuArgs),
    /// Show one catalog row and its pack location
    Stat(cmd::browse::StatArgs),
    /// Write file contents to stdout
    Cat(cmd::read::CatArgs),
    /// Materialise a file or subtree
    Extract(cmd::read::ExtractArgs),
    /// Open a temp copy in $EDITOR and print the diff (never written back)
    Edit(cmd::read::EditArgs),
    /// Regex search over distinct blobs, hits mapped to every path
    Grep(cmd::grep::GrepArgs),
    /// Run SQL over the catalog and derived tables (DuckDB)
    Sql(cmd::sql::SqlArgs),
    /// (Re)build derived tables from packs
    Derive(cmd::derive::DeriveArgs),
    /// Consistency check; --deep re-hashes every blob
    Verify(cmd::verify::VerifyArgs),
    /// Stage the store's new files, commit, optionally push
    Commit(cmd::commit::CommitArgs),
    /// Export the agent skill
    Skill(cmd::skill::SkillArgs),
    /// Git hook entry points (also reached when the binary is invoked as `pre-commit`)
    Hook(cmd::hook::HookArgs),
    /// Measure pack and verb timings; write the regression-baseline JSON
    Bench(cmd::bench::BenchArgs),
    /// Mount a store (or every store under store_root) as a read-only directory tree (FUSE)
    Mount(cmd::mount::MountArgs),
    /// Unmount a `traj mount` (all of them when no mountpoint is given)
    Umount(cmd::mount::UmountArgs),
}

fn main() {
    // Invoked through a hook symlink: `.git/hooks/pre-commit -> traj`
    let argv0 = std::env::args().next().unwrap_or_default();
    let base = std::path::Path::new(&argv0)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let code = if base == "pre-commit" {
        run(cmd::hook::pre_commit())
    } else {
        let cli = Cli::parse();
        run(dispatch(cli))
    };
    std::process::exit(code);
}

fn run(r: Result<i32>) -> i32 {
    match r {
        Ok(c) => c,
        Err(e) => {
            eprintln!("traj: {e:#}");
            2
        }
    }
}

fn dispatch(cli: Cli) -> Result<i32> {
    let stores = cli.store;
    match cli.cmd {
        Cmd::Init(a) => cmd::init::run(a),
        Cmd::Doctor => cmd::init::doctor(),
        Cmd::Pack(a) => cmd::pack::run(a),
        Cmd::Watch(a) => cmd::pack::watch(a),
        Cmd::Ls(a) => cmd::browse::ls(&stores, a),
        Cmd::Tree(a) => cmd::browse::tree(&stores, a),
        Cmd::Find(a) => cmd::browse::find(&stores, a),
        Cmd::Du(a) => cmd::browse::du(&stores, a),
        Cmd::Stat(a) => cmd::browse::stat(&stores, a),
        Cmd::Cat(a) => cmd::read::cat(&stores, a),
        Cmd::Extract(a) => cmd::read::extract(&stores, a),
        Cmd::Edit(a) => cmd::read::edit(&stores, a),
        Cmd::Grep(a) => cmd::grep::run(&stores, a),
        Cmd::Sql(a) => cmd::sql::run(&stores, a),
        Cmd::Derive(a) => cmd::derive::run(&stores, a),
        Cmd::Verify(a) => cmd::verify::run(&stores, a),
        Cmd::Commit(a) => cmd::commit::run(a),
        Cmd::Skill(a) => cmd::skill::run(a),
        Cmd::Hook(a) => cmd::hook::run(a),
        Cmd::Bench(a) => cmd::bench::run(&stores, a),
        Cmd::Mount(a) => cmd::mount::run(&stores, a),
        Cmd::Umount(a) => cmd::mount::umount(a),
    }
}

/// The single store named by `-S` (or TRAJ_STORE).
pub(crate) fn one_store(stores: &[String]) -> Result<trajfs_core::Store> {
    let cfg = config::Config::try_find()?;
    let arg = match stores {
        [] => anyhow::bail!("no store given: pass -S <store-dir|store-id> or set TRAJ_STORE"),
        [s] => s,
        _ => anyhow::bail!("this verb takes exactly one store"),
    };
    let p = config::resolve_store(arg, cfg.as_ref())?;
    trajfs_core::Store::open(&p)
}

pub(crate) fn human(n: i64) -> String {
    let f = n as f64;
    if f < 1024.0 {
        format!("{n} B")
    } else if f < 1024.0 * 1024.0 {
        format!("{:.1} KB", f / 1024.0)
    } else if f < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", f / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", f / 1024.0 / 1024.0 / 1024.0)
    }
}

/// Normalise a user path argument: trim `./`, leading and trailing `/`.
pub(crate) fn norm(p: &str) -> String {
    let mut s = p;
    while let Some(r) = s.strip_prefix("./") {
        s = r;
    }
    let s = s.trim_matches('/');
    if s == "." {
        String::new()
    } else {
        s.to_string()
    }
}
