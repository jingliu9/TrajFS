#![cfg(feature = "sql")]

use assert_cmd::Command;
use predicates::str::contains;
use std::fs;
use std::path::{Path, PathBuf};
use trajfs_core::ingest::{ingest, IngestOptions};
use trajfs_core::rules::Rules;
use trajfs_core::{Adapter, NoAdapter, Store};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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
        let mut command = Command::cargo_bin("traj").unwrap();
        command
            .current_dir(self.root.path())
            .env_remove("TRAJ_CONFIG");
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
