use rusqlite::Connection;
use serde_json::json;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;
use yarp_cli::archive::{Archive, CallIdentity, SessionIdentity};

fn yarp(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(arguments)
        .output()
        .expect("run yarp")
}

fn yarp_with_archive(arguments: &[&str], path: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(arguments)
        .env("YARP_ARCHIVE_PATH", path)
        .output()
        .expect("run yarp")
}

fn session() -> SessionIdentity {
    SessionIdentity {
        agent: "pi".to_owned(),
        account: "test".to_owned(),
        source_session_id: "session-1".to_owned(),
        started_at_ms: Some(10),
    }
}

fn call(source_call_id: &str, tool_name: &str) -> CallIdentity {
    CallIdentity {
        source_call_id: source_call_id.to_owned(),
        tool_name: tool_name.to_owned(),
        provider: Some("openai".to_owned()),
        model: Some("gpt".to_owned()),
        working_directory: Some("/tmp".to_owned()),
        started_at_ms: 20,
        requires_streams: matches!(tool_name, "bash" | "exec_command"),
    }
}

#[test]
fn reports_help_and_version() {
    let help = yarp(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("yarp rewrite"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("yarp archive verify"));

    let version = yarp(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("yarp 0.1.0"));
}

#[test]
fn exposes_the_bundled_yarp_skill_through_skillflag() {
    let listed = yarp(&["--skill", "list"]);
    assert!(listed.status.success());
    let list_text = String::from_utf8_lossy(&listed.stdout);
    assert!(list_text.starts_with("yarp\t"));
    assert!(!list_text.contains("skillflag\t"));

    let listed_json = yarp(&["--skill", "list", "--json"]);
    assert!(listed_json.status.success());
    let payload: serde_json::Value =
        serde_json::from_slice(&listed_json.stdout).expect("skill list JSON");
    let skills = payload["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["id"], "yarp");
    assert_eq!(skills[0]["files"], 1);
    assert!(
        skills[0]["digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );

    let shown = yarp(&["--skill", "show", "yarp"]);
    assert!(shown.status.success());
    assert_eq!(shown.stdout, include_bytes!("../skills/yarp/SKILL.md"));

    let first_export = yarp(&["--skill", "export", "yarp"]);
    let second_export = yarp(&["--skill", "export", "yarp"]);
    assert!(first_export.status.success());
    assert_eq!(first_export.stdout, second_export.stdout);
    assert!(
        first_export
            .stdout
            .windows(b"yarp/SKILL.md".len())
            .any(|window| window == b"yarp/SKILL.md")
    );
}

#[test]
fn skillflag_only_intercepts_the_top_level_skill_flag() {
    let output = yarp(&["rewrite", "printf --skill list"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
}

#[test]
fn rewrite_has_clear_success_and_passthrough_statuses() {
    let rewritten = yarp(&["rewrite", "cargo test --workspace"]);
    assert!(rewritten.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rewritten.stdout),
        format!(
            "yarp run --selected-pack 'yarp-builtins' --selected-rule 'rust/cargo-test' --selected-digest '{}' -- cargo test --workspace",
            yarp_cli::rules::digest_hex(&yarp_cli::rules::BUILTIN_SOURCE_DIGEST)
        )
    );

    let archived = yarp(&[
        "rewrite",
        "--archive-agent",
        "pi",
        "--archive-account",
        "test",
        "--archive-session",
        "session-1",
        "--archive-call",
        "call-1",
        "git status",
    ]);
    assert!(archived.status.success());
    assert!(String::from_utf8_lossy(&archived.stdout).contains("--archive-call 'call-1'"));

    let passthrough = yarp(&["rewrite", "cat .env"]);
    assert_eq!(passthrough.status.code(), Some(3));
    assert!(passthrough.stdout.is_empty());
}

#[test]
fn rejects_invalid_cli_and_disallowed_direct_execution() {
    let invalid = yarp(&["rewrite"]);
    assert_eq!(invalid.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("rewrite requires"));

    let disallowed = yarp(&["run", "--", "printf", "secret"]);
    assert_eq!(disallowed.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&disallowed.stderr).contains("not on the YARP allowlist"));
}

#[test]
fn runs_an_allowlisted_command() {
    let output = yarp(&["run", "--", "git", "status", "--short"]);
    assert!(output.status.success());
}

#[test]
fn direct_run_does_not_require_a_temporary_directory() {
    let output = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(["run", "--", "git", "status", "--short"])
        .env("TMPDIR", "/definitely/not/a/directory")
        .output()
        .expect("run without temp directory");
    assert!(output.status.success());
}

#[test]
fn preserves_child_failure_exit_code() {
    let directory = TempDir::new().expect("temp directory");
    let left = directory.path().join("left");
    let right = directory.path().join("right");
    std::fs::write(&left, "left\n").expect("write left");
    std::fs::write(&right, "right\n").expect("write right");

    let output = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(["run", "--", "git", "diff", "--no-index"])
        .arg(&left)
        .arg(&right)
        .output()
        .expect("run yarp git diff");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("-left"));
}

#[test]
fn archives_raw_and_pruned_shell_streams() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("archive/tool-calls.sqlite3");
    let left = directory.path().join("left");
    let right = directory.path().join("right");
    let left_text = numbered_lines("left", 260);
    let right_text = numbered_lines("right", 260);
    std::fs::write(&left, left_text).expect("write left");
    std::fs::write(&right, right_text).expect("write right");

    let mut archive = Archive::open_path(database.clone()).expect("archive");
    archive
        .begin_call(
            &session(),
            &call("call-shell", "exec_command"),
            &json!({}),
            &json!({}),
            20,
        )
        .expect("begin call");
    drop(archive);

    let output = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args([
            "run",
            "--archive-agent",
            "pi",
            "--archive-account",
            "test",
            "--archive-session",
            "session-1",
            "--archive-call",
            "call-shell",
            "--",
            "git",
            "diff",
            "--no-index",
        ])
        .arg(&left)
        .arg(&right)
        .env("YARP_ARCHIVE_PATH", &database)
        .output()
        .expect("archived diff");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("[yarp: omitted"));

    let restored = yarp_with_archive(
        &[
            "archive",
            "restore",
            "--archive-agent",
            "pi",
            "--archive-account",
            "test",
            "--archive-session",
            "session-1",
            "--archive-call",
            "call-shell",
        ],
        &database,
    );
    assert!(restored.status.success());
    assert!(restored.stdout.len() > output.stdout.len());
    assert!(!String::from_utf8_lossy(&restored.stdout).contains("[yarp: omitted"));

    let mut archive = Archive::open_path(database.clone()).expect("reopen");
    archive
        .result_before(&session(), "call-shell", &json!({"exitCode": 1}), None, 35)
        .expect("result before");
    archive
        .finish_call(
            &session(),
            "call-shell",
            &json!({"exitCode": 1}),
            false,
            true,
            40,
        )
        .expect("finish");
    assert!(archive.verify().expect("verify").errors.is_empty());
    drop(archive);

    let connection = Connection::open(database).expect("sqlite");
    let stream_snapshots: i64 = connection
        .query_row(
            "SELECT count(*) FROM snapshots WHERE subject IN ('stdout', 'stderr')",
            [],
            |row| row.get(0),
        )
        .expect("count streams");
    assert_eq!(stream_snapshots, 4);
    let distinct_stdout: i64 = connection
        .query_row(
            "SELECT count(DISTINCT hex(payload_sha256)) FROM snapshots WHERE subject = 'stdout'",
            [],
            |row| row.get(0),
        )
        .expect("stdout hashes");
    assert_eq!(distinct_stdout, 2);
}

#[test]
fn archive_search_help_is_model_sized_and_stable() {
    let output = yarp(&["search", "--help"]);
    assert!(output.status.success());
    assert!(output.stdout.len() <= 2 * 1_024);
    let text = String::from_utf8(output.stdout).expect("help UTF-8");
    assert!(text.contains("yarp search REF PATTERN [options]"));
    assert!(text.contains("yarp read REF stdout 118:130"));
    assert!(text.contains("-F/--fixed-strings"));
    assert!(text.contains("-m/--max-results"));
}

#[test]
fn searches_and_reads_verified_archived_output_by_opaque_reference() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("archive/tool-calls.sqlite3");
    let mut archive = Archive::open_path(database.clone()).expect("archive");
    let archive_ref = archive
        .begin_call(
            &session(),
            &call("call-query", "exec_command"),
            &json!({"cmd": "cargo test"}),
            &json!({"cmd": "cargo test"}),
            20,
        )
        .expect("begin call");
    archive
        .result_text(
            &session(),
            "call-query",
            "alpha\n\x1b[31mERROR failure\x1b[0m\nomega\n",
            yarp_cli::archive::SourceCompleteness::Incomplete,
            30,
        )
        .expect("result text");
    drop(archive);

    let searched = yarp_with_archive(
        &[
            "search",
            &archive_ref,
            "error",
            "-i",
            "-C",
            "1",
            "--max-results",
            "1",
        ],
        &database,
    );
    assert!(searched.status.success());
    let search_text = String::from_utf8(searched.stdout).expect("search UTF-8");
    assert!(search_text.contains("source=result_text complete=false"));
    assert!(search_text.contains("max_results=1"));
    assert!(search_text.contains("result_text:2:ERROR failure"));
    assert!(search_text.contains(&format!("yarp read {archive_ref} result_text 1:3")));
    assert!(!search_text.contains("\x1b[31m"));

    let read = yarp_with_archive(&["read", &archive_ref, "result_text", "1:3"], &database);
    assert!(read.status.success());
    assert_eq!(read.stdout, b"alpha\n\x1b[31mERROR failure\x1b[0m\nomega\n");

    let no_match = yarp_with_archive(&["search", &archive_ref, "absent"], &database);
    assert_eq!(no_match.status.code(), Some(1));
    assert_eq!(no_match.stdout, b"No matches\n");

    let invalid = yarp_with_archive(&["search", "not-a-ref", "error"], &database);
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
}

