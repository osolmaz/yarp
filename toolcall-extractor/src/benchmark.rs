use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use yarp_cli::rules::{Reducer, Rule};

use crate::database::Database;
use crate::error::Result;

#[derive(Clone, Debug, Default, Serialize)]
pub struct PruningMetrics {
    pub results: u64,
    pub affected_results: u64,
    pub original_characters: u64,
    pub pruned_characters: u64,
    pub removed_characters: u64,
    pub original_bytes: u64,
    pub pruned_bytes: u64,
    pub removed_bytes: u64,
    pub original_lines: u64,
    pub pruned_lines: u64,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    pub evaluated_results: u64,
    pub evaluated_output_characters: u64,
    pub shell_results: u64,
    pub shell_output_characters: u64,
    pub eligible_results: u64,
    pub passthrough_results: u64,
    pub ambiguous_results: u64,
    pub unsupported_results: u64,
    pub unknown_status_results: u64,
    pub affected_results: u64,
    pub affected_percent_of_eligible: f64,
    pub eligible_original_characters: u64,
    pub eligible_pruned_characters: u64,
    pub removed_characters: u64,
    pub removed_percent_of_eligible: f64,
    pub removed_percent_of_shell_output: f64,
    pub removed_percent_of_all_output: f64,
    pub diagnostic_vetoes: u64,
    pub diagnostic_vetoes_by_term: BTreeMap<String, u64>,
    pub eligible_original_bytes: u64,
    pub eligible_pruned_bytes: u64,
    pub removed_bytes: u64,
    pub original_lines: u64,
    pub pruned_lines: u64,
    pub removed_lines: u64,
    pub elapsed_ms: u128,
    pub processed_megabytes_per_second: f64,
    pub by_agent: BTreeMap<String, PruningMetrics>,
    pub by_tool: BTreeMap<String, PruningMetrics>,
    pub by_rule: BTreeMap<String, PruningMetrics>,
    pub by_reducer: BTreeMap<String, PruningMetrics>,
    pub by_status: BTreeMap<String, PruningMetrics>,
    pub by_size_band: BTreeMap<String, PruningMetrics>,
}

