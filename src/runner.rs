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
    raw: Option<RawSpool>,
    archive_error: Option<String>,
    raw_emitted: bool,
}

struct RawSpool {
    file: NamedTempFile,
    bytes_written: usize,
    #[cfg(test)]
    fail_after: Option<usize>,
}

struct SpoolWriteError {
    source: io::Error,
    bytes_written: usize,
}

impl RawSpool {
    fn new() -> io::Result<Self> {
        Ok(Self {
            file: NamedTempFile::new()?,
            bytes_written: 0,
            #[cfg(test)]
            fail_after: None,
        })
    }

    #[cfg(test)]
    fn failing_after(limit: usize) -> io::Result<Self> {
        let mut spool = Self::new()?;
        spool.fail_after = Some(limit);
        Ok(spool)
    }

    fn write_chunk(&mut self, body: &[u8]) -> Result<(), SpoolWriteError> {
        #[cfg(test)]
        let permitted = self.fail_after.map_or(body.len(), |limit| {
            limit.saturating_sub(self.bytes_written).min(body.len())
        });
        #[cfg(not(test))]
        let permitted = body.len();

        let mut offset = 0;
        while offset < permitted {
            match self.file.write(&body[offset..permitted]) {
                Ok(0) => {
                    return Err(SpoolWriteError {
                        source: io::Error::new(
                            io::ErrorKind::WriteZero,
                            "archive spool wrote zero bytes",
                        ),
                        bytes_written: offset,
                    });
                }
                Ok(count) => {
                    offset += count;
                    self.bytes_written = self.bytes_written.saturating_add(count);
                }
                Err(source) => {
                    return Err(SpoolWriteError {
                        source,
                        bytes_written: offset,
                    });
                }
            }
        }

        if permitted != body.len() {
            return Err(SpoolWriteError {
                source: io::Error::other("simulated archive spool failure"),
                bytes_written: offset,
            });
        }
        Ok(())
    }

    fn as_file_mut(&mut self) -> &mut std::fs::File {
        self.file.as_file_mut()
    }
}

