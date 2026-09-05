//! `trajfs.toml` discovery and the two-roots rule (docs/PLAN.md §6.1).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_NAME: &str = "trajfs.toml";
pub const DEFAULT_MAX_FILE_BYTES: u64 = 65 << 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigFile {
    pub data_root: PathBuf,
    #[serde(default = "default_store_root")]
    pub store_root: PathBuf,
    #[serde(default = "default_adapter")]
    pub adapter: String,
    #[serde(default = "default_rules")]
    pub rules: String,
    #[serde(default)]
    pub hook: HookConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookConfig {
    /// Regexes on repo-relative paths that must never be committed (raw run output).
    #[serde(default)]
    pub raw_patterns: Vec<String>,
    #[serde(default = "default_max_paths")]
    pub max_added_paths: usize,
    #[serde(default = "default_max_bytes")]
    pub max_file_bytes: u64,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            raw_patterns: Vec::new(),
            max_added_paths: default_max_paths(),
            max_file_bytes: default_max_bytes(),
        }
    }
}

fn default_max_paths() -> usize {
    10_000
}
fn default_max_bytes() -> u64 {
    DEFAULT_MAX_FILE_BYTES
}

pub fn artifact_target_bytes(cfg: Option<&Config>) -> u64 {
    cfg.map(|config| config.file.hook.max_file_bytes)
        .unwrap_or(DEFAULT_MAX_FILE_BYTES)
        .min(trajfs_core::ARTIFACT_TARGET_BYTES)
}

fn default_store_root() -> PathBuf {
    "stores".into()
}
fn default_adapter() -> String {
    "none".into()
}
fn default_rules() -> String {
    "no-build-products".into()
}

#[derive(Clone, Debug)]
pub struct Config {
    pub file: ConfigFile,
    /// Directory containing trajfs.toml (the repo root).
    pub dir: PathBuf,
}

impl Config {
    pub fn find_from(start: &Path) -> Option<Config> {
        let mut cur = Some(start.to_path_buf());
        while let Some(d) = cur {
            let p = d.join(CONFIG_NAME);
            if p.is_file() {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Ok(file) = toml::from_str::<ConfigFile>(&text) {
                        return Some(Config { file, dir: d });
                    }
                }
            }
            cur = d.parent().map(|p| p.to_path_buf());
        }
        None
    }

    pub fn find() -> Option<Config> {
        if let Ok(p) = std::env::var("TRAJ_CONFIG") {
            let p = PathBuf::from(p);
            let text = std::fs::read_to_string(&p).ok()?;
            let file = toml::from_str::<ConfigFile>(&text).ok()?;
            return Some(Config {
                file,
                dir: p.parent()?.to_path_buf(),
            });
        }
        Self::find_from(&std::env::current_dir().ok()?)
    }

    pub fn require() -> Result<Config> {
        Self::find().context("no trajfs.toml found above the current directory (run `traj init` in the repo, or set TRAJ_CONFIG)")
    }

    pub fn data_root(&self) -> PathBuf {
        self.file.data_root.clone()
    }

    pub fn store_root(&self) -> PathBuf {
        if self.file.store_root.is_absolute() {
            self.file.store_root.clone()
        } else {
            self.dir.join(&self.file.store_root)
        }
    }

    /// `<store_root>/<id>.trajstore`
    pub fn store_path(&self, id: &str) -> PathBuf {
        self.store_root()
            .join(format!("{}.trajstore", id.trim_end_matches(".trajstore")))
    }

    /// The configured adapter, resolved relative to the config directory.
    pub fn adapter(&self) -> Result<Box<dyn trajfs_core::Adapter>> {
        trajfs_adapters::resolve(&self.file.adapter, Some(&self.dir))
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(&self.file)?;
        std::fs::write(self.dir.join(CONFIG_NAME), format!("# trajfs configuration (docs/PLAN.md §6.1). data_root must be outside every git work tree.\n{text}"))?;
        Ok(())
    }
}

/// Canonical path if it exists, else the lexical path.
pub fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The git work tree containing `p`, found by walking up for a `.git` entry (no git invocation).
pub fn git_toplevel(p: &Path) -> Option<PathBuf> {
    let mut cur = Some(canon(p));
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d);
        }
        cur = d.parent().map(|x| x.to_path_buf());
    }
    None
}

pub fn is_inside(child: &Path, parent: &Path) -> bool {
    canon(child).starts_with(canon(parent))
}

/// Resolve a `-S` argument: an existing store directory, or a store id under the configured store_root.
pub fn resolve_store(arg: &str, cfg: Option<&Config>) -> Result<PathBuf> {
    let p = PathBuf::from(arg);
    if p.join("MANIFEST.json").is_file() {
        return Ok(p);
    }
    if let Some(c) = cfg {
        let cand = c.store_path(arg);
        if cand.join("MANIFEST.json").is_file() {
            return Ok(cand);
        }
    }
    bail!("{arg}: not a trajfs store (no MANIFEST.json) and no such store id under the configured store_root")
}

/// Refuse the nesting mistakes of §6.1.
pub fn check_separation(
    cfg: &Config,
    src: Option<&Path>,
    store: Option<&Path>,
    strict: bool,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let data = cfg.data_root();
    let stores = cfg.store_root();
    if is_inside(&stores, &data) || is_inside(&data, &stores) {
        bail!(
            "data_root {} and store_root {} are nested",
            data.display(),
            stores.display()
        );
    }
    if let Some(top) = git_toplevel(&data) {
        bail!(
            "data_root {} is inside the git work tree {}",
            data.display(),
            top.display()
        );
    }
    if let Some(s) = store {
        if is_inside(s, &data) {
            bail!(
                "store {} is inside data_root {}",
                s.display(),
                data.display()
            );
        }
    }
    if let Some(s) = src {
        if is_inside(s, &stores) {
            bail!(
                "source {} is inside store_root {}",
                s.display(),
                stores.display()
            );
        }
        if let Some(top) = git_toplevel(s) {
            let msg = format!(
                "source {} is inside the git work tree {}; run outputs belong in data_root",
                s.display(),
                top.display()
            );
            if strict {
                bail!("{msg}");
            }
            warnings.push(msg);
        }
    }
    Ok(warnings)
}
