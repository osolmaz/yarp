use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

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
    pub removed_percent_of_all_output: f64,
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
    let mut passthrough_results = 0_u64;
    let mut ambiguous_results = 0_u64;
    let mut unsupported_results = 0_u64;
    let mut unknown_status_results = 0_u64;
    let mut total = PruningMetrics::default();
    let mut by_agent = BTreeMap::<String, PruningMetrics>::new();
    let mut by_tool = BTreeMap::<String, PruningMetrics>::new();
    let mut by_rule = BTreeMap::<String, PruningMetrics>::new();
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
        let Ok((_, selection)) = yarp_cli::rewrite::select_builtin_command(&command) else {
            unsupported_results = unsupported_results.saturating_add(1);
            continue;
        };
        let selected = match selection {
            yarp_cli::rules::Selection::Reduce(selected) => selected,
            yarp_cli::rules::Selection::Passthrough(_) => {
                passthrough_results = passthrough_results.saturating_add(1);
                continue;
            }
            yarp_cli::rules::Selection::Ambiguous(_) => {
                ambiguous_results = ambiguous_results.saturating_add(1);
                continue;
            }
            yarp_cli::rules::Selection::Unsupported => {
                unsupported_results = unsupported_results.saturating_add(1);
                continue;
            }
        };
        let succeeded = result_succeeded(is_error, output_json.as_deref());
        if succeeded.is_none() {
            unknown_status_results = unknown_status_results.saturating_add(1);
        }
        let pruned = yarp_cli::reducers::reduce_bytes(
            &selected.rule,
            output.as_bytes(),
            succeeded.unwrap_or(false),
        )
        .unwrap_or_else(|_| output.as_bytes().to_vec());
        let metrics = measure(&output, &pruned);
        total.add(&metrics);
        by_agent.entry(agent).or_default().add(&metrics);
        by_tool.entry(tool).or_default().add(&metrics);
        by_rule
            .entry(format!("{}/{}", selected.pack_id, selected.rule.id))
            .or_default()
            .add(&metrics);
    }
    let elapsed = started.elapsed();
    let seconds = elapsed.as_secs_f64();
    Ok(BenchmarkReport {
        evaluated_results,
        evaluated_output_characters,
        shell_results,
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
        removed_percent_of_all_output: percent(
            total.removed_characters,
            evaluated_output_characters,
        ),
        eligible_original_bytes: total.original_bytes,
        eligible_pruned_bytes: total.pruned_bytes,
        removed_bytes: total.removed_bytes,
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
    })
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
    if is_error == Some(true) {
        return Some(false);
    }
    if let Some(value) = output_json.and_then(|value| serde_json::from_str::<Value>(value).ok()) {
        let exit_code = value.get("exit_code").and_then(Value::as_i64).or_else(|| {
            value
                .get("details")
                .and_then(|details| details.get("exit_code"))
                .and_then(Value::as_i64)
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
