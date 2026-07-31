use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use rusqlite::OpenFlags;
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::common::{JsonLine, first_json_value, process_jsonl};
use super::{register_root, source_item};
use crate::error::{Error, Result};
use crate::keys;
use crate::model::{
    InputFormat, ObservationRecord, RecordKind, SessionRecord, ToolCallRecord, ToolResultRecord,
};
use crate::private_fs;
use crate::sink::Sink;

pub fn extract(
    unix_user: &str,
    sessions: &Path,
    state_db: Option<&Path>,
    sink: &mut impl Sink,
) -> Result<u64> {
    let metadata = state_db
        .map(load_state_metadata)
        .transpose()?
        .unwrap_or_default();
    let root = register_root(unix_user, "codex", "sessions", sessions, sink)?;
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
        let context = CodexContext::from_header(unix_user, &first, &metadata)?;
        let file_state = RefCell::new(FileState::default());
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
                    &mut file_state.borrow_mut(),
                    target,
                )
            },
            |target| flush_pending(&context, &mut file_state.borrow_mut(), target),
        )?;
        if did_process {
            processed = processed.saturating_add(1);
        }
    }
    Ok(processed)
}

#[derive(Clone, Debug, Default)]
struct StateMetadata {
    created_at_ms: Option<i64>,
    cwd: Option<String>,
    provider: Option<String>,
    model: Option<String>,
}

fn load_state_metadata(path: &Path) -> Result<HashMap<String, StateMetadata>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", true)?;
    let mut statement = connection
        .prepare("SELECT id, created_at_ms, created_at, cwd, model_provider, model FROM threads")?;
    let rows = statement.query_map([], |row| {
        let created_at_ms: Option<i64> = row.get(1)?;
        let created_at: i64 = row.get(2)?;
        Ok((
            row.get::<_, String>(0)?,
            StateMetadata {
                created_at_ms: created_at_ms.or(Some(created_at.saturating_mul(1000))),
                cwd: row.get(3)?,
                provider: row.get(4)?,
                model: row.get(5)?,
            },
        ))
    })?;
    let mut metadata = HashMap::new();
    for row in rows {
        let (id, value) = row?;
        metadata.insert(id, value);
    }
    Ok(metadata)
}

struct CodexContext {
    unix_user: String,
    session: SessionRecord,
    working_directory: Option<String>,
    provider: Option<String>,
    model: Option<String>,
}

impl CodexContext {
    fn from_header(
        unix_user: &str,
        value: &Value,
        metadata: &HashMap<String, StateMetadata>,
    ) -> Result<Self> {
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            return Err(Error::InvalidSource(
                "Codex rollout does not start with session_meta".to_owned(),
            ));
        }
        let payload = value
            .get("payload")
            .and_then(Value::as_object)
            .ok_or_else(|| Error::InvalidSource("Codex session metadata is missing".to_owned()))?;
        let native_session_id = payload
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidSource("Codex thread id is missing".to_owned()))?;
        let state = metadata.get(native_session_id);
        let started_at_ms =
            keys::parse_timestamp_ms(value.get("timestamp").and_then(Value::as_str))
                .or_else(|| state.and_then(|entry| entry.created_at_ms));
        Ok(Self {
            unix_user: unix_user.to_owned(),
            session: SessionRecord {
                session_key: keys::key(&[
                    b"session",
                    unix_user.as_bytes(),
                    b"codex",
                    native_session_id.as_bytes(),
                ]),
                unix_user: unix_user.to_owned(),
                agent: "codex".to_owned(),
                native_session_id: native_session_id.to_owned(),
                started_at_ms,
            },
            working_directory: payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| state.and_then(|entry| entry.cwd.clone())),
            provider: payload
                .get("model_provider")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| state.and_then(|entry| entry.provider.clone())),
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| state.and_then(|entry| entry.model.clone())),
        })
    }
}

#[derive(Default)]
struct FileState {
    calls: HashMap<String, String>,
    pending_results: HashMap<String, PendingResult>,
}