#[test]
fn rewrite_disagreement_archives_and_emits_exact_passthrough_streams() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("archive/tool-calls.sqlite3");
    let left = directory.path().join("left");
    let right = directory.path().join("right");
    let left_text = numbered_lines("left", 260);
    let right_text = numbered_lines("right", 260);
    std::fs::write(&left, &left_text).expect("write left");
    std::fs::write(&right, &right_text).expect("write right");

    let mut archive = Archive::open_path(database.clone()).expect("archive");
    archive
        .begin_call(
            &session(),
            &call("call-passthrough", "exec_command"),
            &json!({}),
            &json!({}),
            20,
        )
        .expect("begin call");
    drop(archive);

    let selected_digest = yarp_cli::rules::digest_hex(&yarp_cli::rules::BUILTIN_SOURCE_DIGEST);
    let output = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args([
            "run",
            "--selected-pack",
            "yarp-builtins",
            "--selected-rule",
            "different-rule",
            "--selected-digest",
        ])
        .arg(selected_digest)
        .args([
            "--archive-agent",
            "pi",
            "--archive-account",
            "test",
            "--archive-session",
            "session-1",
            "--archive-call",
            "call-passthrough",
            "--",
            "git",
            "diff",
            "--no-index",
        ])
        .arg(&left)
        .arg(&right)
        .env("YARP_ARCHIVE_PATH", &database)
        .output()
        .expect("passthrough diff");
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("[yarp:"));

    let connection = Connection::open(database).expect("sqlite");
    let snapshots: i64 = connection
        .query_row(
            "SELECT count(*) FROM snapshots WHERE subject = 'stdout'",
            [],
            |row| row.get(0),
        )
        .expect("stdout snapshots");
    let distinct: i64 = connection
        .query_row(
            "SELECT count(DISTINCT hex(payload_sha256)) FROM snapshots WHERE subject = 'stdout'",
            [],
            |row| row.get(0),
        )
        .expect("stdout hashes");
    assert_eq!(snapshots, 2);
    assert_eq!(distinct, 1);
}