pub fn run(path: &Path) -> Result<BenchmarkReport> {
    let connection = Database::open_read_only(path)?;
    let mut statement = connection.prepare(
        "SELECT s.agent, c.tool_name, c.input_format, c.input_text, r.output_text, r.is_error,
                r.output_json
         FROM tool_calls c
         JOIN sessions s ON s.session_key = c.session_key
         JOIN tool_results r ON r.call_key = c.call_key
         WHERE r.output_text IS NOT NULL",
    )?;
    let started = Instant::now();
    let mut rows = statement.query([])?;
    let mut evaluated_results = 0_u64;
    let mut evaluated_output_characters = 0_u64;
    let mut shell_results = 0_u64;
    let mut shell_output_characters = 0_u64;
    let mut diagnostic_vetoes = 0_u64;
    let mut diagnostic_vetoes_by_term = BTreeMap::<String, u64>::new();
    let mut passthrough_results = 0_u64;
    let mut ambiguous_results = 0_u64;
    let mut unsupported_results = 0_u64;
    let mut unknown_status_results = 0_u64;
    let mut total = PruningMetrics::default();
    let mut by_agent = BTreeMap::<String, PruningMetrics>::new();
    let mut by_tool = BTreeMap::<String, PruningMetrics>::new();
    let mut by_rule = BTreeMap::<String, PruningMetrics>::new();
    let mut by_reducer = BTreeMap::<String, PruningMetrics>::new();
    let mut by_status = BTreeMap::<String, PruningMetrics>::new();
    let mut by_size_band = BTreeMap::<String, PruningMetrics>::new();
    while let Some(row) = rows.next()? {
        let agent: String = row.get(0)?;
        let tool: String = row.get(1)?;
        let input_format: String = row.get(2)?;
        let input: String = row.get(3)?;
        let output: String = row.get(4)?;
        let is_error: Option<bool> = row.get(5)?;
        let output_json: Option<String> = row.get(6)?;
        evaluated_results = evaluated_results.saturating_add(1);
        evaluated_output_characters = evaluated_output_characters
            .saturating_add(u64::try_from(output.chars().count()).unwrap_or(u64::MAX));
        let Some(command) = shell_command(&tool, &input_format, &input) else {
            continue;
        };
        shell_results = shell_results.saturating_add(1);
        shell_output_characters = shell_output_characters
            .saturating_add(u64::try_from(output.chars().count()).unwrap_or(u64::MAX));
        let (rule, rule_label) = match benchmark_selection(&command) {
            BenchmarkSelection::Reduce { rule, label } => (rule, label),
            BenchmarkSelection::Passthrough => {
                passthrough_results = passthrough_results.saturating_add(1);
                continue;
            }
            BenchmarkSelection::Ambiguous => {
                ambiguous_results = ambiguous_results.saturating_add(1);
                continue;
            }
            BenchmarkSelection::Unsupported => {
                unsupported_results = unsupported_results.saturating_add(1);
                continue;
            }
        };
        let Some(reducer) = rule.reducer.as_ref() else {
            unsupported_results = unsupported_results.saturating_add(1);
            continue;
        };
        let reducer_label = reducer_name(reducer);
        let succeeded = result_succeeded(is_error, output_json.as_deref());
        if succeeded.is_none() {
            unknown_status_results = unknown_status_results.saturating_add(1);
        }
        let pruned = yarp_cli::reducers::reduce_bytes_with_recovery(
            &rule,
            output.as_bytes(),
            succeeded.unwrap_or(false),
            Some(yarp_cli::reducers::RecoveryMarker {
                archive_ref: "yr_0123456789abcdef0123456789abcdef",
                source: "result_text",
                completeness: "unknown",
            }),
        )
        .unwrap_or_else(|_| output.as_bytes().to_vec());
        let metrics = measure(&output, &pruned);
        let lost_diagnostics = lost_registered_diagnostics(&output, &pruned);
        diagnostic_vetoes =
            diagnostic_vetoes.saturating_add(u64::from(!lost_diagnostics.is_empty()));
        for term in lost_diagnostics {
            let count = diagnostic_vetoes_by_term
                .entry(term.to_owned())
                .or_default();
            *count = count.saturating_add(1);
        }
        total.add(&metrics);
        by_agent.entry(agent).or_default().add(&metrics);
        by_tool.entry(tool).or_default().add(&metrics);
        by_rule.entry(rule_label).or_default().add(&metrics);
        by_reducer.entry(reducer_label).or_default().add(&metrics);
        by_status
            .entry(
                if succeeded == Some(true) {
                    "success"
                } else if succeeded == Some(false) {
                    "failure"
                } else {
                    "unknown"
                }
                .to_owned(),
            )
            .or_default()
            .add(&metrics);
        by_size_band
            .entry(size_band(output.len()).to_owned())
            .or_default()
            .add(&metrics);
    }
    let elapsed = started.elapsed();
    let seconds = elapsed.as_secs_f64();
    Ok(BenchmarkReport {
        evaluated_results,
        evaluated_output_characters,
        shell_results,
        shell_output_characters,
        eligible_results: total.results,
        passthrough_results,
        ambiguous_results,
        unsupported_results,
        unknown_status_results,
        affected_results: total.affected_results,
        affected_percent_of_eligible: percent(total.affected_results, total.results),
        eligible_original_characters: total.original_characters,
        eligible_pruned_characters: total.pruned_characters,
        removed_characters: total.removed_characters,
        removed_percent_of_eligible: percent(total.removed_characters, total.original_characters),
        removed_percent_of_shell_output: percent(total.removed_characters, shell_output_characters),
        removed_percent_of_all_output: percent(
            total.removed_characters,
            evaluated_output_characters,
        ),
        eligible_original_bytes: total.original_bytes,
        eligible_pruned_bytes: total.pruned_bytes,
        removed_bytes: total.removed_bytes,
        diagnostic_vetoes,
        diagnostic_vetoes_by_term,
        original_lines: total.original_lines,
        pruned_lines: total.pruned_lines,
        removed_lines: total.original_lines.saturating_sub(total.pruned_lines),
        elapsed_ms: elapsed.as_millis(),
        processed_megabytes_per_second: if seconds > 0.0 {
            total.original_bytes as f64 / 1_000_000.0 / seconds
        } else {
            0.0
        },
        by_agent,
        by_tool,
        by_rule,
        by_reducer,
        by_status,
        by_size_band,
    })
}

