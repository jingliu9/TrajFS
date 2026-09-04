//! Generic trajectory-format parsers and the TOML-declared adapter (docs/PLAN.md §3.6).
//! Nothing in this crate knows a particular runner; a runner's adapter is a TOML file in the runner's repo.

use std::path::Path;
use trajfs_core::events::Event;
use trajfs_core::Adapter;

pub mod claude_code;
pub mod copilot_cli;
pub mod declared;
pub mod jsonl;

/// Built-in, layout-free adapters (format parsers only).
pub const BUILTIN: &[&str] = &["none", "jsonl", "copilot-cli", "claude-code"];

/// Resolve an adapter: a built-in name, `jsonl:<globs>`, or a path to an adapter TOML file
/// (relative paths are taken from `base` when given).
pub fn resolve(spec: &str, base: Option<&Path>) -> anyhow::Result<Box<dyn Adapter>> {
    if spec.ends_with(".toml") || spec.contains('/') {
        let mut p = std::path::PathBuf::from(spec);
        if p.is_relative() {
            if let Some(b) = base {
                p = b.join(p);
            }
        }
        return Ok(Box::new(declared::Declared::load(&p)?));
    }
    let (name, arg) = match spec.split_once(':') {
        Some((n, a)) => (n, Some(a)),
        None => (spec, None),
    };
    Ok(match name {
        "none" => Box::new(trajfs_core::NoAdapter),
        "jsonl" => Box::new(jsonl::Jsonl::new(arg.unwrap_or("**/*.jsonl"))?),
        "copilot-cli" => Box::new(FormatOnly {
            name: "copilot-cli",
            globs: globset_of(&["**/events.jsonl"])?,
            parser: copilot_cli::parse,
        }),
        "claude-code" => Box::new(FormatOnly {
            name: "claude-code",
            globs: globset_of(&["**/*.jsonl"])?,
            parser: claude_code::parse,
        }),
        other => anyhow::bail!(
            "unknown adapter '{other}' (built-in: {}; or a path to an adapter .toml)",
            BUILTIN.join(", ")
        ),
    })
}

/// Backwards-compatible alias.
pub fn by_name(spec: &str) -> anyhow::Result<Box<dyn Adapter>> {
    resolve(spec, None)
}

fn globset_of(globs: &[&str]) -> anyhow::Result<globset::GlobSet> {
    let mut b = globset::GlobSetBuilder::new();
    for g in globs {
        b.add(globset::Glob::new(g)?);
    }
    Ok(b.build()?)
}

/// A built-in adapter that only knows a trajectory format.
struct FormatOnly {
    name: &'static str,
    globs: globset::GlobSet,
    parser: fn(&[u8]) -> Vec<Event>,
}

impl Adapter for FormatOnly {
    fn name(&self) -> &str {
        self.name
    }
    fn is_trajectory(&self, path: &str) -> bool {
        self.globs.is_match(path)
    }
    fn parse_events(&self, _path: &str, bytes: &[u8]) -> Vec<Event> {
        (self.parser)(bytes)
    }
}

/// Shared helpers for JSON-line trajectories.
pub(crate) mod util {
    use super::Event;
    use serde_json::Value;

    pub fn ts_us(v: Option<&Value>) -> Option<i64> {
        match v? {
            Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp_micros()),
            Value::Number(n) => {
                let f = n.as_f64()?;
                Some(if f > 1e12 {
                    (f * 1_000.0) as i64
                } else {
                    (f * 1_000_000.0) as i64
                })
            }
            _ => None,
        }
    }

    pub fn str_of(v: &Value, keys: &[&str]) -> Option<String> {
        for k in keys {
            if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
        }
        None
    }

    pub fn i32_of(v: &Value, keys: &[&str]) -> Option<i32> {
        for k in keys {
            if let Some(n) = v.get(k).and_then(|x| x.as_i64()) {
                return Some(n as i32);
            }
        }
        None
    }

    /// Iterate JSON lines; unparsable lines become `type = "_unparsed"` events so no data is lost.
    pub fn lines(bytes: &[u8], mut f: impl FnMut(i32, Option<Value>, &str)) {
        let text = String::from_utf8_lossy(bytes);
        for (i, line) in text.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(v) => f(i as i32, Some(v), line),
                Err(_) => f(i as i32, None, line),
            }
        }
    }

    pub fn unparsed(seq: i32, line: &str) -> Event {
        Event {
            seq,
            r#type: "_unparsed".into(),
            payload_json: serde_json::to_string(line).unwrap_or_default(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_adapter_attrs_and_rules() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("adapter.toml");
        std::fs::write(&p, "name = \"t\"\nrules = \"r.toml\"\n[[attrs]]\npattern = '^rounds/round-(?P<round>\\d+)/(?P<role>[^/]+)(?:/|$)'\nstrip_leading_zeros = [\"round\"]\n[trajectories]\nglobs = [\"**/events.jsonl\"]\nformat = \"copilot-cli\"\n").unwrap();
        std::fs::write(dir.path().join("r.toml"), "name = \"r\"\n").unwrap();
        let a = resolve(p.to_str().unwrap(), None).unwrap();
        assert_eq!(
            a.attrs("rounds/round-0037/builder/x"),
            vec![
                ("round".to_string(), "37".to_string()),
                ("role".to_string(), "builder".to_string())
            ]
        );
        assert!(a.attrs("other").is_empty());
        assert!(a.is_trajectory("a/b/events.jsonl"));
        assert!(a.rule_profile().unwrap().ends_with("/r.toml"));
    }

    #[test]
    fn declared_adapter_labels_from_the_nearest_matching_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = dir.path().join("adapter.toml");
        std::fs::write(
            &adapter,
            "name = \"t\"\n[batch_ready]\nrun_glob = \"**/run-*\"\n\
             markers = [\"rounds/round-*/reviewer/review.json\"]\n\
             label_ancestor_pattern = '^round-\\d+$'\n",
        )
        .unwrap();
        let run = dir.path().join("run-1");
        std::fs::create_dir_all(run.join("rounds/round-0037/reviewer")).unwrap();
        std::fs::write(run.join("rounds/round-0037/reviewer/review.json"), "{}\n").unwrap();
        let declared = resolve(adapter.to_str().unwrap(), None).unwrap();
        assert_eq!(
            declared.batch_ready(&run, &[]).as_deref(),
            Some("round-0037")
        );
    }
}
