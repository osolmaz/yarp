use std::io::{Read, Seek, SeekFrom, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::archive::{Archive, SourceCompleteness, SourceName};
use crate::reducers::{RecoveryMarker, StreamReducer};

const RESULT_PROTOCOL_VERSION: u32 = 1;
const MAX_RESULT_FRAME_BYTES: u64 = 4 * 1024 * 1024;
const SETUP_DIAGNOSTIC_OVERLAP_BYTES: usize = 31;

#[derive(Debug, Default)]
struct SetupDiagnosticScanner {
    tail: Vec<u8>,
    matched: bool,
}

impl SetupDiagnosticScanner {
    fn push(&mut self, chunk: &[u8]) {
        if self.matched {
            return;
        }
        let prefix_len = chunk.len().min(SETUP_DIAGNOSTIC_OVERLAP_BYTES);
        let mut boundary = Vec::with_capacity(self.tail.len().saturating_add(prefix_len));
        boundary.extend_from_slice(&self.tail);
        boundary.extend_from_slice(&chunk[..prefix_len]);
        self.matched = crate::rewrite::contains_setup_diagnostic(&boundary)
            || crate::rewrite::contains_setup_diagnostic(chunk);
        if chunk.len() >= SETUP_DIAGNOSTIC_OVERLAP_BYTES {
            self.tail
                .extend_from_slice(&chunk[chunk.len() - SETUP_DIAGNOSTIC_OVERLAP_BYTES..]);
            self.tail
                .drain(..self.tail.len() - SETUP_DIAGNOSTIC_OVERLAP_BYTES);
        } else {
            self.tail.extend_from_slice(chunk);
            if self.tail.len() > SETUP_DIAGNOSTIC_OVERLAP_BYTES {
                self.tail
                    .drain(..self.tail.len() - SETUP_DIAGNOSTIC_OVERLAP_BYTES);
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReduceRequest {
    schema_version: u32,
    command: String,
    text: String,
    is_error: bool,
    exit_code: Option<i32>,
    archive_ref: String,
    source_completeness: SourceCompleteness,
    prefer_archive_source: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReduceResponse<'a> {
    schema_version: u32,
    changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_completeness: Option<SourceCompleteness>,
    needs_result_text: bool,
}

/// Read one framed post-result request and write one framed response.
///
/// # Errors
///
/// Returns an error for malformed or oversized protocol data and unavailable requested archive
/// sources. Unsupported commands return an unchanged response.
pub fn run(input: impl Read, output: impl Write) -> Result<(), String> {
    let request: ReduceRequest = read_frame(input)?;
    if request.schema_version != RESULT_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported result-reducer schema version {}; expected {RESULT_PROTOCOL_VERSION}",
            request.schema_version
        ));
    }
    validate_archive_ref(&request.archive_ref)?;
    let reduced = reduce(&request)?;
    write_frame(output, &reduced)
}

fn reduce(request: &ReduceRequest) -> Result<OwnedResponse, String> {
    let Ok(plan) = crate::rewrite::select_result_plan(&request.command) else {
        return Ok(OwnedResponse::unchanged());
    };
    let reported_success = request
        .exit_code
        .map_or(!request.is_error, |exit_code| exit_code == 0);
    let success =
        reported_success && plan.status_confidence == crate::rewrite::StatusConfidence::Complete;
    let rule = &plan.rule;
    if request.prefer_archive_source {
        let archive = Archive::open_read_only()?;
        let mut source = archive
            .searchable_sources(&request.archive_ref)?
            .into_iter()
            .find(|source| source.name == SourceName::SourceOutput)
            .ok_or_else(|| "documented complete source_output is unavailable".to_owned())?;
        if source.media_type != "text/plain; charset=utf-8" {
            return Ok(OwnedResponse::unchanged());
        }
        source
            .body
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("could not rewind source_output: {error}"))?;
        let mut reducer =
            StreamReducer::new_with_transform_diagnostics(rule, plan.transform_diagnostics)?;
        let mut setup_diagnostics = SetupDiagnosticScanner::default();
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let count = source
                .body
                .read(&mut buffer)
                .map_err(|error| format!("could not read source_output: {error}"))?;
            if count == 0 {
                break;
            }
            if plan.fail_open_setup_diagnostics {
                setup_diagnostics.push(&buffer[..count]);
                if setup_diagnostics.matched {
                    return Ok(OwnedResponse::unchanged());
                }
            }
            reducer.push(&buffer[..count]);
            hasher.update(&buffer[..count]);
        }
        let content = reducer.finish(
            success,
            Some(RecoveryMarker {
                archive_ref: &request.archive_ref,
                source: SourceName::SourceOutput.as_str(),
                completeness: SourceCompleteness::Complete.as_str(),
            }),
        );
        let unchanged = content.len() as u64 == source.byte_length
            && Sha256::digest(&content).as_slice() == hasher.finalize().as_slice();
        if unchanged {
            return Ok(OwnedResponse::unchanged());
        }
        let content = String::from_utf8(content)
            .map_err(|_| "source_output summary was not valid UTF-8".to_owned())?;
        return Ok(OwnedResponse::changed(
            content,
            SourceName::SourceOutput,
            SourceCompleteness::Complete,
            false,
        ));
    }

    if plan.fail_open_setup_diagnostics
        && crate::rewrite::contains_setup_diagnostic(request.text.as_bytes())
    {
        return Ok(OwnedResponse::unchanged());
    }
    let mut reducer =
        StreamReducer::new_with_transform_diagnostics(rule, plan.transform_diagnostics)?;
    for chunk in request.text.as_bytes().chunks(8 * 1024) {
        reducer.push(chunk);
    }
    let content = reducer.finish(
        success,
        Some(RecoveryMarker {
            archive_ref: &request.archive_ref,
            source: SourceName::ResultText.as_str(),
            completeness: request.source_completeness.as_str(),
        }),
    );
    if content == request.text.as_bytes() {
        return Ok(OwnedResponse::unchanged());
    }
    let content = String::from_utf8(content)
        .map_err(|_| "result text summary was not valid UTF-8".to_owned())?;
    Ok(OwnedResponse::changed(
        content,
        SourceName::ResultText,
        request.source_completeness,
        true,
    ))
}

