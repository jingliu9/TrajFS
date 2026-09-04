//! The derived `events` envelope (PLAN.md §3.5) and its Parquet writer.

use anyhow::{Context, Result};
use arrow::array::{
    ArrayBuilder, ArrayRef, Int32Builder, StringBuilder, TimestampMicrosecondBuilder, UInt16Builder,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use std::path::Path;
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
            if self.rows % crate::ROW_GROUP == 0 {
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
        self.writer.write(&batch).context("write events row group")?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<usize> {
        self.flush()?;
        self.writer.close()?;
        Ok(self.rows)
    }
}