#[test]
fn archive_failure_returns_the_unpruned_stream() {
    let directory = TempDir::new().expect("temp directory");
    let invalid_database = directory.path().join("database-directory");
    std::fs::create_dir(&invalid_database).expect("create invalid database path");
    let left = directory.path().join("left");
    let right = directory.path().join("right");
    std::fs::write(&left, numbered_lines("left", 260)).expect("write left");
    std::fs::write(&right, numbered_lines("right", 260)).expect("write right");

    let output = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args([
            "run",
            "--archive-agent",
            "pi",
            "--archive-account",
            "test",
            "--archive-session",
            "session-1",
            "--archive-call",
            "call-failure",
            "--",
            "git",
            "diff",
            "--no-index",
        ])
        .arg(&left)
        .arg(&right)
        .env("YARP_ARCHIVE_PATH", &invalid_database)
        .output()
        .expect("failed archive diff");
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("[yarp: omitted"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("archive failed"));
}

#[test]
fn archive_commands_report_verify_and_prune() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("yarp/tool-calls.sqlite3");
    let mut archive = Archive::open_path(database.clone()).expect("archive");
    archive
        .begin_call(
            &session(),
            &call("call-cli", "read"),
            &json!({}),
            &json!({}),
            20,
        )
        .expect("begin");
    archive
        .finish_call(
            &session(),
            "call-cli",
            &json!({"ok": true}),
            false,
            false,
            40,
        )
        .expect("finish");
    drop(archive);

    let stats = yarp_with_archive(&["archive", "stats"], &database);
    assert!(stats.status.success());
    assert!(String::from_utf8_lossy(&stats.stdout).contains("calls: 1"));

    let verify = yarp_with_archive(&["archive", "verify"], &database);
    assert!(verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("archive: ok"));

    let invalid = yarp_with_archive(&["archive", "prune", "--before", "not-a-time"], &database);
    assert_eq!(invalid.status.code(), Some(64));

    let offset = yarp_with_archive(
        &["archive", "prune", "--before", "1970-01-01T00:00:00+01:00"],
        &database,
    );
    assert_eq!(offset.status.code(), Some(64));

    let prune = yarp_with_archive(
        &["archive", "prune", "--before", "1970-01-01T00:00:00.050Z"],
        &database,
    );
    assert!(prune.status.success());
    assert!(String::from_utf8_lossy(&prune.stdout).contains("pruned_calls: 1"));
}

