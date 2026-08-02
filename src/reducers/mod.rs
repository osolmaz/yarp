mod bounded;
mod build;
mod classify;
mod diff;
mod evidence;
pub(crate) mod filter;
mod list;
mod log;
mod search;
mod status;
mod test;

use bounded::{LineAccumulator, LineView, ShortRaw};
pub use evidence::RecoveryMarker;
use evidence::{EvidenceClass, EvidenceCollector};
use filter::{AnsiStripper, line_filter_keeps};
use yarp_rule_pack::{Action, OutputPolicy, Reducer, Rule};

const DIAGNOSTIC_CONTEXT_LINES: usize = 6;

#[derive(Debug, Default)]
struct RegisteredDiagnosticTracker {
    matched: [bool; 5],
    window: u128,
}

impl RegisteredDiagnosticTracker {
    fn push(&mut self, byte: u8) {
        let lower = byte.to_ascii_lowercase();
        self.window = (self.window << 8) | u128::from(lower);
        match lower {
            b'e' if tail_matches(self.window, FAILURE) => self.matched[0] = true,
            b'c' if tail_matches(self.window, PANIC) => self.matched[1] = true,
            b'r' if tail_matches(self.window, ERROR) => self.matched[2] = true,
            b'g' if tail_matches(self.window, WARNING) => self.matched[3] = true,
            b't' if tail_matches(self.window, TEST_RESULT) => self.matched[4] = true,
            _ => {}
        }
    }

    fn observe_slice(&mut self, body: &[u8]) {
        for (matched, term) in self
            .matched
            .iter_mut()
            .zip(classify::REGISTERED_DIAGNOSTICS)
        {
            if !*matched && classify::contains_ascii_insensitive(body, term) {
                *matched = true;
            }
        }
    }

    fn end_line(&mut self) {
        self.window = 0;
    }
}

const FAILURE: (u128, usize) = (0x66_61_69_6c_75_72_65, 56);
const PANIC: (u128, usize) = (0x70_61_6e_69_63, 40);
const ERROR: (u128, usize) = (0x65_72_72_6f_72, 40);
const WARNING: (u128, usize) = (0x77_61_72_6e_69_6e_67, 56);
const TEST_RESULT: (u128, usize) = (0x74_65_73_74_20_72_65_73_75_6c_74, 88);

fn tail_matches(window: u128, term: (u128, usize)) -> bool {
    let mask = (1_u128 << term.1) - 1;
    window & mask == term.0
}

#[derive(Debug)]
pub struct StreamReducer {
    kind: Reducer,
    success_policy: OutputPolicy,
    failure_policy: OutputPolicy,
    success: EvidenceCollector,
    failure: EvidenceCollector,
    raw: ShortRaw,
    line: LineAccumulator,
    ansi: Option<AnsiStripper>,
    line_number: u64,
    diagnostic_context: usize,
    registered_diagnostics: RegisteredDiagnosticTracker,
}

impl StreamReducer {
    /// Build bounded state for one validated reduction rule.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied rule is not a complete reduction rule.
    pub fn new(rule: &Rule) -> Result<Self, String> {
        if rule.action != Action::Reduce {
            return Err("cannot create a reducer for a passthrough rule".to_owned());
        }
        let memory_bound = yarp_rule_pack::stream_memory_bound(rule)?;
        if memory_bound > yarp_rule_pack::MAX_STREAM_MEMORY_BYTES {
            return Err(format!(
                "rule requires {memory_bound} bytes per stream, above the {}-byte limit",
                yarp_rule_pack::MAX_STREAM_MEMORY_BYTES
            ));
        }
        let kind = rule
            .reducer
            .clone()
            .ok_or_else(|| "reduction rule is missing a reducer".to_owned())?;
        let success_policy = rule
            .success
            .ok_or_else(|| "reduction rule is missing a success policy".to_owned())?;
        let failure_policy = rule
            .failure
            .ok_or_else(|| "reduction rule is missing a failure policy".to_owned())?;
        let max_line = success_policy
            .max_line_bytes
            .max(failure_policy.max_line_bytes);
        let strip_ansi = !matches!(
            kind,
            Reducer::LineFilter {
                strip_ansi: false,
                ..
            }
        );
        let family = family_name(&kind);
        Ok(Self {
            kind,
            success_policy,
            failure_policy,
            success: EvidenceCollector::new(family, success_policy),
            failure: EvidenceCollector::new(family, failure_policy),
            raw: ShortRaw::new(success_policy, failure_policy),
            line: LineAccumulator::new(max_line, max_line),
            ansi: strip_ansi.then(AnsiStripper::new),
            line_number: 0,
            diagnostic_context: 0,
            registered_diagnostics: RegisteredDiagnosticTracker::default(),
        })
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.raw.push(chunk);
        for byte in chunk {
            if self.line.observe_source(*byte) {
                self.registered_diagnostics.push(*byte);
            }
            let output = self
                .ansi
                .as_mut()
                .map_or(Some(*byte), |stripper| stripper.push_byte(*byte));
            if let Some(output) = output {
                self.line.push_output(output);
            }
            if *byte == b'\n' {
                let line = self.line.take();
                self.process_line(&line);
            }
        }
    }