enum BenchmarkSelection {
    Reduce { rule: Box<Rule>, label: String },
    Passthrough,
    Ambiguous,
    Unsupported,
}

fn benchmark_selection(command: &str) -> BenchmarkSelection {
    match yarp_cli::rewrite::select_builtin_command(command) {
        Ok((_, yarp_cli::rules::Selection::Reduce(selected))) => BenchmarkSelection::Reduce {
            label: format!("{}/{}", selected.pack_id, selected.rule.id),
            rule: Box::new((*selected.rule).clone()),
        },
        Ok((_, yarp_cli::rules::Selection::Passthrough(_))) => BenchmarkSelection::Passthrough,
        Ok((_, yarp_cli::rules::Selection::Ambiguous(_))) => BenchmarkSelection::Ambiguous,
        Ok((_, yarp_cli::rules::Selection::Unsupported)) | Err(_) => {
            match yarp_cli::rewrite::select_result_rule(command) {
                Ok(rule) => {
                    let Some(reducer) = rule.reducer.as_ref() else {
                        return BenchmarkSelection::Unsupported;
                    };
                    BenchmarkSelection::Reduce {
                        label: format!("compound/{}", reducer_name(reducer)),
                        rule: Box::new(rule),
                    }
                }
                Err(_) => BenchmarkSelection::Unsupported,
            }
        }
    }
}

fn reducer_name(reducer: &Reducer) -> String {
    match reducer {
        Reducer::SearchSummary => "search_summary",
        Reducer::DiffSummary => "diff_summary",
        Reducer::TestSummary => "test_summary",
        Reducer::BuildSummary => "build_summary",
        Reducer::LogSummary => "log_summary",
        Reducer::StatusSummary => "status_summary",
        Reducer::ListSummary => "list_summary",
        Reducer::LineFilter { .. } => "line_filter",
    }
    .to_owned()
}

fn size_band(bytes: usize) -> &'static str {
    match bytes {
        0..1_000 => "000000-000999",
        1_000..10_000 => "001000-009999",
        10_000..100_000 => "010000-099999",
        _ => "100000+",
    }
}

fn lost_registered_diagnostics(original: &str, pruned: &[u8]) -> Vec<&'static str> {
    if original.as_bytes() == pruned {
        return Vec::new();
    }
    let original = original.to_ascii_lowercase();
    let pruned = String::from_utf8_lossy(pruned).to_ascii_lowercase();
    ["failure", "panic", "error", "warning", "test result"]
        .into_iter()
        .filter(|term| original.contains(term) && !pruned.contains(term))
        .collect()
}

impl PruningMetrics {
    fn add(&mut self, other: &Self) {
        self.results = self.results.saturating_add(other.results);
        self.affected_results = self.affected_results.saturating_add(other.affected_results);
        self.original_characters = self
            .original_characters
            .saturating_add(other.original_characters);
        self.pruned_characters = self
            .pruned_characters
            .saturating_add(other.pruned_characters);
        self.removed_characters = self
            .removed_characters
            .saturating_add(other.removed_characters);
        self.original_bytes = self.original_bytes.saturating_add(other.original_bytes);
        self.pruned_bytes = self.pruned_bytes.saturating_add(other.pruned_bytes);
        self.removed_bytes = self.removed_bytes.saturating_add(other.removed_bytes);
        self.original_lines = self.original_lines.saturating_add(other.original_lines);
        self.pruned_lines = self.pruned_lines.saturating_add(other.pruned_lines);
    }
}

