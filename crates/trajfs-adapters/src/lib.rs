//! Generic trajectory-format parsers and the TOML-declared adapter (docs/PLAN.md §3.6).
//! Nothing in this crate knows a particular runner; a runner's adapter is a TOML file in the runner's repo.

use std::path::Path;
use trajfs_core::events::Event;
use trajfs_core::Adapter;

pub mod auto;
pub mod claude_code;
pub mod codex_cli;
pub mod copilot_cli;
pub mod declared;
pub mod jsonl;

/// Built-in, layout-free adapters (format parsers only).
pub const BUILTIN: &[&str] = &[
    "none",
    "jsonl",
    "copilot-cli",
    "codex-cli",
    "claude-code",
    "auto",
];

/// Resolve an adapter: a built-in name, `jsonl:<globs>`, or a path to an adapter TOML file
/// (relative paths are taken from `base` when given).
pub fn resolve(spec: &str, base: Option<&Path>) -> anyhow::Result<Box<dyn Adapter>> {
    if let Some(globs) = spec.strip_prefix("jsonl:") {
        return Ok(Box::new(jsonl::Jsonl::new(globs)?));
    }
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
        "codex-cli" => Box::new(FormatOnly {
            name: "codex-cli",
            globs: globset_of(&["**/events.jsonl"])?,
            parser: codex_cli::parse,
        }),
        "auto" => Box::new(FormatOnly {
            name: "auto",
            globs: globset_of(&["**/events.jsonl"])?,
            parser: auto::parse,
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
                // Numeric epochs below 10^12 in magnitude are seconds; larger ones are milliseconds.
                if let Some(n) = n.as_i64() {
                    let scale = if n.unsigned_abs() >= 1_000_000_000_000 {
                        1_000
                    } else {
                        1_000_000
                    };
                    return n.checked_mul(scale);
                }
                let f = n.as_f64()?;
                let micros = if f.abs() >= 1e12 {
                    f * 1_000.0
                } else {
                    f * 1_000_000.0
                };
                if micros.is_finite() && micros >= i64::MIN as f64 && micros < -(i64::MIN as f64) {
                    Some(micros as i64)
                } else {
                    None
                }
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
            if let Some(n) = v
                .get(k)
                .and_then(|x| x.as_i64())
                .and_then(|n| i32::try_from(n).ok())
            {
                return Some(n);
            }
        }
        None
    }

    /// Iterate JSON lines; unparsable lines become `type = "_unparsed"` events so no data is lost.
    pub fn lines(bytes: &[u8], mut f: impl FnMut(i32, Option<Value>, &[u8])) {
        for (i, raw) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
            let line = match raw.strip_suffix(b"\n") {
                Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
                None => raw,
            };
            if line.iter().all(|byte| matches!(byte, b' ' | b'\t' | b'\r')) {
                continue;
            }
            match serde_json::from_slice::<Value>(line) {
                Ok(v) => f(i as i32, Some(v), line),
                Err(_) => f(i as i32, None, line),
            }
        }
    }

    /// UTF-8 lines stay JSON strings; invalid UTF-8 uses `{"bytes": [...]}` without replacement.
    pub fn unparsed(seq: i32, line: &[u8]) -> Event {
        Event {
            seq,
            r#type: "_unparsed".into(),
            payload_json: match std::str::from_utf8(line) {
                Ok(line) => serde_json::to_string(line).expect("serialize a string"),
                Err(_) => serde_json::json!({ "bytes": line }).to_string(),
            },
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_globs_with_slashes_are_not_adapter_file_paths() {
        let adapter = resolve("jsonl:**/events.jsonl,logs/*.jsonl", None).unwrap();
        assert!(adapter.is_trajectory("rounds/round-001/events.jsonl"));
        assert!(adapter.is_trajectory("logs/other.jsonl"));
        assert!(!adapter.is_trajectory("notes.txt"));
    }

    #[test]
    fn numeric_envelopes_do_not_wrap_or_saturate() {
        let events = jsonl::parse(
            br#"{"timestamp":1000000000000,"exit_code":4294967296,"returncode":7}
{"timestamp":-1000000000000,"exit_code":-2147483649}
{"timestamp":9223372036854775807}
{"timestamp":18446744073709551615}
{"timestamp":1e50}
{"timestamp":-1e50}
{"timestamp":1.25,"exit_code":-2147483648}
{"timestamp":1756947602000,"exit_code":2147483647}"#,
        );
        assert_eq!(events[0].ts_us, Some(1_000_000_000_000_000));
        assert_eq!(events[0].exit_code, Some(7));
        assert_eq!(events[1].ts_us, Some(-1_000_000_000_000_000));
        assert_eq!(events[1].exit_code, None);
        for event in &events[2..6] {
            assert_eq!(event.ts_us, None);
        }
        assert_eq!(events[6].ts_us, Some(1_250_000));
        assert_eq!(events[6].exit_code, Some(i32::MIN));
        assert_eq!(events[7].ts_us, Some(1_756_947_602_000_000));
        assert_eq!(events[7].exit_code, Some(i32::MAX));
    }

    #[test]
    fn every_parser_preserves_partial_records_and_invalid_utf8_bytes() {
        for parser in [
            jsonl::parse as fn(&[u8]) -> Vec<Event>,
            copilot_cli::parse,
            claude_code::parse,
            codex_cli::parse,
            auto::parse,
        ] {
            let valid_partial = b"{\"text\":\"unfinished\r";
            let events = parser(valid_partial);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].r#type, "_unparsed");
            let payload: String = serde_json::from_str(&events[0].payload_json).unwrap();
            assert_eq!(payload.as_bytes(), valid_partial);

            for invalid in [
                b"{\"type\":\"message\",\"text\":\"\xe2\x82".as_slice(),
                b"{\"type\":\"message\",\"text\":\"\xff\"}".as_slice(),
            ] {
                let events = parser(invalid);
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].r#type, "_unparsed");
                let payload: serde_json::Value =
                    serde_json::from_str(&events[0].payload_json).unwrap();
                let bytes: Vec<u8> = serde_json::from_value(payload["bytes"].clone()).unwrap();
                assert_eq!(bytes, invalid);
            }
        }
    }

    #[test]
    fn jsonl_keeps_physical_sequence_numbers_and_complete_final_records() {
        let events = jsonl::parse(
            b"{ \"type\": \"one\" }\r\n\n \t\n{\"type\":\"two\",\"parentId\":\"one\"}",
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[0].payload_json, "{ \"type\": \"one\" }");
        assert_eq!(events[1].seq, 3);
        assert_eq!(events[1].parent_id.as_deref(), Some("one"));
        assert_eq!(events[1].r#type, "two");
    }

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

    #[test]
    fn declared_attributes_have_unique_keys_with_later_rules_winning() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let adapter = dir.path().join("adapter.toml");
        std::fs::write(
            &adapter,
            "name = 't'\n\
             [[attrs]]\npattern = '^(?P<role>[^/]+)/'\n\
             [[attrs]]\npattern = '/(?P<role>[^/]+)$'\n",
        )
        .unwrap();
        let declared = resolve(adapter.to_str().unwrap(), None).unwrap();
        assert_eq!(
            declared.attrs("builder/reviewer"),
            vec![("role".into(), "reviewer".into())]
        );
    }

    #[test]
    fn declared_markers_work_at_any_configured_depth_and_labels_are_deterministic() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let adapter = dir.path().join("adapter.toml");
        std::fs::write(
            &adapter,
            "name = 't'\n[batch_ready]\nmarkers = ['**/DONE', '**/review.json']\n\
             label_ancestor_pattern = '^round-\\d+$'\n",
        )
        .unwrap();
        let run = dir.path().join("run");
        for marker in [
            "a/b/c/d/e/f/round-0002/reviewer/review.json",
            "a/b/c/d/e/f/round-0010/DONE",
            "a/b/c/d/e/f/round-0002/DONE",
        ] {
            let path = run.join(marker);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}").unwrap();
        }
        let declared = resolve(adapter.to_str().unwrap(), None).unwrap();
        assert_eq!(
            declared.batch_ready(&run, &[]).as_deref(),
            Some("round-0010")
        );
        assert_eq!(
            declared
                .batch_ready(&run, &["round-0010".into()])
                .as_deref(),
            Some("round-0002")
        );
        assert_eq!(
            declared.batch_ready(&run, &["round-0002".into(), "round-0010".into()]),
            None
        );
    }
}
