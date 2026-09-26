//! `traj skill`: the exported skill text mentions every verb, carries its version, has no unrendered
//! placeholders, and every worked example runs against a packed fixture.

mod common;

use common::*;

#[test]
fn skill_mentions_every_verb_and_carries_the_version() {
    let out = traj()
        .args(["skill", "export", "--stdout"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("{{"), "unrendered placeholders");
    let verbs = traj().args(["skill", "verbs"]).output().unwrap();
    for v in String::from_utf8_lossy(&verbs.stdout).lines() {
        assert!(
            s.contains(&format!("traj {v}")),
            "skill does not mention `traj {v}`"
        );
    }
    let help = traj().arg("--help").output().unwrap();
    let h = String::from_utf8_lossy(&help.stdout);
    for v in String::from_utf8_lossy(&verbs.stdout).lines() {
        assert!(
            h.lines().any(|l| l.trim_start().starts_with(v)),
            "verb {v} missing from --help"
        );
    }
    assert!(s.contains("traj-skill-version: "));

    // every worked example runs against the fixture store
    let e = packed("none");
    let mut ran = 0;
    let mut skipped = 0;
    let mut in_examples = false;
    for line in s.lines() {
        if line.starts_with("## ") {
            in_examples = line.contains("Worked examples");
        }
        if !in_examples || !line.starts_with("traj ") {
            continue;
        }
        let args = shell_words(line);
        if args.get(1).map(String::as_str) == Some("sql") && !sql_available() {
            eprintln!("skipping SQL checks: built without the sql feature ({line})");
            skipped += 1;
            continue;
        }
        let args: Vec<String> = args
            .into_iter()
            .skip(1)
            .map(|a| {
                if a == "S" {
                    e.store.display().to_string()
                } else {
                    a
                }
            })
            .collect();
        let out = traj().args(&args).output().unwrap();
        assert!(
            out.status.success(),
            "example failed: {line}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        ran += 1;
    }
    // with the sql feature nothing is skipped, so this is the plain `ran >= 5`
    assert!(
        ran + skipped >= 5,
        "expected the worked examples to run, ran {ran} (skipped {skipped})"
    );
    assert!(ran >= 1, "the non-SQL worked examples must run");
}

/// Minimal quoting-aware splitter for the skill's one-line examples (single and double quotes).
fn shell_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                has = true;
            }
            (None, ' ') => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}
