use yarp_rule_pack::{LinePattern, PatternCase, PatternKind, PatternTrim};

use super::bounded::LineView;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AnsiState {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

#[derive(Debug)]
pub struct AnsiStripper {
    state: AnsiState,
}

impl AnsiStripper {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AnsiState::Ground,
        }
    }

    pub fn push_byte(&mut self, byte: u8) -> Option<u8> {
        if matches!(byte, b'\r' | b'\n') {
            return Some(byte);
        }
        match self.state {
            AnsiState::Ground if byte == 0x1b => self.state = AnsiState::Escape,
            AnsiState::Ground => return Some(byte),
            AnsiState::Escape => {
                self.state = match byte {
                    b'[' => AnsiState::Csi,
                    b']' => AnsiState::Osc,
                    _ => AnsiState::Ground,
                };
            }
            AnsiState::Csi if (0x40..=0x7e).contains(&byte) => {
                self.state = AnsiState::Ground;
            }
            AnsiState::Osc if byte == 0x07 => self.state = AnsiState::Ground,
            AnsiState::Osc if byte == 0x1b => self.state = AnsiState::OscEscape,
            AnsiState::Csi | AnsiState::Osc => {}
            AnsiState::OscEscape if byte == b'\\' => self.state = AnsiState::Ground,
            AnsiState::OscEscape if byte == 0x1b => {}
            AnsiState::OscEscape => self.state = AnsiState::Osc,
        }
        None
    }
}

#[must_use]
pub fn line_filter_keeps(line: &LineView, drop: &[LinePattern], keep: &[LinePattern]) -> bool {
    if drop.iter().any(|pattern| pattern_matches(pattern, line)) {
        return false;
    }
    keep.is_empty() || keep.iter().any(|pattern| pattern_matches(pattern, line)) || line.truncated
}

#[must_use]
pub fn cargo_test_keeps(line: &LineView) -> bool {
    let body = trim_start(strip_line_ending(&line.prefix));
    let routine = [
        &b"Compiling "[..],
        &b"Checking "[..],
        &b"Fresh "[..],
        &b"Downloaded "[..],
        &b"Downloading "[..],
    ];
    let diagnostic = [
        &b"error"[..],
        &b"failed"[..],
        &b"failure"[..],
        &b"warning"[..],
        &b"panic"[..],
        &b"test result"[..],
    ];
    !routine.iter().any(|prefix| body.starts_with(prefix))
        || diagnostic
            .iter()
            .any(|needle| contains_ascii_insensitive(body, needle))
}

#[must_use]
pub fn pattern_matches(pattern: &LinePattern, line: &LineView) -> bool {
    match pattern.kind {
        PatternKind::Exact => line.full_bytes().is_some_and(|body| {
            compare(
                apply_trim(strip_line_ending(body), pattern.trim),
                pattern.value.as_bytes(),
                pattern.case,
                PatternKind::Exact,
            )
        }),
        PatternKind::Prefix | PatternKind::Contains => {
            let body = strip_line_ending(&line.prefix);
            let body = if line.truncated {
                match pattern.trim {
                    PatternTrim::None => body,
                    PatternTrim::Start | PatternTrim::Both => trim_start(body),
                }
            } else {
                apply_trim(body, pattern.trim)
            };
            compare(body, pattern.value.as_bytes(), pattern.case, pattern.kind)
        }
        PatternKind::Suffix => {
            let tail = line.tail_bytes();
            let body = strip_line_ending(&tail);
            let body = if line.truncated {
                match pattern.trim {
                    PatternTrim::Both => trim_end(body),
                    PatternTrim::None | PatternTrim::Start => body,
                }
            } else {
                apply_trim(body, pattern.trim)
            };
            compare(
                body,
                pattern.value.as_bytes(),
                pattern.case,
                PatternKind::Suffix,
            )
        }
    }
}

fn compare(body: &[u8], value: &[u8], case: PatternCase, kind: PatternKind) -> bool {
    match case {
        PatternCase::Sensitive => match kind {
            PatternKind::Exact => body == value,
            PatternKind::Prefix => body.starts_with(value),
            PatternKind::Suffix => body.ends_with(value),
            PatternKind::Contains => body.windows(value.len()).any(|window| window == value),
        },
        PatternCase::AsciiInsensitive => match kind {
            PatternKind::Exact => body.len() == value.len() && ascii_equal(body, value),
            PatternKind::Prefix => body
                .get(..value.len())
                .is_some_and(|candidate| ascii_equal(candidate, value)),
            PatternKind::Suffix => body
                .get(body.len().saturating_sub(value.len())..)
                .is_some_and(|candidate| {
                    candidate.len() == value.len() && ascii_equal(candidate, value)
                }),
            PatternKind::Contains => contains_ascii_insensitive(body, value),
        },
    }
}

fn contains_ascii_insensitive(body: &[u8], value: &[u8]) -> bool {
    !value.is_empty()
        && body
            .windows(value.len())
            .any(|candidate| ascii_equal(candidate, value))
}

fn ascii_equal(left: &[u8], right: &[u8]) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn apply_trim(body: &[u8], trim: PatternTrim) -> &[u8] {
    match trim {
        PatternTrim::None => body,
        PatternTrim::Start => trim_start(body),
        PatternTrim::Both => trim_end(trim_start(body)),
    }
}

fn trim_start(mut body: &[u8]) -> &[u8] {
    while body
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        body = &body[1..];
    }
    body
}

fn trim_end(mut body: &[u8]) -> &[u8] {
    while body.last().is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
        body = &body[..body.len() - 1];
    }
    body
}

fn strip_line_ending(body: &[u8]) -> &[u8] {
    body.strip_suffix(b"\r\n")
        .or_else(|| body.strip_suffix(b"\n"))
        .unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use yarp_rule_pack::{PatternCase, PatternKind, PatternTrim};

    use super::*;

    fn line(value: &[u8], truncated: bool) -> LineView {
        LineView {
            prefix: value.to_vec(),
            tail: VecDeque::from(value.to_vec()),
            line_ending: if value.ends_with(b"\n") {
                b"\n".to_vec()
            } else {
                Vec::new()
            },
            total_bytes: value.len() + usize::from(truncated),
            truncated,
        }
    }

    #[test]
    fn strips_chunked_ansi_sequences() {
        let mut stripper = AnsiStripper::new();
        let output = [b"before \x1b[3".as_slice(), b"1mred\x1b[0m after\n"]
            .into_iter()
            .flatten()
            .filter_map(|byte| stripper.push_byte(*byte))
            .collect::<Vec<_>>();
        assert_eq!(output, b"before red after\n");
    }

    #[test]
    fn matches_literal_patterns_without_regex() {
        let pattern = LinePattern {
            kind: PatternKind::Prefix,
            value: "warning:".to_owned(),
            case: PatternCase::AsciiInsensitive,
            trim: PatternTrim::Start,
        };
        assert!(pattern_matches(
            &pattern,
            &line(b"  WARNING: detail\n", false)
        ));
    }

    #[test]
    fn uncertain_truncated_keep_patterns_fail_open() {
        let keep = [LinePattern {
            kind: PatternKind::Suffix,
            value: "summary".to_owned(),
            case: PatternCase::Sensitive,
            trim: PatternTrim::None,
        }];
        assert!(line_filter_keeps(&line(b"long prefix", true), &[], &keep));
    }
}
