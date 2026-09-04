//! Parquet catalog: `files`, `dirs`, `excluded` and the pack `index` (docs/PLAN.md §3.1–§3.3).
//! Segments are sorted by path so readers can prune row groups with the column statistics.

use crate::pack::{IndexRow, Loc};
use crate::{FileRow, Kind, Sha, ROW_GROUP};
use anyhow::{Context, Result};
use arrow::array::{
    Array, ArrayRef, AsArray, FixedSizeBinaryBuilder, Int32Builder, Int64Builder, MapBuilder,
    StringBuilder, UInt16Builder, UInt32Builder, UInt8Builder,
};
use arrow::datatypes::{Int32Type, Int64Type, Schema, UInt16Type, UInt32Type, UInt8Type};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::basic::{Compression, ZstdLevel};
use parquet::data_type::AsBytes;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::statistics::Statistics;
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

pub fn arrow_writer(path: &Path, schema: Arc<Schema>) -> Result<ArrowWriter<File>> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_size(ROW_GROUP)
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .build();
    let f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    Ok(ArrowWriter::try_new(f, schema, Some(props))?)
}

/// Writes a batch (or several) to a file; the writer is created from the first batch's schema.
struct LazyWriter {
    path: std::path::PathBuf,
    writer: Option<ArrowWriter<File>>,
    rows: usize,
}

impl LazyWriter {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            writer: None,
            rows: 0,
        }
    }
    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        if self.writer.is_none() {
            self.writer = Some(arrow_writer(&self.path, batch.schema())?);
        }
        self.writer.as_mut().unwrap().write(batch)?;
        self.rows += batch.num_rows();
        Ok(())
    }
    fn finish(self, empty: impl FnOnce() -> Result<RecordBatch>) -> Result<usize> {
        let mut me = self;
        if me.writer.is_none() {
            let b = empty()?;
            me.write(&b)?;
            me.rows = 0;
        }
        me.writer.take().unwrap().close()?;
        Ok(me.rows)
    }
}

// ---------------------------------------------------------------- files

pub struct FilesWriter {
    out: LazyWriter,
    path: StringBuilder,
    dir: StringBuilder,
    name: StringBuilder,
    kind: UInt8Builder,
    mode: UInt16Builder,
    size: Int64Builder,
    sha: FixedSizeBinaryBuilder,
    mtime: Int64Builder,
    batch: UInt32Builder,
    attrs: MapBuilder<StringBuilder, StringBuilder>,
    pending: usize,
}

impl FilesWriter {
    pub fn create(path: &Path) -> Self {
        Self {
            out: LazyWriter::new(path),
            path: StringBuilder::new(),
            dir: StringBuilder::new(),
            name: StringBuilder::new(),
            kind: UInt8Builder::new(),
            mode: UInt16Builder::new(),
            size: Int64Builder::new(),
            sha: FixedSizeBinaryBuilder::new(32),
            mtime: Int64Builder::new(),
            batch: UInt32Builder::new(),
            attrs: MapBuilder::new(None, StringBuilder::new(), StringBuilder::new()),
            pending: 0,
        }
    }

    /// Rows must be pushed in bytewise path order.
    pub fn push(&mut self, r: &FileRow) -> Result<()> {
        self.path.append_value(&r.path);
        self.dir.append_value(r.dir());
        self.name.append_value(r.name());
        self.kind.append_value(r.kind as u8);
        self.mode.append_value(r.mode);
        self.size.append_value(r.size);
        self.sha.append_value(r.sha)?;
        self.mtime.append_value(r.mtime_ns);
        self.batch.append_value(r.batch);
        for (k, v) in &r.attrs {
            self.attrs.keys().append_value(k);
            self.attrs.values().append_value(v);
        }
        self.attrs.append(true)?;
        self.pending += 1;
        if self.pending >= ROW_GROUP {
            self.flush()?;
        }
        Ok(())
    }

