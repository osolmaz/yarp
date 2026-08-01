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

    pub fn start_stream(&mut self) -> Result<()> {
        self.write(StreamRecord::StreamStart)
    }

    pub fn finish_stream(&mut self) -> Result<()> {
        self.write(StreamRecord::StreamEnd)
    }
}

impl<W: Write> Sink for JsonlSink<W> {
    fn begin_source(&mut self) -> Result<()> {
        self.write(StreamRecord::SourceStart)
    }

    fn commit_source(&mut self) -> Result<()> {
        self.write(StreamRecord::SourceCommit)
    }

    fn rollback_source(&mut self) -> Result<()> {
        self.write(StreamRecord::SourceRollback)
    }

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
    let mut stream_started = false;
    let mut stream_finished = false;
    let mut source_active = false;

    for line in reader.lines() {
        let line = line.map_err(|error| Error::io("stdin", error))?;
        let record: StreamRecord = serde_json::from_str(&line)?;
        match record {
            StreamRecord::StreamStart => {
                if stream_started || stream_finished || source_active {
                    return Err(protocol_error("unexpected stream start"));
                }
                stream_started = true;
            }
            StreamRecord::StreamEnd => {
                if !stream_started || stream_finished {
                    return Err(protocol_error("unexpected stream end"));
                }
                if source_active {
                    sink.rollback_source()?;
                    return Err(protocol_error("stream ended before its source committed"));
                }
                stream_finished = true;
            }
            StreamRecord::SourceStart => {
                if !stream_started || stream_finished || source_active {
                    return Err(protocol_error("unexpected source start"));
                }
                sink.begin_source()?;
                source_active = true;
            }
            StreamRecord::SourceCommit => {
                if !source_active {
                    return Err(protocol_error("source commit has no active source"));
                }
                sink.commit_source()?;
                source_active = false;
            }
            StreamRecord::SourceRollback => {
                if !source_active {
                    return Err(protocol_error("source rollback has no active source"));
                }
                sink.rollback_source()?;
                source_active = false;
            }
            StreamRecord::SourceRoot(value) => {
                if !stream_started || stream_finished || source_active {
                    return Err(protocol_error("source root is outside the stream header"));
                }
                sink.source_root(&value)?;
            }
            StreamRecord::SourceItem(value) => {
                require_source(source_active)?;
                sink.source_item(&value)?;
            }
            StreamRecord::ResetSource(value) => {
                require_source(source_active)?;
                sink.reset_source(&value)?;
            }
            StreamRecord::Session(value) => {
                require_source(source_active)?;
                sink.session(&value)?;
            }
            StreamRecord::ToolCall(value) => {
                require_source(source_active)?;
                sink.tool_call(&value)?;
            }
            StreamRecord::ToolResult(value) => {
                require_source(source_active)?;
                sink.tool_result(&value)?;
            }
            StreamRecord::Observation(value) => {
                require_source(source_active)?;
                sink.observation(&value)?;
            }
            StreamRecord::Issue(value) => {
                require_source(source_active)?;
                sink.issue(&value)?;
            }
        }
        count = count.saturating_add(1);
    }
    if source_active {
        sink.rollback_source()?;
        return Err(protocol_error("stream ended with an active source"));
    }
    if !stream_started || !stream_finished {
        return Err(protocol_error("stream ended without a completion record"));
    }
    Ok(count)
}

fn require_source(source_active: bool) -> Result<()> {
    if source_active {
        Ok(())
    } else {
        Err(protocol_error("source record has no active source"))
    }
}

fn protocol_error(message: &str) -> Error {
    Error::InvalidSource(format!("invalid framed stream: {message}"))
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
