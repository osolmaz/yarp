use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::keys;
use crate::model::{ADAPTER_VERSION, IssueRecord, Severity, SourceItemRecord, SourceStatus};
use crate::private_fs::{self, FileIdentity};
use crate::sink::Sink;

pub struct JsonLine {
    pub number: u64,
    pub byte_offset: u64,
    pub bytes: Vec<u8>,
    pub record_sha256: Vec<u8>,
}

pub fn first_json_value(path: &Path) -> Result<Option<serde_json::Value>> {
    let file = File::open(path).map_err(|error| Error::io(path, error))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let count = reader
        .read_until(b'\n', &mut line)
        .map_err(|error| Error::io(path, error))?;
    if count == 0 || !line.ends_with(b"\n") {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&line).ok())
}

pub fn process_jsonl(
    path: &Path,
    source_item: SourceItemRecord,
    sink: &mut impl Sink,
    handle: impl FnMut(&JsonLine, &serde_json::Value, &mut dyn Sink) -> Result<()>,
    finish: impl FnMut(&mut dyn Sink) -> Result<()>,
) -> Result<bool> {
    process_jsonl_mode(path, source_item, sink, true, handle, finish)
}

pub fn process_jsonl_from_start(
    path: &Path,
    source_item: SourceItemRecord,
    sink: &mut impl Sink,
    handle: impl FnMut(&JsonLine, &serde_json::Value, &mut dyn Sink) -> Result<()>,
    finish: impl FnMut(&mut dyn Sink) -> Result<()>,
) -> Result<bool> {
    process_jsonl_mode(path, source_item, sink, false, handle, finish)
}

fn process_jsonl_mode(
    path: &Path,
    mut source_item: SourceItemRecord,
    sink: &mut impl Sink,
    allow_resume: bool,
    mut handle: impl FnMut(&JsonLine, &serde_json::Value, &mut dyn Sink) -> Result<()>,
    mut finish: impl FnMut(&mut dyn Sink) -> Result<()>,
) -> Result<bool> {
    let before = private_fs::identity(path)?;
    if unchanged(sink, &source_item.source_item_key, before)? {
        return Ok(false);
    }

    let (start, prefix_issue, reset_source) = if allow_resume {
        let (start, issue) = resume_offset(path, sink, &source_item.source_item_key, before)?;
        let reset = issue.is_some();
        (start, issue, reset)
    } else {
        let reset = sink.checkpoint(&source_item.source_item_key)?.is_some();
        (0, None, reset)
    };
    sink.begin_source()?;
    if reset_source {
        sink.reset_source(&source_item.source_item_key)?;
    }
    source_item.device_id = Some(before.device_id);
    source_item.inode = Some(before.inode);
    source_item.size_bytes = before.size_bytes;
    source_item.snapshot_mtime_ns = before.mtime_ns;
    source_item.imported_byte_count = Some(start);
    source_item.status = SourceStatus::Deferred;
    sink.source_item(&source_item)?;
    if let Some(issue) = prefix_issue {
        sink.issue(&issue)?;
    }

    let result = process_lines(
        path,
        before,
        start,
        &source_item.source_item_key,
        sink,
        &mut handle,
    );
    match result {
        Ok(outcome) => {
            finish(sink)?;
            let after = private_fs::identity(path)?;
            if after.device_id != before.device_id
                || after.inode != before.inode
                || after.size_bytes < before.size_bytes
                || (after.size_bytes == before.size_bytes && after.mtime_ns != before.mtime_ns)
            {
                sink.rollback_source()?;
                record_unstable_source(sink, &source_item, after)?;
                return Ok(false);
            }
            source_item.imported_byte_count = Some(outcome.imported_bytes);
            source_item.prefix_sha256 = Some(outcome.prefix_sha256);
            source_item.status = if outcome.imported_bytes == before.size_bytes {
                SourceStatus::Complete
            } else {
                SourceStatus::Deferred
            };
            sink.source_item(&source_item)?;
            sink.commit_source()?;
            Ok(true)
        }
        Err(error) => {
            sink.rollback_source()?;
            Err(error)
        }
    }
}

struct ProcessOutcome {
    imported_bytes: u64,
    prefix_sha256: Vec<u8>,
}

fn process_lines(
    path: &Path,
    snapshot: FileIdentity,
    start: u64,
    source_item_key: &str,
    sink: &mut impl Sink,
    handle: &mut impl FnMut(&JsonLine, &serde_json::Value, &mut dyn Sink) -> Result<()>,
) -> Result<ProcessOutcome> {
    let mut file = File::open(path).map_err(|error| Error::io(path, error))?;
    let mut prefix_hash = Sha256::new();
    let mut line_number = 0_u64;
    if start > 0 {
        hash_prefix(&mut file, start, &mut prefix_hash, &mut line_number, path)?;
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|error| Error::io(path, error))?;
    let mut reader = BufReader::new(file.take(snapshot.size_bytes.saturating_sub(start)));
    let mut offset = start;
    loop {
        let mut bytes = Vec::new();
        let count = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| Error::io(path, error))?;
        if count == 0 {
            break;
        }
        if !bytes.ends_with(b"\n") {
            break;
        }
        line_number = line_number.saturating_add(1);
        let record_sha256 = keys::sha256(&bytes);
        prefix_hash.update(&bytes);
        let line = JsonLine {
            number: line_number,
            byte_offset: offset,
            bytes,
            record_sha256,
        };
        offset = offset.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        match serde_json::from_slice::<serde_json::Value>(&line.bytes) {
            Ok(value) => handle(&line, &value, sink)?,
            Err(error) => sink.issue(&malformed_issue(source_item_key, &line, &error))?,
        }
    }
    Ok(ProcessOutcome {
        imported_bytes: offset,
        prefix_sha256: prefix_hash.finalize().to_vec(),
    })
}

