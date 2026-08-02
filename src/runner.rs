use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};

use tempfile::NamedTempFile;

use crate::reducers::{RecoveryMarker, StreamReducer};
use crate::rules::{PackRequest, Registry, SelectedRule, Selection};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedSelection {
    pub pack_id: String,
    pub rule_id: String,
    pub source_digest: [u8; 32],
}

struct CapturedStream {
    output: CapturedOutput,
    raw: Option<RawSpool>,
    archive_error: Option<String>,
    raw_emitted: bool,
}

enum CapturedOutput {
    Reduced(Box<StreamReducer>),
    Passthrough,
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

/// Run one command selected by the built-in rule registry.
///
/// # Errors
///
/// Returns an error when the command is not selected, cannot run, or its output cannot be read.
pub fn run(
    arguments: &[String],
    archive_key: Option<&crate::archive::ArchiveKey>,
) -> Result<i32, String> {
    run_with_rules(arguments, archive_key, &[], None)
}

/// Run one command with explicit rule packs and optional rewrite-process agreement metadata.
///
/// A changed or unavailable pack after rewrite selects exact pass-through output. Direct invocations
/// without agreement metadata still reject unsupported commands.
///
/// # Errors
///
/// Returns an error when direct execution is not selected, the child cannot run, or output cannot
/// be read or written.
pub fn run_with_rules(
    arguments: &[String],
    archive_key: Option<&crate::archive::ArchiveKey>,
    packs: &[PackRequest],
    expected: Option<&ExpectedSelection>,
) -> Result<i32, String> {
    if arguments.is_empty() {
        return Err("child command is missing".to_owned());
    }
    let selected = select_rule(arguments, packs, expected)?;
    let passthrough = selected.is_none();

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

    let stdout_output = selected
        .as_ref()
        .map(|selected| StreamReducer::new(&selected.rule))
        .transpose()?;
    let stderr_output = selected
        .as_ref()
        .map(|selected| StreamReducer::new(&selected.rule))
        .transpose()?;

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

    let stdout_thread = std::thread::spawn(move || {
        capture(
            stdout,
            stdout_spool,
            io::stdout(),
            stdout_output.map_or(CapturedOutput::Passthrough, |reducer| {
                CapturedOutput::Reduced(Box::new(reducer))
            }),
        )
    });
    let stderr_thread = std::thread::spawn(move || {
        capture(
            stderr,
            stderr_spool,
            io::stderr(),
            stderr_output.map_or(CapturedOutput::Passthrough, |reducer| {
                CapturedOutput::Reduced(Box::new(reducer))
            }),
        )
    });
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for child: {error}"))?;
    finish_run(
        status,
        stdout_thread,
        stderr_thread,
        archive_key,
        passthrough,
    )
}

type CaptureThread = std::thread::JoinHandle<io::Result<CapturedStream>>;

fn finish_run(
    status: std::process::ExitStatus,
    stdout_thread: CaptureThread,
    stderr_thread: CaptureThread,
    archive_key: Option<&crate::archive::ArchiveKey>,
    passthrough: bool,
) -> Result<i32, String> {
    let mut stdout = join_capture(stdout_thread, "stdout")?;
    let mut stderr = join_capture(stderr_thread, "stderr")?;
    let succeeded = status.success();
    let archive_ref = if passthrough {
        None
    } else if let Some(key) = archive_key {
        match crate::archive::Archive::open_read_only().and_then(|archive| archive.archive_ref(key))
        {
            Ok(value) => Some(value),
            Err(error) => {
                restore_after_capture_error(&mut stdout, &mut stderr, passthrough)?;
                eprintln!("yarp: archive failed after command execution: {error}");
                return Ok(exit_code(status));
            }
        }
    } else {
        None
    };
    let stdout_after = finish_output(
        std::mem::replace(&mut stdout.output, CapturedOutput::Passthrough),
        succeeded,
        archive_ref.as_deref().map(|archive_ref| RecoveryMarker {
            archive_ref,
            source: "stdout",
            completeness: "complete",
        }),
    );
    let stderr_after = finish_output(
        std::mem::replace(&mut stderr.output, CapturedOutput::Passthrough),
        succeeded,
        archive_ref.as_deref().map(|archive_ref| RecoveryMarker {
            archive_ref,
            source: "stderr",
            completeness: "complete",
        }),
    );
    let capture_errors = capture_errors(&stdout, &stderr);
    if !capture_errors.is_empty() {
        restore_after_capture_error(&mut stdout, &mut stderr, passthrough)?;
        eprintln!(
            "yarp: archive failed after command execution: {}",
            capture_errors.join("; ")
        );
        return Ok(exit_code(status));
    }
    if let Some(key) = archive_key
        && let Err(error) = archive_captures(
            key,
            &mut stdout,
            &mut stderr,
            stdout_after.as_deref(),
            stderr_after.as_deref(),
            passthrough,
        )
    {
        if !passthrough {
            emit_raw(&mut stdout, &mut io::stdout(), "stdout")?;
            emit_raw(&mut stderr, &mut io::stderr(), "stderr")?;
        }
        eprintln!("yarp: archive failed after command execution: {error}");
        return Ok(exit_code(status));
    }
    if let Some(stdout_after) = stdout_after {
        io::stdout()
            .write_all(&stdout_after)
            .map_err(|error| format!("could not write stdout: {error}"))?;
    }
    if let Some(stderr_after) = stderr_after {
        io::stderr()
            .write_all(&stderr_after)
            .map_err(|error| format!("could not write stderr: {error}"))?;
    }
    Ok(exit_code(status))
}

fn capture_errors(stdout: &CapturedStream, stderr: &CapturedStream) -> Vec<String> {
    [
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
    .collect()
}

fn restore_after_capture_error(
    stdout: &mut CapturedStream,
    stderr: &mut CapturedStream,
    passthrough: bool,
) -> Result<(), String> {
    if !passthrough && !stdout.raw_emitted {
        emit_raw(stdout, &mut io::stdout(), "stdout")?;
    }
    if !passthrough && !stderr.raw_emitted {
        emit_raw(stderr, &mut io::stderr(), "stderr")?;
    }
    Ok(())
}

fn archive_captures(
    key: &crate::archive::ArchiveKey,
    stdout: &mut CapturedStream,
    stderr: &mut CapturedStream,
    stdout_after: Option<&[u8]>,
    stderr_after: Option<&[u8]>,
    passthrough: bool,
) -> Result<(), String> {
    let stdout_raw = stdout
        .raw
        .as_mut()
        .ok_or_else(|| "stdout archive spool is missing".to_owned())?;
    let stderr_raw = stderr
        .raw
        .as_mut()
        .ok_or_else(|| "stderr archive spool is missing".to_owned())?;
    let mut archive = crate::archive::Archive::open()?;
    if passthrough {
        archive.capture_passthrough_streams(
            key,
            unix_time_ms(),
            stdout_raw.as_file_mut(),
            stderr_raw.as_file_mut(),
        )
    } else {
        archive.capture_streams(
            key,
            unix_time_ms(),
            stdout_raw.as_file_mut(),
            stderr_raw.as_file_mut(),
            stdout_after.unwrap_or_default(),
            stderr_after.unwrap_or_default(),
        )
    }
}

fn select_rule(
    arguments: &[String],
    packs: &[PackRequest],
    expected: Option<&ExpectedSelection>,
) -> Result<Option<SelectedRule>, String> {
    let mut registry = match Registry::load(packs) {
        Ok(registry) => registry,
        Err(_) if expected.is_some() => return Ok(None),
        Err(error) => return Err(error),
    };
    let selection = match registry.select(arguments) {
        Ok(selection) => selection,
        Err(_) if expected.is_some() => return Ok(None),
        Err(error) => return Err(error),
    };
    match (selection, expected) {
        (Selection::Reduce(selected), Some(expected))
            if selected.pack_id == expected.pack_id
                && selected.rule.id == expected.rule_id
                && selected.source_digest == expected.source_digest =>
        {
            Ok(Some(selected))
        }
        (Selection::Reduce(selected), None) => Ok(Some(selected)),
        (_, Some(_)) => Ok(None),
        (Selection::Unsupported, None) => Err("command is not on the YARP allowlist".to_owned()),
        (Selection::Passthrough(_), None) => {
            Err("command is protected by a YARP pass-through rule".to_owned())
        }
        (Selection::Ambiguous(ids), None) => Err(format!(
            "command matches multiple YARP reduction rules: {}",
            ids.join(", ")
        )),
    }
}

fn capture(
    mut reader: impl Read,
    mut raw: Option<RawSpool>,
    mut fallback: impl Write,
    mut output: CapturedOutput,
) -> io::Result<CapturedStream> {
    let passthrough = matches!(output, CapturedOutput::Passthrough);
    let mut archive_error = None;
    let mut raw_emitted = passthrough;
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        if passthrough {
            fallback.write_all(chunk)?;
        }
        if !passthrough && raw_emitted {
            fallback.write_all(chunk)?;
        } else if let Some(spool) = &mut raw
            && let Err(error) = spool.write_chunk(chunk)
        {
            if !passthrough {
                rewind_and_copy(spool.as_file_mut(), &mut fallback)?;
                fallback.write_all(&chunk[error.bytes_written..])?;
            }
            archive_error = Some(format!("could not write archive spool: {}", error.source));
            raw = None;
            raw_emitted = true;
        }
        if let CapturedOutput::Reduced(reducer) = &mut output {
            reducer.push(chunk);
        }
    }

    if let Some(spool) = &mut raw
        && let Err(error) = spool.file.flush()
    {
        if !passthrough {
            rewind_and_copy(spool.as_file_mut(), &mut fallback)?;
        }
        archive_error = Some(format!("could not flush archive spool: {error}"));
        raw = None;
        raw_emitted = true;
    }
    if raw_emitted {
        fallback.flush()?;
    }
    Ok(CapturedStream {
        output,
        raw,
        archive_error,
        raw_emitted,
    })
}

fn finish_output(
    output: CapturedOutput,
    success: bool,
    recovery: Option<RecoveryMarker<'_>>,
) -> Option<Vec<u8>> {
    match output {
        CapturedOutput::Reduced(reducer) => Some((*reducer).finish(success, recovery)),
        CapturedOutput::Passthrough => None,
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
    use std::io::Cursor;

    fn selected_rule(arguments: &[&str]) -> SelectedRule {
        let mut registry = Registry::builtins_only();
        let arguments = arguments
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let Selection::Reduce(selected) = registry.select(&arguments).expect("selection") else {
            panic!("expected reduction rule");
        };
        selected
    }

    #[test]
    fn capture_reduces_with_the_selected_rule() {
        let selected = selected_rule(&["cargo", "test"]);
        let input = "   Compiling crate\n".repeat(400);
        let reducer = StreamReducer::new(&selected.rule).expect("reducer");
        let captured = capture(
            Cursor::new(input.as_bytes()),
            None,
            io::sink(),
            CapturedOutput::Reduced(Box::new(reducer)),
        )
        .expect("capture");
        let output = finish_output(captured.output, true, None).expect("reduced output");
        assert!(output.len() < input.len());
    }

    #[test]
    fn passthrough_capture_emits_exact_bytes() {
        let input = b"exact\xffbytes\n";
        let mut output = Vec::new();
        let captured = capture(
            Cursor::new(input),
            None,
            &mut output,
            CapturedOutput::Passthrough,
        )
        .expect("capture");
        assert_eq!(output, input);
        assert!(finish_output(captured.output, true, None).is_none());
    }

    #[test]
    fn archive_spool_failure_emits_the_exact_raw_stream() {
        let input = b"raw output that exceeds the simulated limit\n";
        let selected = selected_rule(&["git", "status"]);
        let spool = RawSpool::failing_after(7).expect("spool");
        let mut output = Vec::new();
        let captured = capture(
            Cursor::new(input),
            Some(spool),
            &mut output,
            CapturedOutput::Reduced(Box::new(
                StreamReducer::new(&selected.rule).expect("reducer"),
            )),
        )
        .expect("capture");
        assert_eq!(output, input);
        assert!(captured.raw_emitted);
        assert!(captured.raw.is_none());
        assert!(
            captured
                .archive_error
                .is_some_and(|error| error.contains("archive spool"))
        );
    }

    #[test]
    fn rejects_unselected_direct_children() {
        let result = run(&["cat".to_owned(), ".env".to_owned()], None);
        assert_eq!(
            result,
            Err("command is not on the YARP allowlist".to_owned())
        );
    }

    #[test]
    fn rewrite_disagreement_fails_open() {
        let expected = ExpectedSelection {
            pack_id: "yarp-builtins".to_owned(),
            rule_id: "git/status".to_owned(),
            source_digest: [0_u8; 32],
        };
        let selected = select_rule(
            &["git".to_owned(), "status".to_owned()],
            &[],
            Some(&expected),
        )
        .expect("selection");
        assert!(selected.is_none());
    }
}
