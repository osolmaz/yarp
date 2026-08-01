mod bounded;
mod filter;
mod git_diff;

use bounded::{LineAccumulator, LineView, Retention, ShortRaw};
use filter::{AnsiStripper, cargo_test_keeps, line_filter_keeps};
use git_diff::GitDiffState;
use yarp_rule_pack::{Action, OutputPolicy, Reducer, Rule};

#[derive(Debug)]
pub struct StreamReducer {
    kind: Reducer,
    success_policy: OutputPolicy,
    failure_policy: OutputPolicy,
    success: Retention,
    failure: Retention,
    raw: ShortRaw,
    line: LineAccumulator,
    ansi: Option<AnsiStripper>,
    scratch: Vec<u8>,
    diff: Option<GitDiffState>,
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
        let pattern_tail = match &kind {
            Reducer::LineFilter { drop, keep, .. } => drop
                .iter()
                .chain(keep)
                .map(|pattern| pattern.value.len())
                .max()
                .unwrap_or(0),
            _ => 0,
        };
        let needs_tail = matches!(kind, Reducer::Search)
            || matches!(&kind, Reducer::LineFilter { drop, keep, .. } if drop.iter().chain(keep).any(|pattern| matches!(pattern.kind, yarp_rule_pack::PatternKind::Suffix)));
        let tail_limit = usize::from(needs_tail).saturating_mul(max_line.max(pattern_tail));
        let strip_ansi = matches!(
            kind,
            Reducer::LineFilter {
                strip_ansi: true,
                ..
            } | Reducer::CargoTest
                | Reducer::GitDiff
                | Reducer::GitStatus
                | Reducer::Search
        );
        Ok(Self {
            kind: kind.clone(),
            success_policy,
            failure_policy,
            success: Retention::new(success_policy),
            failure: Retention::new(failure_policy),
            raw: ShortRaw::new(success_policy, failure_policy),
            line: LineAccumulator::new(max_line, tail_limit),
            ansi: strip_ansi.then(AnsiStripper::new),
            scratch: Vec::with_capacity(8 * 1024),
            diff: matches!(kind, Reducer::GitDiff).then(GitDiffState::default),
        })
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.raw.push(chunk);
        for piece in chunk.chunks(8 * 1024) {
            self.scratch.clear();
            if let Some(stripper) = &mut self.ansi {
                stripper.push(piece, &mut self.scratch);
            } else {
                self.scratch.extend_from_slice(piece);
            }
            let length = self.scratch.len();
            for index in 0..length {
                let byte = self.scratch[index];
                self.line.push(byte);
                if byte == b'\n' {
                    let line = self.line.take();
                    self.process_line(line);
                }
            }
        }
    }

    #[must_use]
    pub fn finish(mut self, success: bool) -> Vec<u8> {
        if !self.line.is_empty() {
            let line = self.line.take();
            self.process_line(line);
        }
        if let Some(diff) = self.diff.take() {
            let mut kept = Vec::new();
            diff.finish(|line| kept.push(line));
            for line in kept {
                self.observe(&line, true);
            }
        }
        let (retention, policy) = if success {
            (self.success, self.success_policy)
        } else {
            (self.failure, self.failure_policy)
        };
        self.raw.choose(retention.render(), policy)
    }

    fn process_line(&mut self, line: LineView) {
        match &self.kind {
            Reducer::HeadTail | Reducer::GitStatus | Reducer::Search => {
                self.observe(&line, true);
            }
            Reducer::CargoTest => {
                let keep = cargo_test_keeps(&line);
                self.observe(&line, keep);
            }
            Reducer::LineFilter { drop, keep, .. } => {
                let retain = line_filter_keeps(&line, drop, keep);
                self.observe(&line, retain);
            }
            Reducer::GitDiff => {
                let mut kept = Vec::new();
                let mut omitted = Vec::new();
                self.diff.as_mut().expect("git diff reducer state").push(
                    line,
                    |line| kept.push(line),
                    |line| omitted.push(line),
                );
                for line in omitted {
                    self.observe(&line, false);
                }
                for line in kept {
                    self.observe(&line, true);
                }
            }
        }
    }

    fn observe(&mut self, line: &LineView, keep: bool) {
        let two_sided = matches!(self.kind, Reducer::Search);
        self.success.observe(line, keep, two_sided);
        self.failure.observe(line, keep, two_sided);
    }
}

