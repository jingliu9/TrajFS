//! A TOML-declared adapter: the runner's own repo describes its layout; trajfs supplies the format parsers.

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use trajfs_core::events::Event;
use trajfs_core::Adapter;

#[derive(Deserialize, Debug, Default)]
pub struct AdapterFile {
    pub name: String,
    #[serde(default = "one")]
    pub version: u16,
    /// Built-in rule profile name or a path relative to this file.
    pub rules: Option<String>,
    #[serde(default)]
    pub attrs: Vec<AttrRule>,
    pub trajectories: Option<Trajectories>,
    pub batch_ready: Option<BatchReady>,
    pub hook: Option<HookSpec>,
}

fn one() -> u16 {
    1
}

#[derive(Deserialize, Debug, Default)]
pub struct AttrRule {
    /// Regex on the store path; named capture groups become attribute keys.
    /// Later matching rules replace earlier values with the same key.
    pub pattern: String,
    #[serde(default)]
    pub strip_leading_zeros: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct Trajectories {
    pub globs: Vec<String>,
    /// copilot-cli | codex-cli | claude-code | jsonl | auto
    pub format: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct BatchReady {
    /// Glob relative to data_root selecting run directories (default `*`).
    #[serde(default = "star")]
    pub run_glob: String,
    /// Globs relative to a run directory; a match means the batch labelled by the marker's parent dir is ready.
    #[serde(default)]
    pub markers: Vec<String>,
    /// Optional regex selecting the nearest marker ancestor used as the label.
    pub label_ancestor_pattern: Option<String>,
}

fn star() -> String {
    "*".into()
}

#[derive(Deserialize, Debug, Default)]
pub struct HookSpec {
    #[serde(default)]
    pub raw_patterns: Vec<String>,
}

pub type Parser = fn(&[u8]) -> Vec<Event>;

/// The built-in parser for a `[trajectories].format` name.
///
/// # Examples
///
/// ```
/// use trajfs_adapters::declared::parser_for;
/// let parse = parser_for("jsonl").unwrap();
/// assert_eq!(parse(b"{\"type\":\"x\"}")[0].r#type, "x");
/// assert!(parser_for("yaml").is_err());
/// ```
pub fn parser_for(format: &str) -> Result<Parser> {
    Ok(match format {
        "copilot-cli" => crate::copilot_cli::parse,
        "codex-cli" => crate::codex_cli::parse,
        "auto" => crate::auto::parse,
        "claude-code" => crate::claude_code::parse,
        "jsonl" => crate::jsonl::parse,
        other => bail!("unknown trajectory format '{other}' (copilot-cli, codex-cli, claude-code, jsonl, auto)"),
    })
}

pub struct Declared {
    pub file: AdapterFile,
    pub path: PathBuf,
    attrs: Vec<(Regex, Vec<String>)>,
    traj_globs: Option<GlobSet>,
    parser: Option<Parser>,
    markers: Option<GlobSet>,
    label_ancestor: Option<Regex>,
}

impl Declared {
    pub fn load(path: &Path) -> Result<Declared> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read adapter {}", path.display()))?;
        let file: AdapterFile =
            toml::from_str(&text).with_context(|| format!("parse adapter {}", path.display()))?;
        let mut attrs = Vec::new();
        for a in &file.attrs {
            attrs.push((
                Regex::new(&a.pattern).with_context(|| format!("attrs pattern {}", a.pattern))?,
                a.strip_leading_zeros.clone(),
            ));
        }
        let (traj_globs, parser) = match &file.trajectories {
            Some(t) => {
                let mut b = GlobSetBuilder::new();
                for g in &t.globs {
                    b.add(Glob::new(g)?);
                }
                (Some(b.build()?), Some(parser_for(&t.format)?))
            }
            None => (None, None),
        };
        let markers = match &file.batch_ready {
            Some(br) if !br.markers.is_empty() => {
                let mut b = GlobSetBuilder::new();
                for g in &br.markers {
                    b.add(Glob::new(g)?);
                }
                Some(b.build()?)
            }
            _ => None,
        };
        let label_ancestor = file
            .batch_ready
            .as_ref()
            .and_then(|ready| ready.label_ancestor_pattern.as_ref())
            .map(|pattern| {
                Regex::new(pattern)
                    .with_context(|| format!("batch label ancestor pattern {pattern}"))
            })
            .transpose()?;
        Ok(Declared {
            path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
            file,
            attrs,
            traj_globs,
            parser,
            markers,
            label_ancestor,
        })
    }
}

impl Adapter for Declared {
    fn name(&self) -> &str {
        &self.file.name
    }
    fn version(&self) -> u16 {
        self.file.version
    }
    fn attrs(&self, path: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for (re, strip) in &self.attrs {
            if let Some(c) = re.captures(path) {
                for name in re.capture_names().flatten() {
                    if let Some(m) = c.name(name) {
                        let mut v = m.as_str();
                        if strip.iter().any(|s| s == name) {
                            v = v.trim_start_matches('0');
                            if v.is_empty() {
                                v = "0";
                            }
                        }
                        if let Some((_, value)) = out.iter_mut().find(|(key, _)| key == name) {
                            *value = v.to_string();
                        } else {
                            out.push((name.to_string(), v.to_string()));
                        }
                    }
                }
            }
        }
        out
    }
    fn is_trajectory(&self, path: &str) -> bool {
        self.traj_globs
            .as_ref()
            .map(|g| g.is_match(path))
            .unwrap_or(false)
    }
    fn parse_events(&self, _path: &str, bytes: &[u8]) -> Vec<Event> {
        match self.parser {
            Some(p) => p(bytes),
            None => Vec::new(),
        }
    }
    fn rule_profile(&self) -> Option<String> {
        let r = self.file.rules.as_ref()?;
        if r == "none" || r == "no-build-products" {
            return Some(r.clone());
        }
        let p = PathBuf::from(r);
        let p = if p.is_relative() {
            self.path.parent().unwrap_or(Path::new(".")).join(p)
        } else {
            p
        };
        Some(p.display().to_string())
    }
    fn run_glob(&self) -> Option<String> {
        self.file.batch_ready.as_ref().map(|b| b.run_glob.clone())
    }
    fn batch_ready(&self, src: &Path, already: &[String]) -> Option<String> {
        match self.try_batch_ready(src, already) {
            Ok(label) => label,
            Err(error) => {
                eprintln!("adapter readiness failed: {error:#}");
                None
            }
        }
    }
    fn try_batch_ready(&self, src: &Path, already: &[String]) -> Result<Option<String>> {
        let Some(markers) = self.markers.as_ref() else {
            return Ok(None);
        };
        let mut labels: Vec<String> = Vec::new();
        for e in walkdir::WalkDir::new(src).min_depth(1).into_iter() {
            let e = e.with_context(|| format!("inspect readiness under {}", src.display()))?;
            if !e.file_type().is_file() {
                continue;
            }
            let rel = e.path().strip_prefix(src)?.to_string_lossy().to_string();
            if markers.is_match(&rel) {
                let label = self
                    .label_ancestor
                    .as_ref()
                    .and_then(|pattern| {
                        e.path()
                            .parent()?
                            .ancestors()
                            .take_while(|ancestor| *ancestor != src)
                            .filter_map(|ancestor| ancestor.file_name())
                            .map(|name| name.to_string_lossy().to_string())
                            .find(|name| pattern.is_match(name))
                    })
                    .or_else(|| {
                        e.path()
                            .parent()
                            .and_then(|parent| parent.file_name())
                            .map(|name| name.to_string_lossy().to_string())
                    })
                    .unwrap_or(rel);
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        }
        labels.sort();
        Ok(labels.into_iter().rev().find(|l| !already.contains(l)))
    }
    fn raw_patterns(&self) -> Vec<String> {
        self.file
            .hook
            .as_ref()
            .map(|h| h.raw_patterns.clone())
            .unwrap_or_default()
    }
}

/// Template written by `traj init --scaffold-adapter` for the target repo's agent to complete.
pub const TEMPLATE: &str = r#"# trajfs adapter for this repository (see the traj skill: "Adapter").
# Fill in the layout of the run trees this repo's runner produces. trajfs itself knows nothing about them.
name = "my-runner"
version = 1
# rule profile: "none", "no-build-products", or a path relative to this file (see rules.toml next to it)
rules = "rules.toml"

# path attributes recorded in files.attrs: named capture groups become attribute keys
# [[attrs]]
# pattern = '^episodes/(?P<episode>\d+)/(?P<stage>[^/]+)(?:/|$)'
# strip_leading_zeros = ["episode"]

# which blobs are trajectories and how to split them into events (copilot-cli | codex-cli | claude-code | jsonl | auto)
# [trajectories]
# globs = ["**/events.jsonl"]
# format = "jsonl"

# for `traj watch`: run directories under data_root, and marker files that say a batch is complete
# By default the label is the marker parent's basename. Set
# label_ancestor_pattern to select the nearest matching ancestor instead.
# [batch_ready]
# run_glob = "*"
# markers = ["episodes/*/DONE"]
# label_ancestor_pattern = '^episode-\d+$'

# copied into trajfs.toml [hook] by `traj init`: staged paths matching these regexes are refused by the pre-commit hook
[hook]
raw_patterns = []
"#;

pub const RULES_TEMPLATE: &str = r#"# rule profile for this repository's run trees (fields: see trajfs docs/PLAN.md §4.2)
name = "my-runner-archive"
version = 1
exclude_dirs = [".git", ".cache", "node_modules", "__pycache__", ".venv", "venv", "target", "build"]
exclude_ext = ["pyc", "so", "o", "a", "wasm", "whl"]
max_bytes = 20971520
elf_min_bytes = 65536
# always_keep = ["**/trajectories/**"]
# [[exclude_under]]
# dirs = ["logs", "results"]
# ext = ["csv", "jsonl"]
# max_bytes = 2097152
"#;
