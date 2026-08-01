use std::collections::VecDeque;

use super::bounded::LineView;

const CONTEXT_LINES: usize = 3;

#[derive(Debug, Default)]
pub struct GitDiffState {
    pending_context: VecDeque<LineView>,
    after_change: usize,
}

impl GitDiffState {
    pub fn push(
        &mut self,
        line: LineView,
        mut keep: impl FnMut(LineView),
        mut omit: impl FnMut(LineView),
    ) {
        let body = line
            .prefix
            .strip_suffix(b"\n")
            .unwrap_or(line.prefix.as_slice());
        if is_change(body) {
            while let Some(context) = self.pending_context.pop_front() {
                keep(context);
            }
            keep(line);
            self.after_change = CONTEXT_LINES;
        } else if body.starts_with(b" ") {
            if self.after_change > 0 {
                keep(line);
                self.after_change -= 1;
            } else {
                if self.pending_context.len() == CONTEXT_LINES
                    && let Some(oldest) = self.pending_context.pop_front()
                {
                    omit(oldest);
                }
                self.pending_context.push_back(line);
            }
        } else {
            while let Some(context) = self.pending_context.pop_front() {
                omit(context);
            }
            keep(line);
            self.after_change = 0;
        }
    }

    pub fn finish(mut self, mut keep: impl FnMut(LineView)) {
        while let Some(context) = self.pending_context.pop_front() {
            keep(context);
        }
    }
}

fn is_change(line: &[u8]) -> bool {
    (line.starts_with(b"+") && !line.starts_with(b"+++"))
        || (line.starts_with(b"-") && !line.starts_with(b"---"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn line(value: &str) -> LineView {
        LineView {
            prefix: value.as_bytes().to_vec(),
            tail: VecDeque::from(value.as_bytes().to_vec()),
            line_ending: if value.ends_with('\n') {
                b"\n".to_vec()
            } else {
                Vec::new()
            },
            total_bytes: value.len(),
            truncated: false,
        }
    }

    #[test]
    fn keeps_three_context_lines_around_changes() {
        let mut state = GitDiffState::default();
        let mut kept = Vec::new();
        let mut omitted = Vec::new();
        for value in [" a\n", " b\n", " c\n", " d\n", "+change\n", " e\n"] {
            state.push(
                line(value),
                |line| kept.push(line),
                |line| omitted.push(line),
            );
        }
        state.finish(|line| kept.push(line));
        assert_eq!(omitted.len(), 1);
        assert_eq!(kept.len(), 5);
        assert_eq!(kept[0].prefix, b" b\n");
    }
}
