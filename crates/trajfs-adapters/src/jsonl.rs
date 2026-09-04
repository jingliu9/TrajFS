//! Generic JSON-lines adapter: one line = one event; envelope fields found by common key names.

use crate::util::{i32_of, lines, str_of, ts_us, unparsed};
use globset::{Glob, GlobSet, GlobSetBuilder};
use trajfs_core::events::Event;
use trajfs_core::Adapter;

pub struct Jsonl {
    globs: GlobSet,
}

impl Jsonl {
    pub fn new(globs: &str) -> anyhow::Result<Self> {
        let mut b = GlobSetBuilder::new();
        for g in globs.split(',').map(str::trim).filter(|g| !g.is_empty()) {
            b.add(Glob::new(g)?);
        }
        Ok(Self { globs: b.build()? })
    }
}

pub fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    lines(bytes, |seq, v, line| {
        let Some(v) = v else {
            out.push(unparsed(seq, line));
            return;
        };
        let ts = ["timestamp", "ts", "time", "created_at"]
            .iter()
            .find_map(|k| ts_us(v.get(*k)));
        out.push(Event {
            seq,
            ts_us: ts,
            r#type: str_of(&v, &["type", "event", "kind", "event_type"])
                .unwrap_or_else(|| "_untyped".into()),
            id: str_of(&v, &["id", "uuid", "event_id"]),
            parent_id: str_of(&v, &["parentId", "parent_id", "parentUuid", "parent"]),
            actor: str_of(&v, &["role", "actor", "author"]),
            tool_name: str_of(&v, &["tool_name", "toolName", "tool"]),
            exit_code: i32_of(&v, &["exit_code", "exitCode", "returncode", "status_code"]),
            payload_json: line.to_string(),
        });
    });
    out
}

impl Adapter for Jsonl {
    fn name(&self) -> &str {
        "jsonl"
    }
    fn is_trajectory(&self, path: &str) -> bool {
        self.globs.is_match(path)
    }
    fn parse_events(&self, _path: &str, bytes: &[u8]) -> Vec<Event> {
        parse(bytes)
    }
}
