//! Parquet catalog: `files`, `dirs`, `excluded` and the pack `index` (docs/PLAN.md §3.1–§3.3).
//! Physical segments are size-bounded; file and directory rows remain sorted so readers can prune row groups.

use crate::pack::{IndexRow, Loc};
use crate::{FileRow, Kind, Sha, ROW_GROUP};
use anyhow::{bail, Context, Result};
use arrow::array::{
    Array, ArrayRef, AsArray, FixedSizeBinaryBuilder, Int32Builder, Int64Builder, MapBuilder,
    StringBuilder, UInt16Builder, UInt32Builder, UInt8Builder,
};
use arrow::datatypes::{DataType, Int32Type, Int64Type, Schema, UInt16Type, UInt32Type, UInt8Type};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::basic::{Compression, ZstdLevel};
use parquet::data_type::AsBytes;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::statistics::Statistics;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn require_columns(schema: &Schema, columns: &[(&str, DataType)]) -> Result<()> {
    for (name, expected) in columns {
        let field = schema
            .field_with_name(name)
            .with_context(|| format!("missing catalog column {name}"))?;
        if field.data_type() != expected {
            bail!(
                "catalog column {name} has type {}, expected {expected}",
                field.data_type()
            );
        }
    }
    Ok(())
}

fn files_schema(schema: &Schema) -> Result<()> {
    require_columns(
        schema,
        &[
            ("path", DataType::Utf8),
            ("dir", DataType::Utf8),
            ("name", DataType::Utf8),
            ("kind", DataType::UInt8),
            ("mode", DataType::UInt16),
            ("size", DataType::Int64),
            ("sha", DataType::FixedSizeBinary(32)),
            ("mtime_ns", DataType::Int64),
            ("batch", DataType::UInt32),
        ],
    )?;
    let attrs = schema
        .field_with_name("attrs")
        .context("missing catalog column attrs")?;
    if let DataType::Map(entries, _) = attrs.data_type() {
        if let DataType::Struct(fields) = entries.data_type() {
            if fields.len() == 2 && fields.iter().all(|f| f.data_type() == &DataType::Utf8) {
                return Ok(());
            }
        }
    }
    bail!("catalog column attrs must be a map of UTF-8 keys and values")
}

fn dirs_schema(schema: &Schema) -> Result<()> {
    require_columns(
        schema,
        &[
            ("dir", DataType::Utf8),
            ("parent", DataType::Utf8),
            ("name", DataType::Utf8),
            ("depth", DataType::UInt16),
            ("n_files", DataType::Int64),
            ("n_dirs", DataType::Int64),
            ("bytes", DataType::Int64),
            ("batch", DataType::UInt32),
        ],
    )
}

fn index_schema(schema: &Schema) -> Result<()> {
    require_columns(
        schema,
        &[
            ("sha", DataType::FixedSizeBinary(32)),
            ("pack", DataType::UInt32),
            ("chunk_offset", DataType::Int64),
            ("chunk_len", DataType::Int32),
            ("offset", DataType::Int32),
            ("size", DataType::Int64),
            ("part", DataType::UInt16),
        ],
    )
}

fn require_non_null(batch: &RecordBatch) -> Result<()> {
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        if column.null_count() > 0 {
            bail!("catalog column {} contains null values", field.name());
        }
    }
    Ok(())
}

pub(crate) fn validate_catalog_path(path: &str, root_allowed: bool) -> Result<()> {
    if root_allowed && path.is_empty() {
        return Ok(());
    }
    if path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("invalid relative catalog path {path:?}");
    }
    Ok(())
}

pub fn arrow_writer(path: &Path, schema: Arc<Schema>) -> Result<ArrowWriter<File>> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_size(ROW_GROUP)
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .build();
    let f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    Ok(ArrowWriter::try_new(f, schema, Some(props))?)
}