struct PendingResult {
    line_number: u64,
    byte_offset: u64,
    record_sha256: Vec<u8>,
    call_key: String,
    returned_at_ms: Option<i64>,
    is_error: Option<bool>,
    output_text: Option<String>,
    output_json: Option<String>,
    native_record_kind: String,
    source_item_key: String,
}

fn process_record(
    context: &CodexContext,
    source_item_key: &str,
    line: &JsonLine,
    value: &Value,
    state: &mut FileState,
    sink: &mut dyn Sink,
) -> Result<()> {
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        sink.session(&context.session)?;
        return Ok(());
    }
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return Ok(());
    };
    let Some(kind) = payload.get("type").and_then(Value::as_str) else {
        return Ok(());
    };
    match kind {
        "function_call" | "custom_tool_call" | "tool_search_call" | "web_search_call" => {
            canonical_call(
                context,
                source_item_key,
                line,
                value,
                payload,
                kind,
                state,
                sink,
            )
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            canonical_result(
                context,
                source_item_key,
                line,
                value,
                payload,
                kind,
                state,
                sink,
            )
        }
        kind if kind.ends_with("_end") || kind.ends_with("_response") => projection_result(
            context,
            source_item_key,
            line,
            value,
            payload,
            kind,
            state,
            sink,
        ),
        _ => Ok(()),
    }
}

