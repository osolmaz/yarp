use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;

use prost::Message;
use rusqlite::OpenFlags;
use serde::Deserialize;
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::common::{JsonLine, process_jsonl};
use super::{register_root, source_item};
use crate::error::{Error, Result};
use crate::keys;
use crate::model::{
    InputFormat, IssueRecord, ObservationRecord, RecordKind, SessionRecord, Severity, SourceStatus,
    ToolCallRecord, ToolResultRecord,
};
use crate::private_fs;
use crate::sink::Sink;

#[derive(Clone, PartialEq, Message)]
struct ConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    message_blob_ids: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "8")]
    turn_blob_ids: Vec<Vec<u8>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorMetadata {
    agent_id: Option<String>,
    latest_root_blob_id: Option<String>,
    created_at: Option<Value>,
    subagent_info: Option<CursorSubagentInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorSubagentInfo {
    parent_agent_id: Option<String>,
}

#[derive(Clone)]
struct CallSignature {
    call_key: String,
    tool_name: String,
    input_text: String,
}

pub fn extract(
    unix_user: &str,
    chats: &Path,
    acp_sessions: &Path,
    projects: &Path,
    sink: &mut impl Sink,
) -> Result<u64> {
    let chat_root = register_root(unix_user, "cursor", "chats", chats, sink)?;
    let acp_root = register_root(unix_user, "cursor", "acp_sessions", acp_sessions, sink)?;
    let project_root = register_root(unix_user, "cursor", "projects", projects, sink)?;
    let mut chat_signatures = HashMap::<String, Vec<CallSignature>>::new();
    let mut processed = 0_u64;
    processed = processed.saturating_add(extract_databases(
        unix_user,
        chats,
        &chat_root,
        true,
        sink,
        &mut chat_signatures,
    )?);
    processed = processed.saturating_add(extract_databases(
        unix_user,
        acp_sessions,
        &acp_root,
        false,
        sink,
        &mut HashMap::new(),
    )?);
    processed = processed.saturating_add(validate_transcripts(
        projects,
        &project_root,
        &chat_signatures,
        sink,
    )?);
    Ok(processed)
}

fn extract_databases(
    unix_user: &str,
    root_path: &Path,
    root: &crate::model::SourceRootRecord,
    keep_signatures: bool,
    sink: &mut impl Sink,
    signatures: &mut HashMap<String, Vec<CallSignature>>,
) -> Result<u64> {
    let mut processed = 0_u64;
    for entry in WalkDir::new(root_path).follow_links(false) {
        let entry = entry.map_err(|error| Error::InvalidSource(error.to_string()))?;
        if !entry.file_type().is_file() || entry.file_name() != "store.db" {
            continue;
        }
        let relative = private_fs::relative_path(root_path, entry.path())?;
        let mut item = source_item(root, relative);
        let identity = private_fs::identity(entry.path())?;
        if let Some(checkpoint) = sink.checkpoint(&item.source_item_key)?
            && checkpoint.adapter_version == crate::model::ADAPTER_VERSION
            && checkpoint.device_id == Some(identity.device_id)
            && checkpoint.inode == Some(identity.inode)
            && checkpoint.size_bytes == identity.size_bytes
            && checkpoint.snapshot_mtime_ns == identity.mtime_ns
            && checkpoint.status == "complete"
        {
            continue;
        }
        sink.begin_source()?;
        item.device_id = Some(identity.device_id);
        item.inode = Some(identity.inode);
        item.size_bytes = identity.size_bytes;
        item.snapshot_mtime_ns = identity.mtime_ns;
        item.status = SourceStatus::Deferred;
        sink.source_item(&item)?;
        let result = extract_database(
            unix_user,
            entry.path(),
            &item.source_item_key,
            sink,
            keep_signatures,
        );
        match result {
            Ok((session_id, found_signatures)) => {
                let after = private_fs::identity(entry.path())?;
                if after.device_id != identity.device_id
                    || after.inode != identity.inode
                    || after.size_bytes < identity.size_bytes
                {
                    sink.rollback_source()?;
                    continue;
                }
                item.size_bytes = after.size_bytes;
                item.snapshot_mtime_ns = after.mtime_ns;
                item.status = SourceStatus::Complete;
                sink.source_item(&item)?;
                sink.commit_source()?;
                if keep_signatures {
                    signatures.insert(session_id, found_signatures);
                }
                processed = processed.saturating_add(1);
            }
            Err(error) => {
                sink.rollback_source()?;
                return Err(error);
            }
        }
    }
    Ok(processed)
}

fn extract_database(
    unix_user: &str,
    path: &Path,
    source_item_key: &str,
    sink: &mut dyn Sink,
    keep_signatures: bool,
) -> Result<(String, Vec<CallSignature>)> {
    let mut connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", true)?;
    let transaction = connection.transaction()?;
    let metadata_text: String =
        transaction.query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })?;
    let metadata_bytes = keys::decode_hex(&metadata_text)
        .ok_or_else(|| Error::InvalidSource("Cursor metadata is not valid hex".to_owned()))?;
    let metadata: CursorMetadata = serde_json::from_slice(&metadata_bytes)?;
    let session_id = metadata.agent_id.clone().unwrap_or_else(|| {
        path.parent().and_then(Path::file_name).map_or_else(
            || "unknown".to_owned(),
            |value| value.to_string_lossy().into_owned(),
        )
    });
    let session = SessionRecord {
        session_key: keys::key(&[
            b"session",
            unix_user.as_bytes(),
            b"cursor",
            session_id.as_bytes(),
        ]),
        unix_user: unix_user.to_owned(),
        agent: "cursor".to_owned(),
        native_session_id: session_id.clone(),
        started_at_ms: metadata.created_at.as_ref().and_then(timestamp_ms),
    };
    sink.session(&session)?;
    let current_order = current_blob_order(
        &transaction,
        metadata.latest_root_blob_id.as_deref(),
        source_item_key,
        sink,
    )?;
    let mut calls = HashMap::<String, String>::new();
    let mut signatures = Vec::new();
    let mut statement = transaction.prepare("SELECT rowid, id, data FROM blobs ORDER BY rowid")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (rowid, blob_id, bytes) = row?;
        let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(object) = strict_message(&message) else {
            continue;
        };
        let Some(content) = object.get("content").and_then(Value::as_array) else {
            continue;
        };
        let order = current_order.get(&blob_id).copied();
        let is_current = metadata
            .latest_root_blob_id
            .as_ref()
            .map(|_| order.is_some());
        for (index, block) in content.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("tool-call") => {
                    if let Some(signature) = cursor_call(
                        unix_user,
                        source_item_key,
                        rowid,
                        &blob_id,
                        &bytes,
                        block,
                        index,
                        order,
                        is_current,
                        &session,
                        metadata.subagent_info.as_ref(),
                        sink,
                    )? {
                        calls.insert(
                            block
                                .get("toolCallId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            signature.call_key.clone(),
                        );
                        if keep_signatures {
                            signatures.push(signature);
                        }
                    }
                }
                Some("tool-result") => cursor_result(
                    unix_user,
                    source_item_key,
                    rowid,
                    &blob_id,
                    &bytes,
                    object,
                    block,
                    index,
                    order,
                    is_current,
                    &session,
                    &calls,
                    sink,
                )?,
                _ => {}
            }
        }
    }
    drop(statement);
    transaction.commit()?;
    Ok((session_id, signatures))
}