    fn batch(&mut self) -> Result<RecordBatch> {
        let cols: Vec<(&str, ArrayRef)> = vec![
            ("path", Arc::new(self.path.finish())),
            ("dir", Arc::new(self.dir.finish())),
            ("name", Arc::new(self.name.finish())),
            ("kind", Arc::new(self.kind.finish())),
            ("mode", Arc::new(self.mode.finish())),
            ("size", Arc::new(self.size.finish())),
            ("sha", Arc::new(self.sha.finish())),
            ("mtime_ns", Arc::new(self.mtime.finish())),
            ("batch", Arc::new(self.batch.finish())),
            ("attrs", Arc::new(self.attrs.finish())),
        ];
        self.pending = 0;
        Ok(RecordBatch::try_from_iter(cols)?)
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let b = self.batch()?;
        self.out.write(&b)
    }

    pub fn finish(mut self) -> Result<usize> {
        self.flush()?;
        let empty = self.batch()?;
        self.out.finish(|| Ok(empty))
    }
}

/// Row-group pruning helper: indices of row groups whose `path` statistics intersect `[lo, hi)`.
fn prune(
    builder: &ParquetRecordBatchReaderBuilder<File>,
    col: &str,
    lo: &[u8],
    hi: &[u8],
) -> Vec<usize> {
    let schema = builder.parquet_schema();
    let idx = schema.columns().iter().position(|c| c.name() == col);
    let mut out = Vec::new();
    for (i, rg) in builder.metadata().row_groups().iter().enumerate() {
        let keep = match idx.and_then(|ci| rg.column(ci).statistics()) {
            Some(Statistics::ByteArray(s)) => match (s.min_opt(), s.max_opt()) {
                (Some(min), Some(max)) => max.as_bytes() >= lo && min.as_bytes() < hi,
                _ => true,
            },
            _ => true,
        };
        if keep {
            out.push(i);
        }
    }
    out
}

/// Row groups that can hold a *direct* child of `dir`: a group whose min and max paths both lie below the same
/// subdirectory of `dir` holds none, because direct children sort outside such a group.
fn prune_direct(builder: &ParquetRecordBatchReaderBuilder<File>, dir: &str) -> Vec<usize> {
    let prefix: Vec<u8> = if dir.is_empty() {
        Vec::new()
    } else {
        format!("{dir}/").into_bytes()
    };
    let first_component_dir = |p: &[u8]| -> Option<Vec<u8>> {
        // Some(component) when p is under prefix and has a '/' after its first component
        let rest = p.strip_prefix(prefix.as_slice())?;
        let i = rest.iter().position(|&c| c == b'/')?;
        Some(rest[..i].to_vec())
    };
    let schema = builder.parquet_schema();
    let idx = schema.columns().iter().position(|c| c.name() == "path");
    let mut out = Vec::new();
    let mut lo = prefix.clone();
    let mut hi = prefix.clone();
    if dir.is_empty() {
        hi = vec![0xff];
    } else {
        lo.pop();
        lo.push(b'/');
        hi.pop();
        hi.push(b'0');
    }
    for (i, rg) in builder.metadata().row_groups().iter().enumerate() {
        let keep = match idx.and_then(|ci| rg.column(ci).statistics()) {
            Some(Statistics::ByteArray(s)) => match (s.min_opt(), s.max_opt()) {
                (Some(min), Some(max)) => {
                    let (min, max) = (min.as_bytes(), max.as_bytes());
                    let in_range = dir.is_empty() || (max >= lo.as_slice() && min < hi.as_slice());
                    in_range
                        && match (first_component_dir(min), first_component_dir(max)) {
                            (Some(a), Some(b)) => a != b,
                            _ => true,
                        }
                }
                _ => true,
            },
            _ => true,
        };
        if keep {
            out.push(i);
        }
    }
    out
}

