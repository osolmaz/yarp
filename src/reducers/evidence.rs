use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::rule_pack::OutputPolicy;

use super::bounded::{LineView, render_line};

const MAX_RECORDS_PER_CLASS: usize = 128;
const EXAMPLE_BUCKET_INDEX: usize = 4;
const COMPACT_SUCCESS_SUMMARY_BYTES: usize = 512;

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
    repeats: u64,
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
    fingerprints: BTreeSet<(u64, u64, usize)>,
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
            fingerprints: BTreeSet::new(),
            first_omitted: None,
            last_omitted: None,
        }
    }

    fn observe_repeat(&mut self, body: &[u8], span: SourceSpan) -> bool {
        let fingerprint = record_fingerprint(body);
        if !self.fingerprints.contains(&fingerprint) {
            return false;
        }
        let Some(existing) = self
            .head
            .iter_mut()
            .chain(self.tail.iter_mut())
            .find(|existing| existing.body == body)
        else {
            return false;
        };
        self.total = self.total.saturating_add(1);
        existing.repeats = existing.repeats.saturating_add(1);
        existing.span.last_line = span.last_line;
        true
    }

    fn observe(&mut self, mut record: EvidenceRecord) {
        self.total = self.total.saturating_add(1);
        let fingerprint = record_fingerprint(&record.body);
        if self.fingerprints.contains(&fingerprint) {
            if let Some(existing) = self
                .head
                .iter_mut()
                .chain(self.tail.iter_mut())
                .find(|existing| existing.body == record.body)
            {
                existing.repeats = existing.repeats.saturating_add(1);
                existing.span.last_line = record.span.last_line;
                return;
            }
        } else if self.fingerprints.len() < MAX_RECORDS_PER_CLASS {
            self.fingerprints.insert(fingerprint);
        }
        record.repeats = 1;
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

    fn render_ranked(
        &self,
        limit: usize,
        newline: &[u8],
        represented: &BTreeSet<u64>,
    ) -> RenderedBucket {
        const OMISSION_RESERVE_BYTES: usize = 128;

        if self.total == 0 {
            return RenderedBucket::default();
        }
        let mut records = self.head.iter().chain(&self.tail).collect::<Vec<_>>();
        records.sort_by_key(|record| record.span.first_line);
        let represented_count = records
            .iter()
            .filter(|record| represented.contains(&record.span.first_line))
            .count();
        let candidates = records
            .into_iter()
            .filter(|record| !represented.contains(&record.span.first_line))
            .collect::<Vec<_>>();
        let mut complete = Vec::new();
        append_section_heading(&mut complete, self.class, newline);
        for record in &candidates {
            render_record(&mut complete, record, newline);
        }
        append_omission(
            &mut complete,
            self.class,
            self.total.saturating_sub(
                u64::try_from(candidates.len().saturating_add(represented_count))
                    .unwrap_or(u64::MAX),
            ),
            self.first_omitted,
            self.last_omitted,
            newline,
        );
        if complete.len() <= limit {
            return RenderedBucket {
                body: complete,
                retained_lines: candidates
                    .iter()
                    .map(|record| record.span.first_line)
                    .collect(),
            };
        }

        let heading_bytes = self.class.title().len().saturating_add(7 + newline.len());
        if limit < heading_bytes.saturating_add(OMISSION_RESERVE_BYTES) {
            return RenderedBucket::default();
        }
        let mut selection_order = Vec::with_capacity(candidates.len());
        let (mut first, mut last) = (0_usize, candidates.len());
        while first < last {
            selection_order.push(first);
            first += 1;
            if first < last {
                last -= 1;
                selection_order.push(last);
            }
        }
        let mut selected = BTreeSet::new();
        let mut selected_bytes = heading_bytes;
        for index in selection_order {
            let mut rendered = Vec::new();
            render_record(&mut rendered, candidates[index], newline);
            if selected_bytes
                .saturating_add(rendered.len())
                .saturating_add(OMISSION_RESERVE_BYTES)
                <= limit
            {
                selected_bytes = selected_bytes.saturating_add(rendered.len());
                selected.insert(index);
            }
        }

        let mut body = Vec::with_capacity(limit);
        append_section_heading(&mut body, self.class, newline);
        let mut retained_lines = Vec::with_capacity(selected.len());
        let mut first_omitted = self.first_omitted;
        let mut last_omitted = self.last_omitted;
        for (index, record) in candidates.iter().enumerate() {
            if selected.contains(&index) {
                render_record(&mut body, record, newline);
                retained_lines.push(record.span.first_line);
            } else {
                merge_omitted_range(&mut first_omitted, &mut last_omitted, record.span);
            }
        }
        let represented_or_retained = represented_count.saturating_add(selected.len());
        let omitted = self
            .total
            .saturating_sub(u64::try_from(represented_or_retained).unwrap_or(u64::MAX));
        append_omission(
            &mut body,
            self.class,
            omitted,
            first_omitted,
            last_omitted,
            newline,
        );
        body.truncate(limit);
        RenderedBucket {
            body,
            retained_lines,
        }
    }
}

