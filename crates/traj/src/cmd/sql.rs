//! `traj sql`: DuckDB over the store's Parquet files (docs/PLAN.md §5).

use crate::config::{resolve_store, Config};
use anyhow::{bail, Result};
use clap::Args;

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
    pub root: std::path::PathBuf,
    pub adapter: String,
}

pub fn view_sql(stores: &[StoreTables]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let tables = [
        ("files", "catalog/files-*.parquet"),
        ("dirs", "catalog/dirs-*.parquet"),
        ("excluded", "catalog/excluded-*.parquet"),
        ("blobs", "packs/index-*.parquet"),
    ];
    for (t, glob) in tables {
        let parts: Vec<String> = stores
            .iter()
            .map(|s| {
                format!(
                    "select '{}' as store, * from read_parquet('{}/{}', union_by_name=true)",
                    s.name.replace('\'', "''"),
                    s.root.display(),
                    glob
                )
            })
            .collect();
        out.push((
            t.to_string(),
            format!("create view {t} as {}", parts.join(" union all by name ")),
        ));
    }
    let ev: Vec<String> = stores
        .iter()
        .filter(|s| s.root.join("derived").join(&s.adapter).is_dir())
        .map(|s| format!("select '{}' as store, * from read_parquet('{}/derived/{}/events-*.parquet', union_by_name=true)", s.name.replace('\'', "''"), s.root.display(), s.adapter))
        .collect();
    if !ev.is_empty() {
        out.push((
            "events".into(),
            format!("create view events as {}", ev.join(" union all by name ")),
        ));
    }
    out
}

pub fn tables_for(stores: &[String]) -> Result<Vec<StoreTables>> {
    let cfg = Config::find();
    if stores.is_empty() {
        bail!("no store given: pass -S <store> (repeatable)");
    }
    let mut v = Vec::new();
    for s in stores {
        let root = resolve_store(s, cfg.as_ref())?.canonicalize()?;
        let m = trajfs_core::Manifest::load(&root)?;
        v.push(StoreTables {
            name: m.store_id.clone(),
            root,
            adapter: m.adapter.name.clone(),
        });
    }
    Ok(v)
}

#[cfg(feature = "sql")]
pub fn run(stores: &[String], a: SqlArgs) -> Result<i32> {
    use std::io::Read;
    let tables = tables_for(stores)?;
    let views = view_sql(&tables);
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
    // blob(sha) / text(sha): bytes of a blob by sha (BLOB or hex VARCHAR), looked up in the first store
    udf::install(&conn, &tables[0].root)?;
    let mut stmt = conn.prepare(&query)?;
    let batches: Vec<duckdb::arrow::array::RecordBatch> = stmt.query_arrow([])?.collect();
    if a.csv {
        print_csv(&batches)?;
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
fn print_csv(batches: &[duckdb::arrow::array::RecordBatch]) -> Result<()> {
    use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let opts = FormatOptions::default().with_null("");
    let mut header_done = false;
    for b in batches {
        if !header_done {
            let names: Vec<String> = b
                .schema()
                .fields()
                .iter()
                .map(|f| csv_quote(f.name()))
                .collect();
            writeln!(out, "{}", names.join(","))?;
            header_done = true;
        }
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
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(feature = "sql")]
mod udf {
    use anyhow::Result;
    use duckdb::arrow::array::{
        Array, BinaryArray, BinaryBuilder, FixedSizeBinaryArray, LargeBinaryArray,
        LargeStringArray, RecordBatch, StringArray, StringBuilder,
    };
    use duckdb::arrow::datatypes::DataType;
    use duckdb::vscalar::arrow::{ArrowFunctionSignature, VArrowScalar};
    use std::sync::{Arc, Mutex, OnceLock};
    use trajfs_core::Store;

    static STORE: OnceLock<Arc<Store>> = OnceLock::new();

    fn store() -> Arc<Store> {
        STORE.get().expect("udf store").clone()
    }

    /// Bytes for every input row (None when the sha is unknown or malformed).
    fn resolve(input: &RecordBatch) -> Vec<Option<Vec<u8>>> {
        let st = store();
        let mut reader = st.reader();
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
            let v = sha_at(i).and_then(|sha| st.read_sha(&mut reader, &sha).ok());
            out.push(v);
        }
        out
    }

    pub struct Text;
    impl VArrowScalar for Text {
        type State = ();
        fn invoke(
            _: &(),
            input: RecordBatch,
        ) -> Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
            let mut b = StringBuilder::new();
            for v in resolve(&input) {
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
        type State = ();
        fn invoke(
            _: &(),
            input: RecordBatch,
        ) -> Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
            let mut b = BinaryBuilder::new();
            for v in resolve(&input) {
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

    static INSTALL_LOCK: Mutex<()> = Mutex::new(());

    pub fn install(conn: &duckdb::Connection, root: &std::path::Path) -> Result<()> {
        let _g = INSTALL_LOCK.lock().unwrap();
        if STORE.get().is_none() {
            let _ = STORE.set(Arc::new(Store::open(root)?));
        }
        conn.register_scalar_function::<Text>("text")?;
        conn.register_scalar_function::<Blob>("blob")?;
        Ok(())
    }
}