/// Reduce one in-memory byte stream with the same engine used by child capture.
///
/// # Errors
///
/// Returns an error when the supplied rule is not a complete reduction rule.
pub fn reduce_bytes(rule: &Rule, input: &[u8], success: bool) -> Result<Vec<u8>, String> {
    let mut reducer = StreamReducer::new(rule)?;
    for chunk in input.chunks(8 * 1024) {
        reducer.push(chunk);
    }
    Ok(reducer.finish(success))
}

/// Calculate a conservative retained-memory bound for one stream.
///
/// # Errors
///
/// Returns an error when the rule is incomplete or the bound overflows `usize`.
pub fn configured_memory_bound(rule: &Rule) -> Result<usize, String> {
    let success = rule
        .success
        .ok_or_else(|| "reduction rule is missing a success policy".to_owned())?;
    let failure = rule
        .failure
        .ok_or_else(|| "reduction rule is missing a failure policy".to_owned())?;
    let raw = success
        .max_output_bytes
        .saturating_add(success.min_savings_bytes)
        .max(
            failure
                .max_output_bytes
                .saturating_add(failure.min_savings_bytes),
        );
    let max_line = success.max_line_bytes.max(failure.max_line_bytes);
    let line_state = max_line
        .checked_mul(2)
        .ok_or_else(|| "line memory bound overflowed".to_owned())?;
    let reducer_state = if matches!(rule.reducer, Some(Reducer::GitDiff)) {
        max_line
            .checked_mul(3)
            .ok_or_else(|| "reducer memory bound overflowed".to_owned())?
    } else {
        0
    };
    [
        raw,
        success.max_output_bytes,
        failure.max_output_bytes,
        line_state,
        reducer_state,
        8 * 1024,
        64 * 1024,
    ]
    .into_iter()
    .try_fold(0_usize, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| "stream memory bound overflowed".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use yarp_rule_pack::{CommandMatcher, OutputPolicy};

    use super::*;

    fn policy(head: usize, tail: usize, max: usize, savings: usize) -> OutputPolicy {
        OutputPolicy {
            head_lines: head,
            tail_lines: tail,
            max_line_bytes: 256,
            max_output_bytes: max,
            min_savings_bytes: savings,
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
            reducer: Some(reducer),
            success: Some(policy(2, 1, 1_024, 8)),
            failure: Some(policy(4, 2, 2_048, 8)),
        }
    }

    #[test]
    fn uses_real_exit_status_policy() {
        let input = numbered_lines("line", 20);
        let success = reduce_bytes(&rule(Reducer::HeadTail), input.as_bytes(), true)
            .expect("success reduction");
        let failure = reduce_bytes(&rule(Reducer::HeadTail), input.as_bytes(), false)
            .expect("failure reduction");
        assert!(failure.len() > success.len());
    }

    #[test]
    fn output_is_independent_of_chunk_boundaries() {
        let input = numbered_lines("line", 40);
        let rule = rule(Reducer::Search);
        let expected = reduce_bytes(&rule, input.as_bytes(), true).expect("reduction");
        for chunk_size in 1..32 {
            let mut reducer = StreamReducer::new(&rule).expect("reducer");
            for chunk in input.as_bytes().chunks(chunk_size) {
                reducer.push(chunk);
            }
            assert_eq!(reducer.finish(true), expected, "chunk size {chunk_size}");
        }
    }

    #[test]
    fn cargo_filter_keeps_failures_and_drops_progress() {
        let input = format!(
            "{}error: build failed\ntest result: FAILED\n",
            "   Compiling crate\n".repeat(100)
        );
        let output =
            reduce_bytes(&rule(Reducer::CargoTest), input.as_bytes(), false).expect("reduction");
        let text = String::from_utf8(output).expect("UTF-8");
        assert!(!text.contains("Compiling"));
        assert!(text.contains("error: build failed"));
        assert!(text.contains("test result: FAILED"));
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
        let output = reduce_bytes(&rule(Reducer::Search), &input, true).expect("reduction");
        assert!(output.len() <= 1_024);
    }
}