#[derive(Debug)]
struct OwnedResponse {
    changed: bool,
    content: Option<String>,
    source: Option<SourceName>,
    source_completeness: Option<SourceCompleteness>,
    needs_result_text: bool,
}

impl OwnedResponse {
    const fn unchanged() -> Self {
        Self {
            changed: false,
            content: None,
            source: None,
            source_completeness: None,
            needs_result_text: false,
        }
    }

    fn changed(
        content: String,
        source: SourceName,
        completeness: SourceCompleteness,
        needs_result_text: bool,
    ) -> Self {
        Self {
            changed: true,
            content: Some(content),
            source: Some(source),
            source_completeness: Some(completeness),
            needs_result_text,
        }
    }
}

fn read_frame<T: for<'de> Deserialize<'de>>(mut input: impl Read) -> Result<T, String> {
    let mut length = [0_u8; 8];
    input
        .read_exact(&mut length)
        .map_err(|error| format!("could not read result-reducer frame length: {error}"))?;
    let length = u64::from_be_bytes(length);
    if length == 0 || length > MAX_RESULT_FRAME_BYTES {
        return Err(format!("invalid result-reducer frame length {length}"));
    }
    let length = usize::try_from(length)
        .map_err(|_| "result-reducer frame length does not fit memory".to_owned())?;
    let mut body = vec![0_u8; length];
    input
        .read_exact(&mut body)
        .map_err(|error| format!("could not read result-reducer frame: {error}"))?;
    let mut trailing = [0_u8; 1];
    if input
        .read(&mut trailing)
        .map_err(|error| format!("could not check result-reducer frame end: {error}"))?
        != 0
    {
        return Err("result-reducer request contains trailing bytes".to_owned());
    }
    serde_json::from_slice(&body)
        .map_err(|error| format!("invalid result-reducer request: {error}"))
}

fn write_frame(mut output: impl Write, response: &OwnedResponse) -> Result<(), String> {
    let body = serde_json::to_vec(&ReduceResponse {
        schema_version: RESULT_PROTOCOL_VERSION,
        changed: response.changed,
        content: response.content.as_deref(),
        source: response.source.map(SourceName::as_str),
        source_completeness: response.source_completeness,
        needs_result_text: response.needs_result_text,
    })
    .map_err(|error| format!("could not encode result-reducer response: {error}"))?;
    let length =
        u64::try_from(body.len()).map_err(|_| "result-reducer response is too large".to_owned())?;
    if length == 0 || length > MAX_RESULT_FRAME_BYTES {
        return Err(format!("invalid result-reducer response length {length}"));
    }
    output
        .write_all(&length.to_be_bytes())
        .and_then(|()| output.write_all(&body))
        .and_then(|()| output.flush())
        .map_err(|error| format!("could not write result-reducer response: {error}"))
}

