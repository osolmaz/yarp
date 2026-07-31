use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::common::{JsonLine, process_jsonl};
use super::{register_root, source_item};
use crate::error::{Error, Result};
use crate::keys;
use crate::model::{
    InputFormat, ObservationRecord, RecordKind, SessionRecord, ToolCallRecord, ToolResultRecord,
};
use crate::private_fs;
use crate::sink::Sink;

pub fn extract(unix_user: &str, projects: &Path, sink: &mut impl Sink) -> Result<u64> {
    let root = register_root(unix_user, "claude", "projects", projects, sink)?;
    let mut processed = 0_u64;
    for entry in WalkDir::new(projects).follow_links(false) {
        let entry = entry.map_err(|error| Error::InvalidSource(error.to_string()))?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            continue;
        }
        let relative = private_fs::relative_path(projects, entry.path())?;
        let source = source_item(&root, relative);
        let mut calls = HashMap::<String, String>::new();
        let did_process = process_jsonl(
            entry.path(),
            source.clone(),
            sink,
            |line, value, target| {
                process_record(
                    unix_user,
                    projects,
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

fn process_record(
    unix_user: &str,
    projects: &Path,
    source_item_key: &str,
    line: &JsonLine,
    value: &Value,
    calls: &mut HashMap<String, String>,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(native_session_id) = value.get("sessionId").and_then(Value::as_str) else {
        return Ok(());
    };
    let session = SessionRecord {
        session_key: keys::key(&[
            b"session",
            unix_user.as_bytes(),
            b"claude",
            native_session_id.as_bytes(),
        ]),
        unix_user: unix_user.to_owned(),
        agent: "claude".to_owned(),
        native_session_id: native_session_id.to_owned(),
        started_at_ms: outer_timestamp(value),
    };
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        return Ok(());
    };
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Ok(());
    };
    sink.session(&session)?;
    for (index, block) in content.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => process_call(
                unix_user,
                source_item_key,
                line,
                value,
                message,
                block,
                index,
                &session,
                calls,
                sink,
            )?,
            Some("tool_result") => process_result(
                unix_user,
                projects,
                source_item_key,
                line,
                value,
                block,
                index,
                &session,
                calls,
                sink,
            )?,
            _ => {}
        }
    }
    Ok(())
}

