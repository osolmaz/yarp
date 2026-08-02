use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use toolcall_extractor::adapters;
use toolcall_extractor::database::Database;
use toolcall_extractor::keys;

fn database(path: &Path, agent: &str) -> Database {
    Database::open(path, "test", agent).expect("open database")
}

fn extractor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_toolcall-extractor"))
}

#[test]
fn pi_import_resumes_an_appended_result() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let session = sessions.join("session.jsonl");
    fs::write(
        sessions.join("unsupported.jsonl"),
        "{\"type\":\"session\",\"version\":2,\"id\":\"old\"}\n",
    )
    .expect("unsupported session");
    fs::write(
        &session,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"c1\",\"name\":\"bash\",\"arguments\":{\"command\":\"cargo test\"}}]}}\n",
            "{broken}\n",
            "{\"type\":\"message\""
        ),
    )
    .expect("write session");
    let path = temp.path().join("data/toolcalls.duckdb");
    let mut db = database(&path, "pi");
    adapters::pi::extract("test", &sessions, &mut db).expect("first import");
    db.finish(true).expect("finish");
    drop(db);
    let first = Database::stats(&path).expect("stats");
    assert_eq!(first.tool_calls, 1);
    assert_eq!(first.tool_results, 0);
    assert_eq!(first.issues, 2);

    fs::write(
        &session,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"c1\",\"name\":\"bash\",\"arguments\":{\"command\":\"cargo test\"}}]}}\n",
            "{broken}\n",
            "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"c1\",\"content\":[{\"type\":\"text\",\"text\":\"ok\\n\"}],\"isError\":false}}\n",
            "{\"type\":\"message\",\"id\":\"m3\",\"parentId\":\"m2\",\"timestamp\":123,\"message\":{\"role\":\"assistant\",\"provider\":\"test\",\"model\":\"model\",\"content\":[{\"type\":\"text\",\"text\":\"ignored\"},{\"type\":\"toolCall\",\"id\":\"c2\",\"name\":\"read\"}]}}\n",
            "{\"type\":\"message\",\"id\":\"m4\",\"parentId\":\"m3\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"c2\",\"content\":[{\"type\":\"text\",\"text\":\"text\"},{\"type\":\"image\",\"data\":\"pixels\"}],\"details\":{\"status\":\"ok\",\"output\":\"drop\",\"nested\":[null,{\"value\":1,\"cwd\":\"drop\"}]},\"isError\":true}}\n",
            "{\"type\":\"message\",\"id\":\"m5\",\"parentId\":\"m4\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"reuse\",\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}]}}\n",
            "{\"type\":\"message\",\"id\":\"m6\",\"parentId\":\"m4\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"reuse\",\"name\":\"read\",\"arguments\":{\"path\":\"b\"}}]}}\n",
            "{\"type\":\"message\",\"id\":\"m7\",\"parentId\":\"m5\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"reuse\",\"content\":\"branch a\"}}\n",
            "{\"type\":\"other\"}\n"
        ),
    )
    .expect("append result");
    let mut db = database(&path, "pi");
    adapters::pi::extract("test", &sessions, &mut db).expect("second import");
    db.finish(true).expect("finish");
    let second = Database::stats(&path).expect("stats");
    assert_eq!(second.tool_calls, 4);
    assert_eq!(second.tool_results, 3);
    assert_eq!(second.calls_without_results, 1);
    drop(db);

    fs::write(
        &session,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"message\",\"id\":\"new-call\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"new\",\"name\":\"bash\",\"arguments\":{\"command\":\"cargo check\"}}]}}\n",
            "{\"type\":\"message\",\"id\":\"new-result\",\"parentId\":\"new-call\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"new\",\"content\":\"checked\"}}\n"
        ),
    )
    .expect("replace session");
    let mut db = database(&path, "pi");
    adapters::pi::extract("test", &sessions, &mut db).expect("replacement import");
    db.finish(true).expect("finish replacement");
    let replaced = Database::stats(&path).expect("replacement stats");
    assert_eq!(replaced.tool_calls, 1);
    assert_eq!(replaced.tool_results, 1);
    assert_eq!(replaced.calls_without_results, 0);
    assert_eq!(replaced.issues, 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn codex_imports_calls_and_outputs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let records = [
        serde_json::json!({"timestamp":"invalid","type":"session_meta","payload":{"id":"s1"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok\n"}}),
        serde_json::json!({"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"c1","output":"","status":"completed","exit_code":0}}),
        serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c2","name":"apply_patch","input":"patch"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c2","output":{"status":"ok"}}}),
        serde_json::json!({"type":"response_item","payload":{"type":"tool_search_call","id":"c3","arguments":{"query":"tool"}}}),
        serde_json::json!({"type":"response_item","payload":{"type":"tool_search_output","call_id":"c3","tools":[{"name":"tool"}]}}),
        serde_json::json!({"type":"response_item","payload":{"type":"web_search_call","id":"c4","action":{"query":"news"}}}),
        serde_json::json!({"type":"response_item","payload":{"type":"web_search_response","id":"c4","output":"found","status":"completed","query":"news"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"mcp_tool_call_end","id":"c5","invocation":{"server":"srv","tool":"read"},"stdout":"out","stderr":"warn","success":false,"exit_code":1}}),
        serde_json::json!({"type":"response_item","payload":{"type":"command_execution_end","id":"c6","command":["cargo","test"],"output":"done","status":"failed"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"patch_apply_end","id":"c7","changes":{"files":1},"success":true}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"c8","name":"shell","arguments":"not-json"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c8"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c9","output":"early"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"c9","name":"shell","arguments":"{}"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"c10","name":"exec_command","arguments":"{\"cmd\":\"printf canonical\"}"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c10","output":"canonical"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c10","output":"alternate"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"c11","name":"exec_command","arguments":"{\"cmd\":\"printf canonical\"}"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c11","output":"canonical text"}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"exec_command_end","call_id":"c11","output":"projection text","status":"completed","exit_code":0}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"c12","name":"exec_command","arguments":"{\"cmd\":\"true\"}"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c12","output":"text variant"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c12","output":{"status":"structured variant"}}}),
        serde_json::json!({"type":"response_item","payload":{"type":"ignored"}}),
        serde_json::json!({"type":"event_msg","payload":{}}),
    ];
    let mut rollout = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("record"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    rollout.push_str("{broken}\n");
    rollout.push_str(
        &serde_json::to_string(&serde_json::json!({
            "type":"response_item",
            "payload":{"type":"function_call_output","call_id":"missing","output":{"status":"orphan"}}
        }))
        .expect("orphan record"),
    );
    rollout.push('\n');
    fs::write(sessions.join("rollout.jsonl"), rollout).expect("rollout");
    let state_path = temp.path().join("state.sqlite");
    let state_connection = rusqlite::Connection::open(&state_path).expect("state");
    state_connection
        .execute_batch(
            "CREATE TABLE threads (
                id TEXT, created_at_ms INTEGER, created_at INTEGER, cwd TEXT,
                model_provider TEXT, model TEXT
             );
             INSERT INTO threads VALUES ('s1', NULL, 123, '/tmp', 'openai', 'test');",
        )
        .expect("state schema");
    drop(state_connection);
    let path = temp.path().join("data/toolcalls.duckdb");
    let mut db = database(&path, "codex");
    adapters::codex::extract("test", &sessions, Some(&state_path), &mut db).expect("extract");
    db.finish(true).expect("finish");
    let stats = Database::stats(&path).expect("stats");
    assert_eq!(stats.tool_calls, 12);
    assert_eq!(stats.tool_results, 14);
    assert_eq!(stats.calls_without_results, 0);
    assert_eq!(stats.calls_with_conflicting_results, 2);
    assert_eq!(stats.issues, 2);

    let connection = Database::open_read_only(&path).expect("read-only database");
    let (output_text, output_json, observation_count): (String, String, i64) = connection
        .query_row(
            "SELECT r.output_text, r.output_json,
                    (SELECT count(*) FROM observations o WHERE o.result_key = r.result_key)
             FROM tool_results r
             JOIN tool_calls c ON c.call_key = r.call_key
             WHERE c.native_call_id = 'c1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("merged result");
    assert_eq!(output_text, "ok\n");
    assert_eq!(output_json, r#"{"exit_code":0,"status":"completed"}"#);
    assert_eq!(observation_count, 2);

    let (output_text, output_json): (String, String) = connection
        .query_row(
            "SELECT r.output_text, r.output_json
             FROM tool_results r
             JOIN tool_calls c ON c.call_key = r.call_key
             WHERE c.native_call_id = 'c11'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("projection result");
    assert_eq!(output_text, "canonical text");
    let structured: serde_json::Value =
        serde_json::from_str(&output_json).expect("projection JSON");
    assert_eq!(
        structured["source_projections"][0]["output_text"],
        "projection text"
    );
}

#[test]
fn codex_retains_a_result_until_an_appended_call_resolves_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let rollout = sessions.join("rollout.jsonl");
    fs::write(
        &rollout,
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"c1\",\"output\":\"early\"}}\n"
        ),
    )
    .expect("initial rollout");
    let path = temp.path().join("data/toolcalls.duckdb");
    let mut db = database(&path, "codex");
    adapters::codex::extract("test", &sessions, None, &mut db).expect("initial extract");
    db.finish(true).expect("initial finish");
    drop(db);
    let unresolved = Database::verify(&path).expect("unresolved verification");
    assert_eq!(unresolved.orphan_results, 1);
    assert!(!unresolved.is_valid());

    fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .expect("open rollout")
        .write_all(
            b"{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"exec_command\",\"arguments\":\"{}\"}}\n",
        )
        .expect("append call");
    let mut db = database(&path, "codex");
    adapters::codex::extract("test", &sessions, None, &mut db).expect("resume extract");
    db.finish(true).expect("resume finish");
    let resolved = Database::verify(&path).expect("resolved verification");
    assert_eq!(resolved.orphan_results, 0);
    assert!(resolved.is_valid());
}

#[test]
fn codex_reconciles_a_projection_appended_after_its_result() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let rollout = sessions.join("rollout.jsonl");
    fs::write(
        &rollout,
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"exec_command\",\"arguments\":\"{}\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"c1\",\"output\":\"canonical\"}}\n"
        ),
    )
    .expect("initial rollout");
    let path = temp.path().join("data/toolcalls.duckdb");
    let mut db = database(&path, "codex");
    adapters::codex::extract("test", &sessions, None, &mut db).expect("initial extract");
    db.finish(true).expect("initial finish");
    drop(db);

    fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .expect("open rollout")
        .write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"exec_command_end\",\"call_id\":\"c1\",\"output\":\"projection\",\"status\":\"completed\"}}\n",
        )
        .expect("append projection");
    let mut db = database(&path, "codex");
    adapters::codex::extract("test", &sessions, None, &mut db).expect("resume extract");
    db.finish(true).expect("resume finish");
    drop(db);

    let stats = Database::stats(&path).expect("stats");
    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.tool_results, 1);
    assert_eq!(stats.calls_with_conflicting_results, 0);
    let connection = Database::open_read_only(&path).expect("read-only database");
    let output_json: String = connection
        .query_row("SELECT output_json FROM tool_results", [], |row| row.get(0))
        .expect("projection JSON");
    assert!(output_json.contains("projection"));
}