fn canonical_call(
    context: &CodexContext,
    source_item_key: &str,
    line: &JsonLine,
    outer: &Value,
    payload: &Map<String, Value>,
    kind: &str,
    state: &mut FileState,
    sink: &mut dyn Sink,
) -> Result<()> {
    let native_call_id = payload
        .get("call_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str);
    let Some(native_call_id) = native_call_id else {
        return Ok(());
    };
    let tool_name = match kind {
        "tool_search_call" => "tool_search",
        "web_search_call" => "web_search",
        _ => payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
    };
    let (input_format, input_text) = call_input(payload, kind)?;
    let input_sha256 = keys::sha256(input_text.as_bytes());
    let call_key = call_key(context, native_call_id);
    sink.session(&context.session)?;
    sink.tool_call(&ToolCallRecord {
        call_key: call_key.clone(),
        session_key: context.session.session_key.clone(),
        native_call_id: Some(native_call_id.to_owned()),
        native_worker_id: None,
        called_at_ms: outer_timestamp(outer),
        provider: context.provider.clone(),
        model: context.model.clone(),
        working_directory: context.working_directory.clone(),
        tool_name: tool_name.to_owned(),
        input_format,
        input_text,
        input_sha256,
    })?;
    sink.observation(&call_observation(
        source_item_key,
        line,
        &call_key,
        kind,
        RecordKind::Canonical,
    ))?;
    state.calls.insert(native_call_id.to_owned(), call_key);
    Ok(())
}

fn canonical_result(
    context: &CodexContext,
    source_item_key: &str,
    line: &JsonLine,
    outer: &Value,
    payload: &Map<String, Value>,
    kind: &str,
    state: &mut FileState,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(native_call_id) = payload.get("call_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let call_key = resolve_call(context, native_call_id, state, sink)?
        .unwrap_or_else(|| call_key(context, native_call_id));
    state.pending_results.remove(native_call_id);
    let output = payload.get("output").or_else(|| payload.get("tools"));
    let (output_text, output_json) = output_parts(output)?;
    write_result(
        source_item_key,
        line,
        &call_key,
        kind,
        outer_timestamp(outer),
        payload.get("is_error").and_then(Value::as_bool),
        output_text,
        output_json,
        RecordKind::Canonical,
        sink,
    )
}

fn projection_result(
    context: &CodexContext,
    source_item_key: &str,
    line: &JsonLine,
    outer: &Value,
    payload: &Map<String, Value>,
    kind: &str,
    state: &mut FileState,
    sink: &mut dyn Sink,
) -> Result<()> {
    let Some(native_call_id) = payload
        .get("call_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let call_key = if let Some(existing) = resolve_call(context, native_call_id, state, sink)? {
        existing
    } else {
        let tool_name = projection_tool_name(kind, payload);
        let input = projection_input(payload);
        let input_text = keys::canonical_json(&input)?;
        let input_sha256 = keys::sha256(input_text.as_bytes());
        let key = call_key(context, native_call_id);
        sink.session(&context.session)?;
        sink.tool_call(&ToolCallRecord {
            call_key: key.clone(),
            session_key: context.session.session_key.clone(),
            native_call_id: Some(native_call_id.to_owned()),
            native_worker_id: None,
            called_at_ms: outer_timestamp(outer),
            provider: context.provider.clone(),
            model: context.model.clone(),
            working_directory: context.working_directory.clone(),
            tool_name,
            input_format: InputFormat::Json,
            input_text,
            input_sha256,
        })?;
        sink.observation(&call_observation(
            source_item_key,
            line,
            &key,
            kind,
            RecordKind::Projection,
        ))?;
        state.calls.insert(native_call_id.to_owned(), key.clone());
        key
    };
    let (output_text, output_json) = projection_output(payload)?;
    state.pending_results.insert(
        native_call_id.to_owned(),
        PendingResult {
            line_number: line.number,
            byte_offset: line.byte_offset,
            record_sha256: line.record_sha256.clone(),
            call_key,
            returned_at_ms: outer_timestamp(outer),
            is_error: projection_error(payload),
            output_text,
            output_json,
            native_record_kind: kind.to_owned(),
            source_item_key: source_item_key.to_owned(),
        },
    );
    Ok(())
}

fn flush_pending(
    _context: &CodexContext,
    state: &mut FileState,
    sink: &mut dyn Sink,
) -> Result<()> {
    for pending in state.pending_results.drain().map(|(_, value)| value) {
        let line = JsonLine {
            number: pending.line_number,
            byte_offset: pending.byte_offset,
            bytes: Vec::new(),
            record_sha256: pending.record_sha256,
        };
        write_result(
            &pending.source_item_key,
            &line,
            &pending.call_key,
            &pending.native_record_kind,
            pending.returned_at_ms,
            pending.is_error,
            pending.output_text,
            pending.output_json,
            RecordKind::Projection,
            sink,
        )?;
    }
    Ok(())
}

fn write_result(
    source_item_key: &str,
    line: &JsonLine,
    call_key: &str,
    native_kind: &str,
    returned_at_ms: Option<i64>,
    is_error: Option<bool>,
    output_text: Option<String>,
    output_json: Option<String>,
    record_kind: RecordKind,
    sink: &mut dyn Sink,
) -> Result<()> {
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
        call_key: call_key.to_owned(),
        returned_at_ms,
        is_error,
        output_text: safe_text,
        output_json,
        result_sha256,
    })?;
    sink.observation(&ObservationRecord {
        observation_key: observation_key(source_item_key, line, &result_key, "result"),
        source_item_key: source_item_key.to_owned(),
        call_key: None,
        result_key: Some(result_key),
        record_kind,
        native_record_kind: native_kind.to_owned(),
        sequence_number: i64::try_from(line.number).ok(),
        native_branch_id: None,
        is_current: None,
        line_number: Some(line.number),
        byte_offset: Some(line.byte_offset),
        sqlite_rowid: None,
        sqlite_blob_id: None,
        content_index: None,
        record_sha256: line.record_sha256.clone(),
    })
}

fn call_input(payload: &Map<String, Value>, kind: &str) -> Result<(InputFormat, String)> {
    match kind {
        "function_call" => parse_argument_string(payload.get("arguments")),
        "custom_tool_call" => Ok((
            InputFormat::Text,
            payload
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )),
        "tool_search_call" => Ok((
            InputFormat::Json,
            keys::canonical_json(payload.get("arguments").unwrap_or(&Value::Null))?,
        )),
        "web_search_call" => Ok((
            InputFormat::Json,
            keys::canonical_json(payload.get("action").unwrap_or(&Value::Null))?,
        )),
        _ => Ok((InputFormat::Json, "{}".to_owned())),
    }
}