fn numbered_segment_path(dir: &Path, stem: &str, part: usize) -> PathBuf {
    dir.join(format!("{stem}-{part:04}.parquet"))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

pub(crate) fn validate_segment_stem(stem: &str) -> Result<()> {
    crate::manifest::validate_adapter_name(stem).context("invalid Parquet segment stem")
}

fn publish_segment(source: &Path, destination: &Path) -> Result<()> {
    // Durable before it can be declared: the manifest is only published after its artifacts are on disk.
    File::open(source)?.sync_all()?;
    std::fs::hard_link(source, destination)
        .with_context(|| format!("publish immutable artifact {}", destination.display()))?;
    std::fs::remove_file(source)?;
    Ok(())
}

fn write_segment_attempt<T>(
    dir: &Path,
    stem: &str,
    rows: &[T],
    max_bytes: u64,
    next_part: &mut usize,
    out: &mut Vec<PathBuf>,
    write_one: &impl Fn(&Path, &[T]) -> Result<()>,
) -> Result<()> {
    let final_path = numbered_segment_path(dir, stem, *next_part);
    let tmp = temp_path(&final_path);
    let _ = std::fs::remove_file(&tmp);
    if let Err(error) = write_one(&tmp, rows) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    let size = std::fs::metadata(&tmp)?.len();
    if size <= max_bytes {
        if let Err(error) = publish_segment(&tmp, &final_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        out.push(final_path);
        *next_part += 1;
        return Ok(());
    }
    std::fs::remove_file(&tmp)?;
    if rows.len() <= 1 {
        bail!(
            "{stem} contains a single row whose Parquet encoding is {size} bytes (limit {max_bytes})"
        );
    }

    // Use the observed compressed size to avoid repeatedly bisecting very large
    // inputs. Any uneven partitions are checked again recursively.
    let desired = (max_bytes.saturating_mul(9) / 10).max(1);
    let pieces = usize::try_from(size.div_ceil(desired))
        .unwrap_or(usize::MAX)
        .max(2)
        .min(rows.len());
    let mut start = 0;
    for i in 0..pieces {
        let remaining_rows = rows.len() - start;
        let remaining_pieces = pieces - i;
        let take = remaining_rows.div_ceil(remaining_pieces);
        let end = start + take;
        write_segment_attempt(
            dir,
            stem,
            &rows[start..end],
            max_bytes,
            next_part,
            out,
            write_one,
        )?;
        start = end;
    }
    Ok(())
}

/// Write numbered Parquet segments, checking their final on-disk size. A row
/// group-sized probe estimates the initial partition count for large inputs;
/// exact size checks and recursive splitting are authoritative.
pub(crate) fn write_sized_segments<T>(
    dir: &Path,
    stem: &str,
    rows: &[T],
    max_bytes: u64,
    first_part: usize,
    write_one: impl Fn(&Path, &[T]) -> Result<()>,
) -> Result<Vec<PathBuf>> {
    validate_segment_stem(stem)?;
    if max_bytes == 0 {
        bail!("Parquet segment size limit must be greater than zero");
    }
    std::fs::create_dir_all(dir)?;
    let mut next_part = first_part;
    let mut out = Vec::new();
    if rows.is_empty() {
        write_segment_attempt(
            dir,
            stem,
            rows,
            max_bytes,
            &mut next_part,
            &mut out,
            &write_one,
        )?;
        return Ok(out);
    }

    let pieces = if rows.len() <= ROW_GROUP {
        1
    } else {
        let probe_path = temp_path(&dir.join(format!("{stem}-probe.parquet")));
        let _ = std::fs::remove_file(&probe_path);
        if let Err(error) = write_one(&probe_path, &rows[..ROW_GROUP]) {
            let _ = std::fs::remove_file(&probe_path);
            return Err(error);
        }
        let probe_bytes = std::fs::metadata(&probe_path)?.len();
        std::fs::remove_file(&probe_path)?;
        let estimated = (probe_bytes as u128 * rows.len() as u128).div_ceil(ROW_GROUP as u128);
        let desired = (max_bytes.saturating_mul(4) / 5).max(1) as u128;
        usize::try_from(estimated.div_ceil(desired))
            .unwrap_or(usize::MAX)
            .max(1)
            .min(rows.len())
    };
    let mut start = 0;
    for i in 0..pieces {
        let remaining_rows = rows.len() - start;
        let remaining_pieces = pieces - i;
        let take = remaining_rows.div_ceil(remaining_pieces);
        let end = start + take;
        write_segment_attempt(
            dir,
            stem,
            &rows[start..end],
            max_bytes,
            &mut next_part,
            &mut out,
            &write_one,
        )?;
        start = end;
    }
    Ok(out)
}

pub(crate) fn rename_single_segment(paths: &mut [PathBuf], dir: &Path, stem: &str) -> Result<()> {
    if paths.len() == 1 {
        let legacy = dir.join(format!("{stem}.parquet"));
        publish_segment(&paths[0], &legacy)?;
        paths[0] = legacy;
    }
    Ok(())
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

pub fn write_files(path: &Path, rows: &[FileRow]) -> Result<()> {
    let mut writer = FilesWriter::create(path);
    for row in rows {
        writer.push(row)?;
    }
    writer.finish()?;
    Ok(())
}

/// Row-group pruning helper: indices of row groups whose `path` statistics intersect `[lo, hi)`.
fn prune(
    builder: &ParquetRecordBatchReaderBuilder<File>,
    col: &str,
    lo: &[u8],
    hi: &[u8],
) -> Vec<usize> {
    let schema = builder.parquet_schema();
    let idx = schema
        .columns()
        .iter()
        .position(|c| c.path().parts().len() == 1 && c.path().parts()[0] == col);
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
    let idx = schema
        .columns()
        .iter()
        .position(|c| c.path().parts().len() == 1 && c.path().parts()[0] == "path");
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
    files_schema(builder.schema()).with_context(|| format!("catalog {}", segment.display()))?;
    let groups = prune_direct(&builder, dir);
    if groups.is_empty() {
        return Ok(());
    }
    let mut cols = vec![
        "path", "dir", "name", "kind", "mode", "size", "sha", "mtime_ns", "batch",
    ];
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
        decode_rows(&batch, with_attrs, |p| crate::parent_of(p) == dir, &mut f)
            .with_context(|| format!("catalog {}", segment.display()))?;
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
    files_schema(builder.schema()).with_context(|| format!("catalog {}", segment.display()))?;
    if let Some((lo, hi)) = range {
        let groups = prune(&builder, "path", lo, hi);
        if groups.is_empty() {
            return Ok(());
        }
        builder = builder.with_row_groups(groups);
    }
    let mut cols = vec![
        "path", "dir", "name", "kind", "mode", "size", "sha", "mtime_ns", "batch",
    ];
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
        decode_rows(&batch, with_attrs, pre_range, &mut f)
            .with_context(|| format!("catalog {}", segment.display()))?;
    }
    Ok(())
}

fn decode_rows(
    batch: &RecordBatch,
    with_attrs: bool,
    pre: impl Fn(&str) -> bool,
    f: &mut impl FnMut(FileRow),
) -> Result<()> {
    require_non_null(batch)?;
    let by = |n: &str| {
        batch
            .column_by_name(n)
            .unwrap_or_else(|| panic!("column {n}"))
    };
    let path = by("path").as_string::<i32>();
    let dir = by("dir").as_string::<i32>();
    let name = by("name").as_string::<i32>();
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
        validate_catalog_path(p, false)?;
        if dir.value(i) != crate::parent_of(p) || name.value(i) != crate::basename_of(p) {
            bail!("{p}: catalog parent or basename does not match the path");
        }
        let kind = Kind::from_u8(kind.value(i))
            .with_context(|| format!("{p}: invalid catalog file kind {}", kind.value(i)))?;
        let hash: Sha = sha
            .value(i)
            .try_into()
            .context("invalid catalog SHA width")?;
        if size.value(i) < 0 {
            bail!("{p}: negative catalog file size");
        }
        if mode.value(i) & !0o777 != 0 {
            bail!("{p}: catalog mode contains special permission or file-type bits");
        }
        if kind == Kind::Empty && (size.value(i) != 0 || hash != crate::hash::sha_of_bytes(b"")) {
            bail!("{p}: empty file has a nonempty size or content hash");
        }
        let mut a = Vec::new();
        if let Some(m) = attrs {
            let entries = m.value(i);
            let ks = entries.column(0).as_string::<i32>();
            let vs = entries.column(1).as_string::<i32>();
            if ks.null_count() > 0 || vs.null_count() > 0 {
                bail!("{p}: catalog attrs contain null keys or values");
            }
            for j in 0..entries.len() {
                a.push((ks.value(j).to_string(), vs.value(j).to_string()));
            }
        }
        f(FileRow {
            path: p.to_string(),
            kind,
            mode: mode.value(i),
            size: size.value(i),
            sha: hash,
            mtime_ns: mtime.value(i),
            batch: bt.value(i),
            attrs: a,
        });
    }
    Ok(())
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
    let mut dirs = DirAccumulator::new(batch);
    for r in rows {
        dirs.push(r);
    }
    dirs.into_rows().into_values().collect()
}

pub(crate) struct DirAccumulator {
    batch: u32,
    rows: Vec<DirRow>,
    index: HashMap<String, usize>,
    /// The previous row's directory and the row indices of that directory and every ancestor
    /// (the directory itself first, the root last). Rows arrive sorted by path, so consecutive
    /// files almost always share a directory and the chain is reused instead of re-walked.
    last_dir: Option<String>,
    last_chain: Vec<usize>,
}

impl DirAccumulator {
    pub(crate) fn new(batch: u32) -> Self {
        Self {
            batch,
            rows: Vec::new(),
            index: HashMap::new(),
            last_dir: None,
            last_chain: Vec::new(),
        }
    }

    /// The row index for `dir`, creating an empty row on first sight.
    fn row_for(&mut self, dir: &str) -> (usize, bool) {
        if let Some(&i) = self.index.get(dir) {
            return (i, false);
        }
        let i = self.rows.len();
        self.rows.push(DirRow {
            dir: dir.to_string(),
            depth: if dir.is_empty() {
                0
            } else {
                dir.matches('/').count() as u16 + 1
            },
            batch: self.batch,
            ..Default::default()
        });
        self.index.insert(dir.to_string(), i);
        (i, true)
    }

    pub(crate) fn push(&mut self, r: &FileRow) {
        let dir = r.dir();
        if self.last_dir.as_deref() != Some(dir) {
            // walk up once per distinct directory: create rows, register each new directory as a
            // direct child of its parent, and remember the chain of row indices
            let mut chain = Vec::new();
            let mut cur = dir;
            let mut child_is_new = false;
            loop {
                let (i, created) = self.row_for(cur);
                if child_is_new {
                    // the directory below `cur` was seen for the first time: one more direct child
                    self.rows[i].n_dirs += 1;
                }
                chain.push(i);
                if cur.is_empty() {
                    break;
                }
                child_is_new = created;
                cur = crate::parent_of(cur);
            }
            self.last_dir = Some(dir.to_string());
            self.last_chain = chain;
        }
        // n_files and bytes are recursive (everything below the directory); n_dirs is direct children
        for &i in &self.last_chain {
            let e = &mut self.rows[i];
            e.n_files += 1;
            e.bytes = e.bytes.saturating_add(r.size);
        }
    }

    pub(crate) fn into_rows(self) -> BTreeMap<String, DirRow> {
        self.rows.into_iter().map(|r| (r.dir.clone(), r)).collect()
    }
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
    dirs_schema(builder.schema()).with_context(|| format!("catalog {}", segment.display()))?;
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
        require_non_null(&batch)?;
        let by = |n: &str| {
            batch
                .column_by_name(n)
                .unwrap_or_else(|| panic!("column {n}"))
        };
        let dir = by("dir").as_string::<i32>();
        let parent = by("parent").as_string::<i32>();
        let name = by("name").as_string::<i32>();
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
            validate_catalog_path(d, true)?;
            let expected_depth = if d.is_empty() {
                0
            } else {
                d.split('/').count()
            };
            if depth.value(i) as usize != expected_depth
                || parent.value(i) != crate::parent_of(d)
                || name.value(i) != crate::basename_of(d)
                || nf.value(i) < 0
                || nd.value(i) < 0
                || bytes.value(i) < 0
            {
                bail!("{d}: invalid catalog directory metadata");
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
        rule.append_value(&r.rule);
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

fn pad_empty_segments<T>(
    paths: &mut Vec<PathBuf>,
    count: usize,
    dir: &Path,
    stem: &str,
    max_bytes: u64,
    write_one: impl Fn(&Path, &[T]) -> Result<()>,
) -> Result<()> {
    while paths.len() < count {
        let mut next_part = paths.len() + 1;
        write_segment_attempt(dir, stem, &[], max_bytes, &mut next_part, paths, &write_one)?;
    }
    Ok(())
}

/// Write the four batch-scoped Parquet tables as synchronized physical
/// segments. Every `files` segment has a matching `dirs`, `excluded`, and
/// `index` segment, preserving the manifest and hook contract used by V1
/// stores. Tables that need fewer physical pieces are padded with empty,
/// schema-correct Parquet files.
pub fn write_batch_segments(
    store: &Path,
    batch: u32,
    files: &[FileRow],
    dirs: &[DirRow],
    excluded: &[crate::walk::Excluded],
    index: &[IndexRow],
    max_bytes: u64,
) -> Result<Vec<String>> {
    let catalog_dir = store.join("catalog");
    let packs_dir = store.join("packs");
    let files_stem = format!("files-{batch:04}");
    let dirs_stem = format!("dirs-{batch:04}");
    let excluded_stem = format!("excluded-{batch:04}");
    let index_stem = format!("index-{batch:04}");

    let mut files_paths =
        write_sized_segments(&catalog_dir, &files_stem, files, max_bytes, 1, write_files)?;
    let mut dirs_paths =
        write_sized_segments(&catalog_dir, &dirs_stem, dirs, max_bytes, 1, write_dirs)?;
    let mut excluded_paths = write_sized_segments(
        &catalog_dir,
        &excluded_stem,
        excluded,
        max_bytes,
        1,
        |path, rows| write_excluded(path, rows, batch),
    )?;
    let mut index_paths =
        write_sized_segments(&packs_dir, &index_stem, index, max_bytes, 1, write_index)?;

    let count = [
        files_paths.len(),
        dirs_paths.len(),
        excluded_paths.len(),
        index_paths.len(),
    ]
    .into_iter()
    .max()
    .unwrap_or(1);
    pad_empty_segments(
        &mut files_paths,
        count,
        &catalog_dir,
        &files_stem,
        max_bytes,
        write_files,
    )?;
    pad_empty_segments(
        &mut dirs_paths,
        count,
        &catalog_dir,
        &dirs_stem,
        max_bytes,
        write_dirs,
    )?;
    pad_empty_segments(
        &mut excluded_paths,
        count,
        &catalog_dir,
        &excluded_stem,
        max_bytes,
        |path, rows| write_excluded(path, rows, batch),
    )?;
    pad_empty_segments(
        &mut index_paths,
        count,
        &packs_dir,
        &index_stem,
        max_bytes,
        write_index,
    )?;

    if count == 1 {
        rename_single_segment(&mut files_paths, &catalog_dir, &files_stem)?;
        rename_single_segment(&mut dirs_paths, &catalog_dir, &dirs_stem)?;
        rename_single_segment(&mut excluded_paths, &catalog_dir, &excluded_stem)?;
        rename_single_segment(&mut index_paths, &packs_dir, &index_stem)?;
        Ok(vec![files_stem])
    } else {
        Ok((1..=count)
            .map(|part| format!("{files_stem}-{part:04}"))
            .collect())
    }
}

pub fn read_index(segment: &Path, mut f: impl FnMut(Sha, Loc)) -> Result<()> {
    let file = File::open(segment).with_context(|| format!("open {}", segment.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    index_schema(builder.schema()).with_context(|| format!("index {}", segment.display()))?;
    let reader = builder.with_batch_size(ROW_GROUP).build()?;
    for batch in reader {
        let batch = batch?;
        require_non_null(&batch)?;
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
            let max_frame = (crate::CHUNK_BYTES * 2 + 1024) as i64;
            if pack.value(i) == 0
                || co.value(i) < crate::pack::MAGIC.len() as i64
                || cl.value(i) <= 0
                || cl.value(i) as i64 > max_frame
                || off.value(i) < 0
                || size.value(i) < 0
                || (off.value(i) as i64)
                    .checked_add(size.value(i))
                    .is_none_or(|end| end > max_frame)
            {
                bail!(
                    "index {}: invalid blob location at row {i}",
                    segment.display()
                );
            }
            f(
                sha.value(i).try_into().context("invalid index SHA width")?,
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

/// Read every exclusion record of one `excluded` segment.
pub fn read_excluded(segment: &Path, mut f: impl FnMut(crate::walk::Excluded)) -> Result<()> {
    let file = File::open(segment).with_context(|| format!("open {}", segment.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    require_columns(
        builder.schema(),
        &[
            ("path", DataType::Utf8),
            ("size", DataType::Int64),
            ("rule", DataType::Utf8),
        ],
    )
    .with_context(|| format!("excluded {}", segment.display()))?;
    for batch in builder.with_batch_size(ROW_GROUP).build()? {
        let batch = batch?;
        require_non_null(&batch)?;
        let by = |n: &str| {
            batch
                .column_by_name(n)
                .unwrap_or_else(|| panic!("column {n}"))
        };
        let path = by("path").as_string::<i32>();
        let size = by("size").as_primitive::<Int64Type>();
        let rule = by("rule").as_string::<i32>();
        for i in 0..batch.num_rows() {
            f(crate::walk::Excluded {
                rel: path.value(i).to_string(),
                size: size.value(i).max(0) as u64,
                rule: rule.value(i).to_string().into(),
            });
        }
    }
    Ok(())
}

pub(crate) fn verify_excluded(segment: &Path, deep: bool) -> Result<()> {
    let file = File::open(segment)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    require_columns(
        builder.schema(),
        &[
            ("path", DataType::Utf8),
            ("size", DataType::Int64),
            ("rule", DataType::Utf8),
            ("batch", DataType::UInt32),
        ],
    )?;
    if deep {
        for batch in builder.with_batch_size(ROW_GROUP).build()? {
            require_non_null(&batch?)?;
        }
    }
    Ok(())
}

pub(crate) fn verify_parquet(segment: &Path, deep: bool) -> Result<()> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(segment)?)?;
    if deep {
        for batch in builder.with_batch_size(ROW_GROUP).build()? {
            batch?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::IndexRow;

    #[test]
    fn batch_tables_split_into_synchronized_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog_dir = tmp.path().join("catalog");
        let packs_dir = tmp.path().join("packs");
        let files: Vec<FileRow> = (0..400u32)
            .map(|i| FileRow {
                path: format!(
                    "rounds/round-{i:04}/builder/long-component-{i:08x}/result-{i:04}.json"
                ),
                kind: Kind::File,
                mode: 0o644,
                size: i as i64 + 1,
                sha: [i as u8; 32],
                mtime_ns: i as i64,
                batch: 7,
                attrs: vec![("round".into(), i.to_string())],
            })
            .collect();
        let dirs = dirs_from_files(files.iter(), 7);
        let excluded: Vec<crate::walk::Excluded> = (0..120u32)
            .map(|i| crate::walk::Excluded {
                rel: format!("cache/{i:04}/long-excluded-name-{i:08x}.bin"),
                size: i as u64,
                rule: "test-rule".into(),
            })
            .collect();
        let index: Vec<IndexRow> = files
            .iter()
            .enumerate()
            .map(|(i, row)| IndexRow {
                sha: row.sha,
                loc: Loc {
                    pack: 1,
                    chunk_offset: i as i64 * 100,
                    chunk_len: 100,
                    offset: 0,
                    size: row.size,
                    part: 0,
                },
            })
            .collect();
        let max_bytes = 8 << 10;
        let segments =
            write_batch_segments(tmp.path(), 7, &files, &dirs, &excluded, &index, max_bytes)
                .unwrap();
        assert!(segments.len() > 1, "{segments:?}");

        for segment in &segments {
            let suffix = segment.strip_prefix("files-").unwrap();
            for path in [
                catalog_dir.join(format!("{segment}.parquet")),
                catalog_dir.join(format!("dirs-{suffix}.parquet")),
                catalog_dir.join(format!("excluded-{suffix}.parquet")),
                packs_dir.join(format!("index-{suffix}.parquet")),
            ] {
                assert!(path.is_file(), "{}", path.display());
                assert!(
                    path.metadata().unwrap().len() <= max_bytes,
                    "{} is too large",
                    path.display()
                );
            }
        }

        let rows = |dir: &Path, prefix: &str| -> i64 {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(prefix)
                })
                .map(|path| row_count(&path).unwrap())
                .sum()
        };
        assert_eq!(rows(&catalog_dir, "files-"), files.len() as i64);
        assert_eq!(rows(&catalog_dir, "dirs-"), dirs.len() as i64);
        assert_eq!(rows(&catalog_dir, "excluded-"), excluded.len() as i64);
        assert_eq!(rows(&packs_dir, "index-"), index.len() as i64);
    }
}
