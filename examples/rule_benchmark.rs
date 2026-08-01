use std::fs;
use std::hint::black_box;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use yarp_cli::reducers::{StreamReducer, configured_memory_bound};
use yarp_cli::rules::{PackRequest, Registry, Selection};
use yarp_rule_pack::{SourcePack, compile};

const EXTERNAL_RULES: usize = 1_000;
const MATCH_SAMPLES: usize = 200;
const BUILTIN_TARGET_US: u128 = 100;
const EXTERNAL_TARGET_US: u128 = 5_000;

fn main() -> Result<(), String> {
    let builtin = benchmark_builtin()?;
    let (external_cold, external, pack_bytes) = benchmark_external()?;
    let (throughput, memory_bound) = benchmark_stream()?;
    println!("built_in_match_p95_us: {}", builtin.as_micros());
    println!(
        "external_1000_first_open_match_us: {}",
        external_cold.as_micros()
    );
    println!("external_1000_open_match_p95_us: {}", external.as_micros());
    println!("external_pack_bytes: {pack_bytes}");
    println!("stream_megabytes_per_second: {throughput:.2}");
    println!("configured_stream_memory_bound_bytes: {memory_bound}");
    if builtin.as_micros() > BUILTIN_TARGET_US {
        return Err(format!(
            "built-in p95 exceeded {BUILTIN_TARGET_US} microseconds"
        ));
    }
    if external.as_micros() > EXTERNAL_TARGET_US {
        return Err(format!(
            "external pack p95 exceeded {EXTERNAL_TARGET_US} microseconds"
        ));
    }
    Ok(())
}

fn benchmark_builtin() -> Result<Duration, String> {
    let arguments = vec![
        "cargo".to_owned(),
        "test".to_owned(),
        "--workspace".to_owned(),
    ];
    let mut samples = Vec::with_capacity(MATCH_SAMPLES);
    for _ in 0..MATCH_SAMPLES {
        let mut registry = Registry::builtins_only();
        let started = Instant::now();
        let selection = registry.select(black_box(&arguments))?;
        if !matches!(selection, Selection::Reduce(_)) {
            return Err("built-in benchmark command was not selected".to_owned());
        }
        samples.push(started.elapsed());
    }
    Ok(p95(samples))
}

fn benchmark_external() -> Result<(Duration, Duration, usize), String> {
    let directory = TempDir::new().map_err(|error| error.to_string())?;
    let source_root = directory.path().join("source");
    let rules_root = source_root.join("rules");
    fs::create_dir_all(&rules_root).map_err(|error| error.to_string())?;
    let mut paths = Vec::with_capacity(EXTERNAL_RULES);
    for index in 0..EXTERNAL_RULES {
        let relative = format!("rules/rule-{index:04}.json");
        let id = format!("bench/rule-{index:04}");
        let program = format!("bench{index:04}");
        let body = format!(
            "{{\"id\":{id:?},\"match\":{{\"program\":[{program:?}]}},\"action\":\"reduce\",\"reducer\":{{\"kind\":\"head_tail\"}},\"success\":{{\"head_lines\":10,\"tail_lines\":10,\"max_line_bytes\":16384,\"max_output_bytes\":32768,\"min_savings_bytes\":120}},\"failure\":{{\"head_lines\":20,\"tail_lines\":20,\"max_line_bytes\":16384,\"max_output_bytes\":65536,\"min_savings_bytes\":120}}}}"
        );
        fs::write(source_root.join(&relative), body).map_err(|error| error.to_string())?;
        paths.push(relative);
    }
    let manifest = serde_json::json!({
        "schema_version": 1,
        "id": "benchmark-pack",
        "rules": paths,
    });
    fs::write(
        source_root.join("pack.json"),
        serde_json::to_vec(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let source = SourcePack::load(&source_root)?;
    let body = compile(&source)?;
    let pack_path = directory.path().join("benchmark.yrp");
    fs::write(&pack_path, &body).map_err(|error| error.to_string())?;
    let arguments = vec!["bench0999".to_owned()];
    let request = PackRequest {
        path: pack_path,
        expected_digest: Some(source.source_digest),
    };
    let cold_started = Instant::now();
    let mut cold_registry = Registry::load(std::slice::from_ref(&request))?;
    if !matches!(
        cold_registry.select(black_box(&arguments))?,
        Selection::Reduce(_)
    ) {
        return Err("external benchmark command was not selected".to_owned());
    }
    let cold = cold_started.elapsed();
    let mut samples = Vec::with_capacity(MATCH_SAMPLES);
    for _ in 0..MATCH_SAMPLES {
        let started = Instant::now();
        let mut registry = Registry::load(std::slice::from_ref(&request))?;
        let selection = registry.select(black_box(&arguments))?;
        if !matches!(selection, Selection::Reduce(_)) {
            return Err("external benchmark command was not selected".to_owned());
        }
        samples.push(started.elapsed());
    }
    Ok((cold, p95(samples), body.len()))
}

fn benchmark_stream() -> Result<(f64, usize), String> {
    let mut registry = Registry::builtins_only();
    let Selection::Reduce(selected) = registry.select(&["rg".to_owned(), "needle".to_owned()])?
    else {
        return Err("stream benchmark rule was not selected".to_owned());
    };
    let chunk = b"src/file.rs:42:matching line with useful context\n".repeat(160);
    let bytes = 1_024 * 1_024 * 1_024;
    let started = Instant::now();
    let mut reducer = StreamReducer::new(&selected.rule)?;
    let mut processed = 0;
    while processed < bytes {
        reducer.push(black_box(&chunk));
        processed += chunk.len();
    }
    black_box(reducer.finish(true));
    let processed = u32::try_from(processed)
        .map_err(|_| "stream benchmark byte count does not fit u32".to_owned())?;
    let throughput = f64::from(processed) / 1_000_000.0 / started.elapsed().as_secs_f64();
    Ok((throughput, configured_memory_bound(&selected.rule)?))
}

fn p95(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    let index = samples
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    samples[index]
}