fn cursor_call(
    unix_user: &str,
    source_item_key: &str,
    rowid: i64,
    blob_id: &str,
    bytes: &[u8],
    block: &Value,
    index: usize,
    current_order: Option<i64>,
    is_current: Option<bool>,
    session: &SessionRecord,
    subagent: Option<&CursorSubagentInfo>,
    sink: &mut dyn Sink,
) -> Result<Option<CallSignature>> {
    let Some(native_call_id) = block.get("toolCallId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(tool_name) = block.get("toolName").and_then(Value::as_str) else {
        return Ok(None);
    };
    let (input_format, input_text) = cursor_input(block.get("args"))?;
    let input_sha256 = keys::sha256(input_text.as_bytes());
    let call_key = keys::key(&[
        b"call",
        unix_user.as_bytes(),
        b"cursor",
        session.native_session_id.as_bytes(),
        native_call_id.as_bytes(),
    ]);
    sink.tool_call(&ToolCallRecord {
        call_key: call_key.clone(),
        session_key: session.session_key.clone(),
        native_call_id: Some(native_call_id.to_owned()),
        native_worker_id: subagent.and_then(|value| value.parent_agent_id.clone()),
        called_at_ms: None,
        provider: None,
        model: None,
        working_directory: None,
        tool_name: tool_name.to_owned(),
        input_format,
        input_text: input_text.clone(),
        input_sha256,
    })?;
    sink.observation(&cursor_observation(
        source_item_key,
        rowid,
        blob_id,
        bytes,
        index,
        current_order,
        is_current,
        Some(&call_key),
        None,
        "tool-call",
        RecordKind::Canonical,
    ))?;
    Ok(Some(CallSignature {
        call_key,
        tool_name: tool_name.to_owned(),
        input_text,
    }))
}