fn measure(output: &str, pruned: &[u8]) -> PruningMetrics {
    let original_characters = u64::try_from(output.chars().count()).unwrap_or(u64::MAX);
    let pruned_characters =
        u64::try_from(String::from_utf8_lossy(pruned).chars().count()).unwrap_or(u64::MAX);
    let original_bytes = u64::try_from(output.len()).unwrap_or(u64::MAX);
    let pruned_bytes = u64::try_from(pruned.len()).unwrap_or(u64::MAX);
    PruningMetrics {
        results: 1,
        affected_results: u64::from(pruned != output.as_bytes()),
        original_characters,
        pruned_characters,
        removed_characters: original_characters.saturating_sub(pruned_characters),
        original_bytes,
        pruned_bytes,
        removed_bytes: original_bytes.saturating_sub(pruned_bytes),
        original_lines: line_count(output.as_bytes()),
        pruned_lines: line_count(pruned),
    }
}

fn result_succeeded(is_error: Option<bool>, output_json: Option<&str>) -> Option<bool> {
    if let Some(value) = output_json.and_then(|value| serde_json::from_str::<Value>(value).ok()) {
        let exit_code = ["exit_code", "exitCode"]
            .into_iter()
            .find_map(|key| value.get(key).and_then(Value::as_i64))
            .or_else(|| {
                value.get("details").and_then(|details| {
                    ["exit_code", "exitCode"]
                        .into_iter()
                        .find_map(|key| details.get(key).and_then(Value::as_i64))
                })
            });
        if let Some(exit_code) = exit_code {
            return Some(exit_code == 0);
        }
        if value.get("status").and_then(Value::as_str) == Some("failed") {
            return Some(false);
        }
    }
    is_error.map(|value| !value)
}

fn shell_command(tool: &str, input_format: &str, input: &str) -> Option<String> {
    let lower = tool.to_ascii_lowercase();
    if !(lower == "bash"
        || lower == "exec_command"
        || lower.contains("shell")
        || lower == "run_terminal_cmd")
    {
        return None;
    }
    if input_format == "text" {
        return Some(input.to_owned());
    }
    let value: Value = serde_json::from_str(input).ok()?;
    value
        .get("command")
        .or_else(|| value.get("cmd"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn line_count(bytes: &[u8]) -> u64 {
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
    let unterminated = usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    u64::try_from(newlines.saturating_add(unterminated)).unwrap_or(u64::MAX)
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_supported_shell_commands() {
        assert_eq!(
            shell_command("exec_command", "json", r#"{"cmd":"cargo test"}"#).as_deref(),
            Some("cargo test")
        );
        assert_eq!(shell_command("read", "json", r#"{"path":"x"}"#), None);
    }

    #[test]
    fn prefers_explicit_exit_status() {
        assert_eq!(
            result_succeeded(Some(false), Some(r#"{"exit_code":1}"#)),
            Some(false)
        );
        assert_eq!(
            result_succeeded(None, Some(r#"{"details":{"exit_code":0}}"#)),
            Some(true)
        );
        assert_eq!(
            result_succeeded(Some(false), Some(r#"{"details":{"exitCode":2}}"#)),
            Some(false)
        );
        assert_eq!(
            result_succeeded(Some(true), Some(r#"{"exitCode":0}"#)),
            Some(true)
        );
        assert_eq!(
            result_succeeded(None, Some(r#"{"status":"failed"}"#)),
            Some(false)
        );
        assert_eq!(result_succeeded(None, None), None);
    }

    #[test]
    fn measures_removed_output() {
        let output = (0..250).map(|line| format!("{line}\n")).collect::<String>();
        let (_, selection) =
            yarp_cli::rewrite::select_builtin_command("cargo test").expect("selection");
        let yarp_cli::rules::Selection::Reduce(selected) = selection else {
            panic!("expected reduction rule");
        };
        let pruned = yarp_cli::reducers::reduce_bytes(&selected.rule, output.as_bytes(), true)
            .expect("reduction");
        let metrics = measure(&output, &pruned);
        assert_eq!(metrics.affected_results, 1);
        assert!(metrics.removed_characters > 0);
        assert_eq!(metrics.original_lines, 250);
    }
}
