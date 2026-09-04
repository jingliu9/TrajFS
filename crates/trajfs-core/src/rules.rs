//! Exclusion rule profiles (docs/PLAN.md §4.2), TOML, shipped in the crate or given as a file.

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuleFile {
    pub name: String,
    #[serde(default = "one")]
    pub version: u32,
    /// Directory names excluded wherever they appear (with their whole subtree).
    #[serde(default)]
    pub exclude_dirs: Vec<String>,
    /// File extensions (without dot) excluded everywhere.
    #[serde(default)]
    pub exclude_ext: Vec<String>,
    /// Files larger than this are excluded everywhere (0 = no limit).
    #[serde(default)]
    pub max_bytes: u64,
    /// Extension-less regular files at least this large whose first bytes are ELF are excluded (0 = off).
    #[serde(default)]
    pub elf_min_bytes: u64,
    /// Extra rules that apply only below directories whose name starts with one of `dirs`.
    #[serde(default)]
    pub exclude_under: Vec<Under>,
    /// Globs (relative to the source root) that are always kept, overriding everything above.
    #[serde(default)]
    pub always_keep: Vec<String>,
    /// Globs that are always excluded, even when `always_keep` matches an ancestor.
    #[serde(default)]
    pub always_exclude: Vec<String>,
}

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Under {
    /// Directory-name prefixes (e.g. `logs`, `results`).
    pub dirs: Vec<String>,
    #[serde(default)]
    pub ext: Vec<String>,
    #[serde(default)]
    pub max_bytes: u64,
}

/// Compiled profile.
pub struct Rules {
    pub file: RuleFile,
    keep: GlobSet,
    exclude: GlobSet,
}

/// Outcome for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Keep,
    Exclude(&'static str),
}

pub const PROFILE_NONE: &str = include_str!("../../../rules/none.toml");
pub const PROFILE_NO_BUILD_PRODUCTS: &str = include_str!("../../../rules/no-build-products.toml");

impl Rules {
    pub fn from_toml(text: &str) -> Result<Rules> {
        let file: RuleFile = toml::from_str(text).context("parse rule profile")?;
        Self::compile(file)
    }

    pub fn compile(file: RuleFile) -> Result<Rules> {
        let mut kb = GlobSetBuilder::new();
        for g in &file.always_keep {
            kb.add(Glob::new(g).with_context(|| format!("glob {g}"))?);
        }
        let mut eb = GlobSetBuilder::new();
        for g in &file.always_exclude {
            eb.add(Glob::new(g).with_context(|| format!("glob {g}"))?);
        }
        Ok(Rules {
            file,
            keep: kb.build()?,
            exclude: eb.build()?,
        })
    }

    /// Resolve a profile by built-in name (`none`, `no-build-products`) or by path to a TOML file
    /// (relative paths are taken from `base`, when given).
    pub fn resolve(name_or_path: &str) -> Result<Rules> {
        Self::resolve_from(name_or_path, None)
    }

    pub fn resolve_from(name_or_path: &str, base: Option<&Path>) -> Result<Rules> {
        match name_or_path {
            "none" => Self::from_toml(PROFILE_NONE),
            "no-build-products" => Self::from_toml(PROFILE_NO_BUILD_PRODUCTS),
            p => {
                let mut path = std::path::PathBuf::from(p);
                if path.is_relative() {
                    if let Some(b) = base {
                        path = b.join(&path);
                    }
                }
                if !path.is_file() {
                    bail!("unknown rule profile '{p}' (built-in: none, no-build-products; or a path to a .toml file)");
                }
                Self::from_toml(
                    &std::fs::read_to_string(&path)
                        .with_context(|| format!("read {}", path.display()))?,
                )
            }
        }
    }