fn cursor_result(
    unix_user: &str,
    source_item_key: &str,
    rowid: i64,
    blob_id: &str,
    bytes: &[u8],
    message: &Map<String, Value>,
    block: &Value,
    index: usize,
    current_order: Option<i64>,
    is_current: Option<bool>,
    session: &SessionRecord,
    calls: &HashMap<String, String>,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(native_call_id) = block.get("toolCallId").and_then(Value::as_str) else {
        return Ok(());
    };
    let call_key = calls.get(native_call_id).cloned().unwrap_or_else(|| {
        keys::key(&[
            b"call",
            unix_user.as_bytes(),
            b"cursor",
            session.native_session_id.as_bytes(),
            native_call_id.as_bytes(),
        ])
    });
    let output_text = block
        .get("result")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_default();
    let high_level = message
        .get("providerOptions")
        .and_then(Value::as_object)
        .and_then(|value| value.get("cursor"))
        .and_then(Value::as_object)
        .and_then(|value| value.get("highLevelToolCallResult"));
    let is_error = high_level
        .and_then(|value| value.get("isError"))
        .and_then(Value::as_bool);
    let output_json = cursor_structured_result(high_level, block.get("experimental_content"))
        .map(|value| keys::canonical_json(&value))
        .transpose()?;
    let fingerprint = keys::canonical_json(&serde_json::json!({
        "text": output_text,
        "json": output_json,
        "is_error": is_error,
    }))?;
    let result_sha256 = keys::sha256(fingerprint.as_bytes());
    let result_key = keys::key(&[b"result", call_key.as_bytes(), &result_sha256]);
    sink.tool_result(&ToolResultRecord {
        result_key: result_key.clone(),
        call_key,
        returned_at_ms: None,
        is_error,
        output_text: Some(output_text),
        output_json,
        result_sha256,
    })?;
    sink.observation(&cursor_observation(
        source_item_key,
        rowid,
        blob_id,
        bytes,
        index,
        current_order,
        is_current,
        None,
        Some(&result_key),
        "tool-result",
        RecordKind::Canonical,
    ))
}

