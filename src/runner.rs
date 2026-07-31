use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

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

struct CapturedStream {
    bounded: Captured,
    raw: Option<NamedTempFile>,
}

/// Run one allowlisted command, prune its two output streams, and return its exit code.
///
/// # Errors
///
/// Returns an error when the command is not allowed, cannot run, or its output cannot be read.
pub fn run(
    arguments: &[String],
    archive_key: Option<&crate::archive::ArchiveKey>,
) -> Result<i32, String> {
    if !crate::rewrite::is_allowed_argv(arguments) {
        return Err("command is not on the YARP allowlist".to_owned());
    }

    let (stdout_spool, stderr_spool) = if archive_key.is_some() {
        (
            Some(
                NamedTempFile::new()
                    .map_err(|error| format!("could not create stdout archive spool: {error}"))?,
            ),
            Some(
                NamedTempFile::new()
                    .map_err(|error| format!("could not create stderr archive spool: {error}"))?,
            ),
        )
    } else {
        (None, None)
    };

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

    let stdout_thread = std::thread::spawn(move || capture(stdout, stdout_spool));
    let stderr_thread = std::thread::spawn(move || capture(stderr, stderr_spool));
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for child: {error}"))?;

    let mut stdout = join_capture(stdout_thread, "stdout")?;
    let mut stderr = join_capture(stderr_thread, "stderr")?;
    let stdout_after = stdout.bounded.render();
    let stderr_after = stderr.bounded.render();

    if let Some(key) = archive_key {
        let stdout_raw = stdout
            .raw
            .as_mut()
            .ok_or_else(|| "stdout archive spool is missing".to_owned())?;
        let stderr_raw = stderr
            .raw
            .as_mut()
            .ok_or_else(|| "stderr archive spool is missing".to_owned())?;
        let archived = crate::archive::Archive::open().and_then(|mut archive| {
            archive.capture_streams(
                key,
                unix_time_ms(),
                stdout_raw.as_file_mut(),
                stderr_raw.as_file_mut(),
                &stdout_after,
                &stderr_after,
            )
        });
        if let Err(error) = archived {
            copy_raw(stdout_raw.as_file_mut(), &mut io::stdout(), "stdout")?;
            copy_raw(stderr_raw.as_file_mut(), &mut io::stderr(), "stderr")?;
            eprintln!("yarp: archive failed after command execution: {error}");
            return Ok(exit_code(status));
        }
    }

    io::stdout()
        .write_all(&stdout_after)
        .map_err(|error| format!("could not write stdout: {error}"))?;
    io::stderr()
        .write_all(&stderr_after)
        .map_err(|error| format!("could not write stderr: {error}"))?;

    Ok(exit_code(status))
}

fn capture(mut reader: impl Read, mut raw: Option<NamedTempFile>) -> io::Result<CapturedStream> {
    let mut captured = Captured::default();
    let mut line = Vec::new();
    let mut line_was_truncated = false;
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if let Some(file) = &mut raw {
            file.write_all(&buffer[..count])?;
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
    if let Some(file) = &mut raw {
        file.flush()?;
    }
    Ok(CapturedStream {
        bounded: captured,
        raw,
    })
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
    thread: std::thread::JoinHandle<io::Result<CapturedStream>>,
    stream: &str,
) -> Result<CapturedStream, String> {
    thread
        .join()
        .map_err(|_| format!("{stream} capture thread panicked"))?
        .map_err(|error| format!("could not read child {stream}: {error}"))
}

fn copy_raw(file: &mut std::fs::File, output: &mut impl Write, stream: &str) -> Result<(), String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind raw {stream}: {error}"))?;
    io::copy(file, output).map_err(|error| format!("could not restore raw {stream}: {error}"))?;
    Ok(())
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
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
        let captured = capture(Cursor::new(b"one\ntwo\n"), None).expect("capture");
        assert_eq!(captured.bounded.render(), b"one\ntwo\n");
    }

    #[test]
    fn prunes_middle_lines() {
        let input = numbered_lines("line ", 250);
        let captured = capture(Cursor::new(input), None).expect("capture");
        let rendered = String::from_utf8(captured.bounded.render()).expect("UTF-8 output");
        assert!(rendered.starts_with("line 0\n"));
        assert!(rendered.contains("[yarp: omitted 50 lines]\n"));
        assert!(rendered.ends_with("line 249\n"));
        assert!(!rendered.contains("line 180\n"));
    }

    #[test]
    fn keeps_all_lines_at_the_boundary() {
        let input = numbered_lines("", 200);
        let rendered = capture(Cursor::new(input.as_bytes()), None)
            .expect("capture")
            .bounded
            .render();
        assert_eq!(rendered, input.as_bytes());
    }

    #[test]
    fn truncates_one_very_long_line_without_growing_unbounded() {
        let input = vec![b'x'; MAX_LINE_BYTES * 2];
        let rendered = capture(Cursor::new(input), None)
            .expect("capture")
            .bounded
            .render();
        assert!(rendered.len() < MAX_LINE_BYTES + 64);
        assert!(rendered.ends_with(b"[yarp: line truncated]\n"));
    }

    #[test]
    fn handles_empty_and_unterminated_output() {
        assert!(
            capture(Cursor::new(Vec::<u8>::new()), None)
                .expect("capture")
                .bounded
                .render()
                .is_empty()
        );
        assert_eq!(
            capture(Cursor::new(b"last line"), None)
                .expect("capture")
                .bounded
                .render(),
            b"last line"
        );
    }

    #[test]
    fn raw_spool_keeps_omitted_lines() {
        let input = numbered_lines("line ", 250);
        let spool = NamedTempFile::new().expect("spool");
        let mut captured = capture(Cursor::new(input.as_bytes()), Some(spool)).expect("capture");
        let mut raw = String::new();
        captured
            .raw
            .as_mut()
            .expect("raw spool")
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .expect("seek");
        captured
            .raw
            .as_mut()
            .expect("raw spool")
            .as_file_mut()
            .read_to_string(&mut raw)
            .expect("read");
        assert_eq!(raw, input);
    }

    #[test]
    fn rejects_disallowed_children() {
        let result = run(&["cat".to_owned(), ".env".to_owned()], None);
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
