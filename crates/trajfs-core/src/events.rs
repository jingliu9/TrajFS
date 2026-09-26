//! The derived `events` envelope (docs/PLAN.md §3.5) and its Parquet writer.

use anyhow::{bail, Context, Result};
use arrow::array::{
    ArrayBuilder, ArrayRef, Int32Builder, StringBuilder, TimestampMicrosecondBuilder, UInt16Builder,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub seq: i32,
    pub ts_us: Option<i64>,
    pub r#type: String,
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub actor: Option<String>,
    pub tool_name: Option<String>,
    pub exit_code: Option<i32>,
    pub payload_json: String,
}

pub fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("trajectory", DataType::Utf8, false),
        Field::new("seq", DataType::Int32, false),
        Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        Field::new("type", DataType::Utf8, false),
        Field::new("id", DataType::Utf8, true),
        Field::new("parent_id", DataType::Utf8, true),
        Field::new("actor", DataType::Utf8, true),
        Field::new("tool_name", DataType::Utf8, true),
        Field::new("exit_code", DataType::Int32, true),
        Field::new("payload_json", DataType::Utf8, false),
        Field::new("adapter_version", DataType::UInt16, false),
    ]))
}

/// Streaming writer: call `push` per trajectory, `finish` once.
pub struct EventsWriter {
    writer: parquet::arrow::ArrowWriter<std::fs::File>,
    schema: Arc<Schema>,
    adapter_version: u16,
    rows: usize,
    trajectory: StringBuilder,
    seq: Int32Builder,
    ts: TimestampMicrosecondBuilder,
    ty: StringBuilder,
    id: StringBuilder,
    parent: StringBuilder,
    actor: StringBuilder,
    tool: StringBuilder,
    exit: Int32Builder,
    payload: StringBuilder,
    ver: UInt16Builder,
}

impl EventsWriter {
    pub fn create(path: &Path, adapter_version: u16) -> Result<Self> {
        let schema = schema();
        let writer = crate::catalog::arrow_writer(path, schema.clone())?;
        Ok(Self {
            writer,
            schema,
            adapter_version,
            rows: 0,
            trajectory: StringBuilder::new(),
            seq: Int32Builder::new(),
            ts: TimestampMicrosecondBuilder::new(),
            ty: StringBuilder::new(),
            id: StringBuilder::new(),
            parent: StringBuilder::new(),
            actor: StringBuilder::new(),
            tool: StringBuilder::new(),
            exit: Int32Builder::new(),
            payload: StringBuilder::new(),
            ver: UInt16Builder::new(),
        })
    }