fn current_blob_order(
    connection: &rusqlite::Transaction<'_>,
    root_id: Option<&str>,
    source_item_key: &str,
    sink: &mut dyn Sink,
) -> Result<HashMap<String, i64>> {
    let Some(root_id) = root_id else {
        return Ok(HashMap::new());
    };
    let root_id = root_id.to_ascii_lowercase();
    let root: Option<Vec<u8>> = connection
        .query_row(
            "SELECT data FROM blobs WHERE id = ?",
            [root_id.as_str()],
            |row| row.get(0),
        )
        .ok();
    let Some(root) = root else {
        sink.issue(&IssueRecord {
            issue_key: keys::key(&[b"cursor_missing_root", source_item_key.as_bytes()]),
            source_item_key: Some(source_item_key.to_owned()),
            severity: Severity::Warning,
            code: "cursor_missing_root".to_owned(),
            line_number: None,
            byte_offset: None,
            sqlite_blob_id: Some(root_id),
            record_sha256: None,
            message: "latest Cursor root blob is missing".to_owned(),
            occurrence_count: 1,
        })?;
        return Ok(HashMap::new());
    };
    let structure = ConversationStateStructure::decode(root.as_slice()).map_err(|error| {
        Error::InvalidSource(format!("unsupported Cursor conversation root: {error}"))
    })?;
    Ok(structure
        .message_blob_ids
        .into_iter()
        .filter(|value| value.len() == 32)
        .enumerate()
        .map(|(index, value)| (keys::hex(&value), i64::try_from(index).unwrap_or(i64::MAX)))
        .collect())
}

fn strict_message(value: &Value) -> Option<&Map<String, Value>> {
    let object = value.as_object()?;
    object.get("role")?.as_str()?;
    object.get("content")?.as_array()?;
    Some(object)
}

fn cursor_input(value: Option<&Value>) -> Result<(InputFormat, String)> {
    match value {
        Some(Value::String(text)) => {
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                Ok((InputFormat::Json, keys::canonical_json(&value)?))
            } else {
                Ok((InputFormat::Text, text.clone()))
            }
        }
        Some(value) => Ok((InputFormat::Json, keys::canonical_json(value)?)),
        None => Ok((InputFormat::Json, "{}".to_owned())),
    }
}

fn cursor_structured_result(
    high_level: Option<&Value>,
    experimental: Option<&Value>,
) -> Option<Value> {
    let mut output = Map::new();
    if let Some(Value::Object(high_level)) = high_level {
        let mut clean = high_level.clone();
        clean.remove("isError");
        clean.remove("rawErrorMessages");
        if !clean.is_empty() {
            output.insert("highLevelToolCallResult".to_owned(), Value::Object(clean));
        }
    }
    if let Some(Value::Array(blocks)) = experimental {
        let non_text: Vec<Value> = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) != Some("text"))
            .cloned()
            .collect();
        if !non_text.is_empty() {
            output.insert("content".to_owned(), Value::Array(non_text));
        }
    }
    (!output.is_empty()).then_some(Value::Object(output))
}

fn cursor_observation(
    source_item_key: &str,
    rowid: i64,
    blob_id: &str,
    bytes: &[u8],
    index: usize,
    current_order: Option<i64>,
    is_current: Option<bool>,
    call_key: Option<&str>,
    result_key: Option<&str>,
    native_kind: &str,
    record_kind: RecordKind,
) -> ObservationRecord {
    let target = call_key.or(result_key).unwrap_or("");
    ObservationRecord {
        observation_key: keys::key(&[
            b"observation",
            source_item_key.as_bytes(),
            blob_id.as_bytes(),
            &u64::try_from(index).unwrap_or(u64::MAX).to_be_bytes(),
            target.as_bytes(),
        ]),
        source_item_key: source_item_key.to_owned(),
        call_key: call_key.map(ToOwned::to_owned),
        result_key: result_key.map(ToOwned::to_owned),
        record_kind,
        native_record_kind: native_kind.to_owned(),
        sequence_number: current_order.map(|order| {
            order
                .saturating_mul(1000)
                .saturating_add(i64::try_from(index).unwrap_or(i64::MAX))
        }),
        native_branch_id: None,
        is_current,
        line_number: None,
        byte_offset: None,
        sqlite_rowid: Some(rowid),
        sqlite_blob_id: Some(blob_id.to_owned()),
        content_index: u32::try_from(index).ok(),
        record_sha256: keys::sha256(bytes),
    }
}

