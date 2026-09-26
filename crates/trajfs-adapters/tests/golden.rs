//! Golden-file tests for every built-in trajectory parser.
//!
//! Each `tests/fixtures/<name>.jsonl` is a small hand-written log with at least one
//! malformed line and a truncated final line (no trailing newline). The parsed events
//! are rendered as JSON with a fixed field order and compared to
//! `tests/fixtures/<name>.expected.json`. Run with `UPDATE_GOLDEN=1` to rewrite the
//! expectations after an intentional parser change; review the diff before committing.

use std::fs;
use std::path::{Path, PathBuf};
use trajfs_core::events::Event;

type Parser = fn(&[u8]) -> Vec<Event>;

const CASES: &[(&str, Parser)] = &[
    ("copilot-cli", trajfs_adapters::copilot_cli::parse),
    ("codex-cli", trajfs_adapters::codex_cli::parse),
    ("claude-code", trajfs_adapters::claude_code::parse),
    ("generic", trajfs_adapters::jsonl::parse),
    ("mixed-auto", trajfs_adapters::auto::parse),
    ("mixed-fallback", trajfs_adapters::auto::parse),
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("serialize a string")
}

fn json_opt(s: &Option<String>) -> String {
    s.as_deref().map(json_str).unwrap_or_else(|| "null".into())
}

/// One JSON object per event with a stable, readable field order.
fn render(events: &[Event]) -> String {
    let mut out = String::from("[\n");
    for (i, e) in events.iter().enumerate() {
        out.push_str("  {\n");
        out.push_str(&format!("    \"seq\": {},\n", e.seq));
        out.push_str(&format!(
            "    \"ts_us\": {},\n",
            e.ts_us
                .map(|t| t.to_string())
                .unwrap_or_else(|| "null".into())
        ));
        out.push_str(&format!("    \"type\": {},\n", json_str(&e.r#type)));
        out.push_str(&format!("    \"id\": {},\n", json_opt(&e.id)));
        out.push_str(&format!("    \"parent_id\": {},\n", json_opt(&e.parent_id)));
        out.push_str(&format!("    \"actor\": {},\n", json_opt(&e.actor)));
        out.push_str(&format!("    \"tool_name\": {},\n", json_opt(&e.tool_name)));
        out.push_str(&format!(
            "    \"exit_code\": {},\n",
            e.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "null".into())
        ));
        out.push_str(&format!(
            "    \"payload_json\": {}\n",
            json_str(&e.payload_json)
        ));
        out.push_str(if i + 1 == events.len() {
            "  }\n"
        } else {
            "  },\n"
        });
    }
    out.push_str("]\n");
    out
}

fn check_golden(name: &str, parser: Parser) -> (Vec<u8>, Vec<Event>) {
    let input_path = fixtures().join(format!("{name}.jsonl"));
    let expected_path = fixtures().join(format!("{name}.expected.json"));
    let input = fs::read(&input_path).unwrap_or_else(|e| panic!("{}: {e}", input_path.display()));
    let events = parser(&input);
    let actual = render(&events);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&expected_path, &actual).unwrap();
    }
    let expected = fs::read_to_string(&expected_path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}\n(run with UPDATE_GOLDEN=1 to create it)",
            expected_path.display()
        )
    });
    assert!(
        actual == expected,
        "{name}: parsed events differ from {}\n--- expected\n{expected}\n--- actual\n{actual}\n(run with UPDATE_GOLDEN=1 to accept)",
        expected_path.display()
    );
    (input, events)
}

/// The physical lines of a fixture (index, bytes without the line terminator), skipping blank ones
/// the way the parsers do.
fn physical_lines(input: &[u8]) -> Vec<(i32, &[u8])> {
    input
        .split_inclusive(|b| *b == b'\n')
        .enumerate()
        .map(|(i, raw)| {
            let line = raw.strip_suffix(b"\n").unwrap_or(raw);
            (i as i32, line.strip_suffix(b"\r").unwrap_or(line))
        })
        .filter(|(_, line)| !line.iter().all(|b| matches!(b, b' ' | b'\t' | b'\r')))
        .collect()
}

