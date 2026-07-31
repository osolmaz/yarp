pub mod claude;
pub mod codex;
mod common;
pub mod cursor;
pub mod pi;

use std::path::Path;

use crate::error::{Error, Result};
use crate::keys;
use crate::model::{ADAPTER_VERSION, SourceRootRecord};
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

fn source_item(root: &SourceRootRecord, relative_path: String) -> crate::model::SourceItemRecord {
    crate::model::SourceItemRecord {
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
        status: crate::model::SourceStatus::Deferred,
    }
}
