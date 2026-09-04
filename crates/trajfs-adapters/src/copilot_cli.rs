//! GitHub Copilot CLI `events.jsonl`: `{type, timestamp, id, parentId, ephemeral, data}`.

use crate::util::{i32_of, lines, str_of, ts_us, unparsed};
use std::collections::HashMap;
use trajfs_core::events::Event;
use trajfs_core::Adapter;

pub struct CopilotCli;

pub fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    lines(bytes, |seq, v, line| {
        let Some(v) = v else {
            out.push(unparsed(seq, line));
            return;
        };
        let ty = str_of(&v, &["type"]).unwrap_or_else(|| "_untyped".into());
        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
        let mut tool_name = str_of(&data, &["toolName", "name", "tool"]);
        if let Some(id) = str_of(&data, &["toolCallId"]) {
            match &tool_name {
                Some(n) => {
                    tool_names.insert(id, n.clone());
                }
                None => tool_name = tool_names.get(&id).cloned(),
            }
        }
        let actor = if ty.starts_with("assistant.") || ty.starts_with("model.") {
            Some("assistant".into())
        } else if ty.starts_with("tool.") {
            Some("tool".into())
        } else if ty.starts_with("user.") {
            Some("user".into())
        } else if ty.starts_with("session.") {
            Some("system".into())
        } else {
            None
        };
        out.push(Event {
            seq,
            ts_us: ts_us(v.get("timestamp")),
            r#type: ty,
            id: str_of(&v, &["id"]),
            parent_id: str_of(&v, &["parentId"]),
            actor,
            tool_name,
            exit_code: i32_of(&data, &["exitCode", "exit_code"]),
            payload_json: data.to_string(),
        });
    });
    out
}

impl Adapter for CopilotCli {
    fn name(&self) -> &str {
        "copilot-cli"
    }
    fn is_trajectory(&self, path: &str) -> bool {
        trajfs_core::basename_of(path) == "events.jsonl"
    }
    fn parse_events(&self, _path: &str, bytes: &[u8]) -> Vec<Event> {
        parse(bytes)
    }
}
