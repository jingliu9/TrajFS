//! GitHub Copilot CLI `events.jsonl`: `{type, timestamp, id, parentId, ephemeral, data}`.

use crate::util::{i32_of, lines, str_of, ts_us, unparsed};
use std::collections::HashMap;
use trajfs_core::events::Event;
use trajfs_core::Adapter;

pub struct CopilotCli;

// Deserialize only to locate value boundaries; reserializing a Value would change
// number spellings, whitespace, escaped strings and duplicate payload keys.
fn raw_data(line: &str) -> &str {
    let mut rest = line
        .trim_start()
        .strip_prefix('{')
        .expect("data belongs to a parsed JSON object");
    let mut data = None;
    loop {
        let mut key = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let name = key.next().unwrap().expect("parsed object key");
        rest = rest[key.byte_offset()..]
            .trim_start()
            .strip_prefix(':')
            .expect("parsed object separator")
            .trim_start();
        let mut value =
            serde_json::Deserializer::from_str(rest).into_iter::<serde::de::IgnoredAny>();
        value.next().unwrap().expect("parsed object value");
        let end = value.byte_offset();
        if name == "data" {
            data = Some(&rest[..end]);
        }
        rest = rest[end..].trim_start();
        match rest.strip_prefix(',') {
            Some(next) => rest = next,
            None => break,
        }
    }
    data.expect("parsed object has data")
}

/// Parse a Copilot CLI `events.jsonl`. The payload of each event is the raw
/// `data` value, byte for byte; tool results inherit the name of their call.
///
/// # Examples
///
/// ```
/// let events = trajfs_adapters::copilot_cli::parse(concat!(
///     "{\"type\":\"tool.execution_start\",\"id\":\"2\",\"parentId\":\"1\",",
///     "\"data\":{\"toolCallId\":\"c1\",\"toolName\":\"bash\"}}\n",
///     "{\"type\":\"tool.execution_complete\",\"id\":\"3\",\"parentId\":\"2\",",
///     "\"data\": {\"toolCallId\":\"c1\",\"exitCode\":1}}\n",
/// ).as_bytes());
/// assert_eq!(events[0].actor.as_deref(), Some("tool"));
/// assert_eq!(events[1].tool_name.as_deref(), Some("bash"));
/// assert_eq!(events[1].exit_code, Some(1));
/// assert_eq!(events[1].parent_id.as_deref(), Some("2"));
/// assert_eq!(events[1].payload_json, "{\"toolCallId\":\"c1\",\"exitCode\":1}");
/// ```
pub fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    lines(bytes, |seq, v, line| {
        let Some(v) = v else {
            out.push(unparsed(seq, line));
            return;
        };
        let line = std::str::from_utf8(line).expect("parsed JSON is UTF-8");
        let ty = str_of(&v, &["type"]).unwrap_or_else(|| "_untyped".into());
        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
        let is_tool = ty.starts_with("tool.");
        let mut tool_name = None;
        if is_tool {
            tool_name = str_of(&data, &["toolName", "name", "tool"]);
            if let Some(id) = str_of(&data, &["toolCallId"]) {
                match &tool_name {
                    Some(n) => {
                        tool_names.insert(id, n.clone());
                    }
                    None => tool_name = tool_names.get(&id).cloned(),
                }
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
            exit_code: if is_tool {
                i32_of(&data, &["exitCode", "exit_code"])
            } else {
                None
            },
            payload_json: if v.get("data").is_some() {
                raw_data(line).to_string()
            } else {
                line.to_string()
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_bodies_are_verbatim_and_missing_data_does_not_discard_the_record() {
        let body = r#"{ "toolName": "synthetic", "number": 1e2, "same": 1, "same": 2 }"#;
        let line = format!(
            "{{\"type\":\"tool.execution_start\",\"id\":\"call\",\"parentId\":\"parent\",\"data\": {body}}}"
        );
        let event = parse(line.as_bytes()).remove(0);
        assert_eq!(event.payload_json, body);
        assert_eq!(event.parent_id.as_deref(), Some("parent"));

        let line = r#"{"type":"notice","message":"synthetic body without data"}"#;
        let event = parse(line.as_bytes()).remove(0);
        assert_eq!(event.payload_json, line);
    }

    #[test]
    fn payload_boundaries_handle_every_json_type_and_escaped_or_repeated_keys() {
        for body in [
            "null",
            "true",
            "-1.25e2",
            r#""synthetic \" }, data: [""#,
            "[ null, { \"nested\": [1, 2] } ]",
            "{ }",
        ] {
            let line = format!(
                " {{\"before\":{{\"data\":\"nested\"}},\"d\\u0061ta\": {body},\"after\":[1,2]}} \r"
            );
            assert_eq!(parse(line.as_bytes())[0].payload_json, body);
        }
        let line = br#"{"data":{"old":1},"data": [ 2, 3 ]}"#;
        assert_eq!(parse(line)[0].payload_json, "[ 2, 3 ]");
    }

    #[test]
    fn only_tool_events_supply_tool_envelopes_and_results_keep_the_call_name() {
        let events = parse(
            br#"{"type":"session.start","data":{"name":"session label","exitCode":42}}
{"type":"tool.execution_start","id":"start","data":{"toolCallId":"call","toolName":"synthetic"}}
{"type":"tool.execution_complete","parentId":"start","data":{"toolCallId":"call","exitCode":3}}"#,
        );
        assert_eq!(events[0].tool_name, None);
        assert_eq!(events[0].exit_code, None);
        assert_eq!(events[2].tool_name.as_deref(), Some("synthetic"));
        assert_eq!(events[2].exit_code, Some(3));
        assert_eq!(events[2].parent_id.as_deref(), Some("start"));
    }
}
