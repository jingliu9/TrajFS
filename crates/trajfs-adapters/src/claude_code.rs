//! Claude Code session logs (`~/.claude/projects/**/<session>.jsonl`):
//! message lines `{type: user|assistant, uuid, parentUuid, timestamp, message: {role, content}}` plus
//! session-state lines without ids.

use crate::util::{lines, str_of, ts_us, unparsed};
use std::collections::HashMap;
use trajfs_core::events::Event;
use trajfs_core::Adapter;

pub struct ClaudeCode;

/// Parse a Claude Code session file. `tool_result` blocks resolve their tool
/// name through the `tool_use` id seen earlier in the same session.
///
/// # Examples
///
/// ```
/// let events = trajfs_adapters::claude_code::parse(concat!(
///     "{\"type\":\"assistant\",\"uuid\":\"a1\",\"timestamp\":\"2026-09-04T01:00:00Z\",",
///     "\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\"}]}}\n",
///     "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":\"a1\",",
///     "\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\"}]}}\n",
/// ).as_bytes());
/// assert_eq!(events[0].ts_us, Some(1_788_483_600_000_000));
/// assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
/// assert_eq!(events[1].parent_id.as_deref(), Some("a1"));
/// assert_eq!(events[1].tool_name.as_deref(), Some("Bash"));
/// assert_eq!(events[1].actor.as_deref(), Some("user"));
/// ```
pub fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut tool_names = HashMap::new();
    lines(bytes, |seq, v, line| {
        let Some(v) = v else {
            out.push(unparsed(seq, line));
            return;
        };
        let line = std::str::from_utf8(line).expect("parsed JSON is UTF-8");
        let ty = str_of(&v, &["type"]).unwrap_or_else(|| "_untyped".into());
        let msg = v.get("message");
        let actor = msg
            .and_then(|m| str_of(m, &["role"]))
            .or_else(|| match ty.as_str() {
                "user" | "assistant" | "system" => Some(ty.clone()),
                _ => None,
            });
        let mut tool_name = None;
        if let Some(content) = msg
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for block in content {
                let name = match block.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        let name = str_of(block, &["name"]);
                        if let (Some(id), Some(name)) = (str_of(block, &["id"]), &name) {
                            tool_names.insert(id, name.clone());
                        }
                        name
                    }
                    Some("tool_result") => {
                        str_of(block, &["tool_use_id"]).and_then(|id| tool_names.get(&id).cloned())
                    }
                    _ => None,
                };
                if tool_name.is_none() {
                    tool_name = name;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_resolve_names_by_call_id_and_keep_parent_ids() {
        let events = parse(
            br#"{"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"first","name":"First"},{"type":"tool_use","id":"second","name":"Second"}]}}
{"type":"user","uuid":"u","parentUuid":"a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"second","content":"synthetic"}]}}"#,
        );
        assert_eq!(events[0].tool_name.as_deref(), Some("First"));
        assert_eq!(events[1].parent_id.as_deref(), Some("a"));
        assert_eq!(events[1].tool_name.as_deref(), Some("Second"));
        assert_eq!(events[1].actor.as_deref(), Some("user"));
    }
}
