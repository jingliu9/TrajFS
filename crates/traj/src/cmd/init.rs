use crate::config::{
    canon, check_separation, git_toplevel, is_inside, Config, ConfigFile, CONFIG_NAME,
};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Absolute directory for raw run trees; must be outside every git work tree
    #[arg(long)]
    pub data_root: PathBuf,
    /// Directory for stores, relative to the repo root
    #[arg(long, default_value = "stores")]
    pub store_root: PathBuf,
    /// Adapter: a built-in name (none, jsonl, copilot-cli, claude-code) or a path to this repo's adapter TOML
    #[arg(long)]
    pub adapter: Option<String>,
    /// Write trajfs/adapter.toml and trajfs/rules.toml templates into this repo for its agent to complete
    #[arg(long)]
    pub scaffold_adapter: bool,
    /// Do not ask about scaffolding an adapter when none is given
    #[arg(long)]
    pub no_adapter: bool,
    /// Rule profile when the adapter does not name one
    #[arg(long, default_value = "no-build-products")]
    pub rules: String,
    /// Extra regexes on repo-relative paths the pre-commit hook must refuse
    #[arg(long = "raw-pattern")]
    pub raw_patterns: Vec<String>,
    /// Replace an existing pre-commit hook that is not traj
    #[arg(long)]
    pub force: bool,
    /// Do not install the agent skill files
    #[arg(long)]
    pub no_skill: bool,
    /// Default mountpoint for `traj mount` (absolute, outside every git work tree)
    #[arg(long)]
    pub mount_root: Option<PathBuf>,
}

pub fn run(a: InitArgs) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let repo = git_toplevel(&cwd).context("traj init must run inside a git repository")?;
    if !a.data_root.is_absolute() {
        bail!("--data-root must be an absolute path");
    }
    trajfs_core::rules::Rules::resolve(&a.rules)?;
    std::fs::create_dir_all(&a.data_root)?;
    // adapter: given, scaffolded, or asked for (docs/PLAN.md §3.6: the adapter belongs to the target repo)
    let mut adapter = a.adapter.clone();
    let mut scaffold = a.scaffold_adapter;
    if adapter.is_none()
        && !scaffold
        && !a.no_adapter
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
    {
        eprint!("No adapter given. Write trajfs/adapter.toml + trajfs/rules.toml templates for this repo's agent to fill in? [Y/n] ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        scaffold = !line.trim().to_ascii_lowercase().starts_with('n');
    }
    if scaffold {
        let dir = repo.join("trajfs");
        std::fs::create_dir_all(&dir)?;
        for (name, text) in [
            ("adapter.toml", trajfs_adapters::declared::TEMPLATE),
            ("rules.toml", trajfs_adapters::declared::RULES_TEMPLATE),
        ] {
            let p = dir.join(name);
            if !p.exists() {
                std::fs::write(&p, text)?;
            }
        }
        adapter = Some("trajfs/adapter.toml".into());
        println!("scaffolded trajfs/adapter.toml and trajfs/rules.toml: fill them in (see the traj skill, section Adapter)");
    }
    let adapter = adapter.unwrap_or_else(|| "none".into());
    let ad = trajfs_adapters::resolve(&adapter, Some(&repo))?;
    let mut raw_patterns = ad.raw_patterns();
    raw_patterns.extend(a.raw_patterns.iter().cloned());
    let hook = crate::config::HookConfig {
        raw_patterns,
        ..Default::default()
    };
    let cfg = Config {
        file: ConfigFile {
            data_root: canon(&a.data_root),
            store_root: a.store_root.clone(),
            adapter: adapter.clone(),
            rules: a.rules.clone(),
            mount_root: a.mount_root.clone(),
            hook,
        },
        dir: repo.clone(),
    };
    check_separation(&cfg, None, None, true)?;
    cfg.save()?;
    let store_root = cfg.store_root();
    std::fs::create_dir_all(&store_root)?;
    std::fs::write(store_root.join(".gitkeep"), "")?;
    std::fs::write(
        store_root.join(".gitattributes"),
        "*.pack -diff -delta binary\n*.parquet -diff -delta binary\n",
    )?;
    let ignore = a.data_root.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "# trajfs data_root: raw run trees are never committed; pack them with `traj pack`.\n*\n")?;
    }
    install_hook(&repo, a.force)?;
    if let Some(mr) = &a.mount_root {
        let mr = if mr.is_absolute() {
            mr.clone()
        } else {
            repo.join(mr)
        };
        let f = crate::cmd::mount::write_vscode_exclude(&repo, &mr)?;
        println!(
            "mount_root {}; VS Code watcher exclude written to {}",
            mr.display(),
            f.display()
        );
    }
    if !a.no_skill {
        crate::cmd::skill::export_all(&cfg)?;
    }
    println!(
        "initialised {} (data_root {}, store_root {}, adapter {}, rules {}, {} hook patterns)",
        repo.join(CONFIG_NAME).display(),
        cfg.data_root().display(),
        store_root.display(),
        adapter,
        a.rules,
        cfg.file.hook.raw_patterns.len()
    );
    println!("next: traj doctor; then commit {CONFIG_NAME}, {}/.gitkeep, .claude/skills/traj, AGENTS.md{}", a.store_root.display(), if scaffold { ", trajfs/" } else { "" });
    Ok(0)
}

pub fn hook_path(repo: &Path) -> PathBuf {
    repo.join(".git").join("hooks").join("pre-commit")
}