#[cfg(unix)]
#[test]
fn verify_reports_insecure_permissions_without_repairing_them() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = TempDir::new().expect("temp directory");
    let archive_directory = directory.path().join("yarp");
    let database = archive_directory.join("tool-calls.sqlite3");
    drop(Archive::open_path(database.clone()).expect("archive"));
    std::fs::set_permissions(&archive_directory, std::fs::Permissions::from_mode(0o755))
        .expect("directory permissions");
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644))
        .expect("database permissions");

    let verify = yarp_with_archive(&["archive", "verify"], &database);
    assert_eq!(verify.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&verify.stderr).contains("expected 700"));
    assert_eq!(
        std::fs::metadata(&archive_directory)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o755,
    );
    assert_eq!(
        std::fs::metadata(&database)
            .expect("database metadata")
            .permissions()
            .mode()
            & 0o777,
        0o644,
    );
}

#[test]
fn ingest_cli_commits_and_acknowledges_a_call() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("yarp/tool-calls.sqlite3");
    let mut child = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(["archive", "ingest"])
        .env("YARP_ARCHIVE_PATH", &database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ingest");
    let request = json!({
        "operation": "begin_call",
        "requestId": 9,
        "schemaVersion": 1,
        "session": session(),
        "call": call("call-ingest", "read"),
        "inputBefore": {},
        "inputAfter": {},
        "capturedAtMs": 20
    });
    let body = serde_json::to_vec(&request).expect("request");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(&(body.len() as u64).to_be_bytes())
        .expect("length");
    stdin.write_all(&body).expect("body");
    stdin.flush().expect("flush");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut ack = String::new();
    stdout.read_line(&mut ack).expect("ack");
    assert!(ack.contains("\"requestId\":9"));
    drop(stdin);
    assert!(child.wait().expect("wait").success());

    let archive = Archive::open_path(database).expect("reopen");
    assert_eq!(archive.stats().expect("stats").incomplete_calls, 1);
}

