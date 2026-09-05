//! `traj sql`: DuckDB over the store's Parquet files (docs/PLAN.md §5).

use crate::config::{resolve_store, Config};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::sync::Arc;

#[derive(Args, Debug)]
pub struct SqlArgs {
    /// SQL text; `-` reads it from stdin
    pub query: String,
    /// Output as CSV instead of a table
    #[arg(long)]
    pub csv: bool,
    /// Print the views that would be registered and exit
    #[arg(long)]
    pub show_views: bool,
}

pub struct StoreTables {
    pub name: String,
    pub files: Vec<std::path::PathBuf>,
    pub dirs: Vec<std::path::PathBuf>,
    pub excluded: Vec<std::path::PathBuf>,
    pub indexes: Vec<std::path::PathBuf>,
    pub events: Vec<std::path::PathBuf>,
    store: Arc<trajfs_core::Store>,
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn sql_path(path: &std::path::Path) -> Result<String> {
    let path = path.to_str().context("SQL artifact paths must be UTF-8")?;
    if cfg!(unix) && path.contains('\\') && path.contains(['*', '?', '[']) {
        bail!("DuckDB cannot read a literal path containing both a backslash and glob characters");
    }
    // read_parquet treats even explicitly listed filenames as globs.
    let mut literal = String::new();
    for c in path.chars() {
        match c {
            '*' => literal.push_str("[*]"),
            '?' => literal.push_str("[?]"),
            '[' => literal.push_str("[[]"),
            _ => literal.push(c),
        }
    }
    Ok(sql_string(&literal))
}

fn add_view(
    out: &mut Vec<(String, String)>,
    stores: &[StoreTables],
    table: &str,
    paths: impl Fn(&StoreTables) -> &[std::path::PathBuf],
) -> Result<()> {
    let mut parts = Vec::new();
    for store in stores {
        let files = paths(store);
        if files.is_empty() {
            continue;
        }
        let files = files
            .iter()
            .map(|path| sql_path(path))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        parts.push(format!(
            "select {} as store, * from read_parquet([{}], union_by_name=true, hive_partitioning=false)",
            sql_string(&store.name),
            files
        ));
    }
    if !parts.is_empty() {
        out.push((
            table.to_string(),
            format!(
                "create view {table} as {}",
                parts.join(" union all by name ")
            ),
        ));
    }
    Ok(())
}

pub fn view_sql(stores: &[StoreTables]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    add_view(&mut out, stores, "files", |store| &store.files)?;
    add_view(&mut out, stores, "dirs", |store| &store.dirs)?;
    add_view(&mut out, stores, "excluded", |store| &store.excluded)?;
    add_view(&mut out, stores, "blobs", |store| &store.indexes)?;
    add_view(&mut out, stores, "events", |store| &store.events)?;
    Ok(out)
}

pub fn tables_for(stores: &[String]) -> Result<Vec<StoreTables>> {
    let cfg = Config::try_find()?;
    if stores.is_empty() {
        bail!("no store given: pass -S <store> (repeatable)");
    }
    let mut v = Vec::new();
    for s in stores {
        let root = resolve_store(s, cfg.as_ref())?.canonicalize()?;
        let store = Arc::new(trajfs_core::Store::open(&root)?);
        let files = store.files_segments().to_vec();
        let dirs = store.dirs_segments().to_vec();
        let excluded = store.excluded_segments().to_vec();
        let indexes = store.index_segments().to_vec();
        let events = store
            .derived_segments()
            .iter()
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("events-"))
            })
            .cloned()
            .collect();
        v.push(StoreTables {
            name: store.manifest.store_id.clone(),
            files,
            dirs,
            excluded,
            indexes,
            events,
            store,
        });
    }
    Ok(v)
}

