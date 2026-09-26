//! The content verbs `grep`, `cat`, `edit` and `sql`: matches and bytes agree with the packed tree and with
//! `grep -rlE` on an extracted copy, content is verified as it is read, and the SQL layer quotes paths,
//! registers only published segments and exposes blob contents and adapter payloads faithfully.

mod common;

use common::*;
use std::fs;

#[test]
fn grep_reports_line_matches_and_every_path_sharing_a_blob() {
    let e = packed("none");
    let s = traj_stdout(&e.store, &["grep", "-e", "Traceback"]);
    assert_eq!(
        s,
        "rounds/round-0001/builder/logs/b.stderr:1:Traceback: boom\n"
    );
    // a hit in duplicated content maps to every path
    let s = traj_stdout(&e.store, &["grep", "-l", "-e", "^hello"]);
    assert_eq!(s.lines().count(), 2);
}

#[test]
fn cat_returns_the_exact_bytes() {
    let e = packed("none");
    let c = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["cat", "rounds/round-0001/reviewer/review.json"])
        .output()
        .unwrap();
    assert_eq!(c.stdout, b"{\"verdict\":\"not done\"}\n");
}

#[test]
fn grep_matches_grep_rl_for_random_patterns() {
    let e = packed("none");
    let out = extracted(&e);
    let patterns = [
        "hello",
        "^Trace",
        "o{2}",
        "not done",
        "line",
        "[0-9]+",
        "sh$",
        "grüße",
        "zzz-never",
        "e.*l",
        "\\{",
        "boom|verdict",
        "^$",
        "a",
        "e",
        "t.o",
        "run",
        "^#!",
        "n$",
        "json",
    ];
    for p in patterns {
        let out_grep = std::process::Command::new("grep")
            .args(["-rlE", "--", p, "."])
            .current_dir(&out)
            .output()
            .unwrap();
        let mut want: Vec<String> = String::from_utf8_lossy(&out_grep.stdout)
            .lines()
            .map(|l| l.trim_start_matches("./").to_string())
            .filter(|l| !l.contains("big.bin"))
            .collect();
        want.sort();
        let got = traj()
            .arg("-S")
            .arg(&e.store)
            .args(["grep", "-l", "-e", p])
            .output()
            .unwrap();
        let mut got: Vec<String> = String::from_utf8_lossy(&got.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect();
        got.sort();
        assert_eq!(got, want, "pattern {p}");
    }
}

#[test]
fn grep_rejects_a_missing_directory() {
    let fixture = SmallStore::new();
    fixture
        .traj()
        .args(["grep", "-e", "match", "--path", "missing"])
        .assert()
        .code(2);
}

#[test]
fn grep_verifies_content_and_reports_errors_even_with_matches() {
    use trajfs_core::catalog;
    use trajfs_core::pack::IndexRow;
    use trajfs_core::Store;
    let fixture = SmallStore::new();
    let store = Store::open(&fixture.store).unwrap();
    let a = store.stat("a.txt").unwrap().unwrap();
    let b = store.stat("b.txt").unwrap().unwrap();
    let other = store.parts(&b.sha).unwrap()[0];
    drop(store);
    let index = fs::read_dir(fixture.store.join("packs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("index-")
        })
        .unwrap();
    let mut rows = Vec::new();
    catalog::read_index(&index, |sha, loc| {
        rows.push(IndexRow {
            sha,
            loc: if sha == a.sha { other } else { loc },
        });
    })
    .unwrap();
    catalog::write_index(&index, &rows).unwrap();
    fixture
        .traj()
        .args(["grep", "-e", "match", "-l"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("b.txt"));
}

#[test]
fn cat_stat_find_and_grep_work_on_a_healthy_store_and_empty_files() {
    let fixture = SmallStore::new();
    fixture
        .traj()
        .args(["cat", "empty", "a.txt"])
        .assert()
        .success()
        .stdout("match aaa\n");
    fixture
        .traj()
        .args(["stat", "empty"])
        .assert()
        .success()
        .stdout(predicates::str::contains("size:     0"));
    fixture
        .traj()
        .args(["find", "--name", "*.txt"])
        .assert()
        .success()
        .stdout("a.txt\nb.txt\n");
    fixture
        .traj()
        .args(["grep", "-e", "aaa"])
        .assert()
        .success()
        .stdout("a.txt:1:match aaa\n");
    fixture
        .traj()
        .args(["grep", "-e", "not-present"])
        .assert()
        .code(1);
}

#[cfg(unix)]
#[test]
fn edit_accepts_editor_flags_and_keeps_the_store_unchanged() {
    let fixture = SmallStore::new();
    let root = fixture.root();
    let script = root.join("editor script");
    fs::write(
        &script,
        "test \"$1\" = --wait || exit 9\nprintf 'edited\\n' > \"$2\"\n",
    )
    .unwrap();
    let copies = root.join("copies");
    fs::create_dir(&copies).unwrap();
    fixture
        .traj()
        .env("TMPDIR", &copies)
        .env("EDITOR", format!("/bin/sh '{}' --wait", script.display()))
        .args(["edit", "a.txt"])
        .assert()
        .success()
        .stdout(predicates::str::contains("+edited"));
    fixture
        .traj()
        .args(["cat", "a.txt"])
        .assert()
        .success()
        .stdout("match aaa\n");
    let edits: Vec<_> = fs::read_dir(&copies)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(fs::read(edits[0].join("a.txt")).unwrap(), b"edited\n");
    fixture
        .traj()
        .env("TMPDIR", &copies)
        .env("EDITOR", "/bin/true")
        .args(["edit", "a.txt"])
        .assert()
        .success()
        .stderr(predicates::str::contains("unchanged"));
    assert_eq!(fs::read_dir(copies).unwrap().count(), 1);
}

/// The DuckDB layer: `text()`/`blob()` resolve content in every attached store, paths are quoted literally,
/// only published segments are registered, errors propagate and payloads round-trip exactly.
#[cfg(feature = "sql")]
mod sql_layer {
    use super::*;
    use assert_cmd::Command;
    use predicates::str::contains;
    use std::path::{Path, PathBuf};
    use trajfs_core::ingest::{ingest, IngestOptions};
    use trajfs_core::rules::Rules;
    use trajfs_core::{Adapter, NoAdapter, Store};

    #[test]
    fn sql_exposes_derived_events_and_path_attributes() {
        let e = packed("none");
        let s = traj_stdout(
            &e.store,
            &[
                "sql",
                "--csv",
                "select seq, type, tool_name, exit_code from events order by seq",
            ],
        );
        assert!(s.contains("2,tool.execution_complete,bash,1"), "{s}");
        assert!(
            s.contains("3,_unparsed,,"),
            "unparsable lines must be kept: {s}"
        );
        let s = traj_stdout(
            &e.store,
            &[
                "sql",
                "--csv",
                "select count(*) from files where attrs['round']='1'",
            ],
        );
        assert!(s.contains("\n8"), "{s}");
    }

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::write(
                root.path().join("trajfs.toml"),
                format!(
                    "data_root = {}\nstore_root = \"stores\"\n",
                    serde_json::to_string(&root.path().join("source")).unwrap()
                ),
            )
            .unwrap();
            Self { root }
        }

        fn pack(&self, name: &str, files: &[(&str, &[u8])], adapter: &dyn Adapter) -> PathBuf {
            let source = self.root.path().join("source").join(name);
            let store = self
                .root
                .path()
                .join("stores")
                .join(format!("{name}.trajstore"));
            fs::create_dir_all(&source).unwrap();
            for (path, bytes) in files {
                let path = source.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, bytes).unwrap();
            }
            ingest(
                &source,
                &store,
                IngestOptions {
                    rules: Rules::from_toml("name = 'none'\n").unwrap(),
                    rules_name: "none".into(),
                    adapter,
                    label: "fixture".into(),
                    jobs: 2,
                    derive: true,
                    store_id: Some(name.into()),
                },
            )
            .unwrap();
            store
        }

        fn sql(&self, stores: &[&Path], query: &str) -> Command {
            let mut command = traj();
            command.current_dir(self.root.path());
            for store in stores {
                command.arg("-S").arg(store);
            }
            command.args(["sql", "--csv", query]);
            command
        }
    }

    #[test]
    fn sql_resolves_content_in_every_store_and_quotes_paths() {
        let fixture = Fixture::new();
        let left = fixture.pack("a'left", &[("a.txt", b"first")], &NoAdapter);
        let right = fixture.pack("z'right", &[("a.txt", b"second")], &NoAdapter);
        fixture
            .sql(
                &[&left, &right],
                "select store, text(sha) as content, \
                 blob(sha) = encode(text(sha)) as blob_matches, \
                 text(hex(sha)) as hex_content from files order by store",
            )
            .assert()
            .success()
            .stdout(
                "store,content,blob_matches,hex_content\n\
                 a'left,first,true,first\n\
                 z'right,second,true,second\n",
            );
    }

    #[test]
    fn sql_paths_are_literal_and_do_not_add_hive_partition_columns() {
        let fixture = Fixture::new();
        let literal = fixture.pack("literal[1]", &[("expected.txt", b"first")], &NoAdapter);
        fixture.pack("literal1", &[("unexpected.txt", b"second")], &NoAdapter);
        fixture
            .sql(&[&literal], "select path from files")
            .assert()
            .success()
            .stdout("path\nexpected.txt\n");

        let hive = fixture.pack("batch=other", &[("expected.txt", b"first")], &NoAdapter);
        fixture
            .sql(&[&hive], "select batch from files")
            .assert()
            .success()
            .stdout("batch\n1\n");
    }

    #[cfg(unix)]
    #[test]
    fn sql_refuses_ambiguous_backslash_and_glob_paths() {
        let fixture = Fixture::new();
        let store = fixture.pack(
            "back\\slash[1]",
            &[("expected.txt", b"synthetic")],
            &NoAdapter,
        );
        fixture
            .sql(&[&store], "select path from files")
            .assert()
            .failure()
            .stderr(contains("backslash"));
    }

    #[test]
    fn sql_handles_empty_binary_null_and_unknown_content() {
        let fixture = Fixture::new();
        let store = fixture.pack(
            "content",
            &[("empty", b""), ("binary", b"\0\xff\r\n")],
            &NoAdapter,
        );
        fixture
            .sql(
                &[&store],
                "select name, octet_length(blob(sha)) as size, \
                 blob(hex(sha)) = blob(sha) as hex_matches \
                 from files order by name",
            )
            .assert()
            .success()
            .stdout("name,size,hex_matches\nbinary,4,true\nempty,0,true\n");
        fixture
            .sql(
                &[&store],
                "select text(NULL::varchar) is null as null_text, \
                 blob(NULL::blob) is null as null_blob, \
                 text('not a sha') is null as malformed, \
                 blob(repeat('0', 64)) is null as unknown, \
                 text(sha) = '' as empty_text from files where name = 'empty'",
            )
            .assert()
            .success()
            .stdout(
                "null_text,null_blob,malformed,unknown,empty_text\n\
                 true,true,true,true,true\n",
            );
    }

    #[test]
    fn sql_propagates_pack_read_errors_instead_of_returning_null() {
        let fixture = Fixture::new();
        let store = fixture.pack("broken", &[("a.txt", b"synthetic")], &NoAdapter);
        fs::write(store.join("packs/0001.pack"), b"broken pack").unwrap();
        fixture
            .sql(&[&store], "select text(sha) from files")
            .assert()
            .failure();
    }

    #[test]
    fn sql_verifies_blob_hashes() {
        let fixture = Fixture::new();
        let store = fixture.pack("original", &[("a.txt", b"first")], &NoAdapter);
        let other = fixture.pack("replacement", &[("a.txt", b"other")], &NoAdapter);
        let pack = store.join("packs/0001.pack");
        let other_pack = other.join("packs/0001.pack");
        assert_eq!(
            fs::metadata(&pack).unwrap().len(),
            fs::metadata(&other_pack).unwrap().len()
        );
        fs::copy(other_pack, pack).unwrap();
        fixture
            .sql(&[&store], "select blob(sha) from files")
            .assert()
            .failure()
            .stderr(contains("does not match"));
    }

    #[test]
    fn sql_csv_quotes_carriage_returns_and_keeps_empty_result_headers() {
        let fixture = Fixture::new();
        let store = fixture.pack("csv", &[("a.txt", b"synthetic")], &NoAdapter);
        fixture
            .sql(&[&store], "select 'a' || chr(13) || 'b' as value")
            .assert()
            .success()
            .stdout("value\n\"a\rb\"\n");
        fixture
            .sql(&[&store], "select path, size from files where false")
            .assert()
            .success()
            .stdout("path,size\n");
    }

    #[test]
    fn sql_registers_only_published_catalog_and_event_segments() {
        let fixture = Fixture::new();
        let adapter = trajfs_adapters::jsonl::Jsonl::new("*.jsonl").unwrap();
        let store = fixture.pack(
            "published",
            &[("events.jsonl", b"{\"type\":\"synthetic\"}\n")],
            &adapter,
        );
        let snapshot = Store::open(&store).unwrap();
        let files = snapshot.files_segments()[0].clone();
        let events = snapshot.derived_segments()[0].clone();
        drop(snapshot);
        fs::copy(files, store.join("catalog/files-9999.parquet")).unwrap();
        fs::copy(events, store.join("derived/jsonl/events-9999.parquet")).unwrap();
        fixture
            .sql(
                &[&store],
                "select (select count(*) from files) as files, \
                 (select count(*) from events) as events",
            )
            .assert()
            .success()
            .stdout("files,events\n1,1\n");
    }

    #[test]
    fn sql_preserves_adapter_payloads_and_parent_ids() {
        let fixture = Fixture::new();
        let body = r#"{ "toolName": "synthetic", "number": 1e2, "same": 1, "same": 2 }"#;
        let mut bytes = format!(
            "{{\"type\":\"tool.execution_start\",\"id\":\"call\",\"parentId\":\"parent\",\"data\": {body}}}\n"
        )
        .into_bytes();
        let partial = b"{\"data\":\"\xe2\x82";
        bytes.extend_from_slice(partial);
        let store = fixture.pack(
            "payloads",
            &[("events.jsonl", &bytes)],
            &trajfs_adapters::copilot_cli::CopilotCli,
        );
        fixture
            .sql(
                &[&store],
                &format!(
                    "select parent_id, tool_name, payload_json = '{}' as exact \
                     from events where seq = 0",
                    body.replace('\'', "''")
                ),
            )
            .assert()
            .success()
            .stdout("parent_id,tool_name,exact\nparent,synthetic,true\n");
        fixture
            .sql(
                &[&store],
                &format!(
                    "select type, json_extract(payload_json, '$.bytes') = '{}'::json as exact \
                     from events where seq = 1",
                    serde_json::to_string(partial.as_slice()).unwrap()
                ),
            )
            .assert()
            .success()
            .stdout("type,exact\n_unparsed,true\n");
        fixture
            .sql(
                &[&store],
                &format!(
                    "select hex(blob(sha)) = '{}' as exact from files",
                    hex::encode_upper(&bytes)
                ),
            )
            .assert()
            .success()
            .stdout("exact\ntrue\n");
    }

    #[test]
    fn sql_attributes_have_one_value_per_key() {
        let fixture = Fixture::new();
        let adapter_path = fixture.root.path().join("adapter.toml");
        fs::write(
            &adapter_path,
            "name = 'attrs'\n\
             [[attrs]]\npattern = '^(?P<role>[^/]+)/'\n\
             [[attrs]]\npattern = '/(?P<role>[^/]+)$'\n",
        )
        .unwrap();
        let adapter = trajfs_adapters::declared::Declared::load(&adapter_path).unwrap();
        let store = fixture.pack("attrs", &[("builder/reviewer", b"synthetic")], &adapter);
        fixture
            .sql(&[&store], "select attrs['role'] as role from files")
            .assert()
            .success()
            .stdout("role\nreviewer\n");
    }

    #[test]
    fn sql_rejects_an_invalid_explicit_configuration() {
        let fixture = Fixture::new();
        let store = fixture.pack("config", &[("a.txt", b"synthetic")], &NoAdapter);
        let config = fixture.root.path().join("invalid.toml");
        fs::write(&config, "data_root = [\n").unwrap();
        fixture
            .sql(&[&store], "select count(*) from files")
            .env("TRAJ_CONFIG", config)
            .assert()
            .failure()
            .stderr(contains("configuration"));
    }
}
