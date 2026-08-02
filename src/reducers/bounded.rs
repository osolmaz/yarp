use std::collections::VecDeque;

use yarp_rule_pack::OutputPolicy;

const LINE_MARKER_RESERVE: usize = 96;

#[derive(Clone, Debug)]
pub struct LineView {
    pub prefix: Vec<u8>,
    pub tail: VecDeque<u8>,
    pub line_ending: Vec<u8>,
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
    previous_byte: Option<u8>,
    last_byte: Option<u8>,
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
            previous_byte: None,
            last_byte: None,
        }
    }

    pub fn observe_source(&mut self, byte: u8) -> bool {
        self.total_bytes = self.total_bytes.saturating_add(1);
        self.previous_byte = self.last_byte;
        self.last_byte = Some(byte);
        self.total_bytes >= self.prefix_limit.saturating_sub(10)
    }

    pub fn push_output(&mut self, byte: u8) {
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
        let line_ending = match (self.previous_byte.take(), self.last_byte.take()) {
            (Some(b'\r'), Some(b'\n')) => b"\r\n".to_vec(),
            (_, Some(b'\n')) => b"\n".to_vec(),
            _ => Vec::new(),
        };
        LineView {
            prefix: std::mem::replace(
                &mut self.prefix,
                Vec::with_capacity(self.prefix_limit.min(64 * 1024)),
            ),
            tail: std::mem::replace(
                &mut self.tail,
                VecDeque::with_capacity(self.tail_limit.min(64 * 1024)),
            ),
            line_ending,
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
pub struct ShortRaw {
    body: Option<Vec<u8>>,
    limit: usize,
    total_bytes: usize,
}

impl ShortRaw {
    #[must_use]
    pub fn new(success: OutputPolicy, failure: OutputPolicy) -> Self {
        let limit = success.max_output_bytes.max(failure.max_output_bytes);
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
        if raw.len() > policy.max_output_bytes {
            return reduced;
        }
        let savings = raw.len().saturating_sub(reduced.len());
        let proportional = (savings as u128).saturating_mul(10_000)
            >= (raw.len() as u128).saturating_mul(u128::from(policy.min_savings_basis_points));
        if reduced.len() >= raw.len() || savings < policy.min_savings_bytes || !proportional {
            raw
        } else {
            reduced
        }
    }
}

pub(crate) fn render_line(line: &LineView, limit: usize, two_sided: bool) -> (Vec<u8>, bool) {
    if line.total_bytes <= limit {
        return (line.prefix.clone(), false);
    }
    let tail = line.tail_bytes();
    let newline = line.line_ending.as_slice();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> OutputPolicy {
        OutputPolicy {
            max_line_bytes: 256,
            max_output_bytes: 1_024,
            min_savings_bytes: 8,
            min_savings_basis_points: 1_000,
        }
    }

    #[test]
    fn preserves_the_ending_after_a_truncated_line() {
        let mut accumulator = LineAccumulator::new(256, 0);
        for byte in b"a".repeat(300).into_iter().chain([b'\n']) {
            accumulator.observe_source(byte);
            accumulator.push_output(byte);
        }
        let line = accumulator.take();
        assert_eq!(line.line_ending, b"\n");
        assert!(render_line(&line, 256, false).0.ends_with(b"]\n"));
    }

    #[test]
    fn short_raw_requires_both_savings_gates() {
        let mut raw = ShortRaw::new(policy(), policy());
        raw.push(b"short\n");
        assert_eq!(raw.choose(b"tiny\n".to_vec(), policy()), b"short\n");

        let mut strict = policy();
        strict.min_savings_basis_points = 9_000;
        let mut raw = ShortRaw::new(strict, strict);
        raw.push(b"1234567890");
        assert_eq!(raw.choose(b"123456".to_vec(), strict), b"1234567890");

        let input = vec![b'x'; policy().max_output_bytes + 1];
        let mut raw = ShortRaw::new(policy(), policy());
        raw.push(&input);
        assert_eq!(raw.choose(b"reduced".to_vec(), policy()), b"reduced");
    }
}