fn hash_prefix(
    file: &mut File,
    count: u64,
    hash: &mut Sha256,
    line_number: &mut u64,
    path: &Path,
) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| Error::io(path, error))?;
    let mut reader = BufReader::new(file.take(count));
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| Error::io(path, error))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
        *line_number = line_number.saturating_add(
            u64::try_from(buffer[..read].iter().filter(|byte| **byte == b'\n').count())
                .unwrap_or(u64::MAX),
        );
    }
    Ok(())
}

fn unchanged(sink: &impl Sink, key: &str, identity: FileIdentity) -> Result<bool> {
    let Some(checkpoint) = sink.checkpoint(key)? else {
        return Ok(false);
    };
    Ok(checkpoint.adapter_version == ADAPTER_VERSION
        && checkpoint.device_id == Some(identity.device_id)
        && checkpoint.inode == Some(identity.inode)
        && checkpoint.size_bytes == identity.size_bytes
        && checkpoint.snapshot_mtime_ns == identity.mtime_ns
        && checkpoint.imported_byte_count == Some(identity.size_bytes)
        && checkpoint.status == "complete")
}

fn resume_offset(
    path: &Path,
    sink: &impl Sink,
    key: &str,
    identity: FileIdentity,
) -> Result<(u64, Option<IssueRecord>)> {
    let Some(checkpoint) = sink.checkpoint(key)? else {
        return Ok((0, None));
    };
    let start = checkpoint.imported_byte_count.unwrap_or(0);
    if checkpoint.adapter_version != ADAPTER_VERSION
        || checkpoint.device_id != Some(identity.device_id)
        || checkpoint.inode != Some(identity.inode)
        || start > identity.size_bytes
    {
        return Ok((0, Some(checkpoint_issue(key, "source identity changed"))));
    }
    let Some(expected_hash) = checkpoint.prefix_sha256 else {
        return Ok((0, Some(checkpoint_issue(key, "checkpoint hash is missing"))));
    };
    let mut file = File::open(path).map_err(|error| Error::io(path, error))?;
    let mut hash = Sha256::new();
    let mut remaining = start;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let count = file
            .read(&mut buffer[..limit])
            .map_err(|error| Error::io(path, error))?;
        if count == 0 {
            return Ok((
                0,
                Some(checkpoint_issue(key, "checkpoint prefix is truncated")),
            ));
        }
        hash.update(&buffer[..count]);
        remaining = remaining.saturating_sub(u64::try_from(count).unwrap_or(u64::MAX));
    }
    if hash.finalize().as_slice() != expected_hash {
        return Ok((0, Some(checkpoint_issue(key, "checkpoint prefix changed"))));
    }
    Ok((start, None))
}

fn malformed_issue(
    source_item_key: &str,
    line: &JsonLine,
    error: &serde_json::Error,
) -> IssueRecord {
    IssueRecord {
        issue_key: keys::key(&[
            b"malformed_jsonl",
            &line.byte_offset.to_be_bytes(),
            &line.record_sha256,
        ]),
        source_item_key: Some(source_item_key.to_owned()),
        severity: Severity::Warning,
        code: "malformed_jsonl".to_owned(),
        line_number: Some(line.number),
        byte_offset: Some(line.byte_offset),
        sqlite_blob_id: None,
        record_sha256: Some(line.record_sha256.clone()),
        message: format!("JSON parse failed: {:?}", error.classify()),
        occurrence_count: 1,
    }
}

fn checkpoint_issue(source_item_key: &str, message: &str) -> IssueRecord {
    IssueRecord {
        issue_key: keys::key(&[
            b"checkpoint_reset",
            source_item_key.as_bytes(),
            message.as_bytes(),
        ]),
        source_item_key: Some(source_item_key.to_owned()),
        severity: Severity::Warning,
        code: "checkpoint_reset".to_owned(),
        line_number: None,
        byte_offset: None,
        sqlite_blob_id: None,
        record_sha256: None,
        message: message.to_owned(),
        occurrence_count: 1,
    }
}

fn record_unstable_source(
    sink: &mut impl Sink,
    source_item: &SourceItemRecord,
    after: FileIdentity,
) -> Result<()> {
    sink.begin_source()?;
    let mut rejected = source_item.clone();
    rejected.size_bytes = after.size_bytes;
    rejected.snapshot_mtime_ns = after.mtime_ns;
    rejected.imported_byte_count = None;
    rejected.prefix_sha256 = None;
    rejected.status = SourceStatus::Rejected;
    sink.source_item(&rejected)?;
    sink.issue(&IssueRecord {
        issue_key: keys::key(&[b"source_changed", source_item.source_item_key.as_bytes()]),
        source_item_key: Some(source_item.source_item_key.clone()),
        severity: Severity::Warning,
        code: "source_changed_during_read".to_owned(),
        line_number: None,
        byte_offset: None,
        sqlite_blob_id: None,
        record_sha256: None,
        message: "source changed during read; checkpoint was not advanced".to_owned(),
        occurrence_count: 1,
    })?;
    sink.commit_source()
}