    /// Decide for a regular file or symlink. `rel` uses `/` separators. `is_elf` is evaluated lazily by the caller
    /// through the closure only when the rule needs it.
    pub fn decide(
        &self,
        rel: &str,
        size: u64,
        is_symlink: bool,
        is_elf: &dyn Fn() -> bool,
    ) -> Decision {
        if self.exclude.is_match(rel) {
            return Decision::Exclude("always_exclude");
        }
        if self.keep.is_match(rel) {
            return Decision::Keep;
        }
        let comps: Vec<&str> = rel.split('/').collect();
        let (dirs, name) = comps.split_at(comps.len() - 1);
        let name = name[0];
        for d in dirs {
            if self.file.exclude_dirs.iter().any(|x| x == d) {
                return Decision::Exclude("exclude_dirs");
            }
        }
        if is_symlink {
            return Decision::Keep;
        }
        let ext = match name.rfind('.') {
            Some(i) if i > 0 => &name[i + 1..],
            _ => "",
        };
        if !ext.is_empty() && self.file.exclude_ext.iter().any(|x| x == ext) {
            return Decision::Exclude("exclude_ext");
        }
        if self.file.max_bytes > 0 && size > self.file.max_bytes {
            return Decision::Exclude("max_bytes");
        }
        for u in &self.file.exclude_under {
            let under = dirs
                .iter()
                .any(|d| u.dirs.iter().any(|p| d.starts_with(p.as_str())));
            if !under {
                continue;
            }
            if !ext.is_empty() && u.ext.iter().any(|x| x == ext) {
                return Decision::Exclude("exclude_under.ext");
            }
            if u.max_bytes > 0 && size > u.max_bytes {
                return Decision::Exclude("exclude_under.max_bytes");
            }
        }
        if ext.is_empty()
            && self.file.elf_min_bytes > 0
            && size >= self.file.elf_min_bytes
            && is_elf()
        {
            return Decision::Exclude("elf");
        }
        Decision::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_parse() {
        for p in ["none", "no-build-products"] {
            Rules::resolve(p).unwrap();
        }
    }

    const EVIDENCE_PROFILE: &str = r#"
name = "evidence-archive"
version = 1
exclude_dirs = [".cache", "bin"]
exclude_ext = ["pyc"]
max_bytes = 20971520
elf_min_bytes = 65536
always_keep = ["**/call*/**"]
always_exclude = ["**/call*/**/.cache/**"]
[[exclude_under]]
dirs = ["logs", "results"]
ext = ["csv"]
max_bytes = 2097152
"#;

    #[test]
    fn rule_engine_semantics() {
        let r = Rules::from_toml(EVIDENCE_PROFILE).unwrap();
        let no = &|| false;
        assert_eq!(
            r.decide("a/.cache/x.json", 10, false, no),
            Decision::Exclude("exclude_dirs")
        );
        assert_eq!(
            r.decide("a/x.pyc", 10, false, no),
            Decision::Exclude("exclude_ext")
        );
        assert_eq!(
            r.decide("a/logs/round3/out.csv", 10, false, no),
            Decision::Exclude("exclude_under.ext")
        );
        assert_eq!(
            r.decide("a/logs/round3/out.stdout", 3 << 20, false, no),
            Decision::Exclude("exclude_under.max_bytes")
        );
        assert_eq!(
            r.decide("a/logs/round3/out.stdout", 100, false, no),
            Decision::Keep
        );
        assert_eq!(
            r.decide("a/big.txt", 30 << 20, false, no),
            Decision::Exclude("max_bytes")
        );
        assert_eq!(
            r.decide(
                "rounds/round-0001/builder/call/events.jsonl",
                30 << 20,
                false,
                no
            ),
            Decision::Keep
        );
        assert_eq!(
            r.decide("rounds/round-0001/builder/call/.cache/x", 1, false, no),
            Decision::Exclude("always_exclude")
        );
        assert_eq!(
            r.decide("bin/tool", 100 << 10, false, &|| true),
            Decision::Exclude("exclude_dirs")
        );
        assert_eq!(
            r.decide("src/tool", 100 << 10, false, &|| true),
            Decision::Exclude("elf")
        );
    }
}
