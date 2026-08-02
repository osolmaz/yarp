use std::collections::VecDeque;

use yarp_rule_pack::OutputPolicy;

use super::bounded::{LineView, render_line};

const MAX_RECORDS_PER_CLASS: usize = 128;
const RENDER_RESERVE_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EvidenceClass {
    Outcome,
    Diagnostic,
    Structure,
    Context,
    Example,
    Noise,
}

impl EvidenceClass {
    const fn index(self) -> Option<usize> {
        match self {
            Self::Diagnostic => Some(0),
            Self::Outcome => Some(1),
            Self::Structure => Some(2),
            Self::Context => Some(3),
            Self::Example => Some(4),
            Self::Noise => None,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Outcome => "outcome",
            Self::Diagnostic => "diagnostics",
            Self::Structure => "structure",
            Self::Context => "diagnostic context",
            Self::Example => "examples",
            Self::Noise => "noise",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceSpan {
    pub first_line: u64,
    pub last_line: u64,
}

#[derive(Clone, Debug)]
struct EvidenceRecord {
    span: SourceSpan,
    body: Vec<u8>,
}

#[derive(Debug)]
struct Bucket {
    class: EvidenceClass,
    head: Vec<EvidenceRecord>,
    tail: VecDeque<EvidenceRecord>,
    head_bytes: usize,
    tail_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    total: u64,
    first_omitted: Option<u64>,
    last_omitted: Option<u64>,
}

impl Bucket {
    fn new(class: EvidenceClass, budget: usize) -> Self {
        let head_budget = budget / 2;
        Self {
            class,
            head: Vec::new(),
            tail: VecDeque::new(),
            head_bytes: 0,
            tail_bytes: 0,
            head_budget,
            tail_budget: budget.saturating_sub(head_budget),
            total: 0,
            first_omitted: None,
            last_omitted: None,
        }
    }

    fn observe(&mut self, record: EvidenceRecord) {
        self.total = self.total.saturating_add(1);
        if self.head.len() < MAX_RECORDS_PER_CLASS / 2
            && self.head_bytes.saturating_add(record.body.len()) <= self.head_budget
        {
            self.head_bytes = self.head_bytes.saturating_add(record.body.len());
            self.head.push(record);
            return;
        }
        if record.body.len() > self.tail_budget {
            self.note_omitted(record.span);
            return;
        }
        while self.tail.len() >= MAX_RECORDS_PER_CLASS / 2
            || self.tail_bytes.saturating_add(record.body.len()) > self.tail_budget
        {
            let Some(removed) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes = self.tail_bytes.saturating_sub(removed.body.len());
            self.note_omitted(removed.span);
        }
        if self.tail_bytes.saturating_add(record.body.len()) <= self.tail_budget {
            self.tail_bytes = self.tail_bytes.saturating_add(record.body.len());
            self.tail.push_back(record);
        } else {
            self.note_omitted(record.span);
        }
    }

    fn note_omitted(&mut self, span: SourceSpan) {
        self.first_omitted = Some(
            self.first_omitted
                .map_or(span.first_line, |current| current.min(span.first_line)),
        );
        self.last_omitted = Some(
            self.last_omitted
                .map_or(span.last_line, |current| current.max(span.last_line)),
        );
    }

    fn retained(&self) -> u64 {
        u64::try_from(self.head.len().saturating_add(self.tail.len())).unwrap_or(u64::MAX)
    }

    fn append_retained_lines(&self, lines: &mut Vec<u64>) {
        lines.extend(self.head.iter().map(|record| record.span.first_line));
        lines.extend(self.tail.iter().map(|record| record.span.first_line));
    }

    fn render(self, output: &mut Vec<u8>, newline: &[u8]) {
        let retained = self.retained();
        let omitted = self.total.saturating_sub(retained);
        if retained == 0 && omitted == 0 {
            return;
        }
        output.extend_from_slice(format!("== {} ==", self.class.title()).as_bytes());
        output.extend_from_slice(newline);
        for record in self.head {
            render_record(output, &record, newline);
        }
        if omitted > 0 {
            let range = match (self.first_omitted, self.last_omitted) {
                (Some(first), Some(last)) => format!("; source lines {first}:{last}"),
                _ => String::new(),
            };
            output.extend_from_slice(
                format!(
                    "[yarp: omitted {omitted} {} line(s){range}]",
                    self.class.title()
                )
                .as_bytes(),
            );
            output.extend_from_slice(newline);
        }
        for record in self.tail {
            render_record(output, &record, newline);
        }
    }
}

fn render_record(output: &mut Vec<u8>, record: &EvidenceRecord, newline: &[u8]) {
    if record.span.first_line == record.span.last_line {
        output.extend_from_slice(format!("L{}: ", record.span.first_line).as_bytes());
    } else {
        output.extend_from_slice(
            format!("L{}-{}: ", record.span.first_line, record.span.last_line).as_bytes(),
        );
    }
    output.extend_from_slice(strip_line_ending(&record.body));
    output.extend_from_slice(newline);
}

#[derive(Clone, Copy, Debug)]
pub struct RecoveryMarker<'a> {
    pub archive_ref: &'a str,
    pub source: &'a str,
    pub completeness: &'a str,
}

#[derive(Debug)]
pub struct EvidenceCollector {
    family: &'static str,
    policy: OutputPolicy,
    buckets: Vec<Bucket>,
    total_lines: u64,
    noise_lines: u64,
    newline: Vec<u8>,
    newline_observed: bool,
    truncated_lines: u64,
    priority_terms: [Option<EvidenceRecord>; 5],
}

impl EvidenceCollector {
    #[must_use]
    pub fn new(family: &'static str, policy: OutputPolicy) -> Self {
        let reserve = RENDER_RESERVE_BYTES.min(policy.max_output_bytes / 4);
        let available = policy.max_output_bytes.saturating_sub(reserve);
        let weights = [40_usize, 15, 15, 15, 15];
        let classes = [
            EvidenceClass::Diagnostic,
            EvidenceClass::Outcome,
            EvidenceClass::Structure,
            EvidenceClass::Context,
            EvidenceClass::Example,
        ];
        let buckets = classes
            .into_iter()
            .zip(weights)
            .map(|(class, weight)| Bucket::new(class, available.saturating_mul(weight) / 100))
            .collect();
        Self {
            family,
            policy,
            buckets,
            total_lines: 0,
            noise_lines: 0,
            newline: b"\n".to_vec(),
            newline_observed: false,
            truncated_lines: 0,
            priority_terms: [const { None }; 5],
        }
    }

    pub fn observe(&mut self, line_number: u64, line: &LineView, class: EvidenceClass) {
        self.total_lines = self.total_lines.saturating_add(1);
        if !self.newline_observed && !line.line_ending.is_empty() {
            self.newline.clone_from(&line.line_ending);
            self.newline_observed = true;
        }
        let source_term_in_bounds = super::classify::has_registered_diagnostic(&line.prefix)
            || (line.truncated && super::classify::has_registered_diagnostic(&line.tail_bytes()));
        if class == EvidenceClass::Noise {
            self.noise_lines = self.noise_lines.saturating_add(1);
            if !source_term_in_bounds {
                return;
            }
        }
        let (body, truncated) = render_line(
            line,
            self.policy.max_line_bytes,
            matches!(
                class,
                EvidenceClass::Diagnostic | EvidenceClass::Example | EvidenceClass::Noise
            ),
        );
        self.truncated_lines = self.truncated_lines.saturating_add(u64::from(truncated));
        let record = EvidenceRecord {
            span: SourceSpan {
                first_line: line_number,
                last_line: line_number,
            },
            body,
        };
        if source_term_in_bounds {
            for (slot, term) in self
                .priority_terms
                .iter_mut()
                .zip(super::classify::REGISTERED_DIAGNOSTICS)
            {
                if slot.is_none() && super::classify::contains_ascii_insensitive(&record.body, term)
                {
                    *slot = Some(diagnostic_representative(&record, term));
                }
            }
        }
        let Some(index) = class.index() else {
            return;
        };
        self.buckets[index].observe(record);
    }

    #[must_use]
    pub fn render(
        self,
        recovery: Option<RecoveryMarker<'_>>,
        registered_diagnostics: [bool; 5],
    ) -> Vec<u8> {
        let mut priority_terms = self
            .priority_terms
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        priority_terms.sort_by_key(|record| record.span.first_line);
        priority_terms.dedup_by_key(|record| record.span.first_line);
        let mut retained_lines = priority_terms
            .iter()
            .map(|record| record.span.first_line)
            .collect::<Vec<_>>();
        for bucket in &self.buckets {
            bucket.append_retained_lines(&mut retained_lines);
        }
        retained_lines.sort_unstable();
        retained_lines.dedup();
        let retained = u64::try_from(retained_lines.len()).unwrap_or(u64::MAX);
        let omitted = self.total_lines.saturating_sub(retained);
        let mut output = Vec::with_capacity(self.policy.max_output_bytes.min(64 * 1024));
        let registered = registered_diagnostics
            .into_iter()
            .zip(["failure", "panic", "error", "warning", "test result"])
            .filter_map(|(present, term)| present.then_some(term))
            .collect::<Vec<_>>();
        let diagnostic_suffix = if registered.is_empty() {
            String::new()
        } else {
            format!("; source_terms={}", registered.join(","))
        };
        output.extend_from_slice(
            format!(
                "[yarp summary: {}; source_lines={}{}]",
                self.family, self.total_lines, diagnostic_suffix
            )
            .as_bytes(),
        );
        output.extend_from_slice(&self.newline);
        if omitted > 0 || self.truncated_lines > 0 {
            append_recovery_marker(
                &mut output,
                &self.newline,
                omitted,
                self.truncated_lines,
                recovery,
            );
        }
        for bucket in self.buckets {
            bucket.render(&mut output, &self.newline);
        }
        if !priority_terms.is_empty() {
            output.extend_from_slice(b"== source term samples ==");
            output.extend_from_slice(&self.newline);
            for record in &priority_terms {
                render_record(&mut output, record, &self.newline);
            }
        }
        if self.noise_lines > 0 {
            output.extend_from_slice(
                format!("[yarp: omitted {} routine line(s)]", self.noise_lines).as_bytes(),
            );
            output.extend_from_slice(&self.newline);
        }
        if self.truncated_lines > 0 {
            output.extend_from_slice(
                format!(
                    "[yarp: truncated {} retained line(s)]",
                    self.truncated_lines
                )
                .as_bytes(),
            );
            output.extend_from_slice(&self.newline);
        }
        output.truncate(self.policy.max_output_bytes);
        output
    }
}

fn diagnostic_representative(record: &EvidenceRecord, term: &[u8]) -> EvidenceRecord {
    const MAX_REPRESENTATIVE_BYTES: usize = 64;
    let body = strip_line_ending(&record.body);
    if body.len() <= MAX_REPRESENTATIVE_BYTES {
        return record.clone();
    }
    let position = body
        .windows(term.len())
        .position(|window| {
            window
                .iter()
                .zip(term)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
        .unwrap_or(0);
    let prefix = position.saturating_sub(24);
    let end = prefix
        .saturating_add(MAX_REPRESENTATIVE_BYTES)
        .min(body.len());
    let mut excerpt = Vec::with_capacity(MAX_REPRESENTATIVE_BYTES + 12);
    if prefix > 0 {
        excerpt.extend_from_slice(b"[...] ");
    }
    excerpt.extend_from_slice(&body[prefix..end]);
    if end < body.len() {
        excerpt.extend_from_slice(b" [...]");
    }
    EvidenceRecord {
        span: record.span,
        body: excerpt,
    }
}

fn append_recovery_marker(
    output: &mut Vec<u8>,
    newline: &[u8],
    omitted: u64,
    truncated: u64,
    recovery: Option<RecoveryMarker<'_>>,
) {
    let description = if truncated == 0 {
        format!("omitted {omitted} line(s)")
    } else {
        format!("omitted {omitted} line(s), truncated {truncated} line(s)")
    };
    if let Some(recovery) = recovery {
        output.extend_from_slice(
            format!(
                "[yarp: {description}; ref={}; {} {}]",
                recovery.archive_ref, recovery.source, recovery.completeness
            )
            .as_bytes(),
        );
        output.extend_from_slice(newline);
        output.extend_from_slice(
            format!(
                "Search omitted output: yarp search {} 'term|alternate'",
                recovery.archive_ref
            )
            .as_bytes(),
        );
        output.extend_from_slice(newline);
    } else {
        output.extend_from_slice(format!("[yarp: {description}]").as_bytes());
        output.extend_from_slice(newline);
    }
}

fn strip_line_ending(body: &[u8]) -> &[u8] {
    body.strip_suffix(b"\r\n")
        .or_else(|| body.strip_suffix(b"\n"))
        .unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn line(value: &str) -> LineView {
        LineView {
            prefix: value.as_bytes().to_vec(),
            tail: VecDeque::from(value.as_bytes().to_vec()),
            line_ending: if value.ends_with("\r\n") {
                b"\r\n".to_vec()
            } else if value.ends_with('\n') {
                b"\n".to_vec()
            } else {
                Vec::new()
            },
            total_bytes: value.len(),
            truncated: false,
        }
    }

    fn policy() -> OutputPolicy {
        OutputPolicy {
            max_line_bytes: 256,
            max_output_bytes: 4_096,
            min_savings_bytes: 8,
            min_savings_basis_points: 1_000,
        }
    }

    #[test]
    fn renders_typed_sections_and_copyable_recovery() {
        let mut collector = EvidenceCollector::new("test", policy());
        collector.observe(1, &line("test one ... ok\r\n"), EvidenceClass::Noise);
        collector.observe(2, &line("error: failed\n"), EvidenceClass::Diagnostic);
        collector.observe(3, &line("test result: FAILED\n"), EvidenceClass::Outcome);
        let output = String::from_utf8(collector.render(
            Some(RecoveryMarker {
                archive_ref: "yr_0123456789abcdef0123456789abcdef",
                source: "stderr",
                completeness: "complete",
            }),
            [false; 5],
        ))
        .expect("UTF-8");
        assert!(output.contains("== outcome ==\r\n"));
        assert!(output.contains("L2: error: failed"));
        assert!(output.contains("yarp search yr_0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn compact_output_keeps_the_complete_recovery_instruction() {
        let mut compact = policy();
        compact.max_output_bytes = 704;
        let mut collector = EvidenceCollector::new("search", compact);
        for index in 0..1_000 {
            collector.observe(
                index + 1,
                &line(&format!("src/file.rs:{index}: match\n")),
                EvidenceClass::Example,
            );
        }
        let output = collector.render(
            Some(RecoveryMarker {
                archive_ref: "yr_0123456789abcdef0123456789abcdef",
                source: "result_text",
                completeness: "unknown",
            }),
            [false; 5],
        );
        let text = String::from_utf8_lossy(&output);
        assert!(output.len() <= compact.max_output_bytes);
        assert!(text.contains(
            "Search omitted output: yarp search yr_0123456789abcdef0123456789abcdef 'term|alternate'"
        ));
    }

    #[test]
    fn keeps_one_source_line_for_every_registered_diagnostic_category() {
        let mut compact = policy();
        compact.max_output_bytes = 1_024;
        let mut collector = EvidenceCollector::new("test", compact);
        for index in 0..100 {
            collector.observe(
                index + 1,
                &line(&format!("error: repeated {index}\n")),
                EvidenceClass::Diagnostic,
            );
        }
        for (offset, value) in [
            "warning: late\n",
            "panic: late\n",
            "failure: late\n",
            "test result: FAILED\n",
        ]
        .into_iter()
        .enumerate()
        {
            collector.observe(
                u64::try_from(101 + offset).expect("line"),
                &line(value),
                EvidenceClass::Diagnostic,
            );
        }
        let output =
            String::from_utf8_lossy(&collector.render(None, [true; 5])).to_ascii_lowercase();
        for term in ["error", "warning", "panic", "failure", "test result"] {
            assert!(output.contains(term), "missing {term}: {output}");
        }
    }

    #[test]
    fn collector_memory_is_bounded_by_record_and_byte_budgets() {
        let mut collector = EvidenceCollector::new("list", policy());
        for index in 0..10_000 {
            collector.observe(
                index + 1,
                &line(&format!("record {index}\n")),
                EvidenceClass::Example,
            );
        }
        let output = collector.render(None, [false; 5]);
        assert!(output.len() <= policy().max_output_bytes);
        assert!(String::from_utf8_lossy(&output).contains("omitted"));
    }
}
