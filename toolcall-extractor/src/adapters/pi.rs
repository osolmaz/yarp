use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::common::{JsonLine, first_json_value, process_jsonl};
use super::{register_root, source_item};
use crate::error::{Error, Result};
use crate::keys;
use crate::model::{
    InputFormat, IssueRecord, ObservationRecord, RecordKind, SessionRecord, Severity,
    ToolCallRecord, ToolResultRecord,
};
use crate::private_fs;
use crate::sink::Sink;

pub fn extract(unix_user: &str, sessions: &Path, sink: &mut impl Sink) -> Result<u64> {
    let root = register_root(unix_user, "pi", "sessions", sessions, sink)?;
    let mut processed = 0_u64;
    for entry in WalkDir::new(sessions).follow_links(false) {
        let entry = entry.map_err(|error| Error::InvalidSource(error.to_string()))?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            continue;
        }
        let relative = private_fs::relative_path(sessions, entry.path())?;
        let source = source_item(&root, relative);
        let Some(first) = first_json_value(entry.path())? else {
            continue;
        };
        let context = PiContext::from_header(unix_user, &first)?;
        let mut calls = HashMap::new();
        let did_process = process_jsonl(
            entry.path(),
            source.clone(),
            sink,
            |line, value, target| {
                process_record(
                    &context,
                    &source.source_item_key,
                    line,
                    value,
                    &mut calls,
                    target,
                )
            },
            |_| Ok(()),
        )?;
        if did_process {
            processed = processed.saturating_add(1);
        }
    }
    Ok(processed)
}

struct PiContext {
    unix_user: String,
    session: SessionRecord,
    working_directory: Option<String>,
}

impl PiContext {
    fn from_header(unix_user: &str, value: &Value) -> Result<Self> {
        if value.get("type").and_then(Value::as_str) != Some("session") {
            return Err(Error::InvalidSource(
                "Pi session does not start with a session record".to_owned(),
            ));
        }
        if value.get("version").and_then(Value::as_u64) != Some(3) {
            return Err(Error::InvalidSource(
                "unsupported Pi session version".to_owned(),
            ));
        }
        let native_session_id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidSource("Pi session id is missing".to_owned()))?;
        let session_key = keys::key(&[
            b"session",
            unix_user.as_bytes(),
            b"pi",
            native_session_id.as_bytes(),
        ]);
        Ok(Self {
            unix_user: unix_user.to_owned(),
            session: SessionRecord {
                session_key,
                unix_user: unix_user.to_owned(),
                agent: "pi".to_owned(),
                native_session_id: native_session_id.to_owned(),
                started_at_ms: timestamp_ms(value.get("timestamp")),
            },
            working_directory: value
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }
}

fn process_record(
    context: &PiContext,
    source_item_key: &str,
    line: &JsonLine,
    value: &Value,
    calls: &mut HashMap<String, String>,
    sink: &mut dyn Sink,
) -> Result<()> {
    if value.get("type").and_then(Value::as_str) == Some("session") {
        sink.session(&context.session)?;
        return Ok(());
    }
    if value.get("type").and_then(Value::as_str) != Some("message") {
        return Ok(());
    }
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        return Ok(());
    };
    match message.get("role").and_then(Value::as_str) {
        Some("assistant") => {
            process_assistant(context, source_item_key, line, value, message, calls, sink)
        }
        Some("toolResult") => {
            process_result(context, source_item_key, line, value, message, calls, sink)
        }
        _ => Ok(()),
    }
}

fn process_assistant(
    context: &PiContext,
    source_item_key: &str,
    line: &JsonLine,
    entry: &Value,
    message: &Map<String, Value>,
    calls: &mut HashMap<String, String>,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Ok(());
    };
    sink.session(&context.session)?;
    let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    for (index, block) in content.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
            continue;
        }
        let Some(native_call_id) = block.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(tool_name) = block.get("name").and_then(Value::as_str) else {
            continue;
        };
        let input = block
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let input_text = keys::canonical_json(&input)?;
        let input_sha256 = keys::sha256(input_text.as_bytes());
        let call_key = keys::key(&[
            b"call",
            context.unix_user.as_bytes(),
            b"pi",
            context.session.native_session_id.as_bytes(),
            native_call_id.as_bytes(),
            entry_id.as_bytes(),
            &u64::try_from(index).unwrap_or(u64::MAX).to_be_bytes(),
            &input_sha256,
        ]);
        sink.tool_call(&ToolCallRecord {
            call_key: call_key.clone(),
            session_key: context.session.session_key.clone(),
            native_call_id: Some(native_call_id.to_owned()),
            native_worker_id: None,
            called_at_ms: timestamp_ms(entry.get("timestamp"))
                .or_else(|| timestamp_ms(message.get("timestamp"))),
            provider: message
                .get("provider")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            model: message
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            working_directory: context.working_directory.clone(),
            tool_name: tool_name.to_owned(),
            input_format: InputFormat::Json,
            input_text,
            input_sha256,
        })?;
        sink.observation(&ObservationRecord {
            observation_key: observation_key(source_item_key, line, index, &call_key),
            source_item_key: source_item_key.to_owned(),
            call_key: Some(call_key.clone()),
            result_key: None,
            record_kind: RecordKind::Canonical,
            native_record_kind: "assistant.toolCall".to_owned(),
            sequence_number: i64::try_from(line.number).ok(),
            native_branch_id: entry
                .get("parentId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            is_current: None,
            line_number: Some(line.number),
            byte_offset: Some(line.byte_offset),
            sqlite_rowid: None,
            sqlite_blob_id: None,
            content_index: u32::try_from(index).ok(),
            record_sha256: line.record_sha256.clone(),
        })?;
        calls.insert(native_call_id.to_owned(), call_key);
    }
    Ok(())
}

