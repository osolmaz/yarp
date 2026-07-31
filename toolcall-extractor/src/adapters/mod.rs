pub mod claude;
pub mod codex;
mod common;
pub mod cursor;
pub mod pi;

use std::path::Path;

use crate::error::{Error, Result};
use crate::keys;
use crate::model::{
    ADAPTER_VERSION, IssueRecord, Severity, SourceItemRecord, SourceRootRecord, SourceStatus,
};
use crate::private_fs;
use crate::sink::Sink;

fn source_root(
    unix_user: &str,
    agent: &str,
    source_kind: &str,
    root: &Path,
) -> Result<SourceRootRecord> {
    let canonical = std::fs::canonicalize(root).map_err(|error| Error::io(root, error))?;
    if !canonical.is_dir() {
        return Err(Error::InvalidSource(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let root_path = canonical.to_string_lossy().into_owned();
    Ok(SourceRootRecord {
        source_root_key: keys::key(&[
            b"source_root",
            unix_user.as_bytes(),
            agent.as_bytes(),
            source_kind.as_bytes(),
            root_path.as_bytes(),
        ]),
        unix_user: unix_user.to_owned(),
        agent: agent.to_owned(),
        source_kind: source_kind.to_owned(),
        root_path,
    })
}

fn register_root(
    unix_user: &str,
    agent: &str,
    source_kind: &str,
    root: &Path,
    sink: &mut impl Sink,
) -> Result<SourceRootRecord> {
    let record = source_root(unix_user, agent, source_kind, root)?;
    sink.source_root(&record)?;
    Ok(record)
}

fn source_item(root: &SourceRootRecord, relative_path: String) -> SourceItemRecord {
    SourceItemRecord {
        source_item_key: keys::key(&[
            b"source_item",
            root.source_root_key.as_bytes(),
            relative_path.as_bytes(),
        ]),
        source_root_key: root.source_root_key.clone(),
        relative_path,
        adapter_version: ADAPTER_VERSION,
        device_id: None,
        inode: None,
        size_bytes: 0,
        snapshot_mtime_ns: 0,
        imported_byte_count: None,
        prefix_sha256: None,
        status: SourceStatus::Deferred,
    }
}

fn reject_source_item(
    path: &Path,
    mut item: SourceItemRecord,
    code: &str,
    message: &str,
    sink: &mut impl Sink,
) -> Result<()> {
    let identity = private_fs::identity(path)?;
    item.device_id = Some(identity.device_id);
    item.inode = Some(identity.inode);
    item.size_bytes = identity.size_bytes;
    item.snapshot_mtime_ns = identity.mtime_ns;
    item.status = SourceStatus::Rejected;
    sink.begin_source()?;
    sink.reset_source(&item.source_item_key)?;
    sink.source_item(&item)?;
    sink.issue(&IssueRecord {
        issue_key: keys::key(&[
            b"rejected_source",
            item.source_item_key.as_bytes(),
            code.as_bytes(),
        ]),
        source_item_key: Some(item.source_item_key),
        severity: Severity::Warning,
        code: code.to_owned(),
        line_number: Some(1),
        byte_offset: Some(0),
        sqlite_blob_id: None,
        record_sha256: None,
        message: keys::bounded_message(message),
        occurrence_count: 1,
    })?;
    sink.commit_source()
}
