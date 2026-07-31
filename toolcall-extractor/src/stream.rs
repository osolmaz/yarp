use std::io::{BufRead, Write};

use crate::error::{Error, Result};
use crate::model::{
    IssueRecord, ObservationRecord, SessionRecord, SourceItemRecord, SourceRootRecord,
    StreamRecord, ToolCallRecord, ToolResultRecord,
};
use crate::sink::Sink;

pub struct JsonlSink<W> {
    writer: W,
}

impl<W: Write> JsonlSink<W> {
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    fn write(&mut self, record: StreamRecord) -> Result<()> {
        serde_json::to_writer(&mut self.writer, &record)?;
        self.writer
            .write_all(b"\n")
            .map_err(|error| Error::io("stdout", error))
    }
}

impl<W: Write> Sink for JsonlSink<W> {
    fn reset_source(&mut self, source_item_key: &str) -> Result<()> {
        self.write(StreamRecord::ResetSource(source_item_key.to_owned()))
    }

    fn source_root(&mut self, record: &SourceRootRecord) -> Result<()> {
        self.write(StreamRecord::SourceRoot(record.clone()))
    }

    fn source_item(&mut self, record: &SourceItemRecord) -> Result<()> {
        self.write(StreamRecord::SourceItem(record.clone()))
    }

    fn session(&mut self, record: &SessionRecord) -> Result<()> {
        self.write(StreamRecord::Session(record.clone()))
    }

    fn tool_call(&mut self, record: &ToolCallRecord) -> Result<()> {
        self.write(StreamRecord::ToolCall(record.clone()))
    }

    fn tool_result(&mut self, record: &ToolResultRecord) -> Result<()> {
        self.write(StreamRecord::ToolResult(record.clone()))
    }

    fn observation(&mut self, record: &ObservationRecord) -> Result<()> {
        self.write(StreamRecord::Observation(record.clone()))
    }

    fn issue(&mut self, record: &IssueRecord) -> Result<()> {
        self.write(StreamRecord::Issue(record.clone()))
    }
}

pub fn ingest(reader: impl BufRead, sink: &mut impl Sink) -> Result<u64> {
    let mut count = 0_u64;
    sink.begin_source()?;
    for line in reader.lines() {
        let line = line.map_err(|error| Error::io("stdin", error))?;
        let record: StreamRecord = serde_json::from_str(&line)?;
        match record {
            StreamRecord::SourceRoot(value) => sink.source_root(&value)?,
            StreamRecord::SourceItem(value) => sink.source_item(&value)?,
            StreamRecord::ResetSource(value) => sink.reset_source(&value)?,
            StreamRecord::Session(value) => sink.session(&value)?,
            StreamRecord::ToolCall(value) => sink.tool_call(&value)?,
            StreamRecord::ToolResult(value) => sink.tool_result(&value)?,
            StreamRecord::Observation(value) => sink.observation(&value)?,
            StreamRecord::Issue(value) => sink.issue(&value)?,
        }
        count = count.saturating_add(1);
        if count.is_multiple_of(10_000) {
            sink.commit_source()?;
            sink.begin_source()?;
        }
    }
    sink.commit_source()?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InputFormat, SourceStatus};

    #[test]
    fn writes_jsonl_records() {
        let mut bytes = Vec::new();
        let mut sink = JsonlSink::new(&mut bytes);
        sink.source_item(&SourceItemRecord {
            source_item_key: "item".into(),
            source_root_key: "root".into(),
            relative_path: "session.jsonl".into(),
            adapter_version: 1,
            device_id: None,
            inode: None,
            size_bytes: 4,
            snapshot_mtime_ns: 0,
            imported_byte_count: Some(4),
            prefix_sha256: None,
            status: SourceStatus::Complete,
        })
        .expect("write");
        let value: StreamRecord = serde_json::from_slice(&bytes).expect("parse");
        assert!(matches!(value, StreamRecord::SourceItem(_)));
        let _ = InputFormat::Json;
    }
}