fn process_call(
    unix_user: &str,
    source_item_key: &str,
    line: &JsonLine,
    outer: &Value,
    message: &Map<String, Value>,
    block: &Value,
    index: usize,
    session: &SessionRecord,
    calls: &mut HashMap<String, String>,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(native_call_id) = block.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(tool_name) = block.get("name").and_then(Value::as_str) else {
        return Ok(());
    };
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    let input_text = keys::canonical_json(&input)?;
    let input_sha256 = keys::sha256(input_text.as_bytes());
    let call_key = call_key(unix_user, &session.native_session_id, native_call_id);
    sink.tool_call(&ToolCallRecord {
        call_key: call_key.clone(),
        session_key: session.session_key.clone(),
        native_call_id: Some(native_call_id.to_owned()),
        native_worker_id: outer
            .get("agentId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        called_at_ms: outer_timestamp(outer),
        provider: None,
        model: message
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        working_directory: outer
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        tool_name: tool_name.to_owned(),
        input_format: InputFormat::Json,
        input_text,
        input_sha256,
    })?;
    sink.observation(&observation(
        source_item_key,
        line,
        index,
        Some(&call_key),
        None,
        "tool_use",
        outer,
    ))?;
    calls.insert(native_call_id.to_owned(), call_key);
    Ok(())
}

fn process_result(
    unix_user: &str,
    projects: &Path,
    source_item_key: &str,
    line: &JsonLine,
    outer: &Value,
    block: &Value,
    index: usize,
    session: &SessionRecord,
    calls: &HashMap<String, String>,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(native_call_id) = block.get("tool_use_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let call_key = calls
        .get(native_call_id)
        .cloned()
        .unwrap_or_else(|| call_key(unix_user, &session.native_session_id, native_call_id));
    let mut output_text = rendered_text(block.get("content"));
    if let Some(reference) = output_text
        .as_deref()
        .and_then(persisted_output_path)
        .and_then(|path| read_referenced_output(projects, &path).ok())
    {
        output_text = Some(reference);
    }
    let output_json = outer
        .get("toolUseResult")
        .and_then(sanitize_tool_result)
        .map(|value| keys::canonical_json(&value))
        .transpose()?;
    let is_error = block.get("is_error").and_then(Value::as_bool);
    let safe_text = output_text.or_else(|| output_json.is_none().then(String::new));
    let fingerprint = keys::canonical_json(&serde_json::json!({
        "text": safe_text,
        "json": output_json,
        "is_error": is_error,
    }))?;
    let result_sha256 = keys::sha256(fingerprint.as_bytes());
    let result_key = keys::key(&[b"result", call_key.as_bytes(), &result_sha256]);
    sink.tool_result(&ToolResultRecord {
        result_key: result_key.clone(),
        call_key,
        returned_at_ms: outer_timestamp(outer),
        is_error,
        output_text: safe_text,
        output_json,
        result_sha256,
    })?;
    sink.observation(&observation(
        source_item_key,
        line,
        index,
        None,
        Some(&result_key),
        "tool_result",
        outer,
    ))
}

fn observation(
    source_item_key: &str,
    line: &JsonLine,
    index: usize,
    call_key: Option<&str>,
    result_key: Option<&str>,
    native_kind: &str,
    outer: &Value,
) -> ObservationRecord {
    let target = call_key.or(result_key).unwrap_or("");
    ObservationRecord {
        observation_key: keys::key(&[
            b"observation",
            source_item_key.as_bytes(),
            &line.byte_offset.to_be_bytes(),
            &u64::try_from(index).unwrap_or(u64::MAX).to_be_bytes(),
            target.as_bytes(),
        ]),
        source_item_key: source_item_key.to_owned(),
        call_key: call_key.map(ToOwned::to_owned),
        result_key: result_key.map(ToOwned::to_owned),
        record_kind: RecordKind::Canonical,
        native_record_kind: native_kind.to_owned(),
        sequence_number: i64::try_from(line.number).ok(),
        native_branch_id: outer
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        is_current: None,
        line_number: Some(line.number),
        byte_offset: Some(line.byte_offset),
        sqlite_rowid: None,
        sqlite_blob_id: None,
        content_index: u32::try_from(index).ok(),
        record_sha256: line.record_sha256.clone(),
    }
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

fn sanitize_tool_result(value: &Value) -> Option<Value> {
    const EXCLUDED: &[&str] = &[
        "content",
        "newString",
        "oldString",
        "originalFile",
        "outputFile",
        "persistedOutputPath",
        "prompt",
        "stderr",
        "stdout",
    ];
    match value {
        Value::Object(object) => {
            let mut clean = Map::new();
            for (key, child) in object {
                if EXCLUDED.contains(&key.as_str()) {
                    continue;
                }
                if let Some(child) = sanitize_tool_result(child) {
                    clean.insert(key.clone(), child);
                }
            }
            (!clean.is_empty()).then_some(Value::Object(clean))
        }
        Value::Array(values) => {
            let clean: Vec<Value> = values.iter().filter_map(sanitize_tool_result).collect();
            (!clean.is_empty()).then_some(Value::Array(clean))
        }
        Value::Null => None,
        value => Some(value.clone()),
    }
}

fn persisted_output_path(text: &str) -> Option<PathBuf> {
    let marker = "Full output saved to: ";
    let start = text.find(marker)?.saturating_add(marker.len());
    let path = text[start..].lines().next()?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn read_referenced_output(root: &Path, path: &Path) -> Result<String> {
    let canonical_root = fs::canonicalize(root).map_err(|error| Error::io(root, error))?;
    let canonical = fs::canonicalize(path).map_err(|error| Error::io(path, error))?;
    if !canonical.starts_with(&canonical_root)
        || !canonical
            .components()
            .any(|part| part.as_os_str() == "tool-results")
    {
        return Err(Error::InvalidSource(
            "Claude result reference escapes the projects root".to_owned(),
        ));
    }
    fs::read_to_string(&canonical).map_err(|error| Error::io(canonical, error))
}

fn call_key(unix_user: &str, session_id: &str, native_call_id: &str) -> String {
    keys::key(&[
        b"call",
        unix_user.as_bytes(),
        b"claude",
        session_id.as_bytes(),
        native_call_id.as_bytes(),
    ])
}

fn outer_timestamp(value: &Value) -> Option<i64> {
    keys::parse_timestamp_ms(value.get("timestamp").and_then(Value::as_str))
}