#[cfg(feature = "sql")]
pub fn run(stores: &[String], a: SqlArgs) -> Result<i32> {
    use std::io::Read;
    let tables = tables_for(stores)?;
    let views = view_sql(&tables)?;
    if a.show_views {
        for (_, v) in &views {
            println!("{v};");
        }
        return Ok(0);
    }
    let query = if a.query == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        a.query.clone()
    };
    let conn = duckdb::Connection::open_in_memory()?;
    for (_, v) in &views {
        conn.execute_batch(v)?;
    }
    udf::install(
        &conn,
        tables.iter().map(|table| table.store.clone()).collect(),
    )?;
    let mut stmt = conn.prepare(&query)?;
    let result = stmt.query_arrow([])?;
    let schema = result.get_schema();
    let batches: Vec<duckdb::arrow::array::RecordBatch> = result.collect();
    if a.csv {
        print_csv(&schema, &batches)?;
    } else if batches.is_empty() {
        println!("(no rows)");
    } else {
        println!(
            "{}",
            duckdb::arrow::util::pretty::pretty_format_batches(&batches)?
        );
    }
    Ok(0)
}

#[cfg(not(feature = "sql"))]
pub fn run(_stores: &[String], _a: SqlArgs) -> Result<i32> {
    bail!("this build has no SQL support (built without the `sql` feature)")
}

#[cfg(feature = "sql")]
fn print_csv(
    schema: &duckdb::arrow::datatypes::Schema,
    batches: &[duckdb::arrow::array::RecordBatch],
) -> Result<()> {
    use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let opts = FormatOptions::default().with_null("");
    let names: Vec<String> = schema
        .fields()
        .iter()
        .map(|field| csv_quote(field.name()))
        .collect();
    writeln!(out, "{}", names.join(","))?;
    for b in batches {
        let fmts: Vec<ArrayFormatter> = b
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &opts))
            .collect::<Result<_, _>>()?;
        for row in 0..b.num_rows() {
            let cells: Vec<String> = fmts
                .iter()
                .map(|f| csv_quote(&f.value(row).to_string()))
                .collect();
            writeln!(out, "{}", cells.join(","))?;
        }
    }
    Ok(())
}

#[cfg(feature = "sql")]
fn csv_quote(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(feature = "sql")]
mod udf {
    use anyhow::{Context, Result};
    use duckdb::arrow::array::{
        Array, BinaryArray, BinaryBuilder, FixedSizeBinaryArray, LargeBinaryArray,
        LargeStringArray, RecordBatch, StringArray, StringBuilder,
    };
    use duckdb::arrow::datatypes::DataType;
    use duckdb::vscalar::arrow::{ArrowFunctionSignature, VArrowScalar};
    use std::sync::Arc;
    use trajfs_core::Store;

    /// Bytes for every input row (None when the sha is unknown or malformed).
    fn resolve(stores: &[Arc<Store>], input: &RecordBatch) -> Result<Vec<Option<Vec<u8>>>> {
        let mut readers: Vec<_> = stores.iter().map(|store| store.reader()).collect();
        let empty_sha = trajfs_core::hash::sha_of_bytes(b"");
        let col = input.column(0);
        let n = col.len();
        let mut out = Vec::with_capacity(n);
        let sha_at = |i: usize| -> Option<[u8; 32]> {
            if col.is_null(i) {
                return None;
            }
            let bytes: Vec<u8> = if let Some(a) = col.as_any().downcast_ref::<BinaryArray>() {
                a.value(i).to_vec()
            } else if let Some(a) = col.as_any().downcast_ref::<LargeBinaryArray>() {
                a.value(i).to_vec()
            } else if let Some(a) = col.as_any().downcast_ref::<FixedSizeBinaryArray>() {
                a.value(i).to_vec()
            } else if let Some(a) = col.as_any().downcast_ref::<StringArray>() {
                hex::decode(a.value(i)).ok()?
            } else if let Some(a) = col.as_any().downcast_ref::<LargeStringArray>() {
                hex::decode(a.value(i)).ok()?
            } else {
                return None;
            };
            bytes.as_slice().try_into().ok()
        };
        for i in 0..n {
            let Some(sha) = sha_at(i) else {
                out.push(None);
                continue;
            };
            // Empty files have a content hash but no pack-index entry.
            if sha == empty_sha {
                out.push(Some(Vec::new()));
                continue;
            }
            let mut bytes = None;
            for (store, reader) in stores.iter().zip(&mut readers) {
                if store.index()?.contains_key(&sha) {
                    bytes = Some(store.read_blob(reader, &sha, true).with_context(|| {
                        format!(
                            "read blob {} from store {}",
                            hex::encode(sha),
                            store.root.display()
                        )
                    })?);
                    break;
                }
            }
            out.push(bytes);
        }
        Ok(out)
    }

