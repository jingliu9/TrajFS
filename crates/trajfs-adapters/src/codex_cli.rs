//! Native `codex exec --json` records. One physical line remains one event.

use crate::util::{i32_of, lines, str_of, ts_us, unparsed};
use trajfs_core::events::Event;

/// Parse a native `codex exec --json` stream. Items carry their id, tool and
/// exit code; every later record's parent is the thread started first.
///
/// # Examples
///
/// ```
/// let events = trajfs_adapters::codex_cli::parse(concat!(
///     "{\"type\":\"thread.started\",\"thread_id\":\"thr\"}\n",
///     "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",",
///     "\"type\":\"command_execution\",\"command\":\"false\",\"exit_code\":1}}\n",
///     "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1e2}}",
/// ).as_bytes());
/// assert_eq!(events[0].id.as_deref(), Some("thr"));
/// assert_eq!(events[1].parent_id.as_deref(), Some("thr"));
/// assert_eq!(events[1].tool_name.as_deref(), Some("command_execution"));
/// assert_eq!(events[1].exit_code, Some(1));
/// // native spellings survive: the payload is the original line
/// assert!(events[2].payload_json.contains("1e2"));
/// ```
pub fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut thread = None;
    lines(bytes, |seq, value, line| {
        let Some(value) = value else {
            out.push(unparsed(seq, line));
            return;
        };
        let ty = str_of(&value, &["type"]).unwrap_or_else(|| "_untyped".into());
        if ty == "thread.started" {
            thread = str_of(&value, &["thread_id"]);
        }
        let item = value.get("item").unwrap_or(&serde_json::Value::Null);
        let kind = item.get("type").and_then(serde_json::Value::as_str);
        let tool = match kind {
            Some("command_execution" | "file_change" | "web_search" | "collab_tool_call") => {
                str_of(item, &["tool", "type"])
            }
            Some("mcp_tool_call") => str_of(item, &["tool", "name"]),
            _ => None,
        };
        let actor = if tool.is_some() {
            Some("tool".into())
        } else if matches!(kind, Some("agent_message" | "reasoning")) {
            Some("assistant".into())
        } else {
            None
        };
        out.push(Event {
            seq,
            ts_us: ts_us(value.get("timestamp")),
            r#type: ty.clone(),
            id: str_of(item, &["id"]).or_else(|| str_of(&value, &["id", "thread_id"])),
            parent_id: str_of(&value, &["parent_id"]).or_else(|| {
                if ty == "thread.started" {
                    None
                } else {
                    thread.clone()
                }
            }),
            actor,
            exit_code: if tool.is_some() {
                i32_of(item, &["exit_code"])
            } else {
                None
            },
            tool_name: tool,
            // Preserve native fields and lexical spelling, including cumulative usage.
            payload_json: std::str::from_utf8(line)
                .expect("parsed JSON is UTF-8")
                .to_owned(),
        });
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_items_and_usage_keep_their_original_envelope() {
        let bytes = br#"{"type":"thread.started","thread_id":"root"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"exit 7","exit_code":null}}
{"type":"item.completed","item":{"id":"item_0","type":"command_execution","exit_code":7}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"done"}}
{ "type": "turn.completed", "usage": {"input_tokens": 1e2, "output_tokens": 3} }
truncated"#;
        let events = parse(bytes);
        assert_eq!(events.len(), 7);
        assert_eq!(events[0].id.as_deref(), Some("root"));
        assert_eq!(events[2].tool_name.as_deref(), Some("command_execution"));
        assert_eq!(events[2].exit_code, None);
        assert_eq!(events[3].exit_code, Some(7));
        assert_eq!(events[3].parent_id.as_deref(), Some("root"));
        assert_eq!(events[4].actor.as_deref(), Some("assistant"));
        assert!(events[5].payload_json.contains("1e2"));
        assert_eq!(events[6].r#type, "_unparsed");
    }
}