fn parse_argument_string(value: Option<&Value>) -> Result<(InputFormat, String)> {
    let text = value.and_then(Value::as_str).unwrap_or_default();
    if let Ok(json) = serde_json::from_str::<Value>(text) {
        return Ok((InputFormat::Json, keys::canonical_json(&json)?));
    }
    Ok((InputFormat::Text, text.to_owned()))
}

fn output_parts(output: Option<&Value>) -> Result<(Option<String>, Option<String>)> {
    match output {
        Some(Value::String(text)) => Ok((Some(text.clone()), None)),
        Some(value) => Ok((None, Some(keys::canonical_json(value)?))),
        None => Ok((Some(String::new()), None)),
    }
}

fn projection_input(payload: &Map<String, Value>) -> Value {
    for field in ["invocation", "action", "command", "cmd", "query"] {
        if let Some(value) = payload.get(field) {
            return value.clone();
        }
    }
    Value::Object(Map::new())
}

fn projection_output(payload: &Map<String, Value>) -> Result<(Option<String>, Option<String>)> {
    let text = payload
        .get("output")
        .or_else(|| payload.get("stdout"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut structured = Map::new();
    for field in [
        "stderr",
        "status",
        "success",
        "exit_code",
        "result",
        "changes",
        "action",
        "query",
    ] {
        if let Some(value) = payload.get(field) {
            structured.insert(field.to_owned(), value.clone());
        }
    }
    let json = if structured.is_empty() {
        None
    } else {
        Some(keys::canonical_json(&Value::Object(structured))?)
    };
    Ok((text, json))
}

fn projection_error(payload: &Map<String, Value>) -> Option<bool> {
    payload
        .get("success")
        .and_then(Value::as_bool)
        .map(|success| !success)
        .or_else(|| {
            payload
                .get("status")
                .and_then(Value::as_str)
                .map(|status| matches!(status, "failed" | "error" | "cancelled"))
        })
}

fn projection_tool_name(kind: &str, payload: &Map<String, Value>) -> String {
    if let Some(invocation) = payload.get("invocation").and_then(Value::as_object)
        && let Some(tool) = invocation.get("tool").and_then(Value::as_str)
    {
        return format!(
            "mcp:{}:{tool}",
            invocation
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    kind.strip_suffix("_end")
        .or_else(|| kind.strip_suffix("_response"))
        .unwrap_or(kind)
        .to_owned()
}

fn resolve_call(
    context: &CodexContext,
    native_call_id: &str,
    state: &FileState,
    sink: &dyn Sink,
) -> Result<Option<String>> {
    if let Some(key) = state.calls.get(native_call_id) {
        return Ok(Some(key.clone()));
    }
    sink.resolve_call(&context.session.session_key, native_call_id)
}

fn call_key(context: &CodexContext, native_call_id: &str) -> String {
    keys::key(&[
        b"call",
        context.unix_user.as_bytes(),
        b"codex",
        context.session.native_session_id.as_bytes(),
        native_call_id.as_bytes(),
    ])
}

fn call_observation(
    source_item_key: &str,
    line: &JsonLine,
    call_key: &str,
    kind: &str,
    record_kind: RecordKind,
) -> ObservationRecord {
    ObservationRecord {
        observation_key: observation_key(source_item_key, line, call_key, "call"),
        source_item_key: source_item_key.to_owned(),
        call_key: Some(call_key.to_owned()),
        result_key: None,
        record_kind,
        native_record_kind: kind.to_owned(),
        sequence_number: i64::try_from(line.number).ok(),
        native_branch_id: None,
        is_current: None,
        line_number: Some(line.number),
        byte_offset: Some(line.byte_offset),
        sqlite_rowid: None,
        sqlite_blob_id: None,
        content_index: None,
        record_sha256: line.record_sha256.clone(),
    }
}

fn observation_key(source_item_key: &str, line: &JsonLine, target: &str, role: &str) -> String {
    keys::key(&[
        b"observation",
        source_item_key.as_bytes(),
        &line.byte_offset.to_be_bytes(),
        role.as_bytes(),
        target.as_bytes(),
    ])
}

fn outer_timestamp(value: &Value) -> Option<i64> {
    keys::parse_timestamp_ms(value.get("timestamp").and_then(Value::as_str))
}