/// Prune one captured output stream with the same limits used for child processes.
#[must_use]
pub fn prune_bytes(input: &[u8]) -> Vec<u8> {
    capture(std::io::Cursor::new(input), None, io::sink())
        .map_or_else(|_| input.to_vec(), |captured| captured.bounded.render())
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
                RawSpool::new()
                    .map_err(|error| format!("could not create stdout archive spool: {error}"))?,
            ),
            Some(
                RawSpool::new()
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

    let stdout_thread = std::thread::spawn(move || capture(stdout, stdout_spool, io::stdout()));
    let stderr_thread = std::thread::spawn(move || capture(stderr, stderr_spool, io::stderr()));
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for child: {error}"))?;

    let mut stdout = join_capture(stdout_thread, "stdout")?;
    let mut stderr = join_capture(stderr_thread, "stderr")?;
    let stdout_after = std::mem::take(&mut stdout.bounded).render();
    let stderr_after = std::mem::take(&mut stderr.bounded).render();

    let capture_errors = [
        stdout
            .archive_error
            .as_ref()
            .map(|error| format!("stdout: {error}")),
        stderr
            .archive_error
            .as_ref()
            .map(|error| format!("stderr: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !capture_errors.is_empty() {
        if !stdout.raw_emitted {
            emit_raw(&mut stdout, &mut io::stdout(), "stdout")?;
        }
        if !stderr.raw_emitted {
            emit_raw(&mut stderr, &mut io::stderr(), "stderr")?;
        }
        eprintln!(
            "yarp: archive failed after command execution: {}",
            capture_errors.join("; ")
        );
        return Ok(exit_code(status));
    }

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

fn capture(
    mut reader: impl Read,
    mut raw: Option<RawSpool>,
    mut fallback: impl Write,
) -> io::Result<CapturedStream> {
    let mut captured = Captured::default();
    let mut line = Vec::new();
    let mut line_was_truncated = false;
    let mut archive_error = None;
    let mut raw_emitted = false;
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if raw_emitted {
            fallback.write_all(&buffer[..count])?;
        } else if let Some(spool) = &mut raw
            && let Err(error) = spool.write_chunk(&buffer[..count])
        {
            rewind_and_copy(spool.as_file_mut(), &mut fallback)?;
            fallback.write_all(&buffer[error.bytes_written..count])?;
            archive_error = Some(format!("could not write archive spool: {}", error.source));
            raw = None;
            raw_emitted = true;
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
    if let Some(spool) = &mut raw
        && let Err(error) = spool.file.flush()
    {
        rewind_and_copy(spool.as_file_mut(), &mut fallback)?;
        archive_error = Some(format!("could not flush archive spool: {error}"));
        raw = None;
        raw_emitted = true;
    }
    if raw_emitted {
        fallback.flush()?;
    }
    Ok(CapturedStream {
        bounded: captured,
        raw,
        archive_error,
        raw_emitted,
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

fn rewind_and_copy(file: &mut std::fs::File, output: &mut impl Write) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    io::copy(file, output)?;
    Ok(())
}

fn copy_raw(file: &mut std::fs::File, output: &mut impl Write, stream: &str) -> Result<(), String> {
    rewind_and_copy(file, output)
        .map_err(|error| format!("could not restore raw {stream}: {error}"))
}

fn emit_raw(
    captured: &mut CapturedStream,
    output: &mut impl Write,
    stream: &str,
) -> Result<(), String> {
    let spool = captured
        .raw
        .as_mut()
        .ok_or_else(|| format!("raw {stream} archive spool is missing"))?;
    copy_raw(spool.as_file_mut(), output, stream)
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
        let captured = capture(Cursor::new(b"one\ntwo\n"), None, io::sink()).expect("capture");
        assert_eq!(captured.bounded.render(), b"one\ntwo\n");
        assert_eq!(prune_bytes(b"one\ntwo\n"), b"one\ntwo\n");
    }

    #[test]
    fn prunes_middle_lines() {
        let input = numbered_lines("line ", 250);
        let captured = capture(Cursor::new(input), None, io::sink()).expect("capture");
        let rendered = String::from_utf8(captured.bounded.render()).expect("UTF-8 output");
        assert!(rendered.starts_with("line 0\n"));
        assert!(rendered.contains("[yarp: omitted 50 lines]\n"));
        assert!(rendered.ends_with("line 249\n"));
        assert!(!rendered.contains("line 180\n"));
    }

    #[test]
    fn keeps_all_lines_at_the_boundary() {
        let input = numbered_lines("", 200);
        let rendered = capture(Cursor::new(input.as_bytes()), None, io::sink())
            .expect("capture")
            .bounded
            .render();
        assert_eq!(rendered, input.as_bytes());
    }

    #[test]
    fn truncates_one_very_long_line_without_growing_unbounded() {
        let input = vec![b'x'; MAX_LINE_BYTES * 2];
        let rendered = capture(Cursor::new(input), None, io::sink())
            .expect("capture")
            .bounded
            .render();
        assert!(rendered.len() < MAX_LINE_BYTES + 64);
        assert!(rendered.ends_with(b"[yarp: line truncated]\n"));
    }

    #[test]
    fn handles_empty_and_unterminated_output() {
        assert!(
            capture(Cursor::new(Vec::<u8>::new()), None, io::sink())
                .expect("capture")
                .bounded
                .render()
                .is_empty()
        );
        assert_eq!(
            capture(Cursor::new(b"last line"), None, io::sink())
                .expect("capture")
                .bounded
                .render(),
            b"last line"
        );
    }

    #[test]
    fn raw_spool_keeps_omitted_lines() {
        let input = numbered_lines("line ", 250);
        let spool = RawSpool::new().expect("spool");
        let mut captured =
            capture(Cursor::new(input.as_bytes()), Some(spool), io::sink()).expect("capture");
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
    fn archive_spool_failure_emits_the_exact_raw_stream() {
        let input = numbered_lines("raw ", 250);
        let spool = RawSpool::failing_after(37).expect("spool");
        let mut output = Vec::new();
        let captured =
            capture(Cursor::new(input.as_bytes()), Some(spool), &mut output).expect("capture");
        assert_eq!(output, input.as_bytes());
        assert!(captured.raw_emitted);
        assert!(captured.raw.is_none());
        assert!(
            captured
                .archive_error
                .is_some_and(|error| error.contains("archive spool"))
        );
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
