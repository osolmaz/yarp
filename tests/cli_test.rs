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
fn rewrite_has_clear_success_and_passthrough_statuses() {
    let rewritten = yarp(&["rewrite", "cargo test --workspace"]);
    assert!(rewritten.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rewritten.stdout),
        "yarp run -- cargo test --workspace"
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
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid arguments"));

    let disallowed = yarp(&["run", "--", "cat", ".env"]);
    assert_eq!(disallowed.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&disallowed.stderr).contains("not on the YARP allowlist"));
}

#[test]
fn runs_an_allowlisted_command() {
    let output = yarp(&["run", "--", "git", "status", "--short"]);
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

    let mut archive = Archive::open_path(database.clone()).expect("reopen");
    archive
        .finish_call(&session(), "call-shell", &json!({"exitCode": 1}), false, 40)
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
        .finish_call(&session(), "call-cli", &json!({"ok": true}), false, 40)
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

fn numbered_lines(prefix: &str, count: usize) -> String {
    let mut output = String::new();
    for line in 0..count {
        writeln!(&mut output, "{prefix}-{line}").expect("write line");
    }
    output
}