#[derive(Debug, Default)]
struct RenderedBucket {
    body: Vec<u8>,
    retained_lines: Vec<u64>,
}

fn append_section_heading(output: &mut Vec<u8>, class: EvidenceClass, newline: &[u8]) {
    output.extend_from_slice(format!("== {} ==", class.title()).as_bytes());
    output.extend_from_slice(newline);
}

fn append_omission(
    output: &mut Vec<u8>,
    class: EvidenceClass,
    omitted: u64,
    first_omitted: Option<u64>,
    last_omitted: Option<u64>,
    newline: &[u8],
) {
    if omitted == 0 {
        return;
    }
    let range = match (first_omitted, last_omitted) {
        (Some(first), Some(last)) => format!("; source lines {first}:{last}"),
        _ => String::new(),
    };
    output.extend_from_slice(
        format!("[yarp: omitted {omitted} {} line(s){range}]", class.title()).as_bytes(),
    );
    output.extend_from_slice(newline);
}

fn merge_omitted_range(first: &mut Option<u64>, last: &mut Option<u64>, span: SourceSpan) {
    *first = Some(first.map_or(span.first_line, |current| current.min(span.first_line)));
    *last = Some(last.map_or(span.last_line, |current| current.max(span.last_line)));
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
    if record.repeats > 1 {
        output.extend_from_slice(format!(" [repeated {} times]", record.repeats).as_bytes());
    }
    output.extend_from_slice(newline);
}

fn example_key<'a>(family: &str, body: &'a [u8]) -> Option<&'a [u8]> {
    let body = strip_line_ending(body);
    let key = match family {
        "search" | "build" => body.split(|byte| *byte == b':').next()?,
        "test" => body
            .windows(3)
            .position(|window| window == b"...")
            .map_or(body, |position| &body[..position]),
        "list" => {
            let item = body
                .rsplit(u8::is_ascii_whitespace)
                .find(|part| !part.is_empty())?;
            let separator = item.iter().position(|byte| *byte == b'/')?;
            &item[..separator]
        }
        _ => return None,
    };
    if key.is_empty() {
        return None;
    }
    Some(&key[..key.len().min(128)])
}

fn record_fingerprint(body: &[u8]) -> (u64, u64, usize) {
    let mut first = 0xcbf2_9ce4_8422_2325_u64;
    let mut second = 0x9e37_79b9_7f4a_7c15_u64;
    for byte in body {
        first ^= u64::from(*byte);
        first = first.wrapping_mul(0x0000_0100_0000_01b3);
        second ^= u64::from(*byte).wrapping_add(0x9e37_79b9);
        second = second.rotate_left(7).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    }
    (first, second, body.len())
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
    example_keys: BTreeMap<(u64, u64, usize), Vec<u8>>,
}