    pub fn push(&mut self, trajectory: &str, events: &[Event]) -> Result<()> {
        for e in events {
            self.trajectory.append_value(trajectory);
            self.seq.append_value(e.seq);
            self.ts.append_option(e.ts_us);
            self.ty.append_value(&e.r#type);
            self.id.append_option(e.id.as_deref());
            self.parent.append_option(e.parent_id.as_deref());
            self.actor.append_option(e.actor.as_deref());
            self.tool.append_option(e.tool_name.as_deref());
            self.exit.append_option(e.exit_code);
            self.payload.append_value(&e.payload_json);
            self.ver.append_value(self.adapter_version);
            self.rows += 1;
            if self.rows.is_multiple_of(crate::ROW_GROUP) {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.trajectory.len() == 0 {
            return Ok(());
        }
        let cols: Vec<ArrayRef> = vec![
            Arc::new(self.trajectory.finish()),
            Arc::new(self.seq.finish()),
            Arc::new(self.ts.finish()),
            Arc::new(self.ty.finish()),
            Arc::new(self.id.finish()),
            Arc::new(self.parent.finish()),
            Arc::new(self.actor.finish()),
            Arc::new(self.tool.finish()),
            Arc::new(self.exit.finish()),
            Arc::new(self.payload.finish()),
            Arc::new(self.ver.finish()),
        ];
        let batch = RecordBatch::try_new(self.schema.clone(), cols)?;
        self.writer
            .write(&batch)
            .context("write events row group")?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<usize> {
        self.flush()?;
        self.writer.close()?;
        Ok(self.rows)
    }
}

struct EventRow {
    trajectory: String,
    event: Event,
}

fn event_row_weight(row: &EventRow) -> usize {
    let event = &row.event;
    row.trajectory
        .len()
        .saturating_add(event.r#type.len())
        .saturating_add(event.id.as_ref().map_or(0, String::len))
        .saturating_add(event.parent_id.as_ref().map_or(0, String::len))
        .saturating_add(event.actor.as_ref().map_or(0, String::len))
        .saturating_add(event.tool_name.as_ref().map_or(0, String::len))
        .saturating_add(event.payload_json.len())
        .saturating_add(96)
}

fn write_event_rows(path: &Path, rows: &[EventRow], adapter_version: u16) -> Result<()> {
    let mut writer = EventsWriter::create(path, adapter_version)?;
    for row in rows {
        writer.push(&row.trajectory, std::slice::from_ref(&row.event))?;
    }
    writer.finish()?;
    Ok(())
}

pub struct SegmentedEventsWriter {
    dir: PathBuf,
    stem: String,
    adapter_version: u16,
    max_bytes: u64,
    buffer: Vec<EventRow>,
    buffer_weight: u64,
    paths: Vec<PathBuf>,
    rows: usize,
}

impl SegmentedEventsWriter {
    pub fn create(
        dir: &Path,
        stem: impl Into<String>,
        adapter_version: u16,
        max_bytes: u64,
    ) -> Result<Self> {
        let stem = stem.into();
        crate::catalog::validate_segment_stem(&stem)?;
        if max_bytes == 0 {
            bail!("Parquet segment size limit must be greater than zero");
        }
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            stem,
            adapter_version,
            max_bytes,
            buffer: Vec::new(),
            buffer_weight: 0,
            paths: Vec::new(),
            rows: 0,
        })
    }

    pub fn push(&mut self, trajectory: &str, events: Vec<Event>) -> Result<()> {
        let budget = (self.max_bytes.saturating_mul(3) / 4).max(1);
        for event in events {
            let row = EventRow {
                trajectory: trajectory.to_string(),
                event,
            };
            let weight = event_row_weight(&row).max(1) as u64;
            if !self.buffer.is_empty() && self.buffer_weight.saturating_add(weight) > budget {
                self.flush()?;
            }
            self.buffer_weight = self.buffer_weight.saturating_add(weight);
            self.buffer.push(row);
            self.rows += 1;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.buffer);
        self.buffer_weight = 0;
        let mut paths = crate::catalog::write_sized_segments(
            &self.dir,
            &self.stem,
            &rows,
            self.max_bytes,
            self.paths.len() + 1,
            |path, rows| write_event_rows(path, rows, self.adapter_version),
        )?;
        self.paths.append(&mut paths);
        Ok(())
    }

    pub fn finish(mut self) -> Result<(usize, Vec<PathBuf>)> {
        self.flush()?;
        if self.paths.is_empty() {
            self.paths = crate::catalog::write_sized_segments(
                &self.dir,
                &self.stem,
                &[],
                self.max_bytes,
                1,
                |path, rows| write_event_rows(path, rows, self.adapter_version),
            )?;
        }
        crate::catalog::rename_single_segment(&mut self.paths, &self.dir, &self.stem)?;
        Ok((self.rows, self.paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_tables_split_at_a_configurable_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let max_bytes = 32 << 10;
        let mut writer =
            SegmentedEventsWriter::create(tmp.path(), "events-0003", 9, max_bytes).unwrap();
        for i in 0..400 {
            writer
                .push(
                    &format!("rounds/{i:04}/events.jsonl"),
                    vec![Event {
                        seq: i,
                        r#type: format!("event-{i}"),
                        payload_json: format!(
                            "{{\"id\":{i},\"payload\":\"{}\"}}",
                            format!("{i:08x}").repeat(64)
                        ),
                        ..Default::default()
                    }],
                )
                .unwrap();
        }
        let (rows, paths) = writer.finish().unwrap();
        assert_eq!(rows, 400);
        assert!(paths.len() > 1, "{paths:?}");
        assert_eq!(
            paths
                .iter()
                .map(|path| crate::catalog::row_count(path).unwrap())
                .sum::<i64>(),
            400
        );
        for path in paths {
            assert!(path.metadata().unwrap().len() <= max_bytes);
        }
    }
}