#[test]
fn killed_ingest_process_leaves_an_integral_incomplete_call() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("yarp/tool-calls.sqlite3");
    let mut child = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(["archive", "ingest"])
        .env("YARP_ARCHIVE_PATH", &database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ingest");
    let request = json!({
        "operation": "begin_call",
        "requestId": 10,
        "schemaVersion": 1,
        "session": session(),
        "call": call("call-crash", "read"),
        "inputBefore": {},
        "inputAfter": {},
        "capturedAtMs": 20
    });
    let body = serde_json::to_vec(&request).expect("request");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(&(body.len() as u64).to_be_bytes())
        .expect("length");
    stdin.write_all(&body).expect("body");
    stdin.flush().expect("flush");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut ack = String::new();
    stdout.read_line(&mut ack).expect("ack");
    assert!(ack.contains("\"ok\":true"));
    child.kill().expect("kill");
    child.wait().expect("wait");

    let archive = Archive::open_path(database).expect("reopen");
    let report = archive.verify().expect("verify");
    assert!(report.errors.is_empty());
    assert_eq!(report.incomplete_calls, 1);
}

#[test]
fn killed_ingest_process_preserves_a_committed_pre_result() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("yarp/tool-calls.sqlite3");
    let mut child = Command::new(env!("CARGO_BIN_EXE_yarp"))
        .args(["archive", "ingest"])
        .env("YARP_ARCHIVE_PATH", &database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ingest");
    let operations = [
        json!({
            "operation": "begin_call",
            "requestId": 20,
            "schemaVersion": 1,
            "session": session(),
            "call": call("call-result-crash", "read"),
            "inputBefore": {},
            "inputAfter": {},
            "capturedAtMs": 20
        }),
        json!({
            "operation": "result_before",
            "requestId": 21,
            "schemaVersion": 1,
            "session": session(),
            "sourceCallId": "call-result-crash",
            "result": {"content": "before"},
            "capturedAtMs": 30
        }),
    ];
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    for operation in operations {
        let body = serde_json::to_vec(&operation).expect("request");
        stdin
            .write_all(&(body.len() as u64).to_be_bytes())
            .expect("length");
        stdin.write_all(&body).expect("body");
        stdin.flush().expect("flush");
        let mut ack = String::new();
        stdout.read_line(&mut ack).expect("ack");
        assert!(ack.contains("\"ok\":true"));
    }
    child.kill().expect("kill");
    child.wait().expect("wait");

    let archive = Archive::open_path(database.clone()).expect("reopen");
    let report = archive.verify().expect("verify");
    assert!(report.errors.is_empty());
    assert_eq!(report.incomplete_calls, 1);
    drop(archive);
    let connection = Connection::open(database).expect("sqlite");
    let pre_results: i64 = connection
        .query_row(
            "SELECT count(*) FROM snapshots WHERE subject = 'result' AND stage = 'before'",
            [],
            |row| row.get(0),
        )
        .expect("pre-result count");
    assert_eq!(pre_results, 1);
}

fn numbered_lines(prefix: &str, count: usize) -> String {
    let mut output = String::new();
    for line in 0..count {
        writeln!(&mut output, "{prefix}-{line}").expect("write line");
    }
    output
}
