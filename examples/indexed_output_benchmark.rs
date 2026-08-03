use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;
use yarp_cli::archive::{Archive, CallIdentity, SessionIdentity, SourceCompleteness};

const WARMUP: usize = 10;
const SAMPLES: usize = 100;
const TARGET: Duration = Duration::from_millis(20);
const PARSER_TARGET: Duration = Duration::from_millis(1);

fn main() -> Result<(), String> {
    let executable = release_yarp()?;
    let directory = TempDir::new().map_err(|error| error.to_string())?;
    let database = directory.path().join("archive/tool-calls.sqlite3");
    let config_home = directory.path().join("config");
    fs::create_dir_all(config_home.join("yarp")).map_err(|error| error.to_string())?;
    let database_text = serde_json::to_string(
        database
            .to_str()
            .ok_or_else(|| "benchmark archive path is not UTF-8".to_owned())?,
    )
    .map_err(|error| error.to_string())?;
    fs::write(
        config_home.join("yarp/config.toml"),
        format!("version = 1\n[archive]\npath = {database_text}\n"),
    )
    .map_err(|error| error.to_string())?;
    let mut archive = Archive::open_path(database.clone())?;
    let session = SessionIdentity {
        agent: "benchmark".to_owned(),
        account: "local".to_owned(),
        source_session_id: "indexed-output".to_owned(),
        started_at_ms: Some(1),
    };
    let call = CallIdentity {
        source_call_id: "query".to_owned(),
        tool_name: "exec_command".to_owned(),
        provider: None,
        model: None,
        working_directory: None,
        started_at_ms: 2,
        requires_streams: false,
    };
    let archive_ref = archive.begin_call(&session, &call, &json!({}), &json!({}), 2)?;
    let source = "path/to/file.rs:42: needle repeated output for bounded query\n".repeat(17_000);
    archive.result_text(&session, "query", &source, SourceCompleteness::Complete, 3)?;
    drop(archive);

    let search = benchmark_command(
        &executable,
        &config_home,
        &["search", &archive_ref, "needle", "--max-results", "20"],
    )?;
    let read = benchmark_command(
        &executable,
        &config_home,
        &["read", &archive_ref, "result_text", "1:200"],
    )?;
    let result = benchmark_result_reducer(&executable, &config_home, &archive_ref)?;
    let representative_source = "set -o pipefail && rg TODO . | sort | uniq | head -50";
    let representative_parser = benchmark_parser(representative_source, WARMUP, SAMPLES)?;
    let maximum_source = format!("rg {}", "x".repeat(256 * 1024 - 3));
    let maximum_parser = benchmark_parser(&maximum_source, 2, 10)?;
    print_stats("search_1m", &search);
    print_stats("read_12k", &read);
    print_stats("result_reducer_16k", &result);
    print_stats("parser_representative", &representative_parser);
    print_stats("parser_maximum_accepted", &maximum_parser);
    println!(
        "parser_representative_input_bytes: {}",
        representative_source.len()
    );
    println!(
        "parser_maximum_accepted_input_bytes: {}",
        maximum_source.len()
    );
    for (name, samples) in [
        ("search", &search),
        ("read", &read),
        ("result reducer", &result),
    ] {
        if percentile(samples, 95) > TARGET {
            return Err(format!("{name} p95 exceeded {} ms", TARGET.as_millis()));
        }
    }
    if percentile(&representative_parser, 95) > PARSER_TARGET {
        return Err(format!(
            "representative parser p95 exceeded {} ms",
            PARSER_TARGET.as_millis()
        ));
    }
    Ok(())
}

fn benchmark_parser(
    source: &str,
    warmup: usize,
    sample_count: usize,
) -> Result<Vec<Duration>, String> {
    let mut samples = Vec::with_capacity(sample_count);
    for iteration in 0..warmup.saturating_add(sample_count) {
        let started = Instant::now();
        std::hint::black_box(yarp_cli::rewrite::select_result_plan(std::hint::black_box(
            source,
        )))
        .map_err(|error| format!("parser benchmark command was rejected: {error}"))?;
        let elapsed = started.elapsed();
        if iteration >= warmup {
            samples.push(elapsed);
        }
    }
    Ok(samples)
}

fn release_yarp() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let release = current
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "could not find release directory".to_owned())?;
    let executable = release.join(if cfg!(windows) { "yarp.exe" } else { "yarp" });
    if !executable.is_file() {
        return Err(format!(
            "{} is missing; run cargo build --release --workspace first",
            executable.display()
        ));
    }
    Ok(executable)
}

fn benchmark_command(
    executable: &Path,
    config_home: &Path,
    arguments: &[&str],
) -> Result<Vec<Duration>, String> {
    let mut samples = Vec::with_capacity(SAMPLES);
    for iteration in 0..WARMUP + SAMPLES {
        let started = Instant::now();
        let output = Command::new(executable)
            .args(arguments)
            .env("XDG_CONFIG_HOME", config_home)
            .output()
            .map_err(|error| error.to_string())?;
        let elapsed = started.elapsed();
        if !output.status.success() {
            return Err(format!(
                "{} failed: {}",
                arguments[0],
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        if iteration >= WARMUP {
            samples.push(elapsed);
        }
    }
    Ok(samples)
}

fn benchmark_result_reducer(
    executable: &Path,
    config_home: &Path,
    archive_ref: &str,
) -> Result<Vec<Duration>, String> {
    let text = "test routine ... ok\n".repeat(800);
    let body = serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "command": "cargo test",
        "text": text,
        "isError": false,
        "exitCode": 0,
        "archiveRef": archive_ref,
        "sourceCompleteness": "unknown",
        "preferArchiveSource": false
    }))
    .map_err(|error| error.to_string())?;
    let mut request = u64::try_from(body.len())
        .map_err(|_| "request is too large".to_owned())?
        .to_be_bytes()
        .to_vec();
    request.extend_from_slice(&body);
    let mut samples = Vec::with_capacity(SAMPLES);
    for iteration in 0..WARMUP + SAMPLES {
        let started = Instant::now();
        let mut child = Command::new(executable)
            .arg("result-reduce")
            .env("XDG_CONFIG_HOME", config_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| error.to_string())?;
        child
            .stdin
            .take()
            .ok_or_else(|| "missing result reducer stdin".to_owned())?
            .write_all(&request)
            .map_err(|error| error.to_string())?;
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        let elapsed = started.elapsed();
        if !output.status.success() {
            return Err(format!(
                "result reducer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        if iteration >= WARMUP {
            samples.push(elapsed);
        }
    }
    Ok(samples)
}

fn print_stats(name: &str, samples: &[Duration]) {
    println!("{name}_median_us: {}", percentile(samples, 50).as_micros());
    println!("{name}_p95_us: {}", percentile(samples, 95).as_micros());
    println!("{name}_max_us: {}", percentile(samples, 100).as_micros());
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = sorted
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len().saturating_sub(1));
    sorted[index]
}
