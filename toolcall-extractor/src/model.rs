use serde::{Deserialize, Serialize};

pub const ADAPTER_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceRootRecord {
    pub source_root_key: String,
    pub unix_user: String,
    pub agent: String,
    pub source_kind: String,
    pub root_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceItemRecord {
    pub source_item_key: String,
    pub source_root_key: String,
    pub relative_path: String,
    pub adapter_version: u32,
    pub device_id: Option<u64>,
    pub inode: Option<u64>,
    pub size_bytes: u64,
    pub snapshot_mtime_ns: i64,
    pub imported_byte_count: Option<u64>,
    pub prefix_sha256: Option<Vec<u8>>,
    pub status: SourceStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Complete,
    Deferred,
    Rejected,
}

impl SourceStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Deferred => "deferred",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionRecord {
    pub session_key: String,
    pub unix_user: String,
    pub agent: String,
    pub native_session_id: String,
    pub started_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCallRecord {
    pub call_key: String,
    pub session_key: String,
    pub native_call_id: Option<String>,
    pub native_worker_id: Option<String>,
    pub called_at_ms: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub working_directory: Option<String>,
    pub tool_name: String,
    pub input_format: InputFormat,
    pub input_text: String,
    pub input_sha256: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputFormat {
    Json,
    Text,
}

impl InputFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Text => "text",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResultRecord {
    pub result_key: String,
    pub call_key: String,
    pub returned_at_ms: Option<i64>,
    pub is_error: Option<bool>,
    pub output_text: Option<String>,
    pub output_json: Option<String>,
    pub result_sha256: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ObservationRecord {
    pub observation_key: String,
    pub source_item_key: String,
    pub call_key: Option<String>,
    pub result_key: Option<String>,
    pub record_kind: RecordKind,
    pub native_record_kind: String,
    pub sequence_number: Option<i64>,
    pub native_branch_id: Option<String>,
    pub is_current: Option<bool>,
    pub line_number: Option<u64>,
    pub byte_offset: Option<u64>,
    pub sqlite_rowid: Option<i64>,
    pub sqlite_blob_id: Option<String>,
    pub content_index: Option<u32>,
    pub record_sha256: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Canonical,
    Projection,
    Validation,
}

impl RecordKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Projection => "projection",
            Self::Validation => "validation",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IssueRecord {
    pub issue_key: String,
    pub source_item_key: Option<String>,
    pub severity: Severity,
    pub code: String,
    pub line_number: Option<u64>,
    pub byte_offset: Option<u64>,
    pub sqlite_blob_id: Option<String>,
    pub record_sha256: Option<Vec<u8>>,
    pub message: String,
    pub occurrence_count: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warning,
    Error,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "record", content = "value", rename_all = "snake_case")]
pub enum StreamRecord {
    SourceRoot(SourceRootRecord),
    SourceItem(SourceItemRecord),
    Session(SessionRecord),
    ToolCall(ToolCallRecord),
    ToolResult(ToolResultRecord),
    Observation(ObservationRecord),
    Issue(IssueRecord),
}