    #[must_use]
    pub fn finish(mut self, success: bool, recovery: Option<RecoveryMarker<'_>>) -> Vec<u8> {
        if !self.line.is_empty() {
            let line = self.line.take();
            self.process_line(&line);
        }
        let (collector, policy) = if success {
            (self.success, self.success_policy)
        } else {
            (self.failure, self.failure_policy)
        };
        self.raw.choose(
            collector.render(recovery, self.registered_diagnostics.matched, success),
            policy,
        )
    }

    fn process_line(&mut self, line: &LineView) {
        self.line_number = self.line_number.saturating_add(1);
        if classify::has_registered_diagnostic(&line.prefix) {
            self.registered_diagnostics.observe_slice(&line.prefix);
        }
        if line.truncated {
            let tail = line.tail_bytes();
            if classify::has_registered_diagnostic(&tail) {
                self.registered_diagnostics.observe_slice(&tail);
            }
        }
        let mut class = if classify::is_tool_outcome(&line.prefix) {
            EvidenceClass::Outcome
        } else {
            classify_line(&self.kind, line)
        };
        if class == EvidenceClass::Diagnostic {
            self.diagnostic_context = DIAGNOSTIC_CONTEXT_LINES;
        } else if self.diagnostic_context > 0 {
            if class == EvidenceClass::Noise || class == EvidenceClass::Example {
                class = EvidenceClass::Context;
            }
            self.diagnostic_context -= 1;
        }
        self.registered_diagnostics.end_line();
        self.success.observe(self.line_number, line, class);
        self.failure.observe(self.line_number, line, class);
    }
}

fn family_name(kind: &Reducer) -> &'static str {
    match kind {
        Reducer::SearchSummary => "search",
        Reducer::DiffSummary => "diff",
        Reducer::TestSummary => "test",
        Reducer::BuildSummary => "build",
        Reducer::LogSummary => "log",
        Reducer::StatusSummary => "status",
        Reducer::ListSummary => "list",
        Reducer::LineFilter { .. } => "line filter",
    }
}

fn classify_line(kind: &Reducer, line: &LineView) -> EvidenceClass {
    match kind {
        Reducer::SearchSummary => search::classify(&line.prefix),
        Reducer::DiffSummary => diff::classify(&line.prefix),
        Reducer::TestSummary => test::classify(&line.prefix),
        Reducer::BuildSummary => build::classify(&line.prefix),
        Reducer::LogSummary => log::classify(&line.prefix),
        Reducer::StatusSummary => status::classify(&line.prefix),
        Reducer::ListSummary => list::classify(&line.prefix),
        Reducer::LineFilter { drop, keep, .. } => {
            if line_filter_keeps(line, drop, keep) {
                EvidenceClass::Example
            } else {
                EvidenceClass::Noise
            }
        }
    }
}

/// Reduce one in-memory byte stream with the same engine used by child capture.
///
/// # Errors
///
/// Returns an error when the supplied rule is not a complete reduction rule.
pub fn reduce_bytes(rule: &Rule, input: &[u8], success: bool) -> Result<Vec<u8>, String> {
    reduce_bytes_with_recovery(rule, input, success, None)
}

