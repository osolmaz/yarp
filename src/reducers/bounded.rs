use std::collections::VecDeque;

use yarp_rule_pack::OutputPolicy;

const MARKER_RESERVE: usize = 160;
const LINE_MARKER_RESERVE: usize = 96;

#[derive(Clone, Debug)]
pub struct LineView {
    pub prefix: Vec<u8>,
    pub tail: VecDeque<u8>,
    pub total_bytes: usize,
    pub truncated: bool,
}

impl LineView {
    #[must_use]
    pub fn full_bytes(&self) -> Option<&[u8]> {
        (!self.truncated).then_some(self.prefix.as_slice())
    }

    #[must_use]
    pub fn tail_bytes(&self) -> Vec<u8> {
        if self.truncated {
            self.tail.iter().copied().collect()
        } else {
            self.prefix.clone()
        }
    }
}

#[derive(Debug)]
pub struct LineAccumulator {
    prefix: Vec<u8>,
    tail: VecDeque<u8>,
    total_bytes: usize,
    prefix_limit: usize,
    tail_limit: usize,
}

impl LineAccumulator {
    #[must_use]
    pub fn new(prefix_limit: usize, tail_limit: usize) -> Self {
        Self {
            prefix: Vec::with_capacity(prefix_limit.min(64 * 1024)),
            tail: VecDeque::with_capacity(tail_limit.min(64 * 1024)),
            total_bytes: 0,
            prefix_limit,
            tail_limit,
        }
    }

    pub fn push(&mut self, byte: u8) {
        self.total_bytes = self.total_bytes.saturating_add(1);
        if self.prefix.len() < self.prefix_limit {
            self.prefix.push(byte);
        }
        if self.tail_limit > 0 {
            if self.tail.len() == self.tail_limit {
                self.tail.pop_front();
            }
            self.tail.push_back(byte);
        }
    }

    pub fn take(&mut self) -> LineView {
        let total_bytes = self.total_bytes;
        self.total_bytes = 0;
        LineView {
            prefix: std::mem::replace(
                &mut self.prefix,
                Vec::with_capacity(self.prefix_limit.min(64 * 1024)),
            ),
            tail: std::mem::replace(
                &mut self.tail,
                VecDeque::with_capacity(self.tail_limit.min(64 * 1024)),
            ),
            total_bytes,
            truncated: total_bytes > self.prefix_limit,
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total_bytes == 0
    }
}

#[derive(Debug)]
pub struct Retention {
    policy: OutputPolicy,
    head: Vec<Vec<u8>>,
    tail: VecDeque<Vec<u8>>,
    head_bytes: usize,
    tail_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    head_closed: bool,
    total_lines: usize,
    truncated_lines: usize,
    newline: Vec<u8>,
}

impl Retention {
    #[must_use]
    pub fn new(policy: OutputPolicy) -> Self {
        let content_budget = policy.max_output_bytes.saturating_sub(MARKER_RESERVE);
        let requested_lines = policy.head_lines.saturating_add(policy.tail_lines);
        let (head_budget, tail_budget) = if requested_lines == 0 {
            (0, 0)
        } else if policy.tail_lines == 0 {
            (content_budget, 0)
        } else if policy.head_lines == 0 {
            (0, content_budget)
        } else {
            let head_budget = content_budget
                .saturating_mul(policy.head_lines)
                .checked_div(requested_lines)
                .unwrap_or(0);
            (head_budget, content_budget.saturating_sub(head_budget))
        };
        Self {
            policy,
            head: Vec::new(),
            tail: VecDeque::new(),
            head_bytes: 0,
            tail_bytes: 0,
            head_budget,
            tail_budget,
            head_closed: false,
            total_lines: 0,
            truncated_lines: 0,
            newline: b"\n".to_vec(),
        }
    }

    pub fn observe(&mut self, line: &LineView, keep: bool, two_sided: bool) {
        self.total_lines = self.total_lines.saturating_add(1);
        self.observe_newline(line);
        if !keep {
            return;
        }
        let (rendered, truncated) = render_line(line, self.policy.max_line_bytes, two_sided);
        self.truncated_lines = self.truncated_lines.saturating_add(usize::from(truncated));
        if !self.head_closed && self.head.len() < self.policy.head_lines {
            if self.head_bytes.saturating_add(rendered.len()) <= self.head_budget {
                self.head_bytes = self.head_bytes.saturating_add(rendered.len());
                self.head.push(rendered);
                return;
            }
            self.head_closed = true;
        }
        if self.policy.tail_lines == 0 || rendered.len() > self.tail_budget {
            return;
        }
        while self.tail.len() >= self.policy.tail_lines
            || self.tail_bytes.saturating_add(rendered.len()) > self.tail_budget
        {
            let Some(removed) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes = self.tail_bytes.saturating_sub(removed.len());
        }
        if self.tail.len() < self.policy.tail_lines
            && self.tail_bytes.saturating_add(rendered.len()) <= self.tail_budget
        {
            self.tail_bytes = self.tail_bytes.saturating_add(rendered.len());
            self.tail.push_back(rendered);
        }
    }