fn process_result(
    context: &PiContext,
    source_item_key: &str,
    line: &JsonLine,
    entry: &Value,
    message: &Map<String, Value>,
    calls: &mut HashMap<String, String>,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(native_call_id) = message.get("toolCallId").and_then(Value::as_str) else {
        return Ok(());
    };
    let call_key = if let Some(key) = calls.get(native_call_id) {
        Some(key.clone())
    } else {
        sink.resolve_call(&context.session.session_key, native_call_id)?
    };
    let Some(call_key) = call_key else {
        sink.issue(&orphan_issue(source_item_key, line, native_call_id))?;
        return Ok(());
    };
    let output_text = rendered_text(message.get("content"));
    let output_json = structured_output(message)?;
    let is_error = message.get("isError").and_then(Value::as_bool);
    let fingerprint = keys::canonical_json(&serde_json::json!({
        "text": output_text,
        "structured": output_json,
        "is_error": is_error,
    }))?;
    let result_sha256 = keys::sha256(fingerprint.as_bytes());
    let result_key = keys::key(&[b"result", call_key.as_bytes(), &result_sha256]);
    sink.tool_result(&ToolResultRecord {
        result_key: result_key.clone(),
        call_key,
        returned_at_ms: timestamp_ms(entry.get("timestamp"))
            .or_else(|| timestamp_ms(message.get("timestamp"))),
        is_error,
        output_text: Some(output_text.unwrap_or_default()),
        output_json,
        result_sha256,
    })?;
    sink.observation(&ObservationRecord {
        observation_key: observation_key(source_item_key, line, 0, &result_key),
        source_item_key: source_item_key.to_owned(),
        call_key: None,
        result_key: Some(result_key),
        record_kind: RecordKind::Canonical,
        native_record_kind: "toolResult".to_owned(),
        sequence_number: i64::try_from(line.number).ok(),
        native_branch_id: entry
            .get("parentId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        is_current: None,
        line_number: Some(line.number),
        byte_offset: Some(line.byte_offset),
        sqlite_rowid: None,
        sqlite_blob_id: None,
        content_index: None,
        record_sha256: line.record_sha256.clone(),
    })
}

fn rendered_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(blocks)) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn structured_output(message: &Map<String, Value>) -> Result<Option<String>> {
    let mut output = Map::new();
    if let Some(details) = message.get("details").and_then(sanitize_details) {
        output.insert("details".to_owned(), details);
    }
    if let Some(Value::Array(blocks)) = message.get("content") {
        let non_text: Vec<Value> = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) != Some("text"))
            .cloned()
            .collect();
        if !non_text.is_empty() {
            output.insert("content".to_owned(), Value::Array(non_text));
        }
    }
    if output.is_empty() {
        Ok(None)
    } else {
        keys::canonical_json(&Value::Object(output)).map(Some)
    }
}

fn sanitize_details(value: &Value) -> Option<Value> {
    const EXCLUDED_KEYS: &[&str] = &[
        "chunk_id",
        "command",
        "completion_notification",
        "cwd",
        "final_output",
        "fullOutputPath",
        "log_path",
        "on_exit_wake",
        "output",
        "session_id",
        "tool_time_utc",
    ];
    match value {
        Value::Object(object) => {
            let mut clean = Map::new();
            for (key, child) in object {
                if EXCLUDED_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if let Some(child) = sanitize_details(child) {
                    clean.insert(key.clone(), child);
                }
            }
            (!clean.is_empty()).then_some(Value::Object(clean))
        }
        Value::Array(values) => {
            let clean: Vec<Value> = values.iter().filter_map(sanitize_details).collect();
            (!clean.is_empty()).then_some(Value::Array(clean))
        }
        Value::Null => None,
        value => Some(value.clone()),
    }
}

fn observation_key(source_item_key: &str, line: &JsonLine, index: usize, target: &str) -> String {
    keys::key(&[
        b"observation",
        source_item_key.as_bytes(),
        &line.byte_offset.to_be_bytes(),
        &u64::try_from(index).unwrap_or(u64::MAX).to_be_bytes(),
        target.as_bytes(),
    ])
}

fn orphan_issue(source_item_key: &str, line: &JsonLine, call_id: &str) -> IssueRecord {
    IssueRecord {
        issue_key: keys::key(&[
            b"orphan_result",
            source_item_key.as_bytes(),
            &line.byte_offset.to_be_bytes(),
            call_id.as_bytes(),
        ]),
        source_item_key: Some(source_item_key.to_owned()),
        severity: Severity::Warning,
        code: "result_without_call".to_owned(),
        line_number: Some(line.number),
        byte_offset: Some(line.byte_offset),
        sqlite_blob_id: None,
        record_sha256: Some(line.record_sha256.clone()),
        message: "Pi tool result has no defensible matching call".to_owned(),
        occurrence_count: 1,
    }
}

fn timestamp_ms(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::String(text)) => keys::parse_timestamp_ms(Some(text)),
        Some(Value::Number(number)) => number.as_i64(),
        _ => None,
    }
}