pub fn install_hook(repo: &Path, force: bool) -> Result<()> {
    let hook = hook_path(repo);
    let me = std::env::current_exe()?;
    if let Ok(md) = std::fs::symlink_metadata(&hook) {
        let is_ours = md.file_type().is_symlink()
            && std::fs::read_link(&hook)
                .map(|t| t.file_name().map(|n| n == "traj").unwrap_or(false))
                .unwrap_or(false);
        if !is_ours && !force {
            bail!(
                "{} exists and is not a traj hook; pass --force to replace it",
                hook.display()
            );
        }
        std::fs::remove_file(&hook)?;
    }
    std::fs::create_dir_all(hook.parent().unwrap())?;
    std::os::unix::fs::symlink(&me, &hook)
        .with_context(|| format!("symlink {} -> {}", hook.display(), me.display()))?;
    Ok(())
}

pub fn doctor() -> Result<i32> {
    let mut problems = 0;
    let Some(cfg) = Config::find() else {
        println!(
            "config:      none (no {CONFIG_NAME} above {}); run `traj init --data-root <abs dir>`",
            std::env::current_dir()?.display()
        );
        return Ok(1);
    };
    println!("config:      {}", cfg.dir.join(CONFIG_NAME).display());
    println!(
        "data_root:   {}{}",
        cfg.data_root().display(),
        if cfg.data_root().is_dir() {
            ""
        } else {
            "  (MISSING)"
        }
    );
    println!("store_root:  {}", cfg.store_root().display());
    match cfg.adapter() {
        Ok(ad) => println!(
            "adapter:     {} (name {}, v{}, {} hook patterns)",
            cfg.file.adapter,
            ad.name(),
            ad.version(),
            cfg.file.hook.raw_patterns.len()
        ),
        Err(e) => {
            println!(
                "adapter:     PROBLEM {} cannot be loaded: {e:#}",
                cfg.file.adapter
            );
            problems += 1;
        }
    }
    println!("rules:       {}", cfg.file.rules);
    if cfg.file.hook.raw_patterns.is_empty() {
        println!("hook rules:  WARNING no raw_patterns; the hook only enforces path-count and size limits (set [hook] raw_patterns or the adapter's [hook])");
    }
    match check_separation(&cfg, None, None, true) {
        Ok(_) => println!("separation:  OK (roots not nested, data_root outside git)"),
        Err(e) => {
            println!("separation:  PROBLEM {e:#}");
            problems += 1;
        }
    }
    if !cfg.data_root().is_dir() {
        problems += 1;
    }
    let hook = hook_path(&cfg.dir);
    match std::fs::read_link(&hook) {
        Ok(t) if t.exists() => println!("hook:        {} -> {}", hook.display(), t.display()),
        Ok(t) => {
            println!(
                "hook:        PROBLEM symlink target {} does not exist",
                t.display()
            );
            problems += 1;
        }
        Err(_) => {
            println!(
                "hook:        PROBLEM {} is not the traj binary (run `traj init` again)",
                hook.display()
            );
            problems += 1;
        }
    }
    let skill = cfg
        .dir
        .join(".claude")
        .join("skills")
        .join("traj")
        .join("SKILL.md");
    match std::fs::read_to_string(&skill) {
        Ok(text) => {
            let v = crate::cmd::skill::version_in(&text);
            if v.as_deref() == Some(crate::VERSION) {
                println!(
                    "skill:       {} (version {})",
                    skill.display(),
                    crate::VERSION
                );
            } else {
                println!("skill:       PROBLEM version {} differs from binary {}; run `traj skill export`", v.unwrap_or_else(|| "?".into()), crate::VERSION);
                problems += 1;
            }
        }
        Err(_) => {
            println!(
                "skill:       PROBLEM {} missing; run `traj skill export`",
                skill.display()
            );
            problems += 1;
        }
    }
    // tracked raw-run paths
    let tracked = crate::cmd::hook::tracked_raw_paths(&cfg.dir)?;
    if tracked.is_empty() {
        println!("tracked:     no raw-run paths tracked in git");
    } else {
        println!("tracked:     PROBLEM {}{} raw-run paths are tracked in git (migrate: `git rm -r --cached <dir>` is allowed by the hook), e.g. {}", if tracked.len() >= 1000 { "at least " } else { "" }, tracked.len(), tracked[0]);
        problems += 1;
    }
    // stores
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(cfg.store_root()) {
        for e in rd.flatten() {
            if e.path().join("MANIFEST.json").is_file() {
                n += 1;
            }
        }
    }
    println!("stores:      {n} under {}", cfg.store_root().display());
    // live traj mounts (docs/PLAN-fuse.md §3)
    let mounts = crate::cmd::mount::traj_mounts();
    if mounts.is_empty() {
        println!("mounts:      none");
    }
    for mp in mounts {
        let stale = crate::cmd::mount::is_stale(&mp);
        let in_repo = git_toplevel(&mp).is_some();
        let in_roots = is_inside(&mp, &cfg.data_root()) || is_inside(&mp, &cfg.store_root());
        if stale {
            println!(
                "mounts:      PROBLEM {} is stale (its traj process is gone); run `traj umount {}`",
                mp.display(),
                mp.display()
            );
            problems += 1;
        } else if in_repo || in_roots {
            println!("mounts:      PROBLEM {} is inside a git work tree or a root; unmount and mount elsewhere", mp.display());
            problems += 1;
        } else {
            println!("mounts:      {}", mp.display());
        }
    }
    let _ = is_inside;
    println!(
        "{}",
        if problems == 0 {
            "OK"
        } else {
            "PROBLEMS: see above"
        }
    );
    Ok(if problems == 0 { 0 } else { 1 })
}