impl EvidenceCollector {
    #[must_use]
    pub fn new(family: &'static str, policy: OutputPolicy) -> Self {
        let classes = [
            EvidenceClass::Diagnostic,
            EvidenceClass::Outcome,
            EvidenceClass::Structure,
            EvidenceClass::Context,
            EvidenceClass::Example,
        ];
        let buckets = classes
            .into_iter()
            .map(|class| Bucket::new(class, policy.max_output_bytes))
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
            example_keys: BTreeMap::new(),
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
        let span = SourceSpan {
            first_line: line_number,
            last_line: line_number,
        };
        if class == EvidenceClass::Example && !source_term_in_bounds {
            if self
                .buckets
                .get_mut(EXAMPLE_BUCKET_INDEX)
                .is_some_and(|bucket| bucket.observe_repeat(&body, span))
            {
                return;
            }
            if let Some(key) = example_key(self.family, &body) {
                let fingerprint = record_fingerprint(key);
                match self.example_keys.get(&fingerprint) {
                    Some(existing) if existing.as_slice() == key => {
                        self.noise_lines = self.noise_lines.saturating_add(1);
                        return;
                    }
                    None if self.example_keys.len() < MAX_RECORDS_PER_CLASS => {
                        self.example_keys.insert(fingerprint, key.to_vec());
                    }
                    Some(_) | None => {}
                }
            }
        }
        let record = EvidenceRecord {
            span,
            body,
            repeats: 1,
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
        success: bool,
    ) -> Vec<u8> {
        let has_typed_diagnostics = self.buckets.first().is_some_and(|bucket| bucket.total > 0);
        let mut priority_terms = self
            .priority_terms
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        priority_terms.sort_by_key(|record| record.span.first_line);
        priority_terms.dedup_by_key(|record| record.span.first_line);
        let represented = priority_terms
            .iter()
            .map(|record| record.span.first_line)
            .collect::<BTreeSet<_>>();
        let source_term_samples = render_source_term_samples(&priority_terms, &self.newline);
        let mandatory_bytes = source_term_samples.len();
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
        let provisional_prefix = render_summary_prefix(
            self.family,
            self.total_lines,
            &diagnostic_suffix,
            self.total_lines,
            self.truncated_lines,
            recovery,
            &self.newline,
        );
        let compact_limit = self
            .policy
            .max_output_bytes
            .min(COMPACT_SUCCESS_SUMMARY_BYTES);
        let compact = success
            && !has_typed_diagnostics
            && registered.is_empty()
            && priority_terms.is_empty()
            && provisional_prefix.len().saturating_add(mandatory_bytes) <= compact_limit;
        let output_limit = if compact {
            compact_limit
        } else {
            self.policy.max_output_bytes
        };
        let structural_markers = render_structural_markers(
            self.noise_lines,
            self.truncated_lines,
            &self.newline,
            recovery.is_none(),
        );
        let available = output_limit
            .saturating_sub(provisional_prefix.len())
            .saturating_sub(mandatory_bytes);
        let (sections, omitted) = self.render_ranked_sections(
            available,
            &represented,
            &source_term_samples,
            &structural_markers,
        );
        let prefix = render_summary_prefix(
            self.family,
            self.total_lines,
            &diagnostic_suffix,
            omitted,
            self.truncated_lines,
            recovery,
            &self.newline,
        );
        let mut output = Vec::with_capacity(output_limit.min(64 * 1024));
        output.extend_from_slice(&prefix);
        output.extend_from_slice(&sections);
        if output.len() > output_limit {
            output.truncate(output_limit);
        }
        output
    }

    fn render_ranked_sections(
        &self,
        mut available: usize,
        represented: &BTreeSet<u64>,
        source_term_samples: &[u8],
        structural_markers: &[u8],
    ) -> (Vec<u8>, u64) {
        let mut sections = Vec::with_capacity(available.min(64 * 1024));
        let mut retained_lines = represented.iter().copied().collect::<Vec<_>>();
        for class in [
            EvidenceClass::Outcome,
            EvidenceClass::Diagnostic,
            EvidenceClass::Context,
        ] {
            append_ranked_class(
                &self.buckets,
                class,
                &mut sections,
                &mut retained_lines,
                &mut available,
                &self.newline,
                represented,
            );
        }
        sections.extend_from_slice(source_term_samples);
        append_ranked_class(
            &self.buckets,
            EvidenceClass::Structure,
            &mut sections,
            &mut retained_lines,
            &mut available,
            &self.newline,
            represented,
        );
        if structural_markers.len() <= available {
            available = available.saturating_sub(structural_markers.len());
            sections.extend_from_slice(structural_markers);
        }
        append_ranked_class(
            &self.buckets,
            EvidenceClass::Example,
            &mut sections,
            &mut retained_lines,
            &mut available,
            &self.newline,
            represented,
        );
        retained_lines.sort_unstable();
        retained_lines.dedup();
        let omitted = self
            .total_lines
            .saturating_sub(u64::try_from(retained_lines.len()).unwrap_or(u64::MAX));
        (sections, omitted)
    }
}

fn append_ranked_class(
    buckets: &[Bucket],
    class: EvidenceClass,
    output: &mut Vec<u8>,
    retained_lines: &mut Vec<u64>,
    available: &mut usize,
    newline: &[u8],
    represented: &BTreeSet<u64>,
) {
    let Some(bucket) = buckets.iter().find(|bucket| bucket.class == class) else {
        return;
    };
    let rendered = bucket.render_ranked(*available, newline, represented);
    *available = available.saturating_sub(rendered.body.len());
    output.extend_from_slice(&rendered.body);
    retained_lines.extend(rendered.retained_lines);
}

fn render_source_term_samples(priority_terms: &[EvidenceRecord], newline: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    if !priority_terms.is_empty() {
        output.extend_from_slice(b"== source term samples ==");
        output.extend_from_slice(newline);
        for record in priority_terms {
            render_record(&mut output, record, newline);
        }
    }
    output
}

fn render_structural_markers(
    noise_lines: u64,
    truncated_lines: u64,
    newline: &[u8],
    include: bool,
) -> Vec<u8> {
    let mut output = Vec::new();
    if !include {
        return output;
    }
    if noise_lines > 0 {
        output
            .extend_from_slice(format!("[yarp: omitted {noise_lines} routine line(s)]").as_bytes());
        output.extend_from_slice(newline);
    }
    if truncated_lines > 0 {
        output.extend_from_slice(
            format!("[yarp: truncated {truncated_lines} retained line(s)]").as_bytes(),
        );
        output.extend_from_slice(newline);
    }
    output
}

fn render_summary_prefix(
    family: &str,
    total_lines: u64,
    diagnostic_suffix: &str,
    omitted: u64,
    truncated: u64,
    recovery: Option<RecoveryMarker<'_>>,
    newline: &[u8],
) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(
        format!("[yarp summary: {family}; source_lines={total_lines}{diagnostic_suffix}]")
            .as_bytes(),
    );
    output.extend_from_slice(newline);
    if omitted > 0 || truncated > 0 || recovery.is_some() {
        append_recovery_marker(&mut output, newline, omitted, truncated, recovery);
    }
    output
}

