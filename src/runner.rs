use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

const HEAD_LINES: usize = 160;
const TAIL_LINES: usize = 40;
const MAX_LINE_BYTES: usize = 16 * 1024;

#[derive(Debug, Default)]
struct Captured {
    head: Vec<Vec<u8>>,
    tail: VecDeque<Vec<u8>>,
    total_lines: usize,
}

impl Captured {
    fn push_line(&mut self, line: Vec<u8>) {
        self.total_lines += 1;
        if self.head.len() < HEAD_LINES {
            self.head.push(line);
            return;
        }
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }

    fn render(self) -> Vec<u8> {
        let omitted = self
            .total_lines
            .saturating_sub(self.head.len() + self.tail.len());
        let mut output = Vec::new();
        for line in self.head {
            output.extend(line);
        }
        if omitted > 0 {
            ensure_newline(&mut output);
            output.extend(format!("[yarp: omitted {omitted} lines]\n").as_bytes());
        }
        for line in self.tail {
            output.extend(line);
        }
        output
    }
}

/// Run one allowlisted command, prune its two output streams, and return its exit code.
///
/// # Errors
///
/// Returns an error when the command is not allowed, cannot run, or its output cannot be read.
pub fn run(arguments: &[String]) -> Result<i32, String> {
    if !crate::rewrite::is_allowed_argv(arguments) {
        return Err("command is not on the YARP allowlist".to_owned());
    }

    let mut child = Command::new(&arguments[0])
        .args(&arguments[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", arguments[0]))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "could not capture child stdout".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "could not capture child stderr".to_owned())?;

    let stdout_thread = std::thread::spawn(move || capture(stdout));
    let stderr_thread = std::thread::spawn(move || capture(stderr));
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for child: {error}"))?;

    let stdout = join_capture(stdout_thread, "stdout")?;
    let stderr = join_capture(stderr_thread, "stderr")?;
    io::stdout()
        .write_all(&stdout.render())
        .map_err(|error| format!("could not write stdout: {error}"))?;
    io::stderr()
        .write_all(&stderr.render())
        .map_err(|error| format!("could not write stderr: {error}"))?;

    Ok(exit_code(status))
}

fn capture(mut reader: impl Read) -> io::Result<Captured> {
    let mut captured = Captured::default();
    let mut line = Vec::new();
    let mut line_was_truncated = false;
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if line.len() < MAX_LINE_BYTES {
                line.push(byte);
            } else {
                line_was_truncated = true;
            }
            if byte == b'\n' {
                finish_line(&mut captured, &mut line, &mut line_was_truncated);
            }
        }
    }

    if !line.is_empty() || line_was_truncated {
        finish_line(&mut captured, &mut line, &mut line_was_truncated);
    }
    Ok(captured)
}

fn finish_line(captured: &mut Captured, line: &mut Vec<u8>, was_truncated: &mut bool) {
    if *was_truncated {
        ensure_newline(line);
        line.extend(b"[yarp: line truncated]\n");
    }
    captured.push_line(std::mem::take(line));
    *was_truncated = false;
}

fn ensure_newline(output: &mut Vec<u8>) {
    if !output.is_empty() && !output.ends_with(b"\n") {
        output.push(b'\n');
    }
}

fn join_capture(
    thread: std::thread::JoinHandle<io::Result<Captured>>,
    stream: &str,
) -> Result<Captured, String> {
    thread
        .join()
        .map_err(|_| format!("{stream} capture thread panicked"))?
        .map_err(|error| format!("could not read child {stream}: {error}"))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map_or(1, |signal| 128 + signal)
    }

    #[cfg(not(unix))]
    {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::io::Cursor;

    #[test]
    fn leaves_short_output_unchanged() {
        let captured = capture(Cursor::new(b"one\ntwo\n")).expect("capture");
        assert_eq!(captured.render(), b"one\ntwo\n");
    }

    #[test]
    fn prunes_middle_lines() {
        let input = numbered_lines("line ", 250);
        let rendered = String::from_utf8(capture(Cursor::new(input)).expect("capture").render())
            .expect("UTF-8 output");
        assert!(rendered.starts_with("line 0\n"));
        assert!(rendered.contains("[yarp: omitted 50 lines]\n"));
        assert!(rendered.ends_with("line 249\n"));
        assert!(!rendered.contains("line 180\n"));
    }

    #[test]
    fn keeps_all_lines_at_the_boundary() {
        let input = numbered_lines("", 200);
        let rendered = capture(Cursor::new(input.as_bytes()))
            .expect("capture")
            .render();
        assert_eq!(rendered, input.as_bytes());
    }

    #[test]
    fn truncates_one_very_long_line_without_growing_unbounded() {
        let input = vec![b'x'; MAX_LINE_BYTES * 2];
        let rendered = capture(Cursor::new(input)).expect("capture").render();
        assert!(rendered.len() < MAX_LINE_BYTES + 64);
        assert!(rendered.ends_with(b"[yarp: line truncated]\n"));
    }

    #[test]
    fn handles_empty_and_unterminated_output() {
        assert!(
            capture(Cursor::new(Vec::<u8>::new()))
                .expect("capture")
                .render()
                .is_empty()
        );
        assert_eq!(
            capture(Cursor::new(b"last line"))
                .expect("capture")
                .render(),
            b"last line"
        );
    }

    #[test]
    fn rejects_disallowed_children() {
        let result = run(&["cat".to_owned(), ".env".to_owned()]);
        assert_eq!(
            result,
            Err("command is not on the YARP allowlist".to_owned())
        );
    }

    fn numbered_lines(prefix: &str, count: usize) -> String {
        let mut output = String::new();
        for line in 0..count {
            writeln!(&mut output, "{prefix}{line}").expect("write to string");
        }
        output
    }
}