#[test]
fn claude_pairs_calls_across_transcript_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let projects = temp.path().join("projects");
    fs::create_dir(&projects).expect("projects");
    fs::create_dir(projects.join("tool-results")).expect("tool results");
    let persisted = projects.join("tool-results/output.txt");
    fs::write(&persisted, "persisted output").expect("persisted output");
    fs::write(
        projects.join("a-result.jsonl"),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"s1\",\"uuid\":\"r1\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"c1\",\"content\":\"ok\",\"is_error\":false}}]}}}}\n{{\"type\":\"user\",\"sessionId\":\"s1\",\"uuid\":\"r2\",\"parentUuid\":\"m2\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"toolUseResult\":{{\"durationMs\":12,\"stdout\":\"duplicate\",\"nested\":[null,{{\"value\":true,\"content\":\"drop\"}}]}},\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"c2\",\"content\":[{{\"type\":\"text\",\"text\":{}}},{{\"type\":\"image\",\"data\":\"ignored\"}}]}}]}}}}\n",
            serde_json::to_string(&format!("Full output saved to: {}", persisted.display()))
                .expect("reference")
        ),
    )
    .expect("result");
    fs::write(
        projects.join("z-call.jsonl"),
        "{\"type\":\"assistant\",\"sessionId\":\"s1\",\"uuid\":\"m1\",\"message\":{\"role\":\"assistant\",\"model\":\"test\",\"content\":[{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo test\"}}]}}\n{\"type\":\"assistant\",\"sessionId\":\"s1\",\"uuid\":\"m2\",\"agentId\":\"worker\",\"cwd\":\"/tmp\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"c2\",\"name\":\"Read\",\"input\":null}]}}\n",
    )
    .expect("call");
    let path = temp.path().join("data/toolcalls.duckdb");
    let mut db = database(&path, "claude");
    adapters::claude::extract("test", &projects, &mut db).expect("extract");
    db.finish(true).expect("finish");
    let verification = Database::verify(&path).expect("verify");
    assert_eq!(verification.orphan_results, 0);
    assert_eq!(verification.calls_without_results, 0);
    let stats = Database::stats(&path).expect("stats");
    assert_eq!(stats.tool_calls, 2);
    assert_eq!(stats.tool_results, 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn command_line_extracts_streams_reports_and_benchmarks() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    let output = (0..250)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\\n");
    fs::write(
        sessions.join("session.jsonl"),
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}}\n{{\"type\":\"message\",\"id\":\"m1\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"toolCall\",\"id\":\"c1\",\"name\":\"bash\",\"arguments\":{{\"command\":\"cargo test\"}}}}]}}}}\n{{\"type\":\"message\",\"id\":\"m2\",\"message\":{{\"role\":\"toolResult\",\"toolCallId\":\"c1\",\"content\":[{{\"type\":\"text\",\"text\":{}}}],\"isError\":false}}}}\n",
            serde_json::to_string(&output).expect("output JSON")
        ),
    )
    .expect("session");
    let database = temp.path().join("direct/toolcalls.duckdb");
    let extract = extractor()
        .args([
            "extract",
            "--database",
            database.to_str().expect("database"),
            "--unix-user",
            "test",
            "pi",
            "--sessions",
            sessions.to_str().expect("sessions"),
        ])
        .output()
        .expect("run extract");
    assert!(
        extract.status.success(),
        "{}",
        String::from_utf8_lossy(&extract.stderr)
    );
    for command in ["stats", "issues", "verify", "benchmark-yarp"] {
        let result = extractor()
            .args([command, "--database", database.to_str().expect("database")])
            .output()
            .expect("run report");
        assert!(
            result.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let ceiling = extractor()
        .args([
            "analyze-ceiling",
            "--database",
            database.to_str().expect("database"),
        ])
        .output()
        .expect("run ceiling analysis");
    assert!(
        ceiling.status.success(),
        "{}",
        String::from_utf8_lossy(&ceiling.stderr)
    );
    let ceiling_report: serde_json::Value =
        serde_json::from_slice(&ceiling.stdout).expect("ceiling JSON");
    assert_eq!(ceiling_report["scope"], "stored_shell_result_text");
    assert_eq!(ceiling_report["totals"]["shell_results"], 1);
    assert!(ceiling_report.get("privacy").is_none());

    let stream = extractor()
        .args([
            "stream",
            "--unix-user",
            "test",
            "pi",
            "--sessions",
            sessions.to_str().expect("sessions"),
        ])
        .output()
        .expect("stream");
    assert!(stream.status.success());
    let streamed_database = temp.path().join("streamed/toolcalls.duckdb");
    let mut ingest = extractor()
        .args([
            "ingest",
            "--database",
            streamed_database.to_str().expect("database"),
            "--unix-user",
            "test",
            "--agent",
            "pi",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn ingest");
    ingest
        .stdin
        .take()
        .expect("ingest stdin")
        .write_all(&stream.stdout)
        .expect("write stream");
    assert!(ingest.wait().expect("ingest status").success());
    assert!(
        extractor()
            .args([
                "verify",
                "--database",
                streamed_database.to_str().expect("database"),
            ])
            .status()
            .expect("verify streamed")
            .success()
    );

    let session_path = sessions.join("session.jsonl");
    let metadata = fs::metadata(&session_path).expect("session metadata");
    let replacement_text = fs::read_to_string(&session_path)
        .expect("read streamed session")
        .replace("cargo test", "cargo lint");
    assert_eq!(
        replacement_text.len(),
        usize::try_from(metadata.len()).expect("session length")
    );
    fs::write(&session_path, replacement_text).expect("replace streamed session");
    fs::OpenOptions::new()
        .write(true)
        .open(&session_path)
        .expect("open replaced session")
        .set_times(
            fs::FileTimes::new()
                .set_accessed(metadata.accessed().expect("session access time"))
                .set_modified(metadata.modified().expect("session modification time")),
        )
        .expect("restore session times");
    let replacement = extractor()
        .args([
            "stream",
            "--unix-user",
            "test",
            "pi",
            "--sessions",
            sessions.to_str().expect("sessions"),
        ])
        .output()
        .expect("replacement stream");
    assert!(replacement.status.success());
    let frames = replacement
        .stdout
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    assert!(frames.len() >= 2);
    let commit: toolcall_extractor::model::StreamRecord =
        serde_json::from_slice(frames[frames.len() - 2]).expect("source commit frame");
    assert!(matches!(
        commit,
        toolcall_extractor::model::StreamRecord::SourceCommit
    ));
    let truncated = frames[..frames.len() - 2].concat();
    let mut truncated_ingest = extractor()
        .args([
            "ingest",
            "--database",
            streamed_database.to_str().expect("database"),
            "--unix-user",
            "test",
            "--agent",
            "pi",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn truncated ingest");
    truncated_ingest
        .stdin
        .take()
        .expect("truncated ingest stdin")
        .write_all(&truncated)
        .expect("write truncated stream");
    assert!(
        !truncated_ingest
            .wait()
            .expect("truncated ingest status")
            .success()
    );
    let connection = Database::open_read_only(&streamed_database).expect("preserved database");
    let preserved_input: String = connection
        .query_row("SELECT input_text FROM tool_calls", [], |row| row.get(0))
        .expect("preserved call");
    assert!(preserved_input.contains("cargo test"));
    drop(connection);

    let mut replacement_ingest = extractor()
        .args([
            "ingest",
            "--database",
            streamed_database.to_str().expect("database"),
            "--unix-user",
            "test",
            "--agent",
            "pi",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn replacement ingest");
    replacement_ingest
        .stdin
        .take()
        .expect("replacement ingest stdin")
        .write_all(&replacement.stdout)
        .expect("write replacement stream");
    assert!(
        replacement_ingest
            .wait()
            .expect("replacement ingest status")
            .success()
    );
    let stats = Database::stats(&streamed_database).expect("replacement stats");
    assert_eq!(stats.tool_calls, 1);
    assert_eq!(stats.tool_results, 1);
    let connection = Database::open_read_only(&streamed_database).expect("replacement database");
    let replacement_input: String = connection
        .query_row("SELECT input_text FROM tool_calls", [], |row| row.get(0))
        .expect("replacement call");
    assert!(replacement_input.contains("cargo lint"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn cursor_reads_current_sqlite_blobs_and_validates_transcript() {
    let temp = tempfile::tempdir().expect("tempdir");
    let chats = temp.path().join("chats/session");
    let acp = temp.path().join("acp");
    let projects = temp.path().join("projects/p/agent-transcripts");
    fs::create_dir_all(&chats).expect("chats");
    let unsupported_chat = chats.parent().expect("chat root").join("unsupported");
    fs::create_dir_all(&unsupported_chat).expect("unsupported chat");
    rusqlite::Connection::open(unsupported_chat.join("store.db")).expect("unsupported SQLite");
    fs::create_dir_all(&acp).expect("acp");
    fs::create_dir_all(&projects).expect("projects");
    let session_id = "11111111-1111-1111-1111-111111111111";
    let call_blob = vec![0x11; 32];
    let result_blob = vec![0x22; 32];
    let root_blob = vec![0x33; 32];
    let metadata = serde_json::json!({
        "agentId": session_id,
        "latestRootBlobId": keys::hex(&root_blob),
        "createdAt": 1,
        "blobEncryptionKey": "must-be-ignored"
    });
    let metadata_hex = keys::hex(serde_json::to_string(&metadata).expect("json").as_bytes());
    let connection = rusqlite::Connection::open(chats.join("store.db")).expect("sqlite");
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
        )
        .expect("schema");
    connection
        .execute("INSERT INTO meta VALUES ('0', ?)", [metadata_hex])
        .expect("meta");
    let mut root = Vec::new();
    for id in [&call_blob, &result_blob] {
        root.extend([0x0a, 0x20]);
        root.extend(id);
    }
    let result = serde_json::json!({
        "role": "tool",
        "content": [{"type":"tool-result","toolCallId":"c1","toolName":"shell","result":"ok","experimental_content":[{"type":"text","text":"ok"}]}],
        "providerOptions": {"cursor":{"highLevelToolCallResult":{"isError":false,"output":{"success":{}}}}}
    });
    let call = serde_json::json!({
        "role": "assistant",
        "content": [{"type":"tool-call","toolCallId":"c1","toolName":"shell","args":{"command":"cargo test"}}]
    });
    connection
        .execute(
            "INSERT INTO blobs VALUES (?, ?)",
            rusqlite::params![
                keys::hex(&result_blob),
                serde_json::to_vec(&result).expect("result")
            ],
        )
        .expect("result blob");
    connection
        .execute(
            "INSERT INTO blobs VALUES (?, ?)",
            rusqlite::params![
                keys::hex(&call_blob),
                serde_json::to_vec(&call).expect("call")
            ],
        )
        .expect("call blob");
    connection
        .execute(
            "INSERT INTO blobs VALUES (?, ?)",
            rusqlite::params![keys::hex(&root_blob), root],
        )
        .expect("root blob");
    fs::write(
        projects.join(format!("{session_id}.jsonl")),
        "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"shell\",\"input\":{\"command\":\"cargo test\"}}]}}\n{\"type\":\"turn_ended\",\"status\":\"success\"}\n",
    )
    .expect("transcript");
    let path = temp.path().join("data/toolcalls.duckdb");
    let mut db = database(&path, "cursor");
    adapters::cursor::extract(
        "test",
        chats.parent().expect("chat root"),
        &acp,
        temp.path().join("projects").as_path(),
        &mut db,
    )
    .expect("extract");
    db.finish(true).expect("finish");
    drop(db);

    let second_call_blob = vec![0x44; 32];
    let second_result_blob = vec![0x55; 32];
    let second_call = serde_json::json!({
        "role": "assistant",
        "content": [{"type":"tool-call","toolCallId":"c2","toolName":"shell","args":{"command":"cargo check"}}]
    });
    let second_result = serde_json::json!({
        "role": "tool",
        "content": [{"type":"tool-result","toolCallId":"c2","toolName":"shell","result":"checked"}]
    });
    connection
        .execute(
            "INSERT INTO blobs VALUES (?, ?)",
            rusqlite::params![
                keys::hex(&second_call_blob),
                serde_json::to_vec(&second_call).expect("second call")
            ],
        )
        .expect("second call blob");
    connection
        .execute(
            "INSERT INTO blobs VALUES (?, ?)",
            rusqlite::params![
                keys::hex(&second_result_blob),
                serde_json::to_vec(&second_result).expect("second result")
            ],
        )
        .expect("second result blob");
    fs::write(
        projects.join(format!("{session_id}.jsonl")),
        "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"shell\",\"input\":{\"command\":\"cargo test\"}}]}}\n{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"shell\",\"input\":{\"command\":\"cargo check\"}}]}}\n",
    )
    .expect("updated transcript");
    let mut db = database(&path, "cursor");
    adapters::cursor::extract(
        "test",
        chats.parent().expect("chat root"),
        &acp,
        temp.path().join("projects").as_path(),
        &mut db,
    )
    .expect("re-extract WAL");
    db.finish(true).expect("finish second");
    drop(connection);
    let stats = Database::stats(&path).expect("stats");
    assert_eq!(stats.tool_calls, 2);
    assert_eq!(stats.tool_results, 2);
    assert_eq!(stats.issues, 2);
}