fn diagnostic_representative(record: &EvidenceRecord, term: &[u8]) -> EvidenceRecord {
    const MAX_REPRESENTATIVE_BYTES: usize = 32;
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
        repeats: record.repeats,
    }
}

fn append_recovery_marker(
    output: &mut Vec<u8>,
    newline: &[u8],
    omitted: u64,
    truncated: u64,
    recovery: Option<RecoveryMarker<'_>>,
) {
    let description = if omitted == 0 && truncated == 0 {
        "exact output archived".to_owned()
    } else if truncated == 0 {
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
            false,
        ))
        .expect("UTF-8");
        assert!(output.contains("== outcome ==\r\n"));
        assert!(output.contains("L2: error: failed"));
        assert!(output.contains("yarp search yr_0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn changed_output_keeps_recovery_even_when_no_line_is_omitted() {
        let mut collector = EvidenceCollector::new("test", policy());
        collector.observe(1, &line("error: failed\n"), EvidenceClass::Diagnostic);
        let output = String::from_utf8(collector.render(
            Some(RecoveryMarker {
                archive_ref: "yr_0123456789abcdef0123456789abcdef",
                source: "stderr",
                completeness: "complete",
            }),
            [false; 5],
            false,
        ))
        .expect("UTF-8");
        assert!(
            output
                .contains("[yarp: exact output archived; ref=yr_0123456789abcdef0123456789abcdef")
        );
        assert!(
            output
                .contains("Search omitted output: yarp search yr_0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn compact_output_keeps_the_complete_recovery_instruction() {
        let mut compact = policy();
        compact.max_output_bytes = 704;
        let mut collector = EvidenceCollector::new("search", compact);
        for index in 0..1_000 {
            collector.observe(
                index + 1,
                &line(&format!("src/file{}.rs:{index}: match\n", index % 40)),
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
            true,
        );
        let text = String::from_utf8_lossy(&output);
        assert!(output.len() <= COMPACT_SUCCESS_SUMMARY_BYTES);
        assert!(text.contains(
            "Search omitted output: yarp search yr_0123456789abcdef0123456789abcdef 'term|alternate'"
        ));
        assert!(text.contains("L1: src/file0.rs:0: match"));
    }

    #[test]
    fn source_term_samples_disable_the_compact_cap_without_tracker_matches() {
        let mut compact = policy();
        compact.max_output_bytes = 704;
        let mut collector = EvidenceCollector::new("test", compact);
        for (index, value) in [
            "failure from source text",
            "panic from source text",
            "error from source text",
            "warning from source text",
            "test result from source text",
        ]
        .into_iter()
        .enumerate()
        {
            collector.observe(
                u64::try_from(index + 1).expect("line"),
                &line(&format!("prefix {value} with enough surrounding detail\n")),
                EvidenceClass::Example,
            );
        }
        let output = String::from_utf8_lossy(&collector.render(
            Some(RecoveryMarker {
                archive_ref: "yr_0123456789abcdef0123456789abcdef",
                source: "result_text",
                completeness: "complete",
            }),
            [false; 5],
            true,
        ))
        .to_ascii_lowercase();
        for term in ["failure", "panic", "error", "warning", "test result"] {
            assert!(output.contains(term), "missing {term}: {output}");
        }
        assert!(output.len() <= 704);
    }

    #[test]
    fn keeps_one_source_line_for_every_registered_diagnostic_category() {
        let mut compact = policy();
        compact.max_output_bytes = 704;
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
        let output = String::from_utf8_lossy(&collector.render(
            Some(RecoveryMarker {
                archive_ref: "yr_0123456789abcdef0123456789abcdef",
                source: "result_text",
                completeness: "complete",
            }),
            [true; 5],
            true,
        ))
        .to_ascii_lowercase();
        for term in ["error", "warning", "panic", "failure", "test result"] {
            assert!(output.contains(term), "missing {term}: {output}");
        }
        assert!(output.contains("yarp search yr_0123456789abcdef0123456789abcdef"));
        assert!(output.len() <= 704);
    }

    #[test]
    fn collapses_exact_repetition_with_a_bounded_counter() {
        let mut collector = EvidenceCollector::new("list", policy());
        for index in 0..1_000 {
            collector.observe(index + 1, &line("same record\n"), EvidenceClass::Example);
        }
        let output = String::from_utf8(collector.render(None, [false; 5], true)).expect("UTF-8");
        assert!(output.contains("repeated 1000 times"));
        assert_eq!(output.matches("same record").count(), 1);
    }

    #[test]
    fn ranks_outcomes_before_diagnostics_and_examples() {
        let mut collector = EvidenceCollector::new("test", policy());
        collector.observe(1, &line("example row\n"), EvidenceClass::Example);
        collector.observe(2, &line("error: failed\n"), EvidenceClass::Diagnostic);
        collector.observe(3, &line("test result: FAILED\n"), EvidenceClass::Outcome);
        let output = String::from_utf8(collector.render(None, [false; 5], false)).expect("UTF-8");
        assert!(
            output.find("== outcome ==").expect("outcome")
                < output.find("== diagnostics ==").expect("diagnostics")
        );
        assert!(
            output.find("== diagnostics ==").expect("diagnostics")
                < output
                    .find("== source term samples ==")
                    .expect("source term samples")
        );
        assert!(
            output
                .find("== source term samples ==")
                .expect("source term samples")
                < output.find("== examples ==").expect("examples")
        );
    }

    #[test]
    fn prefers_different_search_files() {
        let mut collector = EvidenceCollector::new("search", policy());
        for index in 0..100 {
            collector.observe(
                index + 1,
                &line(&format!("src/one.rs:{index}: match {index}\n")),
                EvidenceClass::Example,
            );
        }
        collector.observe(
            101,
            &line("src/two.rs:1: another match\n"),
            EvidenceClass::Example,
        );
        let output = String::from_utf8(collector.render(None, [false; 5], true)).expect("UTF-8");
        assert!(output.contains("src/one.rs"));
        assert!(output.contains("src/two.rs"));
    }

    #[test]
    fn prefers_different_list_groups() {
        let mut collector = EvidenceCollector::new("list", policy());
        collector.observe(1, &line("group/one\n"), EvidenceClass::Example);
        collector.observe(2, &line("group/two\n"), EvidenceClass::Example);
        collector.observe(3, &line("other/three\n"), EvidenceClass::Example);
        let output = String::from_utf8(collector.render(None, [false; 5], true)).expect("UTF-8");
        assert!(output.contains("group/one"));
        assert!(!output.contains("group/two"));
        assert!(output.contains("other/three"));
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
        let output = collector.render(None, [false; 5], true);
        assert!(output.len() <= policy().max_output_bytes);
        assert!(String::from_utf8_lossy(&output).contains("omitted"));
    }
}