    pub struct Text;
    impl VArrowScalar for Text {
        type State = Vec<Arc<Store>>;
        fn invoke(
            stores: &Self::State,
            input: RecordBatch,
        ) -> Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
            let mut b = StringBuilder::new();
            for v in resolve(stores, &input)? {
                match v {
                    Some(bytes) => b.append_value(String::from_utf8_lossy(&bytes)),
                    None => b.append_null(),
                }
            }
            Ok(Arc::new(b.finish()))
        }
        fn signatures() -> Vec<ArrowFunctionSignature> {
            vec![
                ArrowFunctionSignature::exact(vec![DataType::Binary], DataType::Utf8),
                ArrowFunctionSignature::exact(vec![DataType::Utf8], DataType::Utf8),
            ]
        }
    }

    pub struct Blob;
    impl VArrowScalar for Blob {
        type State = Vec<Arc<Store>>;
        fn invoke(
            stores: &Self::State,
            input: RecordBatch,
        ) -> Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
            let mut b = BinaryBuilder::new();
            for v in resolve(stores, &input)? {
                match v {
                    Some(bytes) => b.append_value(bytes),
                    None => b.append_null(),
                }
            }
            Ok(Arc::new(b.finish()))
        }
        fn signatures() -> Vec<ArrowFunctionSignature> {
            vec![
                ArrowFunctionSignature::exact(vec![DataType::Binary], DataType::Binary),
                ArrowFunctionSignature::exact(vec![DataType::Utf8], DataType::Binary),
            ]
        }
    }

    pub fn install(conn: &duckdb::Connection, stores: Vec<Arc<Store>>) -> Result<()> {
        conn.register_scalar_function_with_state::<Text>("text", &stores)?;
        conn.register_scalar_function_with_state::<Blob>("blob", &stores)?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use trajfs_core::ingest::{ingest, IngestOptions};
        use trajfs_core::rules::Rules;

        fn store(root: &std::path::Path, name: &str, bytes: &[u8]) -> Arc<Store> {
            let source = root.join(format!("{name}-source"));
            let destination = root.join(name);
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("content"), bytes).unwrap();
            ingest(
                &source,
                &destination,
                IngestOptions {
                    rules: Rules::from_toml("name = 'none'").unwrap(),
                    rules_name: "none".into(),
                    adapter: &trajfs_core::NoAdapter,
                    label: "fixture".into(),
                    jobs: 2,
                    derive: false,
                    store_id: None,
                },
            )
            .unwrap();
            Arc::new(Store::open(&destination).unwrap())
        }

        fn text(conn: &duckdb::Connection, bytes: &[u8]) -> Option<String> {
            conn.query_row(
                "select text(?)",
                [hex::encode(trajfs_core::hash::sha_of_bytes(bytes))],
                |row| row.get(0),
            )
            .unwrap()
        }

        #[test]
        fn sql_udfs_are_connection_scoped_and_release_store_locks() {
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let left = store(root.path(), "left", b"left");
            let weak_left = Arc::downgrade(&left);
            let right = store(root.path(), "right", b"right");
            let weak_right = Arc::downgrade(&right);
            let a = duckdb::Connection::open_in_memory().unwrap();
            install(&a, vec![left]).unwrap();
            let b = duckdb::Connection::open_in_memory().unwrap();
            install(&b, vec![right]).unwrap();
            assert_eq!(text(&a, b"left").as_deref(), Some("left"));
            assert_eq!(text(&b, b"right").as_deref(), Some("right"));
            assert_eq!(text(&a, b"right"), None);
            assert_eq!(text(&b, b"left"), None);
            assert!(trajfs_core::store::lock_store_exclusive(&root.path().join("left")).is_err());
            drop(a);
            assert!(weak_left.upgrade().is_none());
            assert!(trajfs_core::store::lock_store_exclusive(&root.path().join("left")).is_ok());
            assert_eq!(text(&b, b"right").as_deref(), Some("right"));
            drop(b);
            assert!(weak_right.upgrade().is_none());
        }
    }
}