/// Direct children (files) of `dir` in one segment, materialising only matching rows.
pub fn scan_direct_children(
    segment: &Path,
    dir: &str,
    with_attrs: bool,
    mut f: impl FnMut(FileRow),
) -> Result<()> {
    let file = File::open(segment).with_context(|| format!("open {}", segment.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let groups = prune_direct(&builder, dir);
    if groups.is_empty() {
        return Ok(());
    }
    let mut cols = vec!["path", "kind", "mode", "size", "sha", "mtime_ns", "batch"];
    if with_attrs {
        cols.push("attrs");
    }
    let mask = ProjectionMask::columns(builder.parquet_schema(), cols.iter().copied());
    let reader = builder
        .with_row_groups(groups)
        .with_projection(mask)
        .with_batch_size(ROW_GROUP)
        .build()?;
    for batch in reader {
        let batch = batch?;
        decode_rows(&batch, with_attrs, |p| crate::parent_of(p) == dir, &mut f);
    }
    Ok(())
}

/// Scan one `files` segment. `range` = `[lo, hi)` on the bytewise path order; `None` = everything.
/// `columns` restricts the columns read (attrs is only decoded when requested).
pub fn scan_files(
    segment: &Path,
    range: Option<(&[u8], &[u8])>,
    with_attrs: bool,
    f: impl FnMut(FileRow),
) -> Result<()> {
    scan_files_filtered(segment, range, with_attrs, |_| true, f)
}

/// Like `scan_files`, but rows whose path fails `pre` are skipped before any other column is decoded.
pub fn scan_files_filtered(
    segment: &Path,
    range: Option<(&[u8], &[u8])>,
    with_attrs: bool,
    pre: impl Fn(&str) -> bool,
    mut f: impl FnMut(FileRow),
) -> Result<()> {
    let file = File::open(segment).with_context(|| format!("open {}", segment.display()))?;
    let mut builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    if let Some((lo, hi)) = range {
        let groups = prune(&builder, "path", lo, hi);
        if groups.is_empty() {
            return Ok(());
        }
        builder = builder.with_row_groups(groups);
    }
    let mut cols = vec!["path", "kind", "mode", "size", "sha", "mtime_ns", "batch"];
    if with_attrs {
        cols.push("attrs");
    }
    let mask = ProjectionMask::columns(builder.parquet_schema(), cols.iter().copied());
    let reader = builder
        .with_projection(mask)
        .with_batch_size(ROW_GROUP)
        .build()?;
    for batch in reader {
        let batch = batch?;
        let pre_range = |p: &str| {
            if let Some((lo, hi)) = range {
                let pb = p.as_bytes();
                if pb < lo || pb >= hi {
                    return false;
                }
            }
            pre(p)
        };
        decode_rows(&batch, with_attrs, pre_range, &mut f);
    }
    Ok(())
}

fn decode_rows(
    batch: &RecordBatch,
    with_attrs: bool,
    pre: impl Fn(&str) -> bool,
    f: &mut impl FnMut(FileRow),
) {
    let by = |n: &str| {
        batch
            .column_by_name(n)
            .unwrap_or_else(|| panic!("column {n}"))
    };
    let path = by("path").as_string::<i32>();
    let kind = by("kind").as_primitive::<UInt8Type>();
    let mode = by("mode").as_primitive::<UInt16Type>();
    let size = by("size").as_primitive::<Int64Type>();
    let sha = by("sha").as_fixed_size_binary();
    let mtime = by("mtime_ns").as_primitive::<Int64Type>();
    let bt = by("batch").as_primitive::<UInt32Type>();
    let attrs = if with_attrs {
        Some(by("attrs").as_map())
    } else {
        None
    };
    for i in 0..batch.num_rows() {
        let p = path.value(i);
        if !pre(p) {
            continue;
        }
        let mut a = Vec::new();
        if let Some(m) = attrs {
            let entries = m.value(i);
            let ks = entries.column(0).as_string::<i32>();
            let vs = entries.column(1).as_string::<i32>();
            for j in 0..entries.len() {
                a.push((ks.value(j).to_string(), vs.value(j).to_string()));
            }
        }
        f(FileRow {
            path: p.to_string(),
            kind: Kind::from_u8(kind.value(i)).unwrap_or(Kind::File),
            mode: mode.value(i),
            size: size.value(i),
            sha: sha.value(i).try_into().unwrap(),
            mtime_ns: mtime.value(i),
            batch: bt.value(i),
            attrs: a,
        });
    }
}

// ---------------------------------------------------------------- dirs

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirRow {
    pub dir: String,
    pub depth: u16,
    pub n_files: i64,
    pub n_dirs: i64,
    pub bytes: i64,
    pub batch: u32,
}

impl DirRow {
    pub fn parent(&self) -> &str {
        crate::parent_of(&self.dir)
    }
    pub fn name(&self) -> &str {
        crate::basename_of(&self.dir)
    }
}

/// Aggregate directory rows for a batch from its file rows (every ancestor, including the root `""`).
pub fn dirs_from_files<'a>(rows: impl Iterator<Item = &'a FileRow>, batch: u32) -> Vec<DirRow> {
    let mut m: BTreeMap<String, DirRow> = BTreeMap::new();
    let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in rows {
        let dir = r.dir().to_string();
        // count this file in its own dir and bytes in every ancestor
        let mut cur = dir.clone();
        loop {
            let e = m.entry(cur.clone()).or_insert_with(|| DirRow {
                dir: cur.clone(),
                depth: if cur.is_empty() {
                    0
                } else {
                    cur.matches('/').count() as u16 + 1
                },
                batch,
                ..Default::default()
            });
            // n_files and bytes are recursive (everything below the directory); n_dirs is direct children
            e.n_files += 1;
            e.bytes += r.size;
            if cur.is_empty() {
                break;
            }
            let parent = crate::parent_of(&cur).to_string();
            // register cur as a subdir of parent once
            if seen_dirs.insert(cur.clone()) {
                let pe = m.entry(parent.clone()).or_insert_with(|| DirRow {
                    dir: parent.clone(),
                    depth: if parent.is_empty() {
                        0
                    } else {
                        parent.matches('/').count() as u16 + 1
                    },
                    batch,
                    ..Default::default()
                });
                pe.n_dirs += 1;
            }
            cur = parent;
        }
    }
    m.into_values().collect()
}

pub fn write_dirs(path: &Path, rows: &[DirRow]) -> Result<()> {
    let mut dir = StringBuilder::new();
    let mut parent = StringBuilder::new();
    let mut name = StringBuilder::new();
    let mut depth = UInt16Builder::new();
    let mut nf = Int64Builder::new();
    let mut nd = Int64Builder::new();
    let mut bytes = Int64Builder::new();
    let mut batch = UInt32Builder::new();
    for r in rows {
        dir.append_value(&r.dir);
        parent.append_value(r.parent());
        name.append_value(r.name());
        depth.append_value(r.depth);
        nf.append_value(r.n_files);
        nd.append_value(r.n_dirs);
        bytes.append_value(r.bytes);
        batch.append_value(r.batch);
    }
    let b = RecordBatch::try_from_iter(vec![
        ("dir", Arc::new(dir.finish()) as ArrayRef),
        ("parent", Arc::new(parent.finish())),
        ("name", Arc::new(name.finish())),
        ("depth", Arc::new(depth.finish())),
        ("n_files", Arc::new(nf.finish())),
        ("n_dirs", Arc::new(nd.finish())),
        ("bytes", Arc::new(bytes.finish())),
        ("batch", Arc::new(batch.finish())),
    ])?;
    let mut w = arrow_writer(path, b.schema())?;
    w.write(&b)?;
    w.close()?;
    Ok(())
}

/// Scan a `dirs` segment; `range` prunes on the `dir` column like `scan_files` does on `path`.
pub fn scan_dirs(
    segment: &Path,
    range: Option<(&[u8], &[u8])>,
    mut f: impl FnMut(DirRow),
) -> Result<()> {
    let file = File::open(segment).with_context(|| format!("open {}", segment.display()))?;
    let mut builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    if let Some((lo, hi)) = range {
        let groups = prune(&builder, "dir", lo, hi);
        if groups.is_empty() {
            return Ok(());
        }
        builder = builder.with_row_groups(groups);
    }
    let reader = builder.with_batch_size(ROW_GROUP).build()?;
    for batch in reader {
        let batch = batch?;
        let by = |n: &str| {
            batch
                .column_by_name(n)
                .unwrap_or_else(|| panic!("column {n}"))
        };
        let dir = by("dir").as_string::<i32>();
        let depth = by("depth").as_primitive::<UInt16Type>();
        let nf = by("n_files").as_primitive::<Int64Type>();
        let nd = by("n_dirs").as_primitive::<Int64Type>();
        let bytes = by("bytes").as_primitive::<Int64Type>();
        let bt = by("batch").as_primitive::<UInt32Type>();
        for i in 0..batch.num_rows() {
            let d = dir.value(i);
            if let Some((lo, hi)) = range {
                let db = d.as_bytes();
                if db < lo || db >= hi {
                    continue;
                }
            }
            f(DirRow {
                dir: d.to_string(),
                depth: depth.value(i),
                n_files: nf.value(i),
                n_dirs: nd.value(i),
                bytes: bytes.value(i),
                batch: bt.value(i),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- excluded

pub fn write_excluded(path: &Path, rows: &[crate::walk::Excluded], batch: u32) -> Result<()> {
    let mut p = StringBuilder::new();
    let mut size = Int64Builder::new();
    let mut rule = StringBuilder::new();
    let mut bt = UInt32Builder::new();
    for r in rows {
        p.append_value(&r.rel);
        size.append_value(r.size as i64);
        rule.append_value(r.rule);
        bt.append_value(batch);
    }
    let b = RecordBatch::try_from_iter(vec![
        ("path", Arc::new(p.finish()) as ArrayRef),
        ("size", Arc::new(size.finish())),
        ("rule", Arc::new(rule.finish())),
        ("batch", Arc::new(bt.finish())),
    ])?;
    let mut w = arrow_writer(path, b.schema())?;
    w.write(&b)?;
    w.close()?;
    Ok(())
}

// ---------------------------------------------------------------- index

pub fn write_index(path: &Path, rows: &[IndexRow]) -> Result<()> {
    let mut sha = FixedSizeBinaryBuilder::new(32);
    let mut pack = UInt32Builder::new();
    let mut co = Int64Builder::new();
    let mut cl = Int32Builder::new();
    let mut off = Int32Builder::new();
    let mut size = Int64Builder::new();
    let mut part = UInt16Builder::new();
    for r in rows {
        sha.append_value(r.sha)?;
        pack.append_value(r.loc.pack);
        co.append_value(r.loc.chunk_offset);
        cl.append_value(r.loc.chunk_len);
        off.append_value(r.loc.offset);
        size.append_value(r.loc.size);
        part.append_value(r.loc.part);
    }
    let b = RecordBatch::try_from_iter(vec![
        ("sha", Arc::new(sha.finish()) as ArrayRef),
        ("pack", Arc::new(pack.finish())),
        ("chunk_offset", Arc::new(co.finish())),
        ("chunk_len", Arc::new(cl.finish())),
        ("offset", Arc::new(off.finish())),
        ("size", Arc::new(size.finish())),
        ("part", Arc::new(part.finish())),
    ])?;
    let mut w = arrow_writer(path, b.schema())?;
    w.write(&b)?;
    w.close()?;
    Ok(())
}

pub fn read_index(segment: &Path, mut f: impl FnMut(Sha, Loc)) -> Result<()> {
    let file = File::open(segment).with_context(|| format!("open {}", segment.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(ROW_GROUP)
        .build()?;
    for batch in reader {
        let batch = batch?;
        let by = |n: &str| {
            batch
                .column_by_name(n)
                .unwrap_or_else(|| panic!("column {n}"))
        };
        let sha = by("sha").as_fixed_size_binary();
        let pack = by("pack").as_primitive::<UInt32Type>();
        let co = by("chunk_offset").as_primitive::<Int64Type>();
        let cl = by("chunk_len").as_primitive::<Int32Type>();
        let off = by("offset").as_primitive::<Int32Type>();
        let size = by("size").as_primitive::<Int64Type>();
        let part = by("part").as_primitive::<UInt16Type>();
        for i in 0..batch.num_rows() {
            f(
                sha.value(i).try_into().unwrap(),
                Loc {
                    pack: pack.value(i),
                    chunk_offset: co.value(i),
                    chunk_len: cl.value(i),
                    offset: off.value(i),
                    size: size.value(i),
                    part: part.value(i),
                },
            );
        }
    }
    Ok(())
}

/// Number of rows in any Parquet file (from the footer, no scan).
pub fn row_count(path: &Path) -> Result<i64> {
    let file = File::open(path)?;
    let b = ParquetRecordBatchReaderBuilder::try_new(file)?;
    Ok(b.metadata().file_metadata().num_rows())
}
