//! Claude Code session logs (`~/.claude/projects/**/<session>.jsonl`):
//! message lines `{type: user|assistant, uuid, parentUuid, timestamp, message: {role, content}}` plus
//! session-state lines without ids.

use crate::util::{lines, str_of, ts_us, unparsed};
use trajfs_core::events::Event;
use trajfs_core::Adapter;

pub struct ClaudeCode;

pub fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    lines(bytes, |seq, v, line| {
        let Some(v) = v else {
            out.push(unparsed(seq, line));
            return;
        };
        let ty = str_of(&v, &["type"]).unwrap_or_else(|| "_untyped".into());
        let msg = v.get("message");
        let actor = msg.and_then(|m| str_of(m, &["role"])).or_else(|| match ty.as_str() {
            "user" | "assistant" | "system" => Some(ty.clone()),
            _ => None,
        });
        let mut tool_name = None;
        if let Some(content) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    tool_name = str_of(block, &["name"]);
                    break;
                }
            }
        }
        out.push(Event {
            seq,
            ts_us: ts_us(v.get("timestamp")),
            r#type: ty,
            id: str_of(&v, &["uuid"]),
            parent_id: str_of(&v, &["parentUuid"]),
            actor,
            tool_name,
            exit_code: None,
            payload_json: line.to_string(),
        });
    });
    out
}

impl Adapter for ClaudeCode {
    fn name(&self) -> &str {
        "claude-code"
    }
    fn is_trajectory(&self, path: &str) -> bool {
        path.ends_with(".jsonl")
    }
    fn parse_events(&self, _path: &str, bytes: &[u8]) -> Vec<Event> {
        parse(bytes)
    }
}