/// Reduce one in-memory stream and include a committed recovery reference in the savings decision.
///
/// # Errors
///
/// Returns an error when the supplied rule is not a complete reduction rule.
pub fn reduce_bytes_with_recovery(
    rule: &Rule,
    input: &[u8],
    success: bool,
    recovery: Option<RecoveryMarker<'_>>,
) -> Result<Vec<u8>, String> {
    let mut reducer = StreamReducer::new(rule)?;
    for chunk in input.chunks(8 * 1024) {
        reducer.push(chunk);
    }
    Ok(reducer.finish(success, recovery))
}

/// Calculate a conservative retained-memory bound for one stream.
///
/// # Errors
///
/// Returns an error when the rule is incomplete or the bound overflows `usize`.
pub fn configured_memory_bound(rule: &Rule) -> Result<usize, String> {
    yarp_rule_pack::stream_memory_bound(rule)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use yarp_rule_pack::{CommandMatcher, OutputPolicy};

    use super::*;

    fn policy(max: usize, savings: usize) -> OutputPolicy {
        OutputPolicy {
            max_line_bytes: 256,
            max_output_bytes: max,
            min_savings_bytes: savings,
            min_savings_basis_points: 1_000,
        }
    }

    fn rule(reducer: Reducer) -> Rule {
        Rule {
            id: "test/rule".to_owned(),
            matcher: CommandMatcher {
                program: vec!["test".to_owned()],
                argv_prefix: Vec::new(),
                argv_contains_all: Vec::new(),
            },
            action: Action::Reduce,
            transform: None,
            reducer: Some(reducer),
            success: Some(policy(1_024, 8)),
            failure: Some(policy(2_048, 8)),
        }
    }

    #[test]
    fn uses_real_exit_status_policy() {
        let input = numbered_lines("line", 200);
        let success = reduce_bytes(&rule(Reducer::ListSummary), input.as_bytes(), true)
            .expect("success reduction");
        let failure = reduce_bytes(&rule(Reducer::ListSummary), input.as_bytes(), false)
            .expect("failure reduction");
        assert!(
            failure.len() > success.len(),
            "success={} failure={}",
            success.len(),
            failure.len()
        );
    }

    #[test]
    fn keeps_tool_session_identifiers_needed_for_follow_up() {
        let input = format!(
            "Chunk ID: abc\nProcess running with session ID 12345\n{}",
            numbered_lines("path", 2_000)
        );
        let output =
            reduce_bytes(&rule(Reducer::ListSummary), input.as_bytes(), true).expect("reduction");
        let text = String::from_utf8(output).expect("UTF-8");
        assert!(text.contains("Process running with session ID 12345"));
    }

    #[test]
    fn rejects_rules_above_the_stream_memory_limit() {
        let mut oversized = rule(Reducer::ListSummary);
        let policy = OutputPolicy {
            max_line_bytes: 1_048_576,
            max_output_bytes: 4_194_304,
            min_savings_bytes: 1_048_576,
            min_savings_basis_points: 1_000,
        };
        oversized.success = Some(policy);
        oversized.failure = Some(policy);
        let error = StreamReducer::new(&oversized).expect_err("memory limit");
        assert!(error.contains("per stream"));
    }

    #[test]
    fn output_is_independent_of_chunk_boundaries() {
        let input = numbered_lines("path:1:match", 400);
        let rule = rule(Reducer::SearchSummary);
        let expected = reduce_bytes(&rule, input.as_bytes(), true).expect("reduction");
        for chunk_size in 1..32 {
            let mut reducer = StreamReducer::new(&rule).expect("reducer");
            for chunk in input.as_bytes().chunks(chunk_size) {
                reducer.push(chunk);
            }
            assert_eq!(
                reducer.finish(true, None),
                expected,
                "chunk size {chunk_size}"
            );
        }
    }

    #[test]
    fn test_summary_keeps_failures_and_drops_progress() {
        let input = format!(
            "{}error: build failed\ntest result: FAILED\n",
            "test routine ... ok\n".repeat(1_000)
        );
        let output =
            reduce_bytes(&rule(Reducer::TestSummary), input.as_bytes(), false).expect("reduction");
        let text = String::from_utf8(output).expect("UTF-8");
        assert!(!text.contains("test routine"));
        assert!(text.contains("error: build failed"));
        assert!(text.contains("test result: FAILED"));
    }

    #[test]
    fn keeps_registered_diagnostics_from_the_tail_of_a_bounded_long_line() {
        let mut compact = rule(Reducer::SearchSummary);
        compact.success.as_mut().expect("policy").max_line_bytes = 256;
        compact.failure.as_mut().expect("policy").max_line_bytes = 256;
        let input = format!("{} warning: tail evidence\n", "x".repeat(1_024));
        let output = reduce_bytes(&compact, input.as_bytes(), true).expect("reduction");
        assert!(String::from_utf8_lossy(&output).contains("warning"));
    }

    #[test]
    fn records_registered_diagnostics_from_the_middle_of_an_oversized_line() {
        let mut compact = rule(Reducer::SearchSummary);
        compact.success.as_mut().expect("policy").max_line_bytes = 256;
        compact.failure.as_mut().expect("policy").max_line_bytes = 256;
        let input = format!(
            "{} warning: middle evidence {}\n",
            "x".repeat(1_024),
            "y".repeat(1_024)
        );
        let output = reduce_bytes(&compact, input.as_bytes(), true).expect("reduction");
        assert!(String::from_utf8_lossy(&output).contains("source_terms=warning"));
    }

    #[test]
    fn applies_the_line_limit_before_stripping_ansi() {
        let mut input = b"\x1b]".to_vec();
        input.extend(std::iter::repeat_n(b'x', 400));
        input.extend_from_slice(b"\x07visible\n");
        input.extend_from_slice(numbered_lines("tail", 200).as_bytes());
        let rule = rule(Reducer::LineFilter {
            strip_ansi: true,
            drop: Vec::new(),
            keep: Vec::new(),
        });
        let output = reduce_bytes(&rule, &input, true).expect("reduction");
        assert!(
            String::from_utf8_lossy(&output).contains("line truncated"),
            "raw source length must control line truncation"
        );
    }

    #[test]
    fn every_builtin_reducer_obeys_both_hard_output_budgets() {
        let input = numbered_lines("diagnostic line", 4_000);
        let mut registry = crate::rules::Registry::builtins_only();
        for summary in registry.summaries().expect("built-in summaries") {
            if summary.rule.action != Action::Reduce {
                continue;
            }
            assert!(
                configured_memory_bound(&summary.rule).expect("memory bound") < 4 * 1024 * 1024,
                "{} exceeds the per-stream memory target",
                summary.rule.id
            );
            for success in [true, false] {
                let output = reduce_bytes(&summary.rule, input.as_bytes(), success)
                    .unwrap_or_else(|error| panic!("{}: {error}", summary.rule.id));
                let policy = if success {
                    summary.rule.success.expect("success policy")
                } else {
                    summary.rule.failure.expect("failure policy")
                };
                assert!(
                    output.len() <= policy.max_output_bytes,
                    "{} emitted {} bytes with a {} byte budget",
                    summary.rule.id,
                    output.len(),
                    policy.max_output_bytes
                );
            }
        }
    }

    #[test]
    fn every_builtin_reducer_keeps_short_output_exact() {
        let input = b"one useful line\n";
        let mut registry = crate::rules::Registry::builtins_only();
        for summary in registry.summaries().expect("built-in summaries") {
            if summary.rule.action != Action::Reduce {
                continue;
            }
            let output = reduce_bytes(&summary.rule, input, true)
                .unwrap_or_else(|error| panic!("{}: {error}", summary.rule.id));
            assert_eq!(output, input, "{}", summary.rule.id);
        }
    }

    fn numbered_lines(prefix: &str, count: usize) -> String {
        let mut output = String::new();
        for index in 0..count {
            writeln!(&mut output, "{prefix} {index}: repeated output")
                .expect("write synthetic output");
        }
        output
    }

    #[test]
    fn unknown_binary_bytes_are_processed_without_utf8() {
        let mut input = vec![0xff; 4_096];
        input.push(b'\n');
        let output = reduce_bytes(&rule(Reducer::SearchSummary), &input, true).expect("reduction");
        assert!(output.len() <= 1_024);
    }
}
