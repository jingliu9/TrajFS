//! Explicit mixed-format support: choose once per trajectory, never merge copies.

use trajfs_core::events::Event;

pub fn parse(bytes: &[u8]) -> Vec<Event> {
    // A partial/corrupt first record must not hide a later recognizable header.
    let mut format = None;
    crate::util::lines(bytes, |_, value, _| {
        if format.is_some() {
            return;
        }
        let Some(value) = value else {
            return;
        };
        let ty = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if matches!(
            ty,
            "thread.started"
                | "turn.started"
                | "turn.completed"
                | "turn.failed"
                | "item.started"
                | "item.updated"
                | "item.completed"
        ) {
            format = Some("codex-cli");
        } else if value.get("data").is_some()
            && ["session.", "model.", "assistant.", "tool.", "user."]
                .iter()
                .any(|prefix| ty.starts_with(prefix))
        {
            format = Some("copilot-cli");
        } else if value.get("uuid").is_some()
            && matches!(ty, "user" | "assistant" | "system" | "summary")
        {
            format = Some("claude-code");
        }
    });
    match format {
        Some("codex-cli") => crate::codex_cli::parse(bytes),
        Some("copilot-cli") => crate::copilot_cli::parse(bytes),
        Some("claude-code") => crate::claude_code::parse(bytes),
        _ => crate::jsonl::parse(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_calls_select_one_parser_and_preserve_legacy_payloads() {
        for (bytes, expected) in [
            (b"bad\n{\"type\":\"thread.started\",\"thread_id\":\"root\"}".as_slice(), crate::codex_cli::parse as fn(&[u8]) -> Vec<Event>),
            (b"{\"type\":\"tool.execution_complete\",\"data\": { \"toolName\":\"shell\",\"exitCode\":1 }}".as_slice(), crate::copilot_cli::parse),
            (b"{\"type\":\"assistant\",\"uuid\":\"x\",\"message\":{\"role\":\"assistant\"}}".as_slice(), crate::claude_code::parse),
            (b"{\"type\":\"unknown\",\"extra\":true}\ninvalid".as_slice(), crate::jsonl::parse),
        ] {
            let actual = parse(bytes);
            let expected = expected(bytes);
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(&expected) {
                assert_eq!(actual.payload_json, expected.payload_json);
                assert_eq!(actual.tool_name, expected.tool_name);
                assert_eq!(actual.exit_code, expected.exit_code);
            }
        }
    }
}
