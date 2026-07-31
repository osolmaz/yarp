use crate::error::Result;
use crate::model::{
    IssueRecord, ObservationRecord, SessionRecord, SourceItemRecord, SourceRootRecord,
    ToolCallRecord, ToolResultRecord,
};

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub adapter_version: u32,
    pub device_id: Option<u64>,
    pub inode: Option<u64>,
    pub size_bytes: u64,
    pub snapshot_mtime_ns: i64,
    pub imported_byte_count: Option<u64>,
    pub prefix_sha256: Option<Vec<u8>>,
    pub status: String,
}

pub trait Sink {
    fn checkpoint(&self, _source_item_key: &str) -> Result<Option<Checkpoint>> {
        Ok(None)
    }

    fn resolve_call(&self, _session_key: &str, _native_call_id: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn begin_source(&mut self) -> Result<()> {
        Ok(())
    }

    fn commit_source(&mut self) -> Result<()> {
        Ok(())
    }

    fn rollback_source(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset_source(&mut self, _source_item_key: &str) -> Result<()> {
        Ok(())
    }

    fn source_root(&mut self, record: &SourceRootRecord) -> Result<()>;
    fn source_item(&mut self, record: &SourceItemRecord) -> Result<()>;
    fn session(&mut self, record: &SessionRecord) -> Result<()>;
    fn tool_call(&mut self, record: &ToolCallRecord) -> Result<()>;
    fn tool_result(&mut self, record: &ToolResultRecord) -> Result<()>;
    fn observation(&mut self, record: &ObservationRecord) -> Result<()>;
    fn issue(&mut self, record: &IssueRecord) -> Result<()>;
}