/// Every fixture keeps its malformed lines as `_unparsed` events carrying the original bytes,
/// keeps physical sequence numbers, and never drops or invents a record.
fn check_unparsed_invariants(name: &str, input: &[u8], events: &[Event]) {
    assert!(
        !input.ends_with(b"\n"),
        "{name}: the fixture must end in a truncated line"
    );
    let lines = physical_lines(input);
    assert_eq!(
        events.len(),
        lines.len(),
        "{name}: one event per non-blank line"
    );
    let mut unparsed = 0;
    for ((seq, line), event) in lines.iter().zip(events) {
        assert_eq!(event.seq, *seq, "{name}: physical sequence number");
        let malformed = serde_json::from_slice::<serde_json::Value>(line).is_err();
        if malformed {
            unparsed += 1;
            assert_eq!(event.r#type, "_unparsed", "{name}: line {seq}");
            let payload: String = serde_json::from_str(&event.payload_json)
                .unwrap_or_else(|e| panic!("{name}: _unparsed payload is not a JSON string: {e}"));
            assert_eq!(
                payload.as_bytes(),
                *line,
                "{name}: original bytes of line {seq}"
            );
            assert!(event.id.is_none() && event.tool_name.is_none() && event.exit_code.is_none());
        } else {
            assert_ne!(event.r#type, "_unparsed", "{name}: line {seq} parses");
        }
    }
    let last = events.last().unwrap();
    assert_eq!(last.r#type, "_unparsed", "{name}: truncated final line");
    assert!(
        unparsed >= 2,
        "{name}: needs one malformed line plus the truncated final line"
    );
}

#[test]
fn every_fixture_matches_its_golden_output_and_preserves_malformed_lines() {
    for (name, parser) in CASES {
        let (input, events) = check_golden(name, *parser);
        check_unparsed_invariants(name, &input, &events);
    }
}

#[test]
fn auto_detection_picks_the_native_parser_for_each_single_format_stream() {
    for (name, parser) in CASES {
        if name.starts_with("mixed") {
            continue;
        }
        let input = fs::read(fixtures().join(format!("{name}.jsonl"))).unwrap();
        let expected = if *name == "generic" {
            // the generic fixture has no native header, so auto falls back to jsonl
            trajfs_adapters::jsonl::parse(&input)
        } else {
            parser(&input)
        };
        assert_eq!(
            trajfs_adapters::auto::parse(&input),
            expected,
            "{name}: auto should agree with the native parser"
        );
    }
}

#[test]
fn mixed_stream_is_parsed_once_by_the_first_recognised_header() {
    let input = fs::read(fixtures().join("mixed-auto.jsonl")).unwrap();
    let events = trajfs_adapters::auto::parse(&input);
    // the codex header on line 2 wins, although garbage and an unknown record precede it
    assert_eq!(events, trajfs_adapters::codex_cli::parse(&input));
    assert_eq!(events[2].r#type, "thread.started");
    assert_eq!(events[2].id.as_deref(), Some("thr_mixed"));
    // later copilot- and claude-shaped lines are still one event each, parsed as codex records
    assert_eq!(events[3].r#type, "tool.execution_complete");
    assert_eq!(events[3].tool_name, None, "no codex item: no tool envelope");
    assert_eq!(events[3].parent_id.as_deref(), Some("thr_mixed"));
    assert_eq!(events[4].r#type, "assistant");
    assert_eq!(events[5].exit_code, Some(3));
    assert_eq!(events.last().unwrap().r#type, "_unparsed");

    let input = fs::read(fixtures().join("mixed-fallback.jsonl")).unwrap();
    let events = trajfs_adapters::auto::parse(&input);
    assert_eq!(events, trajfs_adapters::jsonl::parse(&input));
    assert_eq!(events[1].tool_name.as_deref(), Some("grep"));
    assert_eq!(events[1].exit_code, Some(0));
}

#[test]
fn the_declared_adapter_uses_the_same_parsers_as_the_built_ins() {
    let dir = tempfile::tempdir().unwrap();
    for (name, parser) in CASES {
        let format = match *name {
            "generic" => "jsonl",
            "mixed-auto" | "mixed-fallback" => "auto",
            other => other,
        };
        let toml = dir.path().join(format!("{name}.toml"));
        fs::write(
            &toml,
            format!("name = \"golden\"\n[trajectories]\nglobs = [\"**/events.jsonl\"]\nformat = \"{format}\"\n"),
        )
        .unwrap();
        let adapter = trajfs_adapters::resolve(toml.to_str().unwrap(), None).unwrap();
        assert!(adapter.is_trajectory("run/events.jsonl"));
        let input = fs::read(fixtures().join(format!("{name}.jsonl"))).unwrap();
        assert_eq!(
            adapter.parse_events("run/events.jsonl", &input),
            parser(&input),
            "{name}: declared adapter with format {format}"
        );
    }
}