fn validate_archive_ref(value: &str) -> Result<(), String> {
    if value.len() != 35
        || !value.starts_with("yr_")
        || !value[3..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("invalid result-reducer archive reference".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn frame(value: &serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("JSON");
        let mut frame = u64::try_from(body.len())
            .expect("length")
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(&body);
        frame
    }

    #[test]
    fn unsupported_results_fail_open() {
        let request = serde_json::json!({
            "schemaVersion": 1,
            "command": "cat secret",
            "text": "exact\n",
            "isError": false,
            "exitCode": 0,
            "archiveRef": "yr_0123456789abcdef0123456789abcdef",
            "sourceCompleteness": "unknown",
            "preferArchiveSource": false
        });
        let mut output = Vec::new();
        run(frame(&request).as_slice(), &mut output).expect("protocol");
        let length = usize::try_from(u64::from_be_bytes(output[..8].try_into().expect("length")))
            .expect("response length");
        let response: serde_json::Value =
            serde_json::from_slice(&output[8..8 + length]).expect("response");
        assert_eq!(response["changed"], false);
    }

    #[test]
    fn reduces_safe_composite_results_and_preserves_uncertain_ones() {
        let text = (0..2_000).fold(String::new(), |mut output, index| {
            writeln!(output, "src/file{index}.rs:{index}: TODO item")
                .expect("write search fixture");
            output
        });
        let request = ReduceRequest {
            schema_version: 1,
            command: "rg TODO . | sort | head -50".to_owned(),
            text: text.clone(),
            is_error: false,
            exit_code: Some(0),
            archive_ref: "yr_0123456789abcdef0123456789abcdef".to_owned(),
            source_completeness: SourceCompleteness::Unknown,
            prefer_archive_source: false,
        };
        let response = reduce(&request).expect("composite reduction");
        assert!(response.changed);
        assert!(response.content.expect("summary").contains("search"));

        let setup_failure = ReduceRequest {
            schema_version: 1,
            command: "cd /a/very/long/missing/path && cargo test | head -1".to_owned(),
            text: "bash: line 1: cd: /a/very/long/missing/path: No such file or directory\n"
                .to_owned(),
            is_error: true,
            exit_code: Some(1),
            archive_ref: "yr_0123456789abcdef0123456789abcdef".to_owned(),
            source_completeness: SourceCompleteness::Unknown,
            prefer_archive_source: false,
        };
        let response = reduce(&setup_failure).expect("setup failure pass-through");
        assert!(!response.changed);

        let guarded = ReduceRequest {
            command: "rg --json TODO . | jq .".to_owned(),
            ..request
        };
        let response = reduce(&guarded).expect("guarded pass-through");
        assert!(!response.changed);
    }

    #[test]
    fn finds_setup_diagnostics_across_stream_boundaries() {
        let mut scanner = SetupDiagnosticScanner::default();
        scanner.push(b"bash: line 1: c");
        assert!(!scanner.matched);
        scanner.push(b"d: missing: No such file or directory\n");
        assert!(scanner.matched);
    }

    #[test]
    fn reduces_failure_evidence_for_every_supported_composite_shape() {
        let text = format!(
            "{}test result: FAILED\n",
            "test case ... FAILED\n".repeat(1_000)
        );
        for command in [
            "cargo test | head -100",
            "set -o pipefail && cargo test | head -100",
            "cargo test && cargo test",
            "cargo test; cargo test",
            "cargo test || cargo test",
            "cargo test\ncargo test",
            "cargo test 2>&1",
        ] {
            let request = ReduceRequest {
                schema_version: 1,
                command: command.to_owned(),
                text: text.clone(),
                is_error: true,
                exit_code: Some(1),
                archive_ref: "yr_0123456789abcdef0123456789abcdef".to_owned(),
                source_completeness: SourceCompleteness::Unknown,
                prefer_archive_source: false,
            };
            let response = reduce(&request).expect("composite failure reduction");
            assert!(response.changed, "unchanged {command}");
            let content = response.content.expect("summary");
            assert!(content.contains("test result: FAILED"), "{command}");
            assert!(
                content.contains("yarp search yr_0123456789abcdef0123456789abcdef"),
                "{command}"
            );
        }
    }

    #[test]
    fn explicit_exit_code_precedes_generic_error() {
        let request = ReduceRequest {
            schema_version: 1,
            command: "cargo test".to_owned(),
            text: format!("{}test result: ok\n", "test routine ... ok\n".repeat(1_000)),
            is_error: true,
            exit_code: Some(0),
            archive_ref: "yr_0123456789abcdef0123456789abcdef".to_owned(),
            source_completeness: SourceCompleteness::Unknown,
            prefer_archive_source: false,
        };
        let response = reduce(&request).expect("reduction");
        assert!(response.changed);
        assert!(
            response
                .content
                .expect("content")
                .contains("test result: ok")
        );
    }
}