fn validate_transcripts(
    projects: &Path,
    root: &crate::model::SourceRootRecord,
    signatures: &HashMap<String, Vec<CallSignature>>,
    sink: &mut impl Sink,
) -> Result<u64> {
    let mut processed = 0_u64;
    for entry in WalkDir::new(projects).follow_links(false) {
        let entry = entry.map_err(|error| Error::InvalidSource(error.to_string()))?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
            || !entry
                .path()
                .components()
                .any(|part| part.as_os_str() == "agent-transcripts")
        {
            continue;
        }
        let Some(session_id) = entry.path().file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(expected) = signatures.get(session_id) else {
            continue;
        };
        let relative = private_fs::relative_path(projects, entry.path())?;
        let item = source_item(root, relative);
        let ordinal = Cell::new(0_usize);
        let did_process = process_jsonl(
            entry.path(),
            item.clone(),
            sink,
            |line, value, target| {
                let Some(message) = value.get("message").and_then(Value::as_object) else {
                    return Ok(());
                };
                let Some(content) = message.get("content").and_then(Value::as_array) else {
                    return Ok(());
                };
                for (index, block) in content.iter().enumerate() {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let Some(signature) = expected.get(ordinal.get()) else {
                        target.issue(&transcript_issue(
                            &item.source_item_key,
                            line,
                            "Cursor transcript has more calls than SQLite",
                        ))?;
                        return Ok(());
                    };
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = keys::canonical_json(block.get("input").unwrap_or(&Value::Null))?;
                    if name != signature.tool_name || input != signature.input_text {
                        target.issue(&transcript_issue(
                            &item.source_item_key,
                            line,
                            "Cursor transcript call does not match SQLite",
                        ))?;
                    } else {
                        target.observation(&ObservationRecord {
                            observation_key: keys::key(&[
                                b"observation",
                                item.source_item_key.as_bytes(),
                                &line.byte_offset.to_be_bytes(),
                                &u64::try_from(index).unwrap_or(u64::MAX).to_be_bytes(),
                                signature.call_key.as_bytes(),
                            ]),
                            source_item_key: item.source_item_key.clone(),
                            call_key: Some(signature.call_key.clone()),
                            result_key: None,
                            record_kind: RecordKind::Validation,
                            native_record_kind: "transcript.tool_use".to_owned(),
                            sequence_number: i64::try_from(ordinal.get()).ok(),
                            native_branch_id: None,
                            is_current: None,
                            line_number: Some(line.number),
                            byte_offset: Some(line.byte_offset),
                            sqlite_rowid: None,
                            sqlite_blob_id: None,
                            content_index: u32::try_from(index).ok(),
                            record_sha256: line.record_sha256.clone(),
                        })?;
                    }
                    ordinal.set(ordinal.get().saturating_add(1));
                }
                Ok(())
            },
            |target| {
                if ordinal.get() != expected.len() {
                    target.issue(&IssueRecord {
                        issue_key: keys::key(&[
                            b"cursor_transcript_count",
                            item.source_item_key.as_bytes(),
                        ]),
                        source_item_key: Some(item.source_item_key.clone()),
                        severity: Severity::Warning,
                        code: "cursor_transcript_count_mismatch".to_owned(),
                        line_number: None,
                        byte_offset: None,
                        sqlite_blob_id: None,
                        record_sha256: None,
                        message: "Cursor transcript and SQLite call counts differ".to_owned(),
                        occurrence_count: 1,
                    })?;
                }
                Ok(())
            },
        )?;
        if did_process {
            processed = processed.saturating_add(1);
        }
    }
    Ok(processed)
}

fn transcript_issue(source_item_key: &str, line: &JsonLine, message: &str) -> IssueRecord {
    IssueRecord {
        issue_key: keys::key(&[
            b"cursor_transcript_mismatch",
            source_item_key.as_bytes(),
            &line.byte_offset.to_be_bytes(),
        ]),
        source_item_key: Some(source_item_key.to_owned()),
        severity: Severity::Warning,
        code: "cursor_transcript_mismatch".to_owned(),
        line_number: Some(line.number),
        byte_offset: Some(line.byte_offset),
        sqlite_blob_id: None,
        record_sha256: Some(line.record_sha256.clone()),
        message: message.to_owned(),
        occurrence_count: 1,
    }
}

fn timestamp_ms(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => keys::parse_timestamp_ms(Some(text)),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}