    #[must_use]
    pub fn render(self) -> Vec<u8> {
        let retained = self.head.len().saturating_add(self.tail.len());
        let omitted = self.total_lines.saturating_sub(retained);
        let mut output = Vec::with_capacity(self.policy.max_output_bytes);
        for line in self.head {
            output.extend_from_slice(&line);
        }
        if omitted > 0 || self.truncated_lines > 0 {
            ensure_line_boundary(&mut output, &self.newline);
            let marker = match (omitted, self.truncated_lines) {
                (0, truncated) => format!("[yarp: truncated {truncated} lines]"),
                (omitted, 0) => format!("[yarp: omitted {omitted} lines]"),
                (omitted, truncated) => {
                    format!("[yarp: omitted {omitted} lines; truncated {truncated} lines]")
                }
            };
            output.extend_from_slice(marker.as_bytes());
            output.extend_from_slice(&self.newline);
        }
        for line in self.tail {
            output.extend_from_slice(&line);
        }
        if output.len() > self.policy.max_output_bytes {
            output.truncate(self.policy.max_output_bytes);
        }
        output
    }

    fn observe_newline(&mut self, line: &LineView) {
        if line.prefix.ends_with(b"\r\n") {
            self.newline = b"\r\n".to_vec();
        } else if line.prefix.ends_with(b"\n") {
            self.newline = b"\n".to_vec();
        }
    }
}

#[derive(Debug)]
pub struct ShortRaw {
    body: Option<Vec<u8>>,
    limit: usize,
    total_bytes: usize,
}

impl ShortRaw {
    #[must_use]
    pub fn new(success: OutputPolicy, failure: OutputPolicy) -> Self {
        let success_limit = success
            .max_output_bytes
            .saturating_add(success.min_savings_bytes);
        let failure_limit = failure
            .max_output_bytes
            .saturating_add(failure.min_savings_bytes);
        let limit = success_limit.max(failure_limit);
        Self {
            body: Some(Vec::with_capacity(limit.min(64 * 1024))),
            limit,
            total_bytes: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        let Some(body) = &mut self.body else {
            return;
        };
        if body.len().saturating_add(chunk.len()) <= self.limit {
            body.extend_from_slice(chunk);
        } else {
            self.body = None;
        }
    }

    #[must_use]
    pub fn choose(self, reduced: Vec<u8>, policy: OutputPolicy) -> Vec<u8> {
        let Some(raw) = self.body else {
            return reduced;
        };
        if raw.len() != self.total_bytes {
            return reduced;
        }
        let savings = raw.len().saturating_sub(reduced.len());
        if reduced.len() >= raw.len() || savings < policy.min_savings_bytes {
            raw
        } else {
            reduced
        }
    }
}

fn render_line(line: &LineView, limit: usize, two_sided: bool) -> (Vec<u8>, bool) {
    if line.total_bytes <= limit {
        return (line.prefix.clone(), false);
    }
    let tail = line.tail_bytes();
    let newline = if line.prefix.ends_with(b"\r\n") || tail.ends_with(b"\r\n") {
        &b"\r\n"[..]
    } else if line.prefix.ends_with(b"\n") || tail.ends_with(b"\n") {
        &b"\n"[..]
    } else {
        &[][..]
    };
    let mut omitted = line.total_bytes.saturating_sub(limit);
    let (marker, available) = loop {
        let marker = format!("[yarp: line truncated; {omitted} bytes omitted]");
        let available = limit
            .saturating_sub(marker.len().min(LINE_MARKER_RESERVE))
            .saturating_sub(newline.len());
        let actual_omitted = line
            .total_bytes
            .saturating_sub(available.saturating_add(newline.len()));
        if actual_omitted == omitted {
            break (marker, available);
        }
        omitted = actual_omitted;
    };
    let mut output = Vec::with_capacity(limit);
    if two_sided {
        let first = available / 2;
        let last = available.saturating_sub(first);
        output.extend_from_slice(&line.prefix[..first.min(line.prefix.len())]);
        output.extend_from_slice(marker.as_bytes());
        let start = tail
            .len()
            .saturating_sub(last.saturating_add(newline.len()));
        let end = tail.len().saturating_sub(newline.len());
        if start < end {
            output.extend_from_slice(&tail[start..end]);
        }
    } else {
        output.extend_from_slice(&line.prefix[..available.min(line.prefix.len())]);
        output.extend_from_slice(marker.as_bytes());
    }
    output.extend_from_slice(newline);
    output.truncate(limit);
    (output, true)
}

fn ensure_line_boundary(output: &mut Vec<u8>, newline: &[u8]) {
    if !output.is_empty() && !output.ends_with(b"\n") {
        output.extend_from_slice(newline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> OutputPolicy {
        OutputPolicy {
            head_lines: 2,
            tail_lines: 1,
            max_line_bytes: 256,
            max_output_bytes: 1_024,
            min_savings_bytes: 8,
        }
    }

    fn line(value: &[u8]) -> LineView {
        LineView {
            prefix: value.to_vec(),
            tail: value.iter().copied().collect(),
            total_bytes: value.len(),
            truncated: false,
        }
    }

    #[test]
    fn keeps_bounded_head_and_tail() {
        let mut retained = Retention::new(policy());
        for value in [
            &b"one\n"[..],
            &b"two\n"[..],
            &b"three\n"[..],
            &b"four\n"[..],
        ] {
            retained.observe(&line(value), true, false);
        }
        assert_eq!(
            retained.render(),
            b"one\ntwo\n[yarp: omitted 1 lines]\nfour\n"
        );
    }

    #[test]
    fn short_raw_requires_minimum_savings() {
        let mut raw = ShortRaw::new(policy(), policy());
        raw.push(b"short\n");
        assert_eq!(raw.choose(b"tiny\n".to_vec(), policy()), b"short\n");
    }
}
