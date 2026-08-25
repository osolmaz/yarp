use rusqlite::backup::Backup;
use rusqlite::blob::Blob;
use rusqlite::{
    Connection, ErrorCode, MAIN_DB, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
    params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::{NamedTempFile, tempfile};

#[cfg(test)]
use crate::archive_protocol::ArchiveAck;
use crate::archive_protocol::{ArchiveOperation, INGEST_SCHEMA_VERSION, MAX_FRAME_BYTES};
use crate::config;

const SCHEMA_VERSION: i64 = 1;
const ZSTD_LEVEL: i32 = 3;
const COMPRESSION_PERCENT: u64 = 95;
const ARCHIVE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const ARCHIVE_REF_PREFIX: &str = "yr_";
const ARCHIVE_REF_LEN: usize = 35;

const SCHEMA: &str = r"
CREATE TABLE sessions (
    id                INTEGER PRIMARY KEY,
    agent             TEXT NOT NULL,
    account           TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    started_at_ms     INTEGER,
    UNIQUE (agent, account, source_session_id)
);

CREATE TABLE tool_calls (
    id                INTEGER PRIMARY KEY,
    session_id        INTEGER NOT NULL REFERENCES sessions(id),
    source_call_id    TEXT NOT NULL,
    archive_ref       TEXT NOT NULL UNIQUE CHECK (
        length(archive_ref) = 35
        AND substr(archive_ref, 1, 3) = 'yr_'
        AND substr(archive_ref, 4) NOT GLOB '*[^0-9a-f]*'
    ),
    tool_name         TEXT NOT NULL,
    provider          TEXT,
    model             TEXT,
    working_directory TEXT,
    started_at_ms     INTEGER NOT NULL,
    requires_streams  INTEGER NOT NULL CHECK (requires_streams IN (0, 1)),
    finished_at_ms    INTEGER,
    status            TEXT NOT NULL CHECK (status IN ('started', 'finished')),
    is_error          INTEGER CHECK (is_error IN (0, 1)),
    executed          INTEGER CHECK (executed IN (0, 1)),
    UNIQUE (session_id, source_call_id)
);

CREATE TABLE payloads (
    sha256                   BLOB PRIMARY KEY CHECK (length(sha256) = 32),
    compression              TEXT NOT NULL CHECK (compression IN ('none', 'zstd')),
    uncompressed_byte_length INTEGER NOT NULL CHECK (uncompressed_byte_length >= 0),
    body                     BLOB NOT NULL
);

CREATE TABLE snapshots (
    tool_call_id   INTEGER NOT NULL REFERENCES tool_calls(id) ON DELETE CASCADE,
    subject        TEXT NOT NULL CHECK (subject IN ('input', 'result', 'result_text', 'source_output', 'stdout', 'stderr')),
    stage          TEXT NOT NULL CHECK (stage IN ('before', 'after')),
    media_type     TEXT NOT NULL,
    source_completeness TEXT CHECK (source_completeness IN ('complete', 'incomplete', 'unknown')),
    captured_at_ms INTEGER NOT NULL,
    payload_sha256 BLOB NOT NULL REFERENCES payloads(sha256),
    PRIMARY KEY (tool_call_id, subject, stage),
    CHECK (
        (subject = 'result_text' AND stage = 'before' AND source_completeness IS NOT NULL)
        OR (subject <> 'result_text' AND source_completeness IS NULL)
    )
);

CREATE INDEX tool_calls_started_at_idx ON tool_calls(started_at_ms);
CREATE INDEX tool_calls_tool_name_idx ON tool_calls(tool_name);
CREATE INDEX snapshots_payload_idx ON snapshots(payload_sha256);
";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdentity {
    pub agent: String,
    pub account: String,
    pub source_session_id: String,
    pub started_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallIdentity {
    pub source_call_id: String,
    pub tool_name: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub working_directory: Option<String>,
    pub started_at_ms: i64,
    pub requires_streams: bool,
}

#[derive(Clone, Debug)]
pub struct ArchiveKey {
    pub session: SessionIdentity,
    pub source_call_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCompleteness {
    Complete,
    Incomplete,
    Unknown,
}

impl SourceCompleteness {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceName {
    Stdout,
    Stderr,
    SourceOutput,
    ResultText,
}

impl SourceName {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::SourceOutput => "source_output",
            Self::ResultText => "result_text",
        }
    }
}

pub struct VerifiedSource {
    pub name: SourceName,
    pub completeness: SourceCompleteness,
    pub media_type: String,
    pub body: std::fs::File,
    pub byte_length: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ArchiveStats {
    pub sessions: i64,
    pub calls: i64,
    pub incomplete_calls: i64,
    pub logical_payload_bytes: i64,
    pub stored_payload_bytes: i64,
    pub database_bytes: u64,
    pub oldest_call_ms: Option<i64>,
    pub newest_call_ms: Option<i64>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct VerifyReport {
    pub incomplete_calls: i64,
    pub errors: Vec<String>,
}

pub struct Archive {
    connection: Connection,
    path: PathBuf,
}

pub(crate) struct PreparedArchiveOperation {
    estimated_bytes: u64,
    kind: PreparedOperationKind,
}

enum PreparedOperationKind {
    BeginCall {
        session: SessionIdentity,
        call: CallIdentity,
        before: PreparedPayload,
        after: PreparedPayload,
        captured_at_ms: i64,
    },
    ResultBefore {
        session: SessionIdentity,
        source_call_id: String,
        result: PreparedPayload,
        source_output: Option<PreparedSnapshot>,
        captured_at_ms: i64,
    },
    ResultText {
        session: SessionIdentity,
        source_call_id: String,
        text: PreparedPayload,
        completeness: SourceCompleteness,
        captured_at_ms: i64,
    },
    StageResult {
        session: SessionIdentity,
        source_call_id: String,
        result: PreparedPayload,
        is_error: bool,
        captured_at_ms: i64,
    },
    FinishCall {
        session: SessionIdentity,
        source_call_id: String,
        result: PreparedPayload,
        is_error: bool,
        require_pre_result: bool,
        finished_at_ms: i64,
    },
    UpdateFinalResult {
        session: SessionIdentity,
        source_call_id: String,
        result: PreparedPayload,
        is_error: bool,
        finished_at_ms: i64,
    },
    CaptureStreams {
        session: SessionIdentity,
        source_call_id: String,
        captured_at_ms: i64,
        stdout_before: PreparedSnapshot,
        stderr_before: PreparedSnapshot,
        stdout_after: PreparedSnapshot,
        stderr_after: PreparedSnapshot,
    },
    PruneBefore {
        timestamp_ms: i64,
    },
}

struct PreparedSnapshot {
    media_type: &'static str,
    payload: PreparedPayload,
}

struct PreparedPayload {
    sha: Vec<u8>,
    compression: &'static str,
    uncompressed_length: u64,
    stored_length: u64,
    stored: NamedTempFile,
}

#[derive(Debug)]
pub(crate) enum BatchWriteError {
    Busy(String),
    Permanent(String),
}

impl BatchWriteError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Busy(message) | Self::Permanent(message) => message,
        }
    }

    pub(crate) const fn is_busy(&self) -> bool {
        matches!(self, Self::Busy(_))
    }
}

impl PreparedArchiveOperation {
    pub(crate) const fn estimated_bytes(&self) -> u64 {
        self.estimated_bytes
    }
}

impl Archive {
    /// Open the configured archive and initialize schema version 1 when needed.
    ///
    /// # Errors
    ///
    /// Returns an error when the path, permissions, `SQLite` database, or schema is invalid.
    pub(crate) fn open() -> Result<Self, String> {
        let archive = config::load()?.archive;
        Self::open_path_with_policy(archive.path, archive.is_default_path)
    }

    /// Open the configured archive without performing any writes.
    ///
    /// # Errors
    ///
    /// Returns an error when the archive is missing, unreadable, or uses another schema version.
    pub fn open_read_only() -> Result<Self, String> {
        let path = archive_path()?;
        require_private_archive_path(&path)?;
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| {
                format!(
                    "could not open archive {} read-only: {error}",
                    path.display()
                )
            })?;
        connection
            .busy_timeout(ARCHIVE_BUSY_TIMEOUT)
            .map_err(|error| format!("could not set archive busy timeout: {error}"))?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| format!("could not read archive schema version: {error}"))?;
        if version != SCHEMA_VERSION {
            return Err(format!(
                "archive schema version {version}, expected {SCHEMA_VERSION}"
            ));
        }
        if !schema_is_current(&connection)? {
            return Err(
                "archive schema requires migration; open it with a writable YARP command first"
                    .to_owned(),
            );
        }
        Ok(Self { connection, path })
    }

    /// Open an archive at an explicit path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path, permissions, `SQLite` database, or schema is invalid.
    pub fn open_path(path: PathBuf) -> Result<Self, String> {
        Self::open_path_with_policy(path, false)
    }

    fn open_path_with_policy(
        path: PathBuf,
        repair_existing_directory: bool,
    ) -> Result<Self, String> {
        prepare_path(&path, repair_existing_directory)?;
        let mut connection = Connection::open(&path)
            .map_err(|error| format!("could not open archive {}: {error}", path.display()))?;
        connection
            .busy_timeout(ARCHIVE_BUSY_TIMEOUT)
            .map_err(|error| format!("could not set archive busy timeout: {error}"))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| format!("could not enable archive foreign keys: {error}"))?;
        enable_wal_mode(&connection)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| format!("could not set archive sync mode: {error}"))?;

        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| format!("could not read archive schema version: {error}"))?;
        match version {
            0 => initialize_schema(&mut connection)?,
            SCHEMA_VERSION => ensure_current_schema(&mut connection, &path)?,
            newer if newer > SCHEMA_VERSION => {
                return Err(format!(
                    "archive schema version {newer} is newer than supported version {SCHEMA_VERSION}"
                ));
            }
            older => {
                return Err(format!(
                    "archive schema version {older} has no supported migration"
                ));
            }
        }
        set_file_mode(&path, 0o600)?;
        Ok(Self { connection, path })
    }

    pub(crate) fn configure_for_broker(&self) -> Result<(), String> {
        self.connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|error| format!("could not set archive broker busy timeout: {error}"))?;
        self.connection
            .pragma_update(None, "wal_autocheckpoint", 1000)
            .map_err(|error| format!("could not set archive WAL checkpoint limit: {error}"))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive protocol mapping keeps preparation rules in one place"
    )]
    pub(crate) fn prepare_operation(
        &self,
        operation: ArchiveOperation,
    ) -> Result<PreparedArchiveOperation, String> {
        if operation.schema_version() != INGEST_SCHEMA_VERSION {
            return Err(format!(
                "unsupported ingest schema version {}; expected {INGEST_SCHEMA_VERSION}",
                operation.schema_version()
            ));
        }
        let owner_uid = archive_owner_uid(&self.path)?;
        let kind = match operation {
            ArchiveOperation::BeginCall {
                session,
                call,
                input_before,
                input_after,
                captured_at_ms,
                ..
            } => PreparedOperationKind::BeginCall {
                session,
                call,
                before: prepare_payload(&mut Cursor::new(canonical_json(&input_before)?))?,
                after: prepare_payload(&mut Cursor::new(canonical_json(&input_after)?))?,
                captured_at_ms,
            },
            ArchiveOperation::ResultBefore {
                session,
                source_call_id,
                result,
                full_output_path,
                captured_at_ms,
                ..
            } => PreparedOperationKind::ResultBefore {
                session,
                source_call_id,
                result: prepare_payload(&mut Cursor::new(canonical_json(&result)?))?,
                source_output: full_output_path
                    .as_deref()
                    .map(|path| prepare_path_snapshot(path, owner_uid))
                    .transpose()?,
                captured_at_ms,
            },
            ArchiveOperation::ResultText {
                session,
                source_call_id,
                text,
                source_completeness,
                captured_at_ms,
                ..
            } => PreparedOperationKind::ResultText {
                session,
                source_call_id,
                text: prepare_payload(&mut Cursor::new(text.into_bytes()))?,
                completeness: source_completeness,
                captured_at_ms,
            },
            ArchiveOperation::StageResult {
                session,
                source_call_id,
                result,
                is_error,
                captured_at_ms,
                ..
            } => PreparedOperationKind::StageResult {
                session,
                source_call_id,
                result: prepare_payload(&mut Cursor::new(canonical_json(&result)?))?,
                is_error,
                captured_at_ms,
            },
            ArchiveOperation::FinishCall {
                session,
                source_call_id,
                result,
                is_error,
                require_pre_result,
                finished_at_ms,
                ..
            } => PreparedOperationKind::FinishCall {
                session,
                source_call_id,
                result: prepare_payload(&mut Cursor::new(canonical_json(&result)?))?,
                is_error,
                require_pre_result,
                finished_at_ms,
            },
            ArchiveOperation::UpdateFinalResult {
                session,
                source_call_id,
                result,
                is_error,
                finished_at_ms,
                ..
            } => PreparedOperationKind::UpdateFinalResult {
                session,
                source_call_id,
                result: prepare_payload(&mut Cursor::new(canonical_json(&result)?))?,
                is_error,
                finished_at_ms,
            },
            ArchiveOperation::CaptureStreams {
                session,
                source_call_id,
                captured_at_ms,
                stdout_before_path,
                stderr_before_path,
                stdout_after_path,
                stderr_after_path,
                ..
            } => PreparedOperationKind::CaptureStreams {
                session,
                source_call_id,
                captured_at_ms,
                stdout_before: prepare_path_snapshot(&stdout_before_path, owner_uid)?,
                stderr_before: prepare_path_snapshot(&stderr_before_path, owner_uid)?,
                stdout_after: prepare_path_snapshot(&stdout_after_path, owner_uid)?,
                stderr_after: prepare_path_snapshot(&stderr_after_path, owner_uid)?,
            },
            ArchiveOperation::CapturePassthroughStreams {
                session,
                source_call_id,
                captured_at_ms,
                stdout_path,
                stderr_path,
                ..
            } => {
                let stdout_before = prepare_path_snapshot(&stdout_path, owner_uid)?;
                let stdout_after = prepare_path_snapshot(&stdout_path, owner_uid)?;
                let stderr_before = prepare_path_snapshot(&stderr_path, owner_uid)?;
                let stderr_after = prepare_path_snapshot(&stderr_path, owner_uid)?;
                PreparedOperationKind::CaptureStreams {
                    session,
                    source_call_id,
                    captured_at_ms,
                    stdout_before,
                    stderr_before,
                    stdout_after,
                    stderr_after,
                }
            }
            ArchiveOperation::PruneBefore { timestamp_ms, .. } => {
                PreparedOperationKind::PruneBefore { timestamp_ms }
            }
        };
        let estimated_bytes = prepared_kind_bytes(&kind);
        Ok(PreparedArchiveOperation {
            estimated_bytes,
            kind,
        })
    }

    pub(crate) fn apply_prepared_batch<'a>(
        &mut self,
        operations: impl IntoIterator<Item = &'a mut PreparedArchiveOperation>,
    ) -> Result<Vec<Result<Option<String>, String>>, BatchWriteError> {
        let mut operations = operations.into_iter().collect::<Vec<_>>();
        let mut transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| batch_error("could not start archive batch", &error))?;
        let mut results = Vec::with_capacity(operations.len());
        let mut pruned = false;
        for operation in &mut operations {
            let mut savepoint = transaction.savepoint().map_err(|error| {
                batch_error("could not start archive request savepoint", &error)
            })?;
            match apply_prepared_operation(&savepoint, &mut operation.kind) {
                Ok(result) => {
                    pruned |= matches!(&operation.kind, PreparedOperationKind::PruneBefore { .. });
                    savepoint.commit().map_err(|error| {
                        batch_error("could not release archive request savepoint", &error)
                    })?;
                    results.push(Ok(result));
                }
                Err(error) => {
                    savepoint.rollback().map_err(|rollback_error| {
                        batch_error(
                            "could not roll back failed archive request savepoint",
                            &rollback_error,
                        )
                    })?;
                    results.push(Err(error));
                }
            }
        }
        if pruned {
            transaction
                .execute_batch("PRAGMA incremental_vacuum")
                .map_err(|error| batch_error("could not vacuum archive", &error))?;
        }
        transaction
            .commit()
            .map_err(|error| batch_error("could not commit archive batch", &error))?;
        Ok(results)
    }

    pub(crate) fn checkpoint(&self) -> Result<(), String> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
            .map_err(|error| format!("could not checkpoint archive WAL: {error}"))
    }

    /// Store one call and its input snapshots before execution.
    ///
    /// # Errors
    ///
    /// Returns an error when JSON encoding or the `SQLite` transaction fails.
    pub fn begin_call(
        &mut self,
        session: &SessionIdentity,
        call: &CallIdentity,
        input_before: &Value,
        input_after: &Value,
        captured_at_ms: i64,
    ) -> Result<String, String> {
        let before = canonical_json(input_before)?;
        let after = canonical_json(input_after)?;
        let transaction = self.transaction()?;
        let session_id = ensure_session(&transaction, session)?;
        let call_id = ensure_call(&transaction, session_id, call)?;
        let archive_ref = call_archive_ref(&transaction, call_id)?;
        insert_snapshot_bytes(
            &transaction,
            call_id,
            "input",
            "before",
            "application/json",
            captured_at_ms,
            &before,
        )?;
        insert_snapshot_bytes(
            &transaction,
            call_id,
            "input",
            "after",
            "application/json",
            captured_at_ms,
            &after,
        )?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit tool call: {error}"))?;
        Ok(archive_ref)
    }

    /// Store the result exposed before YARP result processing.
    ///
    /// # Errors
    ///
    /// Returns an error when the call is missing or the payload cannot be committed.
    pub fn result_before(
        &mut self,
        session: &SessionIdentity,
        source_call_id: &str,
        result: &Value,
        full_output_path: Option<&Path>,
        captured_at_ms: i64,
    ) -> Result<(), String> {
        let body = canonical_json(result)?;
        let mut full_output = full_output_path
            .map(|path| {
                let file = OpenOptions::new().read(true).open(path).map_err(|error| {
                    format!(
                        "could not open source full output {}: {error}",
                        path.display()
                    )
                })?;
                let metadata = file.metadata().map_err(|error| {
                    format!(
                        "could not inspect source full output {}: {error}",
                        path.display()
                    )
                })?;
                if !metadata.is_file() {
                    return Err(format!(
                        "source full output {} is not a regular file",
                        path.display()
                    ));
                }
                Ok((file, metadata.len()))
            })
            .transpose()?;
        let transaction = self.transaction()?;
        let call_id = find_call(&transaction, session, source_call_id)?;
        insert_snapshot_bytes(
            &transaction,
            call_id,
            "result",
            "before",
            "application/json",
            captured_at_ms,
            &body,
        )?;
        if let Some((file, original_length)) = &mut full_output {
            insert_snapshot_reader(
                &transaction,
                call_id,
                "source_output",
                "before",
                stream_media_type(file)?,
                captured_at_ms,
                file,
            )?;
            let final_length = file
                .metadata()
                .map_err(|error| format!("could not recheck source full output: {error}"))?
                .len();
            if final_length != *original_length {
                return Err(format!(
                    "source full output changed size while archiving: {original_length} to {final_length} bytes"
                ));
            }
        }
        transaction
            .commit()
            .map_err(|error| format!("could not commit pre-YARP result: {error}"))
    }

    /// Store the exact host-exposed result text selected for post-result reduction.
    ///
    /// # Errors
    ///
    /// Returns an error when the call is missing, completeness is invalid, or the snapshot cannot
    /// be committed.
    pub fn result_text(
        &mut self,
        session: &SessionIdentity,
        source_call_id: &str,
        text: &str,
        completeness: SourceCompleteness,
        captured_at_ms: i64,
    ) -> Result<String, String> {
        let transaction = self.transaction()?;
        let call_id = find_call(&transaction, session, source_call_id)?;
        insert_snapshot_reader_with_completeness(
            &transaction,
            call_id,
            "result_text",
            "before",
            "text/plain; charset=utf-8",
            Some(completeness.as_str()),
            captured_at_ms,
            &mut Cursor::new(text.as_bytes()),
        )?;
        let archive_ref = call_archive_ref(&transaction, call_id)?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit result text: {error}"))?;
        Ok(archive_ref)
    }

    /// Resolve the stable reference for one internally identified call.
    ///
    /// # Errors
    ///
    /// Returns an error when the call or reference is missing.
    pub fn archive_ref(&self, key: &ArchiveKey) -> Result<String, String> {
        self.connection
            .query_row(
                "SELECT c.archive_ref
                 FROM tool_calls c
                 JOIN sessions s ON s.id = c.session_id
                 WHERE s.agent = ?1 AND s.account = ?2 AND s.source_session_id = ?3
                   AND c.source_call_id = ?4",
                params![
                    key.session.agent,
                    key.session.account,
                    key.session.source_session_id,
                    key.source_call_id
                ],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not resolve archive call reference: {error}"))
    }

    /// Resolve and verify the canonical recovery sources for one call reference.
    ///
    /// # Errors
    ///
    /// Returns an error when the reference is malformed, missing, or any selected payload fails
    /// integrity verification.
    pub fn searchable_sources(&self, archive_ref: &str) -> Result<Vec<VerifiedSource>, String> {
        validate_archive_ref(archive_ref)?;
        let call_id: i64 = self
            .connection
            .query_row(
                "SELECT id FROM tool_calls WHERE archive_ref = ?1",
                [archive_ref],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not find archive reference {archive_ref}: {error}"))?;
        let stdout = self.snapshot_exists(call_id, "stdout", "before")?;
        let stderr = self.snapshot_exists(call_id, "stderr", "before")?;
        if stdout || stderr {
            let mut sources = Vec::with_capacity(2);
            if stdout {
                sources.push(self.verified_source(
                    call_id,
                    SourceName::Stdout,
                    SourceCompleteness::Complete,
                )?);
            }
            if stderr {
                sources.push(self.verified_source(
                    call_id,
                    SourceName::Stderr,
                    SourceCompleteness::Complete,
                )?);
            }
            return Ok(sources);
        }
        if self.snapshot_exists(call_id, "result_text", "before")? {
            let value: String = self
                .connection
                .query_row(
                    "SELECT source_completeness FROM snapshots
                     WHERE tool_call_id = ?1 AND subject = 'result_text' AND stage = 'before'",
                    [call_id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("could not read result text completeness: {error}"))?;
            let completeness = parse_completeness(&value)?;
            return Ok(vec![self.verified_source(
                call_id,
                SourceName::ResultText,
                completeness,
            )?]);
        }
        if self.snapshot_exists(call_id, "source_output", "before")? {
            return Ok(vec![self.verified_source(
                call_id,
                SourceName::SourceOutput,
                SourceCompleteness::Complete,
            )?]);
        }
        Err(format!(
            "archive reference {archive_ref} has no searchable source"
        ))
    }

    /// Store the provisional post-YARP result while leaving the call incomplete.
    ///
    /// # Errors
    ///
    /// Returns an error when the call is missing, required snapshots are missing, or the payload
    /// cannot be committed.
    pub fn stage_result(
        &mut self,
        session: &SessionIdentity,
        source_call_id: &str,
        result: &Value,
        is_error: bool,
        captured_at_ms: i64,
    ) -> Result<(), String> {
        let body = canonical_json(result)?;
        let transaction = self.transaction()?;
        let call_id = find_call(&transaction, session, source_call_id)?;
        require_call_status(&transaction, call_id, source_call_id, "started")?;
        validate_completion(&transaction, call_id, true)?;
        insert_snapshot_bytes(
            &transaction,
            call_id,
            "result",
            "after",
            "application/json",
            captured_at_ms,
            &body,
        )?;
        transaction
            .execute(
                "UPDATE tool_calls SET is_error = ?1, executed = 1 WHERE id = ?2",
                params![i64::from(is_error), call_id],
            )
            .map_err(|error| format!("could not stage tool result: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit staged tool result: {error}"))
    }

    /// Store the finalized result and mark the call finished.
    ///
    /// # Errors
    ///
    /// Returns an error when the call is missing or the payload cannot be committed.
    pub fn finish_call(
        &mut self,
        session: &SessionIdentity,
        source_call_id: &str,
        result: &Value,
        is_error: bool,
        require_pre_result: bool,
        finished_at_ms: i64,
    ) -> Result<(), String> {
        let body = canonical_json(result)?;
        let transaction = self.transaction()?;
        let call_id = find_call(&transaction, session, source_call_id)?;
        validate_completion(&transaction, call_id, require_pre_result)?;
        insert_snapshot_bytes(
            &transaction,
            call_id,
            "result",
            "after",
            "application/json",
            finished_at_ms,
            &body,
        )?;
        transaction
            .execute(
                "UPDATE tool_calls
                 SET finished_at_ms = ?1, status = 'finished', is_error = ?2, executed = ?3
                 WHERE id = ?4",
                params![
                    finished_at_ms,
                    i64::from(is_error),
                    i64::from(require_pre_result),
                    call_id
                ],
            )
            .map_err(|error| format!("could not finish tool call: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit final tool result: {error}"))
    }

    /// Finalize a staged call with the result after every Pi result hook has run.
    ///
    /// # Errors
    ///
    /// Returns an error when the call is not staged or the final result cannot be committed.
    pub fn update_final_result(
        &mut self,
        session: &SessionIdentity,
        source_call_id: &str,
        result: &Value,
        is_error: bool,
        finished_at_ms: i64,
    ) -> Result<(), String> {
        let body = canonical_json(result)?;
        let transaction = self.transaction()?;
        let call_id = find_call(&transaction, session, source_call_id)?;
        let status: String = transaction
            .query_row(
                "SELECT status FROM tool_calls WHERE id = ?1",
                [call_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not read final tool call status: {error}"))?;
        if status == "finished" {
            let expected_sha = Sha256::digest(&body).to_vec();
            let stored: (Vec<u8>, bool, i64, bool) = transaction
                .query_row(
                    "SELECT s.payload_sha256, c.is_error, c.finished_at_ms, c.executed
                     FROM tool_calls c
                     JOIN snapshots s ON s.tool_call_id = c.id
                     WHERE c.id = ?1 AND s.subject = 'result' AND s.stage = 'after'",
                    [call_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|error| format!("could not check finalized tool result: {error}"))?;
            if stored == (expected_sha, is_error, finished_at_ms, true) {
                return transaction
                    .commit()
                    .map_err(|error| format!("could not commit repeated final result: {error}"));
            }
            return Err(format!(
                "tool call {source_call_id} was already finalized with different content or metadata"
            ));
        }
        if status != "started" {
            return Err(format!(
                "tool call {source_call_id} has status {status}, expected started"
            ));
        }
        validate_completion(&transaction, call_id, true)?;
        let replaced_sha = replace_snapshot_bytes(
            &transaction,
            call_id,
            "result",
            "after",
            "application/json",
            finished_at_ms,
            &body,
        )?;
        transaction
            .execute(
                "UPDATE tool_calls
                 SET finished_at_ms = ?1, status = 'finished', is_error = ?2, executed = 1
                 WHERE id = ?3",
                params![finished_at_ms, i64::from(is_error), call_id],
            )
            .map_err(|error| format!("could not finalize staged tool call: {error}"))?;
        transaction
            .execute(
                "DELETE FROM payloads
                 WHERE sha256 = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM snapshots WHERE snapshots.payload_sha256 = payloads.sha256
                   )",
                [&replaced_sha],
            )
            .map_err(|error| format!("could not remove replaced final payload: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit final staged tool result: {error}"))
    }

    /// Store exact shell streams before and after pruning.
    ///
    /// # Errors
    ///
    /// Returns an error when a stream cannot be read or the transaction cannot commit.
    pub fn capture_streams(
        &mut self,
        key: &ArchiveKey,
        captured_at_ms: i64,
        stdout_before: &mut (impl Read + Seek),
        stderr_before: &mut (impl Read + Seek),
        stdout_after: &[u8],
        stderr_after: &[u8],
    ) -> Result<(), String> {
        let transaction = self.transaction()?;
        let call_id = find_call(&transaction, &key.session, &key.source_call_id)?;
        insert_snapshot_reader(
            &transaction,
            call_id,
            "stdout",
            "before",
            stream_media_type(stdout_before)?,
            captured_at_ms,
            stdout_before,
        )?;
        insert_snapshot_reader(
            &transaction,
            call_id,
            "stderr",
            "before",
            stream_media_type(stderr_before)?,
            captured_at_ms,
            stderr_before,
        )?;
        insert_snapshot_bytes(
            &transaction,
            call_id,
            "stdout",
            "after",
            stream_media_type(&mut Cursor::new(stdout_after))?,
            captured_at_ms,
            stdout_after,
        )?;
        insert_snapshot_bytes(
            &transaction,
            call_id,
            "stderr",
            "after",
            stream_media_type(&mut Cursor::new(stderr_after))?,
            captured_at_ms,
            stderr_after,
        )?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit shell streams: {error}"))
    }

    /// Store exact shell streams when a wrapper deliberately passed output through unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error when a stream cannot be read or the transaction cannot commit.
    pub fn capture_passthrough_streams(
        &mut self,
        key: &ArchiveKey,
        captured_at_ms: i64,
        stdout: &mut (impl Read + Seek),
        stderr: &mut (impl Read + Seek),
    ) -> Result<(), String> {
        let transaction = self.transaction()?;
        let call_id = find_call(&transaction, &key.session, &key.source_call_id)?;
        let stdout_type = stream_media_type(stdout)?;
        insert_snapshot_reader(
            &transaction,
            call_id,
            "stdout",
            "before",
            stdout_type,
            captured_at_ms,
            stdout,
        )?;
        insert_snapshot_reader(
            &transaction,
            call_id,
            "stdout",
            "after",
            stdout_type,
            captured_at_ms,
            stdout,
        )?;
        let stderr_type = stream_media_type(stderr)?;
        insert_snapshot_reader(
            &transaction,
            call_id,
            "stderr",
            "before",
            stderr_type,
            captured_at_ms,
            stderr,
        )?;
        insert_snapshot_reader(
            &transaction,
            call_id,
            "stderr",
            "after",
            stderr_type,
            captured_at_ms,
            stderr,
        )?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit passthrough shell streams: {error}"))
    }

    /// Restore the exact pre-pruning shell streams for one call.
    ///
    /// # Errors
    ///
    /// Returns an error when the call, snapshots, or payload bodies cannot be read.
    pub fn restore_streams(
        &self,
        key: &ArchiveKey,
        mut stdout: impl Write,
        mut stderr: impl Write,
    ) -> Result<(), String> {
        let call_id: i64 = self
            .connection
            .query_row(
                "SELECT c.id
                 FROM tool_calls c
                 JOIN sessions s ON s.id = c.session_id
                 WHERE s.agent = ?1 AND s.account = ?2 AND s.source_session_id = ?3
                   AND c.source_call_id = ?4",
                params![
                    key.session.agent,
                    key.session.account,
                    key.session.source_session_id,
                    key.source_call_id
                ],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not find tool call {}: {error}", key.source_call_id))?;
        let mut restored_stdout = self.verified_snapshot(call_id, "stdout", "before")?;
        let mut restored_stderr = self.verified_snapshot(call_id, "stderr", "before")?;
        io::copy(&mut restored_stdout, &mut stdout)
            .map_err(|error| format!("could not write restored stdout: {error}"))?;
        io::copy(&mut restored_stderr, &mut stderr)
            .map_err(|error| format!("could not write restored stderr: {error}"))?;
        Ok(())
    }

    /// Read aggregate archive statistics without reading payload content.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` queries or filesystem metadata reads fail.
    pub fn stats(&self) -> Result<ArchiveStats, String> {
        let row = self
            .connection
            .query_row(
                "SELECT
                    (SELECT count(*) FROM sessions),
                    (SELECT count(*) FROM tool_calls),
                    (SELECT count(*) FROM tool_calls WHERE status = 'started'),
                    (SELECT coalesce(sum(uncompressed_byte_length), 0) FROM payloads),
                    (SELECT coalesce(sum(length(body)), 0) FROM payloads),
                    (SELECT min(started_at_ms) FROM tool_calls),
                    (SELECT max(started_at_ms) FROM tool_calls)",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .map_err(|error| format!("could not read archive statistics: {error}"))?;
        let database_bytes = archive_file_bytes(&self.path)?;
        Ok(ArchiveStats {
            sessions: row.0,
            calls: row.1,
            incomplete_calls: row.2,
            logical_payload_bytes: row.3,
            stored_payload_bytes: row.4,
            oldest_call_ms: row.5,
            newest_call_ms: row.6,
            database_bytes,
        })
    }

    /// Verify `SQLite` integrity, relationships, snapshots, payloads, and permissions.
    ///
    /// # Errors
    ///
    /// Returns an error when verification itself cannot read the archive.
    pub fn verify(&self) -> Result<VerifyReport, String> {
        let mut report = VerifyReport::default();
        let integrity: String = self
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|error| format!("could not run SQLite integrity check: {error}"))?;
        if integrity != "ok" {
            report
                .errors
                .push(format!("SQLite integrity check: {integrity}"));
        }
        let foreign_keys: i64 = self
            .connection
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .map_err(|error| format!("could not check archive foreign keys: {error}"))?;
        if foreign_keys > 0 {
            report
                .errors
                .push(format!("{foreign_keys} foreign key violation(s)"));
        }
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| format!("could not read archive schema version: {error}"))?;
        if version != SCHEMA_VERSION {
            report.errors.push(format!(
                "archive schema version {version}, expected {SCHEMA_VERSION}"
            ));
        }
        report.incomplete_calls = self
            .connection
            .query_row(
                "SELECT count(*) FROM tool_calls WHERE status = 'started'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not count incomplete calls: {error}"))?;
        self.verify_constrained_values(&mut report)?;
        self.verify_snapshot_sets(&mut report)?;
        let orphaned: i64 = self
            .connection
            .query_row(
                "SELECT count(*) FROM payloads p WHERE NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.payload_sha256 = p.sha256)",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not check unreferenced payloads: {error}"))?;
        if orphaned > 0 {
            report
                .errors
                .push(format!("{orphaned} unreferenced payload(s)"));
        }
        self.verify_payloads(&mut report)?;
        verify_permissions(&self.path, &mut report)?;
        Ok(report)
    }

    /// Delete finished calls older than the given Unix millisecond timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when deletion, payload cleanup, or vacuuming fails.
    pub fn prune_before(&mut self, timestamp_ms: i64) -> Result<i64, String> {
        let transaction = self.transaction()?;
        let deleted = transaction
            .execute(
                "DELETE FROM tool_calls WHERE status = 'finished' AND finished_at_ms < ?1",
                [timestamp_ms],
            )
            .map_err(|error| format!("could not prune tool calls: {error}"))?;
        transaction
            .execute(
                "DELETE FROM payloads WHERE NOT EXISTS (SELECT 1 FROM snapshots WHERE snapshots.payload_sha256 = payloads.sha256)",
                [],
            )
            .map_err(|error| format!("could not prune unreferenced payloads: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("could not commit archive prune: {error}"))?;
        self.connection
            .execute_batch("PRAGMA incremental_vacuum")
            .map_err(|error| format!("could not vacuum archive: {error}"))?;
        i64::try_from(deleted).map_err(|_| "pruned call count does not fit in i64".to_owned())
    }

    fn verify_constrained_values(&self, report: &mut VerifyReport) -> Result<(), String> {
        let checks = [
            (
                "SELECT count(*) FROM tool_calls
                 WHERE status NOT IN ('started', 'finished')",
                "tool call(s) with invalid status",
            ),
            (
                "SELECT count(*) FROM tool_calls
                 WHERE requires_streams NOT IN (0, 1)",
                "tool call(s) with invalid requires_streams",
            ),
            (
                "SELECT count(*) FROM tool_calls
                 WHERE is_error IS NOT NULL AND is_error NOT IN (0, 1)",
                "tool call(s) with invalid is_error",
            ),
            (
                "SELECT count(*) FROM tool_calls
                 WHERE executed IS NOT NULL AND executed NOT IN (0, 1)",
                "tool call(s) with invalid executed",
            ),
            (
                "SELECT count(*) FROM tool_calls
                 WHERE (status = 'started' AND (
                           finished_at_ms IS NOT NULL
                           OR (is_error IS NULL) != (executed IS NULL)
                           OR executed = 0
                       ))
                    OR (status = 'finished' AND (finished_at_ms IS NULL OR is_error IS NULL OR executed IS NULL))",
                "tool call(s) with inconsistent lifecycle state",
            ),
            (
                "SELECT count(*) FROM snapshots
                 WHERE subject NOT IN ('input', 'result', 'result_text', 'source_output', 'stdout', 'stderr')",
                "snapshot(s) with invalid subject",
            ),
            (
                "SELECT count(*) FROM snapshots
                 WHERE (subject = 'result_text' AND (stage != 'before' OR source_completeness NOT IN ('complete', 'incomplete', 'unknown')))
                    OR (subject != 'result_text' AND source_completeness IS NOT NULL)",
                "snapshot(s) with invalid source completeness",
            ),
            (
                "SELECT count(*) FROM snapshots WHERE stage NOT IN ('before', 'after')",
                "snapshot(s) with invalid stage",
            ),
            (
                "SELECT count(*) FROM snapshots
                 WHERE (subject IN ('input', 'result') AND media_type != 'application/json')
                    OR (subject = 'result_text' AND media_type != 'text/plain; charset=utf-8')
                    OR (subject IN ('source_output', 'stdout', 'stderr') AND media_type NOT IN ('text/plain; charset=utf-8', 'application/octet-stream'))",
                "snapshot(s) with invalid media type",
            ),
            (
                "SELECT count(*) FROM tool_calls
                 WHERE length(archive_ref) != 35
                    OR substr(archive_ref, 1, 3) != 'yr_'
                    OR substr(archive_ref, 4) GLOB '*[^0-9a-f]*'",
                "tool call(s) with invalid archive reference",
            ),
            (
                "SELECT count(*) FROM payloads
                 WHERE length(sha256) != 32
                    OR compression NOT IN ('none', 'zstd')
                    OR uncompressed_byte_length < 0",
                "payload(s) with invalid constrained values",
            ),
        ];
        for (query, message) in checks {
            let invalid: i64 = self
                .connection
                .query_row(query, [], |row| row.get(0))
                .map_err(|error| format!("could not verify constrained values: {error}"))?;
            if invalid > 0 {
                report.errors.push(format!("{invalid} {message}"));
            }
        }
        Ok(())
    }

    fn verify_snapshot_sets(&self, report: &mut VerifyReport) -> Result<(), String> {
        let checks = [
            (
                "SELECT count(*) FROM tool_calls c
                 WHERE NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.tool_call_id = c.id AND s.subject = 'input' AND s.stage = 'before')
                    OR NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.tool_call_id = c.id AND s.subject = 'input' AND s.stage = 'after')",
                "call(s) missing input snapshots",
            ),
            (
                "SELECT count(*) FROM tool_calls c
                 WHERE c.status = 'finished'
                   AND NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.tool_call_id = c.id AND s.subject = 'result' AND s.stage = 'after')",
                "finished call(s) missing final results",
            ),
            (
                "SELECT count(*) FROM tool_calls c
                 WHERE c.status = 'finished' AND c.executed = 1
                   AND NOT EXISTS (
                       SELECT 1 FROM snapshots s
                       WHERE s.tool_call_id = c.id AND s.subject = 'result' AND s.stage = 'before'
                   )",
                "executed call(s) missing pre-YARP results",
            ),
            (
                "SELECT count(*) FROM tool_calls c
                 WHERE c.status = 'finished' AND c.requires_streams = 1 AND c.executed = 1
                   AND 4 != (
                       SELECT count(*) FROM snapshots s
                       WHERE s.tool_call_id = c.id
                         AND s.subject IN ('stdout', 'stderr')
                         AND s.stage IN ('before', 'after')
                   )",
                "shell call(s) missing complete stream snapshots",
            ),
        ];
        for (query, message) in checks {
            let missing: i64 = self
                .connection
                .query_row(query, [], |row| row.get(0))
                .map_err(|error| format!("could not verify snapshot sets: {error}"))?;
            if missing > 0 {
                report.errors.push(format!("{missing} {message}"));
            }
        }
        Ok(())
    }

    fn snapshot_exists(&self, call_id: i64, subject: &str, stage: &str) -> Result<bool, String> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM snapshots
                    WHERE tool_call_id = ?1 AND subject = ?2 AND stage = ?3
                )",
                params![call_id, subject, stage],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not inspect {subject}/{stage} snapshot: {error}"))
    }

    fn verified_source(
        &self,
        call_id: i64,
        name: SourceName,
        completeness: SourceCompleteness,
    ) -> Result<VerifiedSource, String> {
        let subject = name.as_str();
        let (media_type, byte_length): (String, i64) = self
            .connection
            .query_row(
                "SELECT s.media_type, p.uncompressed_byte_length
                 FROM snapshots s
                 JOIN payloads p ON p.sha256 = s.payload_sha256
                 WHERE s.tool_call_id = ?1 AND s.subject = ?2 AND s.stage = 'before'",
                params![call_id, subject],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| format!("could not inspect {subject}/before snapshot: {error}"))?;
        let byte_length = u64::try_from(byte_length)
            .map_err(|_| format!("invalid {subject}/before byte length"))?;
        let body = self.verified_snapshot(call_id, subject, "before")?;
        Ok(VerifiedSource {
            name,
            completeness,
            media_type,
            body,
            byte_length,
        })
    }

    fn verified_snapshot(
        &self,
        call_id: i64,
        subject: &str,
        stage: &str,
    ) -> Result<std::fs::File, String> {
        let (rowid, expected_sha, compression, expected_length): (i64, Vec<u8>, String, i64) = self
            .connection
            .query_row(
                "SELECT p.rowid, p.sha256, p.compression, p.uncompressed_byte_length
                 FROM snapshots s
                 JOIN payloads p ON p.sha256 = s.payload_sha256
                 WHERE s.tool_call_id = ?1 AND s.subject = ?2 AND s.stage = ?3",
                params![call_id, subject, stage],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|error| format!("could not find {subject}/{stage} snapshot: {error}"))?;
        let blob = self
            .connection
            .blob_open(MAIN_DB, "payloads", "body", rowid, true)
            .map_err(|error| format!("could not open {subject}/{stage} payload: {error}"))?;
        let mut reader: Box<dyn Read> = match compression.as_str() {
            "none" => Box::new(blob),
            "zstd" => Box::new(
                zstd::stream::read::Decoder::new(blob)
                    .map_err(|error| format!("could not decode {subject}/{stage}: {error}"))?,
            ),
            unknown => return Err(format!("unknown payload compression {unknown}")),
        };
        let expected_length = u64::try_from(expected_length)
            .map_err(|_| format!("invalid {subject}/{stage} payload length {expected_length}"))?;
        let mut restored = tempfile().map_err(|error| {
            format!("could not create {subject}/{stage} restore spool: {error}")
        })?;
        let mut hasher = Sha256::new();
        let mut length = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let count = reader
                .read(&mut buffer)
                .map_err(|error| format!("could not read {subject}/{stage} payload: {error}"))?;
            if count == 0 {
                break;
            }
            restored
                .write_all(&buffer[..count])
                .map_err(|error| format!("could not spool {subject}/{stage} payload: {error}"))?;
            hasher.update(&buffer[..count]);
            length = length.saturating_add(count as u64);
        }
        let actual_sha: [u8; 32] = hasher.finalize().into();
        if length != expected_length || actual_sha.as_slice() != expected_sha {
            return Err(format!(
                "{subject}/{stage} payload failed integrity verification"
            ));
        }
        restored.seek(SeekFrom::Start(0)).map_err(|error| {
            format!("could not rewind {subject}/{stage} restore spool: {error}")
        })?;
        Ok(restored)
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, String> {
        self.connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("could not start archive transaction: {error}"))
    }

    fn verify_payloads(&self, report: &mut VerifyReport) -> Result<(), String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT rowid, sha256, compression, uncompressed_byte_length, length(body) FROM payloads",
            )
            .map_err(|error| format!("could not prepare payload verification: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|error| format!("could not list archive payloads: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("could not read archive payload metadata: {error}"))?;
        drop(statement);

        for (rowid, expected_sha, compression, expected_length, _) in rows {
            let blob = self
                .connection
                .blob_open(MAIN_DB, "payloads", "body", rowid, true)
                .map_err(|error| format!("could not open payload {rowid}: {error}"))?;
            let mut reader: Box<dyn Read> = match compression.as_str() {
                "none" => Box::new(blob),
                "zstd" => Box::new(
                    zstd::stream::read::Decoder::new(blob)
                        .map_err(|error| format!("could not decode payload {rowid}: {error}"))?,
                ),
                unknown => {
                    report
                        .errors
                        .push(format!("payload {rowid} has unknown compression {unknown}"));
                    continue;
                }
            };
            let mut hasher = Sha256::new();
            let mut length = 0_u64;
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let count = reader
                    .read(&mut buffer)
                    .map_err(|error| format!("could not read payload {rowid}: {error}"))?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
                length = length.saturating_add(count as u64);
            }
            if length != u64::try_from(expected_length).unwrap_or(u64::MAX) {
                report.errors.push(format!(
                    "payload {rowid} length {length}, expected {expected_length}"
                ));
            }
            if hasher.finalize().as_slice() != expected_sha {
                report
                    .errors
                    .push(format!("payload {rowid} SHA-256 mismatch"));
            }
        }
        Ok(())
    }
}

/// Read framed archive operations and acknowledge each committed transaction.
///
/// # Errors
///
/// Returns an error when the archive, frame, operation, or acknowledgement is invalid.
pub fn run_ingest(input: impl Read, output: impl Write) -> Result<(), String> {
    crate::archive_client::run_ingest_bridge(input, output)
}

#[cfg(test)]
fn run_ingest_with_archive(
    mut archive: Archive,
    mut input: impl Read,
    mut output: impl Write,
) -> Result<(), String> {
    loop {
        let mut length_bytes = [0_u8; 8];
        match input.read(&mut length_bytes[..1]) {
            Ok(0) => return Ok(()),
            Ok(1) => {}
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(error) => return Err(format!("could not read ingest frame length: {error}")),
        }
        input
            .read_exact(&mut length_bytes[1..])
            .map_err(|error| format!("truncated ingest frame length: {error}"))?;
        let length = u64::from_be_bytes(length_bytes);
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(format!("invalid ingest frame length {length}"));
        }
        let frame_length = usize::try_from(length)
            .map_err(|_| format!("ingest frame length {length} does not fit in memory"))?;
        let mut frame = vec![0_u8; frame_length];
        input
            .read_exact(&mut frame)
            .map_err(|error| format!("could not read ingest frame: {error}"))?;
        let operation: ArchiveOperation = serde_json::from_slice(&frame)
            .map_err(|error| format!("invalid ingest frame: {error}"))?;
        let request_id = operation.request_id();
        let result = apply_operation(&mut archive, operation);
        let ack = match result {
            Ok(archive_ref) => ArchiveAck::success(request_id, archive_ref),
            Err(error) => ArchiveAck::failure(request_id, error),
        };
        serde_json::to_writer(&mut output, &ack)
            .map_err(|error| format!("could not write ingest acknowledgement: {error}"))?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(|error| format!("could not flush ingest acknowledgement: {error}"))?;
    }
}

#[cfg(test)]
fn apply_operation(
    archive: &mut Archive,
    operation: ArchiveOperation,
) -> Result<Option<String>, String> {
    let schema_version = operation.schema_version();
    if schema_version != INGEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported ingest schema version {schema_version}; expected {INGEST_SCHEMA_VERSION}"
        ));
    }
    match operation {
        ArchiveOperation::BeginCall {
            session,
            call,
            input_before,
            input_after,
            captured_at_ms,
            ..
        } => archive
            .begin_call(&session, &call, &input_before, &input_after, captured_at_ms)
            .map(Some),
        ArchiveOperation::ResultBefore {
            session,
            source_call_id,
            result,
            full_output_path,
            captured_at_ms,
            ..
        } => archive
            .result_before(
                &session,
                &source_call_id,
                &result,
                full_output_path.as_deref(),
                captured_at_ms,
            )
            .map(|()| None),
        ArchiveOperation::ResultText {
            session,
            source_call_id,
            text,
            source_completeness,
            captured_at_ms,
            ..
        } => archive
            .result_text(
                &session,
                &source_call_id,
                &text,
                source_completeness,
                captured_at_ms,
            )
            .map(Some),
        ArchiveOperation::StageResult {
            session,
            source_call_id,
            result,
            is_error,
            captured_at_ms,
            ..
        } => archive
            .stage_result(&session, &source_call_id, &result, is_error, captured_at_ms)
            .map(|()| None),
        ArchiveOperation::FinishCall {
            session,
            source_call_id,
            result,
            is_error,
            require_pre_result,
            finished_at_ms,
            ..
        } => archive
            .finish_call(
                &session,
                &source_call_id,
                &result,
                is_error,
                require_pre_result,
                finished_at_ms,
            )
            .map(|()| None),
        ArchiveOperation::UpdateFinalResult {
            session,
            source_call_id,
            result,
            is_error,
            finished_at_ms,
            ..
        } => archive
            .update_final_result(&session, &source_call_id, &result, is_error, finished_at_ms)
            .map(|()| None),
        ArchiveOperation::CaptureStreams { .. }
        | ArchiveOperation::CapturePassthroughStreams { .. }
        | ArchiveOperation::PruneBefore { .. } => {
            Err("operation is available only through the archive broker".to_owned())
        }
    }
}

/// Resolve the archive path from the YARP configuration.
///
/// # Errors
///
/// Returns an error when configuration or path resolution fails.
pub fn archive_path() -> Result<PathBuf, String> {
    config::load().map(|resolved| resolved.archive.path)
}

fn enable_wal_mode(connection: &Connection) -> Result<(), String> {
    const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(25);
    const RETRY_DELAY: Duration = Duration::from_millis(10);
    connection
        .busy_timeout(ATTEMPT_TIMEOUT)
        .map_err(|error| format!("could not set WAL retry timeout: {error}"))?;
    let deadline = Instant::now() + ARCHIVE_BUSY_TIMEOUT;
    loop {
        match connection.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => {
                connection
                    .busy_timeout(ARCHIVE_BUSY_TIMEOUT)
                    .map_err(|error| format!("could not restore archive busy timeout: {error}"))?;
                return Ok(());
            }
            Err(error) if is_lock_error(&error) && Instant::now() < deadline => {
                std::thread::sleep(RETRY_DELAY);
            }
            Err(error) => return Err(format!("could not enable archive WAL mode: {error}")),
        }
    }
}

fn is_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(inner.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn initialize_schema(connection: &mut Connection) -> Result<(), String> {
    connection
        .pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .map_err(|error| format!("could not enable incremental auto-vacuum: {error}"))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(|error| format!("could not start archive migration: {error}"))?;
    let version: i64 = transaction
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| format!("could not recheck archive schema version: {error}"))?;
    match version {
        0 => {
            transaction
                .execute_batch(SCHEMA)
                .map_err(|error| format!("could not create archive schema: {error}"))?;
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|error| format!("could not set archive schema version: {error}"))?;
        }
        SCHEMA_VERSION => {}
        other => {
            return Err(format!(
                "archive schema version changed to {other} while initializing; expected 0 or {SCHEMA_VERSION}"
            ));
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("could not commit archive schema: {error}"))
}

fn schema_is_current(connection: &Connection) -> Result<bool, String> {
    let archive_ref = table_has_column(connection, "tool_calls", "archive_ref")?;
    let source_completeness = table_has_column(connection, "snapshots", "source_completeness")?;
    if archive_ref != source_completeness {
        return Err("archive schema has a partial indexed-output migration".to_owned());
    }
    if !archive_ref {
        return Ok(false);
    }
    let snapshots_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'snapshots'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not inspect snapshots schema: {error}"))?;
    Ok(snapshots_sql.contains("result_text") && snapshots_sql.contains("source_completeness"))
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let query = format!("SELECT count(*) FROM pragma_table_info('{table}') WHERE name = ?1");
    connection
        .query_row(&query, [column], |row| row.get::<_, i64>(0))
        .map(|count| count == 1)
        .map_err(|error| format!("could not inspect {table}.{column}: {error}"))
}

fn ensure_current_schema(connection: &mut Connection, path: &Path) -> Result<(), String> {
    if schema_is_current(connection)? {
        return Ok(());
    }
    create_migration_backup(connection, path)?;
    migrate_indexed_output_schema(connection)
}

fn create_migration_backup(connection: &Connection, path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "archive path has no parent for migration backup".to_owned())?;
    let required = archive_file_bytes(path)?.saturating_mul(2);
    let available = fs2::available_space(parent)
        .map_err(|error| format!("could not measure archive backup space: {error}"))?;
    if available < required {
        return Err(format!(
            "archive migration needs at least {required} free bytes, found {available}"
        ));
    }
    let mut backup_name = path
        .file_name()
        .ok_or_else(|| "archive path has no file name".to_owned())?
        .to_os_string();
    backup_name.push(".pre-indexed-output-v1.backup");
    let backup_path = parent.join(backup_name);
    if backup_path.exists() {
        return Err(format!(
            "archive migration backup already exists: {}",
            backup_path.display()
        ));
    }
    let temporary = NamedTempFile::new_in(parent)
        .map_err(|error| format!("could not create migration backup: {error}"))?;
    set_file_mode(temporary.path(), 0o600)?;
    let mut destination = Connection::open(temporary.path())
        .map_err(|error| format!("could not open migration backup: {error}"))?;
    let backup = Backup::new(connection, &mut destination)
        .map_err(|error| format!("could not start archive backup: {error}"))?;
    backup
        .run_to_completion(128, Duration::from_millis(10), None)
        .map_err(|error| format!("could not write archive backup: {error}"))?;
    drop(backup);
    let integrity: String = destination
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| format!("could not verify migration backup: {error}"))?;
    if integrity != "ok" {
        return Err(format!(
            "migration backup integrity check failed: {integrity}"
        ));
    }
    destination
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| format!("could not checkpoint migration backup: {error}"))?;
    drop(destination);
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("could not sync migration backup: {error}"))?;
    temporary
        .persist(&backup_path)
        .map_err(|error| format!("could not install migration backup: {}", error.error))?;
    set_file_mode(&backup_path, 0o600)?;
    sync_directory(parent)
}

#[expect(
    clippy::too_many_lines,
    reason = "the migration is one ordered transaction with shared invariants and rollback"
)]
fn migrate_indexed_output_schema(connection: &mut Connection) -> Result<(), String> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(|error| format!("could not start indexed-output migration: {error}"))?;
    if table_has_column(&transaction, "tool_calls", "archive_ref")?
        || table_has_column(&transaction, "snapshots", "source_completeness")?
    {
        return Err("archive schema changed while preparing indexed-output migration".to_owned());
    }
    let calls_before: i64 = transaction
        .query_row("SELECT count(*) FROM tool_calls", [], |row| row.get(0))
        .map_err(|error| format!("could not count calls before migration: {error}"))?;
    let snapshots_before: i64 = transaction
        .query_row("SELECT count(*) FROM snapshots", [], |row| row.get(0))
        .map_err(|error| format!("could not count snapshots before migration: {error}"))?;
    transaction
        .execute_batch("ALTER TABLE tool_calls ADD COLUMN archive_ref TEXT")
        .map_err(|error| format!("could not add archive references: {error}"))?;
    transaction
        .execute(
            "UPDATE tool_calls SET archive_ref = 'yr_' || lower(hex(randomblob(16)))",
            [],
        )
        .map_err(|error| format!("could not backfill archive references: {error}"))?;
    for _ in 0..3 {
        let duplicates: i64 = transaction
            .query_row(
                "SELECT count(*) - count(DISTINCT archive_ref) FROM tool_calls",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not check archive reference collisions: {error}"))?;
        if duplicates == 0 {
            break;
        }
        transaction
            .execute(
                "UPDATE tool_calls
                 SET archive_ref = 'yr_' || lower(hex(randomblob(16)))
                 WHERE id IN (
                     SELECT later.id
                     FROM tool_calls later
                     JOIN tool_calls earlier
                       ON earlier.archive_ref = later.archive_ref AND earlier.id < later.id
                 )",
                [],
            )
            .map_err(|error| format!("could not retry archive reference collisions: {error}"))?;
    }
    let duplicates: i64 = transaction
        .query_row(
            "SELECT count(*) - count(DISTINCT archive_ref) FROM tool_calls",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not verify archive reference uniqueness: {error}"))?;
    if duplicates != 0 {
        return Err("archive reference collisions remained after three retries".to_owned());
    }
    transaction
        .execute_batch(
            "CREATE TABLE tool_calls_new (
                id                INTEGER PRIMARY KEY,
                session_id        INTEGER NOT NULL REFERENCES sessions(id),
                source_call_id    TEXT NOT NULL,
                archive_ref       TEXT NOT NULL UNIQUE CHECK (
                    length(archive_ref) = 35
                    AND substr(archive_ref, 1, 3) = 'yr_'
                    AND substr(archive_ref, 4) NOT GLOB '*[^0-9a-f]*'
                ),
                tool_name         TEXT NOT NULL,
                provider          TEXT,
                model             TEXT,
                working_directory TEXT,
                started_at_ms     INTEGER NOT NULL,
                requires_streams  INTEGER NOT NULL CHECK (requires_streams IN (0, 1)),
                finished_at_ms    INTEGER,
                status            TEXT NOT NULL CHECK (status IN ('started', 'finished')),
                is_error          INTEGER CHECK (is_error IN (0, 1)),
                executed          INTEGER CHECK (executed IN (0, 1)),
                UNIQUE (session_id, source_call_id)
            );
            INSERT INTO tool_calls_new
            SELECT id, session_id, source_call_id, archive_ref, tool_name, provider, model,
                   working_directory, started_at_ms, requires_streams, finished_at_ms, status,
                   is_error, executed
            FROM tool_calls;
            CREATE TABLE snapshots_new (
                tool_call_id   INTEGER NOT NULL REFERENCES tool_calls_new(id) ON DELETE CASCADE,
                subject        TEXT NOT NULL CHECK (subject IN ('input', 'result', 'result_text', 'source_output', 'stdout', 'stderr')),
                stage          TEXT NOT NULL CHECK (stage IN ('before', 'after')),
                media_type     TEXT NOT NULL,
                source_completeness TEXT CHECK (source_completeness IN ('complete', 'incomplete', 'unknown')),
                captured_at_ms INTEGER NOT NULL,
                payload_sha256 BLOB NOT NULL REFERENCES payloads(sha256),
                PRIMARY KEY (tool_call_id, subject, stage),
                CHECK (
                    (subject = 'result_text' AND stage = 'before' AND source_completeness IS NOT NULL)
                    OR (subject <> 'result_text' AND source_completeness IS NULL)
                )
            );
            INSERT INTO snapshots_new (
                tool_call_id, subject, stage, media_type, source_completeness,
                captured_at_ms, payload_sha256
            )
            SELECT tool_call_id, subject, stage, media_type, NULL,
                   captured_at_ms, payload_sha256
            FROM snapshots;
            DROP TABLE snapshots;
            DROP TABLE tool_calls;
            ALTER TABLE tool_calls_new RENAME TO tool_calls;
            ALTER TABLE snapshots_new RENAME TO snapshots;
            CREATE INDEX tool_calls_started_at_idx ON tool_calls(started_at_ms);
            CREATE INDEX tool_calls_tool_name_idx ON tool_calls(tool_name);
            CREATE INDEX snapshots_payload_idx ON snapshots(payload_sha256);",
        )
        .map_err(|error| format!("could not rebuild indexed-output tables: {error}"))?;
    let calls_after: i64 = transaction
        .query_row("SELECT count(*) FROM tool_calls", [], |row| row.get(0))
        .map_err(|error| format!("could not count calls after migration: {error}"))?;
    let snapshots_after: i64 = transaction
        .query_row("SELECT count(*) FROM snapshots", [], |row| row.get(0))
        .map_err(|error| format!("could not count snapshots after migration: {error}"))?;
    if calls_before != calls_after || snapshots_before != snapshots_after {
        return Err("archive row counts changed during migration".to_owned());
    }
    let invalid_refs: i64 = transaction
        .query_row(
            "SELECT count(*) FROM tool_calls
             WHERE length(archive_ref) != 35
                OR substr(archive_ref, 1, 3) != 'yr_'
                OR substr(archive_ref, 4) GLOB '*[^0-9a-f]*'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not verify migrated archive references: {error}"))?;
    let foreign_keys: i64 = transaction
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(|error| format!("could not verify migrated foreign keys: {error}"))?;
    if invalid_refs != 0 || foreign_keys != 0 {
        return Err(format!(
            "archive migration verification failed: {invalid_refs} invalid references, {foreign_keys} foreign key violations"
        ));
    }
    transaction
        .commit()
        .map_err(|error| format!("could not commit indexed-output migration: {error}"))
}

fn prepared_kind_bytes(kind: &PreparedOperationKind) -> u64 {
    let payload = |value: &PreparedPayload| value.stored_length;
    let snapshot = |value: &PreparedSnapshot| payload(&value.payload);
    match kind {
        PreparedOperationKind::BeginCall { before, after, .. } => payload(before) + payload(after),
        PreparedOperationKind::ResultBefore {
            result,
            source_output,
            ..
        } => payload(result) + source_output.as_ref().map_or(0, snapshot),
        PreparedOperationKind::ResultText { text, .. } => payload(text),
        PreparedOperationKind::StageResult { result, .. }
        | PreparedOperationKind::FinishCall { result, .. }
        | PreparedOperationKind::UpdateFinalResult { result, .. } => payload(result),
        PreparedOperationKind::CaptureStreams {
            stdout_before,
            stderr_before,
            stdout_after,
            stderr_after,
            ..
        } => {
            snapshot(stdout_before)
                + snapshot(stderr_before)
                + snapshot(stdout_after)
                + snapshot(stderr_after)
        }
        PreparedOperationKind::PruneBefore { .. } => 0,
    }
}

fn prepare_payload(reader: &mut (impl Read + Seek)) -> Result<PreparedPayload, String> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind payload for preparation: {error}"))?;
    let mut raw = NamedTempFile::new()
        .map_err(|error| format!("could not create prepared payload spool: {error}"))?;
    let mut hasher = Sha256::new();
    let mut uncompressed_length = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not read payload for preparation: {error}"))?;
        if count == 0 {
            break;
        }
        raw.write_all(&buffer[..count])
            .map_err(|error| format!("could not spool prepared payload: {error}"))?;
        hasher.update(&buffer[..count]);
        uncompressed_length = uncompressed_length.saturating_add(count as u64);
        if uncompressed_length > MAX_FRAME_BYTES {
            return Err(format!(
                "prepared payload is {uncompressed_length} bytes; maximum is {MAX_FRAME_BYTES}"
            ));
        }
    }
    raw.flush()
        .map_err(|error| format!("could not flush prepared payload: {error}"))?;
    raw.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind prepared payload: {error}"))?;
    let mut compressed = NamedTempFile::new()
        .map_err(|error| format!("could not create compressed payload spool: {error}"))?;
    zstd::stream::copy_encode(&mut raw, &mut compressed, ZSTD_LEVEL)
        .map_err(|error| format!("could not compress prepared payload: {error}"))?;
    compressed
        .flush()
        .map_err(|error| format!("could not flush compressed prepared payload: {error}"))?;
    let compressed_length = compressed
        .as_file()
        .metadata()
        .map_err(|error| format!("could not stat compressed prepared payload: {error}"))?
        .len();
    let use_compressed = compressed_length.saturating_mul(100)
        <= uncompressed_length.saturating_mul(COMPRESSION_PERCENT);
    let (compression, stored_length, mut stored) = if use_compressed {
        ("zstd", compressed_length, compressed)
    } else {
        ("none", uncompressed_length, raw)
    };
    stored
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind stored prepared payload: {error}"))?;
    Ok(PreparedPayload {
        sha: hasher.finalize().to_vec(),
        compression,
        uncompressed_length,
        stored_length,
        stored,
    })
}

fn prepare_path_snapshot(path: &Path, expected_uid: u32) -> Result<PreparedSnapshot, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect archive source file: {error}"))?;
    require_private_source(path, &before, expected_uid)?;
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| format!("could not open archive source file: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("could not inspect open archive source file: {error}"))?;
    require_same_source(path, &before, &opened)?;
    let media_type = stream_media_type(&mut file)?;
    let payload = prepare_payload(&mut file)?;
    let after = fs::symlink_metadata(path)
        .map_err(|error| format!("could not recheck archive source file: {error}"))?;
    require_same_source(path, &opened, &after)?;
    Ok(PreparedSnapshot {
        media_type,
        payload,
    })
}

#[cfg(unix)]
fn archive_owner_uid(path: &Path) -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt as _;
    let parent = path
        .parent()
        .ok_or_else(|| format!("archive path {} has no parent", path.display()))?;
    parent
        .metadata()
        .map(|metadata| metadata.uid())
        .map_err(|error| {
            format!(
                "could not inspect archive directory {}: {error}",
                parent.display()
            )
        })
}

#[cfg(not(unix))]
fn archive_owner_uid(_path: &Path) -> Result<u32, String> {
    Ok(0)
}

#[cfg(unix)]
fn require_private_source(
    _path: &Path,
    metadata: &fs::Metadata,
    expected_uid: u32,
) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("archive source is not a real regular file".to_owned());
    }
    if metadata.uid() != expected_uid {
        return Err("archive source has the wrong owner".to_owned());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("archive source is accessible by other users".to_owned());
    }
    if metadata.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "archive source is {} bytes; maximum is {MAX_FRAME_BYTES}",
            metadata.len()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_private_source(
    _path: &Path,
    metadata: &fs::Metadata,
    _expected_uid: u32,
) -> Result<(), String> {
    if !metadata.is_file() {
        return Err("archive source is not a regular file".to_owned());
    }
    Ok(())
}

#[cfg(unix)]
fn require_same_source(
    _path: &Path,
    expected: &fs::Metadata,
    actual: &fs::Metadata,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    if (
        expected.dev(),
        expected.ino(),
        expected.len(),
        expected.mtime(),
        expected.mtime_nsec(),
    ) != (
        actual.dev(),
        actual.ino(),
        actual.len(),
        actual.mtime(),
        actual.mtime_nsec(),
    ) {
        return Err("archive source changed while being read".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_same_source(
    _path: &Path,
    expected: &fs::Metadata,
    actual: &fs::Metadata,
) -> Result<(), String> {
    if expected.len() != actual.len() {
        return Err("archive source changed while being read".to_owned());
    }
    Ok(())
}

fn insert_prepared_payload(
    connection: &Connection,
    payload: &mut PreparedPayload,
) -> Result<Vec<u8>, String> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM payloads WHERE sha256 = ?1",
            [&payload.sha],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| format!("could not check prepared payload digest: {error}"))?
        .is_some();
    if exists {
        return Ok(payload.sha.clone());
    }
    let stored_length = i64::try_from(payload.stored_length).map_err(|_| {
        format!(
            "payload is too large for SQLite: {} bytes",
            payload.stored_length
        )
    })?;
    let raw_length = i64::try_from(payload.uncompressed_length).map_err(|_| {
        format!(
            "payload is too large: {} bytes",
            payload.uncompressed_length
        )
    })?;
    connection
        .execute(
            "INSERT INTO payloads (sha256, compression, uncompressed_byte_length, body)
             VALUES (?1, ?2, ?3, zeroblob(?4))",
            params![payload.sha, payload.compression, raw_length, stored_length],
        )
        .map_err(|error| format!("could not insert prepared payload: {error}"))?;
    let rowid: i64 = connection
        .query_row(
            "SELECT rowid FROM payloads WHERE sha256 = ?1",
            [&payload.sha],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not locate prepared payload body: {error}"))?;
    let mut blob = connection
        .blob_open(MAIN_DB, "payloads", "body", rowid, false)
        .map_err(|error| format!("could not open prepared payload body: {error}"))?;
    payload
        .stored
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind prepared payload body: {error}"))?;
    copy_exact(&mut payload.stored, &mut blob, payload.stored_length)?;
    Ok(payload.sha.clone())
}

#[expect(
    clippy::too_many_arguments,
    reason = "snapshot identity and source metadata are one strict database record"
)]
fn insert_prepared_snapshot(
    connection: &Connection,
    call_id: i64,
    subject: &str,
    stage: &str,
    media_type: &str,
    source_completeness: Option<&str>,
    captured_at_ms: i64,
    payload: &mut PreparedPayload,
) -> Result<(), String> {
    let sha = insert_prepared_payload(connection, payload)?;
    let inserted = connection
        .execute(
            "INSERT INTO snapshots (
                tool_call_id, subject, stage, media_type, source_completeness,
                captured_at_ms, payload_sha256
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(tool_call_id, subject, stage) DO NOTHING",
            params![
                call_id,
                subject,
                stage,
                media_type,
                source_completeness,
                captured_at_ms,
                sha
            ],
        )
        .map_err(|error| format!("could not store {subject}/{stage} snapshot: {error}"))?;
    if inserted == 0 {
        let existing: (Vec<u8>, Option<String>) = connection
            .query_row(
                "SELECT payload_sha256, source_completeness FROM snapshots
                 WHERE tool_call_id = ?1 AND subject = ?2 AND stage = ?3",
                params![call_id, subject, stage],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| format!("could not check existing snapshot: {error}"))?;
        if existing != (sha, source_completeness.map(str::to_owned)) {
            return Err(format!(
                "snapshot {subject}/{stage} already exists with different content or completeness"
            ));
        }
    }
    Ok(())
}

fn replace_prepared_snapshot(
    connection: &Connection,
    call_id: i64,
    subject: &str,
    stage: &str,
    media_type: &str,
    captured_at_ms: i64,
    payload: &mut PreparedPayload,
) -> Result<Vec<u8>, String> {
    let replaced_sha: Vec<u8> = connection
        .query_row(
            "SELECT payload_sha256 FROM snapshots
             WHERE tool_call_id = ?1 AND subject = ?2 AND stage = ?3",
            params![call_id, subject, stage],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not find {subject}/{stage} snapshot: {error}"))?;
    let sha = insert_prepared_payload(connection, payload)?;
    let updated = connection
        .execute(
            "UPDATE snapshots
             SET media_type = ?1, captured_at_ms = ?2, payload_sha256 = ?3
             WHERE tool_call_id = ?4 AND subject = ?5 AND stage = ?6",
            params![media_type, captured_at_ms, sha, call_id, subject, stage],
        )
        .map_err(|error| format!("could not replace {subject}/{stage} snapshot: {error}"))?;
    if updated != 1 {
        return Err(format!("snapshot {subject}/{stage} does not exist"));
    }
    Ok(replaced_sha)
}

#[expect(
    clippy::too_many_lines,
    reason = "the exhaustive operation mapping keeps one transaction policy"
)]
fn apply_prepared_operation(
    connection: &Connection,
    operation: &mut PreparedOperationKind,
) -> Result<Option<String>, String> {
    match operation {
        PreparedOperationKind::BeginCall {
            session,
            call,
            before,
            after,
            captured_at_ms,
        } => {
            let session_id = ensure_session(connection, session)?;
            let call_id = ensure_call(connection, session_id, call)?;
            let archive_ref = call_archive_ref(connection, call_id)?;
            insert_prepared_snapshot(
                connection,
                call_id,
                "input",
                "before",
                "application/json",
                None,
                *captured_at_ms,
                before,
            )?;
            insert_prepared_snapshot(
                connection,
                call_id,
                "input",
                "after",
                "application/json",
                None,
                *captured_at_ms,
                after,
            )?;
            Ok(Some(archive_ref))
        }
        PreparedOperationKind::ResultBefore {
            session,
            source_call_id,
            result,
            source_output,
            captured_at_ms,
        } => {
            let call_id = find_call(connection, session, source_call_id)?;
            insert_prepared_snapshot(
                connection,
                call_id,
                "result",
                "before",
                "application/json",
                None,
                *captured_at_ms,
                result,
            )?;
            if let Some(source) = source_output {
                insert_prepared_snapshot(
                    connection,
                    call_id,
                    "source_output",
                    "before",
                    source.media_type,
                    None,
                    *captured_at_ms,
                    &mut source.payload,
                )?;
            }
            Ok(None)
        }
        PreparedOperationKind::ResultText {
            session,
            source_call_id,
            text,
            completeness,
            captured_at_ms,
        } => {
            let call_id = find_call(connection, session, source_call_id)?;
            insert_prepared_snapshot(
                connection,
                call_id,
                "result_text",
                "before",
                "text/plain; charset=utf-8",
                Some(completeness.as_str()),
                *captured_at_ms,
                text,
            )?;
            call_archive_ref(connection, call_id).map(Some)
        }
        PreparedOperationKind::StageResult {
            session,
            source_call_id,
            result,
            is_error,
            captured_at_ms,
        } => {
            let call_id = find_call(connection, session, source_call_id)?;
            require_call_status(connection, call_id, source_call_id, "started")?;
            validate_completion(connection, call_id, true)?;
            insert_prepared_snapshot(
                connection,
                call_id,
                "result",
                "after",
                "application/json",
                None,
                *captured_at_ms,
                result,
            )?;
            connection
                .execute(
                    "UPDATE tool_calls SET is_error = ?1, executed = 1 WHERE id = ?2",
                    params![i64::from(*is_error), call_id],
                )
                .map_err(|error| format!("could not stage tool result: {error}"))?;
            Ok(None)
        }
        PreparedOperationKind::FinishCall {
            session,
            source_call_id,
            result,
            is_error,
            require_pre_result,
            finished_at_ms,
        } => {
            let call_id = find_call(connection, session, source_call_id)?;
            validate_completion(connection, call_id, *require_pre_result)?;
            insert_prepared_snapshot(
                connection,
                call_id,
                "result",
                "after",
                "application/json",
                None,
                *finished_at_ms,
                result,
            )?;
            connection
                .execute(
                    "UPDATE tool_calls
                     SET finished_at_ms = ?1, status = 'finished', is_error = ?2, executed = ?3
                     WHERE id = ?4",
                    params![
                        *finished_at_ms,
                        i64::from(*is_error),
                        i64::from(*require_pre_result),
                        call_id
                    ],
                )
                .map_err(|error| format!("could not finish tool call: {error}"))?;
            Ok(None)
        }
        PreparedOperationKind::UpdateFinalResult {
            session,
            source_call_id,
            result,
            is_error,
            finished_at_ms,
        } => {
            let call_id = find_call(connection, session, source_call_id)?;
            let status: String = connection
                .query_row(
                    "SELECT status FROM tool_calls WHERE id = ?1",
                    [call_id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("could not read final tool call status: {error}"))?;
            if status == "finished" {
                let stored: (Vec<u8>, bool, i64, bool) = connection
                    .query_row(
                        "SELECT s.payload_sha256, c.is_error, c.finished_at_ms, c.executed
                         FROM tool_calls c
                         JOIN snapshots s ON s.tool_call_id = c.id
                         WHERE c.id = ?1 AND s.subject = 'result' AND s.stage = 'after'",
                        [call_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .map_err(|error| format!("could not check finalized tool result: {error}"))?;
                if stored == (result.sha.clone(), *is_error, *finished_at_ms, true) {
                    return Ok(None);
                }
                return Err(format!(
                    "tool call {source_call_id} was already finalized with different content or metadata"
                ));
            }
            if status != "started" {
                return Err(format!(
                    "tool call {source_call_id} has status {status}, expected started"
                ));
            }
            validate_completion(connection, call_id, true)?;
            let replaced_sha = replace_prepared_snapshot(
                connection,
                call_id,
                "result",
                "after",
                "application/json",
                *finished_at_ms,
                result,
            )?;
            connection
                .execute(
                    "UPDATE tool_calls
                     SET finished_at_ms = ?1, status = 'finished', is_error = ?2, executed = 1
                     WHERE id = ?3",
                    params![*finished_at_ms, i64::from(*is_error), call_id],
                )
                .map_err(|error| format!("could not finalize staged tool call: {error}"))?;
            connection
                .execute(
                    "DELETE FROM payloads
                     WHERE sha256 = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM snapshots WHERE snapshots.payload_sha256 = payloads.sha256
                       )",
                    [&replaced_sha],
                )
                .map_err(|error| format!("could not remove replaced final payload: {error}"))?;
            Ok(None)
        }
        PreparedOperationKind::CaptureStreams {
            session,
            source_call_id,
            captured_at_ms,
            stdout_before,
            stderr_before,
            stdout_after,
            stderr_after,
        } => {
            let call_id = find_call(connection, session, source_call_id)?;
            for (subject, stage, snapshot) in [
                ("stdout", "before", stdout_before),
                ("stderr", "before", stderr_before),
                ("stdout", "after", stdout_after),
                ("stderr", "after", stderr_after),
            ] {
                insert_prepared_snapshot(
                    connection,
                    call_id,
                    subject,
                    stage,
                    snapshot.media_type,
                    None,
                    *captured_at_ms,
                    &mut snapshot.payload,
                )?;
            }
            Ok(None)
        }
        PreparedOperationKind::PruneBefore { timestamp_ms } => {
            let deleted = connection
                .execute(
                    "DELETE FROM tool_calls WHERE status = 'finished' AND finished_at_ms < ?1",
                    [*timestamp_ms],
                )
                .map_err(|error| format!("could not prune tool calls: {error}"))?;
            connection
                .execute(
                    "DELETE FROM payloads WHERE NOT EXISTS (
                        SELECT 1 FROM snapshots WHERE snapshots.payload_sha256 = payloads.sha256
                     )",
                    [],
                )
                .map_err(|error| format!("could not prune unreferenced payloads: {error}"))?;
            i64::try_from(deleted)
                .map(|count| Some(count.to_string()))
                .map_err(|_| "pruned call count does not fit in i64".to_owned())
        }
    }
}

fn batch_error(context: &str, error: &rusqlite::Error) -> BatchWriteError {
    let message = format!("{context}: {error}");
    if is_lock_error(error) {
        BatchWriteError::Busy(message)
    } else {
        BatchWriteError::Permanent(message)
    }
}

fn ensure_session(transaction: &Connection, session: &SessionIdentity) -> Result<i64, String> {
    transaction
        .execute(
            "INSERT INTO sessions (agent, account, source_session_id, started_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(agent, account, source_session_id) DO UPDATE SET
                 started_at_ms = coalesce(sessions.started_at_ms, excluded.started_at_ms)",
            params![
                session.agent,
                session.account,
                session.source_session_id,
                session.started_at_ms
            ],
        )
        .map_err(|error| format!("could not store archive session: {error}"))?;
    transaction
        .query_row(
            "SELECT id FROM sessions WHERE agent = ?1 AND account = ?2 AND source_session_id = ?3",
            params![session.agent, session.account, session.source_session_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not find archive session: {error}"))
}

fn ensure_call(
    transaction: &Connection,
    session_id: i64,
    call: &CallIdentity,
) -> Result<i64, String> {
    let existing: Option<i64> = transaction
        .query_row(
            "SELECT id FROM tool_calls WHERE session_id = ?1 AND source_call_id = ?2",
            params![session_id, call.source_call_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("could not check stored tool call: {error}"))?;
    if existing.is_none() {
        let mut inserted = false;
        for _ in 0..3 {
            let archive_ref = random_archive_ref(transaction)?;
            match transaction.execute(
                "INSERT INTO tool_calls (
                    session_id, source_call_id, archive_ref, tool_name, provider, model,
                    working_directory, started_at_ms, requires_streams, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'started')",
                params![
                    session_id,
                    call.source_call_id,
                    archive_ref,
                    call.tool_name,
                    call.provider,
                    call.model,
                    call.working_directory,
                    call.started_at_ms,
                    i64::from(call.requires_streams)
                ],
            ) {
                Ok(1) => {
                    inserted = true;
                    break;
                }
                Ok(_) => return Err("could not store tool call".to_owned()),
                Err(rusqlite::Error::SqliteFailure(inner, _))
                    if inner.code == ErrorCode::ConstraintViolation => {}
                Err(error) => return Err(format!("could not store tool call: {error}")),
            }
        }
        if !inserted {
            return Err(
                "could not allocate a unique archive reference after three attempts".to_owned(),
            );
        }
    }
    let stored = transaction
        .query_row(
            "SELECT id, archive_ref, tool_name, provider, model, working_directory, started_at_ms, requires_streams
             FROM tool_calls WHERE session_id = ?1 AND source_call_id = ?2",
            params![session_id, call.source_call_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, bool>(7)?,
                ))
            },
        )
        .map_err(|error| format!("could not find stored tool call: {error}"))?;
    validate_archive_ref(&stored.1)?;
    if stored.2 != call.tool_name
        || stored.3 != call.provider
        || stored.4 != call.model
        || stored.5 != call.working_directory
        || stored.6 != call.started_at_ms
        || stored.7 != call.requires_streams
    {
        return Err(format!(
            "tool call {} was already stored with different metadata",
            call.source_call_id
        ));
    }
    Ok(stored.0)
}

fn random_archive_ref(transaction: &Connection) -> Result<String, String> {
    let archive_ref: String = transaction
        .query_row("SELECT 'yr_' || lower(hex(randomblob(16)))", [], |row| {
            row.get(0)
        })
        .map_err(|error| format!("could not generate archive reference: {error}"))?;
    validate_archive_ref(&archive_ref)?;
    Ok(archive_ref)
}

fn validate_archive_ref(archive_ref: &str) -> Result<(), String> {
    if archive_ref.len() != ARCHIVE_REF_LEN
        || !archive_ref.starts_with(ARCHIVE_REF_PREFIX)
        || !archive_ref[ARCHIVE_REF_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            "archive reference must be yr_ followed by 32 lowercase hexadecimal digits".to_owned(),
        );
    }
    Ok(())
}

fn call_archive_ref(transaction: &Connection, call_id: i64) -> Result<String, String> {
    let archive_ref: String = transaction
        .query_row(
            "SELECT archive_ref FROM tool_calls WHERE id = ?1",
            [call_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not read archive reference: {error}"))?;
    validate_archive_ref(&archive_ref)?;
    Ok(archive_ref)
}

fn parse_completeness(value: &str) -> Result<SourceCompleteness, String> {
    match value {
        "complete" => Ok(SourceCompleteness::Complete),
        "incomplete" => Ok(SourceCompleteness::Incomplete),
        "unknown" => Ok(SourceCompleteness::Unknown),
        _ => Err(format!("invalid source completeness {value}")),
    }
}

fn find_call(
    transaction: &Connection,
    session: &SessionIdentity,
    source_call_id: &str,
) -> Result<i64, String> {
    transaction
        .query_row(
            "SELECT c.id
             FROM tool_calls c
             JOIN sessions s ON s.id = c.session_id
             WHERE s.agent = ?1 AND s.account = ?2 AND s.source_session_id = ?3
               AND c.source_call_id = ?4",
            params![
                session.agent,
                session.account,
                session.source_session_id,
                source_call_id
            ],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not find tool call {source_call_id}: {error}"))
}

fn require_call_status(
    transaction: &Connection,
    call_id: i64,
    source_call_id: &str,
    expected: &str,
) -> Result<(), String> {
    let status: String = transaction
        .query_row(
            "SELECT status FROM tool_calls WHERE id = ?1",
            [call_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not read tool call status: {error}"))?;
    if status != expected {
        return Err(format!(
            "tool call {source_call_id} has status {status}, expected {expected}"
        ));
    }
    Ok(())
}

fn validate_completion(
    transaction: &Connection,
    call_id: i64,
    require_pre_result: bool,
) -> Result<(), String> {
    let requires_streams: bool = transaction
        .query_row(
            "SELECT requires_streams FROM tool_calls WHERE id = ?1",
            [call_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not read tool call requirements: {error}"))?;
    if requires_streams && require_pre_result {
        let streams: i64 = transaction
            .query_row(
                "SELECT count(*) FROM snapshots
                 WHERE tool_call_id = ?1
                   AND subject IN ('stdout', 'stderr')
                   AND stage IN ('before', 'after')",
                [call_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not check shell stream snapshots: {error}"))?;
        if streams != 4 {
            return Err(format!(
                "shell tool call is missing stream snapshots: found {streams} of 4"
            ));
        }
    }
    if require_pre_result {
        let has_result: bool = transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM snapshots
                    WHERE tool_call_id = ?1 AND subject = 'result' AND stage = 'before'
                 )",
                [call_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not check pre-YARP result snapshot: {error}"))?;
        if !has_result {
            return Err("executed tool call is missing its pre-YARP result".to_owned());
        }
    }
    Ok(())
}

fn insert_snapshot_bytes(
    transaction: &Connection,
    call_id: i64,
    subject: &str,
    stage: &str,
    media_type: &str,
    captured_at_ms: i64,
    body: &[u8],
) -> Result<(), String> {
    insert_snapshot_reader(
        transaction,
        call_id,
        subject,
        stage,
        media_type,
        captured_at_ms,
        &mut Cursor::new(body),
    )
}

fn replace_snapshot_bytes(
    transaction: &Connection,
    call_id: i64,
    subject: &str,
    stage: &str,
    media_type: &str,
    captured_at_ms: i64,
    body: &[u8],
) -> Result<Vec<u8>, String> {
    let replaced_sha: Vec<u8> = transaction
        .query_row(
            "SELECT payload_sha256 FROM snapshots
             WHERE tool_call_id = ?1 AND subject = ?2 AND stage = ?3",
            params![call_id, subject, stage],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not find {subject}/{stage} snapshot: {error}"))?;
    let sha = insert_payload(transaction, &mut Cursor::new(body))?;
    let updated = transaction
        .execute(
            "UPDATE snapshots
             SET media_type = ?1, captured_at_ms = ?2, payload_sha256 = ?3
             WHERE tool_call_id = ?4 AND subject = ?5 AND stage = ?6",
            params![media_type, captured_at_ms, sha, call_id, subject, stage],
        )
        .map_err(|error| format!("could not replace {subject}/{stage} snapshot: {error}"))?;
    if updated != 1 {
        return Err(format!("snapshot {subject}/{stage} does not exist"));
    }
    Ok(replaced_sha)
}

fn insert_snapshot_reader(
    transaction: &Connection,
    call_id: i64,
    subject: &str,
    stage: &str,
    media_type: &str,
    captured_at_ms: i64,
    reader: &mut (impl Read + Seek),
) -> Result<(), String> {
    insert_snapshot_reader_with_completeness(
        transaction,
        call_id,
        subject,
        stage,
        media_type,
        None,
        captured_at_ms,
        reader,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "snapshot identity and source metadata are one strict database record"
)]
fn insert_snapshot_reader_with_completeness(
    transaction: &Connection,
    call_id: i64,
    subject: &str,
    stage: &str,
    media_type: &str,
    source_completeness: Option<&str>,
    captured_at_ms: i64,
    reader: &mut (impl Read + Seek),
) -> Result<(), String> {
    let sha = insert_payload(transaction, reader)?;
    let inserted = transaction
        .execute(
            "INSERT INTO snapshots (
                tool_call_id, subject, stage, media_type, source_completeness,
                captured_at_ms, payload_sha256
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(tool_call_id, subject, stage) DO NOTHING",
            params![
                call_id,
                subject,
                stage,
                media_type,
                source_completeness,
                captured_at_ms,
                sha
            ],
        )
        .map_err(|error| format!("could not store {subject}/{stage} snapshot: {error}"))?;
    if inserted == 0 {
        let existing: (Vec<u8>, Option<String>) = transaction
            .query_row(
                "SELECT payload_sha256, source_completeness FROM snapshots
                 WHERE tool_call_id = ?1 AND subject = ?2 AND stage = ?3",
                params![call_id, subject, stage],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| format!("could not check existing snapshot: {error}"))?;
        if existing != (sha, source_completeness.map(str::to_owned)) {
            return Err(format!(
                "snapshot {subject}/{stage} already exists with different content or completeness"
            ));
        }
    }
    Ok(())
}

fn insert_payload(
    transaction: &Connection,
    reader: &mut (impl Read + Seek),
) -> Result<Vec<u8>, String> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind payload: {error}"))?;
    let mut hasher = Sha256::new();
    let mut raw_length = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not read payload: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        raw_length = raw_length.saturating_add(count as u64);
    }
    let sha = hasher.finalize().to_vec();
    let exists = transaction
        .query_row("SELECT 1 FROM payloads WHERE sha256 = ?1", [&sha], |_| {
            Ok(())
        })
        .optional()
        .map_err(|error| format!("could not check payload digest: {error}"))?
        .is_some();
    if exists {
        return Ok(sha);
    }

    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind payload for compression: {error}"))?;
    let mut compressed = NamedTempFile::new()
        .map_err(|error| format!("could not create compressed payload spool: {error}"))?;
    zstd::stream::copy_encode(&mut *reader, &mut compressed, ZSTD_LEVEL)
        .map_err(|error| format!("could not compress payload: {error}"))?;
    compressed
        .flush()
        .map_err(|error| format!("could not flush compressed payload: {error}"))?;
    let compressed_length = compressed
        .as_file()
        .metadata()
        .map_err(|error| format!("could not stat compressed payload: {error}"))?
        .len();
    let use_compressed =
        compressed_length.saturating_mul(100) <= raw_length.saturating_mul(COMPRESSION_PERCENT);
    let stored_length = if use_compressed {
        compressed_length
    } else {
        raw_length
    };
    let stored_length_i64 = i64::try_from(stored_length)
        .map_err(|_| format!("payload is too large for SQLite: {stored_length} bytes"))?;
    let raw_length_i64 = i64::try_from(raw_length)
        .map_err(|_| format!("payload is too large: {raw_length} bytes"))?;
    transaction
        .execute(
            "INSERT INTO payloads (sha256, compression, uncompressed_byte_length, body)
             VALUES (?1, ?2, ?3, zeroblob(?4))",
            params![
                sha,
                if use_compressed { "zstd" } else { "none" },
                raw_length_i64,
                stored_length_i64
            ],
        )
        .map_err(|error| format!("could not insert payload: {error}"))?;
    let rowid: i64 = transaction
        .query_row(
            "SELECT rowid FROM payloads WHERE sha256 = ?1",
            [&sha],
            |row| row.get(0),
        )
        .map_err(|error| format!("could not locate payload body: {error}"))?;
    let mut blob = transaction
        .blob_open(MAIN_DB, "payloads", "body", rowid, false)
        .map_err(|error| format!("could not open payload body: {error}"))?;
    if use_compressed {
        compressed
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("could not rewind compressed payload: {error}"))?;
        copy_exact(&mut compressed, &mut blob, stored_length)?;
    } else {
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("could not rewind raw payload: {error}"))?;
        copy_exact(reader, &mut blob, stored_length)?;
    }
    Ok(sha)
}

fn copy_exact(reader: &mut impl Read, blob: &mut Blob<'_>, expected: u64) -> Result<(), String> {
    let copied =
        io::copy(reader, blob).map_err(|error| format!("could not write payload body: {error}"))?;
    if copied != expected {
        return Err(format!(
            "payload body length changed while storing it: expected {expected}, copied {copied}"
        ));
    }
    Ok(())
}

fn canonical_json(value: &Value) -> Result<Vec<u8>, String> {
    serde_jcs::to_vec(value)
        .map_err(|error| format!("could not canonicalize JSON payload: {error}"))
}

fn stream_media_type(reader: &mut (impl Read + Seek)) -> Result<&'static str, String> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind stream for UTF-8 check: {error}"))?;
    let mut valid = true;
    let mut carry = Vec::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    while valid {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not inspect stream encoding: {error}"))?;
        if count == 0 {
            break;
        }
        let mut chunk = std::mem::take(&mut carry);
        chunk.extend_from_slice(&buffer[..count]);
        if let Err(error) = std::str::from_utf8(&chunk) {
            if error.error_len().is_some() {
                valid = false;
            } else {
                carry.extend_from_slice(&chunk[error.valid_up_to()..]);
                if carry.len() > 3 {
                    valid = false;
                }
            }
        }
    }
    if !carry.is_empty() {
        valid = false;
    }
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind inspected stream: {error}"))?;
    Ok(if valid {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    })
}

fn sync_directory(path: &Path) -> Result<(), String> {
    let directory = fs::File::open(path)
        .map_err(|error| format!("could not open {} for sync: {error}", path.display()))?;
    directory
        .sync_all()
        .map_err(|error| format!("could not sync {}: {error}", path.display()))
}

fn archive_file_bytes(path: &Path) -> Result<u64, String> {
    let main = fs::metadata(path)
        .map_err(|error| format!("could not stat archive {}: {error}", path.display()))?
        .len();
    let companions = ["-wal", "-shm"].into_iter().map(|suffix| {
        let mut value = path.as_os_str().to_owned();
        value.push(suffix);
        PathBuf::from(value)
    });
    Ok(companions.fold(main, |total, companion| {
        total.saturating_add(fs::metadata(companion).map_or(0, |metadata| metadata.len()))
    }))
}

fn prepare_path(path: &Path, repair_existing_directory: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("archive path {} has no parent", path.display()))?;
    let parent_existed = parent.exists();
    create_archive_directory(parent)?;
    if !parent_existed || repair_existing_directory {
        set_file_mode(parent, 0o700)?;
    } else {
        require_private_directory(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("could not create archive {}: {error}", path.display()))?;
    set_file_mode(path, 0o600)
}

fn create_archive_directory(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|error| {
        format!(
            "could not create archive directory {}: {error}",
            path.display()
        )
    })
}

#[cfg(unix)]
fn require_private_archive_path(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    require_private_directory(
        path.parent()
            .ok_or_else(|| format!("archive path {} has no parent", path.display()))?,
    )?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect archive {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "archive {} must be a regular non-symlink file",
            path.display()
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "archive {} has mode {mode:o}, expected 600",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_private_archive_path(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn require_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "archive directory {} has mode {mode:o}, expected 700; use a private directory",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_private_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not set permissions on {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn verify_permissions(path: &Path, report: &mut VerifyReport) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = path
        .parent()
        .ok_or_else(|| format!("archive path {} has no parent", path.display()))?;
    let directory_mode = fs::metadata(directory)
        .map_err(|error| format!("could not stat archive directory: {error}"))?
        .permissions()
        .mode()
        & 0o777;
    if directory_mode != 0o700 {
        report.errors.push(format!(
            "archive directory mode is {directory_mode:o}, expected 700"
        ));
    }
    let file_mode = fs::metadata(path)
        .map_err(|error| format!("could not stat archive file: {error}"))?
        .permissions()
        .mode()
        & 0o777;
    if file_mode != 0o600 {
        report
            .errors
            .push(format!("archive file mode is {file_mode:o}, expected 600"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_permissions(_path: &Path, _report: &mut VerifyReport) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use tempfile::TempDir;

    fn archive() -> (TempDir, Archive) {
        let directory = TempDir::new().expect("temp directory");
        let archive = Archive::open_path(directory.path().join("data/tool-calls.sqlite3"))
            .expect("open archive");
        (directory, archive)
    }

    fn session() -> SessionIdentity {
        SessionIdentity {
            agent: "pi".to_owned(),
            account: "test".to_owned(),
            source_session_id: "session-1".to_owned(),
            started_at_ms: Some(10),
        }
    }

    fn call() -> CallIdentity {
        call_with_id("call-1")
    }

    fn call_with_id(source_call_id: &str) -> CallIdentity {
        CallIdentity {
            source_call_id: source_call_id.to_owned(),
            tool_name: "read".to_owned(),
            provider: Some("openai".to_owned()),
            model: Some("gpt".to_owned()),
            working_directory: Some("/tmp".to_owned()),
            started_at_ms: 20,
            requires_streams: false,
        }
    }

    fn framed(value: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("json");
        let mut frame = Vec::new();
        frame.extend_from_slice(&(body.len() as u64).to_be_bytes());
        frame.extend_from_slice(&body);
        frame
    }

    fn begin_operation(request_id: u64, source_call_id: &str, value: Value) -> ArchiveOperation {
        ArchiveOperation::BeginCall {
            request_id,
            schema_version: INGEST_SCHEMA_VERSION,
            session: session(),
            call: call_with_id(source_call_id),
            input_before: value.clone(),
            input_after: value,
            captured_at_ms: 20,
        }
    }

    #[test]
    fn prepared_batch_commits_multiple_calls_once() {
        let (_directory, mut archive) = archive();
        archive.configure_for_broker().expect("broker policy");
        let busy_timeout: i64 = archive
            .connection
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .expect("busy timeout");
        let wal_autocheckpoint: i64 = archive
            .connection
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .expect("WAL checkpoint limit");
        assert_eq!(busy_timeout, 250);
        assert_eq!(wal_autocheckpoint, 1000);
        let mut first = archive
            .prepare_operation(begin_operation(
                1,
                "call-1",
                serde_json::json!({"value": 1}),
            ))
            .expect("prepare first");
        let mut second = archive
            .prepare_operation(begin_operation(
                2,
                "call-2",
                serde_json::json!({"value": 2}),
            ))
            .expect("prepare second");
        let results = archive
            .apply_prepared_batch([&mut first, &mut second])
            .expect("batch");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(archive.stats().expect("stats").calls, 2);
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[test]
    fn failed_savepoint_does_not_poison_shared_payload_in_next_request() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call_with_id("call-1"),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("existing call");
        let shared = serde_json::json!({"shared": "payload"});
        let conflicting = ArchiveOperation::BeginCall {
            request_id: 1,
            schema_version: INGEST_SCHEMA_VERSION,
            session: session(),
            call: call_with_id("call-1"),
            input_before: serde_json::json!({}),
            input_after: shared.clone(),
            captured_at_ms: 20,
        };
        let mut first = archive
            .prepare_operation(conflicting)
            .expect("prepare conflict");
        let mut second = archive
            .prepare_operation(begin_operation(2, "call-2", shared))
            .expect("prepare second");
        let results = archive
            .apply_prepared_batch([&mut first, &mut second])
            .expect("batch commit");
        assert!(results[0].is_err());
        assert!(results[1].is_ok());
        assert_eq!(archive.stats().expect("stats").calls, 2);
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[test]
    fn prepared_replay_is_idempotent_and_conflicts_on_changed_content() {
        let (_directory, mut archive) = archive();
        let mut first = archive
            .prepare_operation(begin_operation(
                1,
                "call-1",
                serde_json::json!({"value": 1}),
            ))
            .expect("prepare first");
        archive
            .apply_prepared_batch([&mut first])
            .expect("first commit");
        let mut replay = archive
            .prepare_operation(begin_operation(
                2,
                "call-1",
                serde_json::json!({"value": 1}),
            ))
            .expect("prepare replay");
        assert!(
            archive
                .apply_prepared_batch([&mut replay])
                .expect("replay commit")[0]
                .is_ok()
        );
        let mut conflict = archive
            .prepare_operation(begin_operation(
                3,
                "call-1",
                serde_json::json!({"value": 2}),
            ))
            .expect("prepare conflict");
        assert!(
            archive
                .apply_prepared_batch([&mut conflict])
                .expect("conflict batch")[0]
                .is_err()
        );
        assert_eq!(archive.stats().expect("stats").calls, 1);
    }

    #[test]
    fn prepared_batch_covers_the_complete_shell_call_lifecycle() {
        let (directory, mut archive) = archive();
        let mut shell_call = call();
        shell_call.requires_streams = true;
        let source = directory.path().join("data/source-output");
        let stdout_before = directory.path().join("data/stdout-before");
        let stderr_before = directory.path().join("data/stderr-before");
        let stdout_after = directory.path().join("data/stdout-after");
        let stderr_after = directory.path().join("data/stderr-after");
        for (path, body) in [
            (&source, b"source output".as_slice()),
            (&stdout_before, b"stdout raw".as_slice()),
            (&stderr_before, b"stderr raw".as_slice()),
            (&stdout_after, b"stdout short".as_slice()),
            (&stderr_after, b"stderr short".as_slice()),
        ] {
            fs::write(path, body).expect("stream file");
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private stream");
        }
        let operations = [
            ArchiveOperation::BeginCall {
                request_id: 1,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session(),
                call: shell_call,
                input_before: serde_json::json!({"cmd": "test"}),
                input_after: serde_json::json!({"cmd": "test"}),
                captured_at_ms: 20,
            },
            ArchiveOperation::CaptureStreams {
                request_id: 2,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session(),
                source_call_id: "call-1".to_owned(),
                captured_at_ms: 30,
                stdout_before_path: stdout_before,
                stderr_before_path: stderr_before,
                stdout_after_path: stdout_after,
                stderr_after_path: stderr_after,
            },
            ArchiveOperation::ResultBefore {
                request_id: 3,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session(),
                source_call_id: "call-1".to_owned(),
                result: serde_json::json!({"content": "raw"}),
                full_output_path: Some(source),
                captured_at_ms: 40,
            },
            ArchiveOperation::ResultText {
                request_id: 4,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session(),
                source_call_id: "call-1".to_owned(),
                text: "exact visible text".to_owned(),
                source_completeness: SourceCompleteness::Complete,
                captured_at_ms: 41,
            },
            ArchiveOperation::StageResult {
                request_id: 5,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session(),
                source_call_id: "call-1".to_owned(),
                result: serde_json::json!({"content": "staged"}),
                is_error: false,
                captured_at_ms: 50,
            },
            ArchiveOperation::UpdateFinalResult {
                request_id: 6,
                schema_version: INGEST_SCHEMA_VERSION,
                session: session(),
                source_call_id: "call-1".to_owned(),
                result: serde_json::json!({"content": "final"}),
                is_error: false,
                finished_at_ms: 60,
            },
        ];
        let mut prepared = operations
            .into_iter()
            .map(|operation| archive.prepare_operation(operation).expect("prepare"))
            .collect::<Vec<_>>();
        let results = archive
            .apply_prepared_batch(prepared.iter_mut())
            .expect("complete batch");
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(archive.stats().expect("stats").incomplete_calls, 0);
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[test]
    fn prepared_prune_uses_the_broker_transaction_path() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .finish_call(
                &session(),
                "call-1",
                &serde_json::json!({"error": "blocked"}),
                true,
                false,
                30,
            )
            .expect("finish");
        let mut prune = archive
            .prepare_operation(ArchiveOperation::PruneBefore {
                request_id: 7,
                schema_version: INGEST_SCHEMA_VERSION,
                timestamp_ms: 31,
            })
            .expect("prepare prune");
        let result = archive
            .apply_prepared_batch([&mut prune])
            .expect("prune batch");
        assert_eq!(
            result[0].as_ref().expect("prune result").as_deref(),
            Some("1")
        );
        assert_eq!(archive.stats().expect("stats").calls, 0);
    }

    #[test]
    fn result_before_replay_rejects_changed_full_output_path() {
        let (directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        let source = directory.path().join("data/full-output");
        fs::write(&source, b"first").expect("source output");
        #[cfg(unix)]
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).expect("private source");
        let operation = |request_id| ArchiveOperation::ResultBefore {
            request_id,
            schema_version: INGEST_SCHEMA_VERSION,
            session: session(),
            source_call_id: "call-1".to_owned(),
            result: serde_json::json!({"content": "visible"}),
            full_output_path: Some(source.clone()),
            captured_at_ms: 30,
        };
        let mut first = archive
            .prepare_operation(operation(1))
            .expect("prepare first");
        assert!(
            archive
                .apply_prepared_batch([&mut first])
                .expect("first batch")[0]
                .is_ok()
        );
        fs::write(&source, b"second").expect("changed source output");
        let mut replay = archive
            .prepare_operation(operation(2))
            .expect("prepare replay");
        assert!(
            archive
                .apply_prepared_batch([&mut replay])
                .expect("replay batch")[0]
                .is_err()
        );
    }

    #[test]
    fn stores_and_verifies_a_complete_call() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({"path": "file"}),
                &serde_json::json!({"path": "file"}),
                20,
            )
            .expect("begin");
        archive
            .result_before(
                &session(),
                "call-1",
                &serde_json::json!({"content": "raw"}),
                None,
                30,
            )
            .expect("before");
        archive
            .finish_call(
                &session(),
                "call-1",
                &serde_json::json!({"content": "pruned"}),
                false,
                true,
                40,
            )
            .expect("finish");

        let stats = archive.stats().expect("stats");
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.calls, 1);
        assert_eq!(stats.incomplete_calls, 0);
        assert!(stats.logical_payload_bytes > 0);
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[test]
    fn finalizes_the_staged_result_after_other_hooks() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .result_before(
                &session(),
                "call-1",
                &serde_json::json!({"content": "before"}),
                None,
                30,
            )
            .expect("before");
        archive
            .stage_result(
                &session(),
                "call-1",
                &serde_json::json!({"content": "provisional"}),
                false,
                40,
            )
            .expect("stage");
        let staged_report = archive.verify().expect("verify staged call");
        assert_eq!(staged_report.incomplete_calls, 1);
        assert!(staged_report.errors.is_empty());
        archive
            .update_final_result(
                &session(),
                "call-1",
                &serde_json::json!({"content": "final"}),
                true,
                50,
            )
            .expect("update final");
        archive
            .update_final_result(
                &session(),
                "call-1",
                &serde_json::json!({"content": "final"}),
                true,
                50,
            )
            .expect("repeat committed final update");

        let call_id: i64 = archive
            .connection
            .query_row("SELECT id FROM tool_calls", [], |row| row.get(0))
            .expect("call id");
        let mut restored = archive
            .verified_snapshot(call_id, "result", "after")
            .expect("final result");
        let mut body = Vec::new();
        restored.read_to_end(&mut body).expect("read final result");
        assert_eq!(body, br#"{"content":"final"}"#);
        let metadata: (i64, i64) = archive
            .connection
            .query_row(
                "SELECT is_error, finished_at_ms FROM tool_calls WHERE id = ?1",
                [call_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("final metadata");
        assert_eq!(metadata, (1, 50));
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[test]
    fn failed_finalization_keeps_the_staged_call_incomplete() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .result_before(
                &session(),
                "call-1",
                &serde_json::json!({"content": "before"}),
                None,
                30,
            )
            .expect("before");
        archive
            .stage_result(
                &session(),
                "call-1",
                &serde_json::json!({"content": "provisional"}),
                false,
                40,
            )
            .expect("stage");
        archive
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER reject_finish
                 BEFORE UPDATE OF status ON tool_calls
                 WHEN NEW.status = 'finished'
                 BEGIN SELECT RAISE(ABORT, 'simulated final write failure'); END;",
            )
            .expect("failure trigger");

        let error = archive
            .update_final_result(
                &session(),
                "call-1",
                &serde_json::json!({"content": "final"}),
                false,
                50,
            )
            .expect_err("finalization failure");
        assert!(error.contains("simulated final write failure"));
        assert_eq!(archive.stats().expect("stats").incomplete_calls, 1);
        let stored: (String, Option<i64>) = archive
            .connection
            .query_row("SELECT status, finished_at_ms FROM tool_calls", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("stored state");
        assert_eq!(stored, ("started".to_owned(), None));
    }

    #[test]
    fn deduplicates_unchanged_snapshots() {
        let (_directory, mut archive) = archive();
        let value = serde_json::json!({"same": true});
        archive
            .begin_call(&session(), &call(), &value, &value, 20)
            .expect("begin");
        let payloads: i64 = archive
            .connection
            .query_row("SELECT count(*) FROM payloads", [], |row| row.get(0))
            .expect("count");
        assert_eq!(payloads, 1);
    }

    #[test]
    fn duplicate_operations_are_idempotent_but_conflicts_fail() {
        let (_directory, mut archive) = archive();
        let value = serde_json::json!({"same": true});
        archive
            .begin_call(&session(), &call(), &value, &value, 20)
            .expect("begin");
        archive
            .begin_call(&session(), &call(), &value, &value, 20)
            .expect("retry");
        let error = archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({"different": true}),
                &value,
                20,
            )
            .expect_err("conflict");
        assert!(error.contains("different content"));
    }

    #[test]
    fn marks_interrupted_calls_without_failing_verification() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        let report = archive.verify().expect("verify");
        assert_eq!(report.incomplete_calls, 1);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn stores_binary_and_text_streams() {
        let (_directory, mut archive) = archive();
        let archive_ref = archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        let key = ArchiveKey {
            session: session(),
            source_call_id: "call-1".to_owned(),
        };
        let mut stdout = Cursor::new(b"hello\n".to_vec());
        let mut stderr = Cursor::new(vec![0xff, 0x00]);
        archive
            .capture_streams(&key, 30, &mut stdout, &mut stderr, b"hello\n", b"short")
            .expect("streams");
        let media: Vec<String> = archive
            .connection
            .prepare("SELECT media_type FROM snapshots WHERE subject IN ('stdout', 'stderr') ORDER BY subject, stage")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert!(media.contains(&"application/octet-stream".to_owned()));
        assert!(media.contains(&"text/plain; charset=utf-8".to_owned()));

        archive
            .result_text(
                &session(),
                "call-1",
                "capped wrapped summary\n",
                SourceCompleteness::Unknown,
                40,
            )
            .expect("wrapped summary fallback");
        let sources = archive.searchable_sources(&archive_ref).expect("sources");
        assert_eq!(
            sources.iter().map(|source| source.name).collect::<Vec<_>>(),
            vec![SourceName::Stdout, SourceName::Stderr]
        );
    }

    #[test]
    fn stores_exact_source_full_output_with_the_result() {
        let (directory, mut archive) = archive();
        let archive_ref = archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        let full_output_path = directory.path().join("pi-full-output.log");
        let expected = b"complete output\n\xff";
        fs::write(&full_output_path, expected).expect("full output");
        archive
            .result_before(
                &session(),
                "call-1",
                &serde_json::json!({"content": "truncated"}),
                Some(&full_output_path),
                30,
            )
            .expect("result before");

        let call_id: i64 = archive
            .connection
            .query_row("SELECT id FROM tool_calls", [], |row| row.get(0))
            .expect("call id");
        let mut restored = archive
            .verified_snapshot(call_id, "source_output", "before")
            .expect("source output");
        let mut actual = Vec::new();
        restored
            .read_to_end(&mut actual)
            .expect("read source output");
        assert_eq!(actual, expected);

        archive
            .result_text(
                &session(),
                "call-1",
                "exact host text\n",
                SourceCompleteness::Incomplete,
                40,
            )
            .expect("result text");
        let mut sources = archive.searchable_sources(&archive_ref).expect("sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, SourceName::ResultText);
        let mut host_text = String::new();
        sources[0]
            .body
            .read_to_string(&mut host_text)
            .expect("result text body");
        assert_eq!(host_text, "exact host text\n");
    }

    #[test]
    fn prunes_only_finished_calls() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .finish_call(
                &session(),
                "call-1",
                &serde_json::json!({}),
                false,
                false,
                40,
            )
            .expect("finish");
        assert_eq!(archive.prune_before(50).expect("prune"), 1);
        let stats = archive.stats().expect("stats");
        assert_eq!(stats.calls, 0);
        assert_eq!(stats.logical_payload_bytes, 0);
    }

    #[test]
    fn migrates_the_original_version_one_schema_in_place_with_a_private_backup() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("yarp/tool-calls.sqlite3");
        fs::create_dir(path.parent().expect("parent")).expect("archive directory");
        set_file_mode(path.parent().expect("parent"), 0o700).expect("private directory");
        let connection = Connection::open(&path).expect("old database");
        connection
            .execute_batch(include_str!("../tests/fixtures/archive-v1.sql"))
            .expect("old schema");
        drop(connection);
        set_file_mode(&path, 0o600).expect("private database");

        let archive = Archive::open_path(path.clone()).expect("migrate archive");
        let archive_ref: String = archive
            .connection
            .query_row("SELECT archive_ref FROM tool_calls", [], |row| row.get(0))
            .expect("archive ref");
        validate_archive_ref(&archive_ref).expect("valid archive ref");
        assert!(
            table_has_column(&archive.connection, "snapshots", "source_completeness")
                .expect("schema column")
        );
        drop(archive);

        let backup = path.with_file_name("tool-calls.sqlite3.pre-indexed-output-v1.backup");
        assert!(backup.is_file());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&backup)
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let reopened = Archive::open_path(path).expect("reopen migrated archive");
        let reopened_ref: String = reopened
            .connection
            .query_row("SELECT archive_ref FROM tool_calls", [], |row| row.get(0))
            .expect("stable archive ref");
        assert_eq!(archive_ref, reopened_ref);
    }

    #[test]
    fn result_text_is_on_demand_and_canonical_source_selection_is_stable() {
        let (_directory, mut archive) = archive();
        let archive_ref = archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        let repeated_ref = archive
            .result_text(
                &session(),
                "call-1",
                "first\nerror\nlast\n",
                SourceCompleteness::Incomplete,
                30,
            )
            .expect("result text");
        assert_eq!(archive_ref, repeated_ref);
        let mut sources = archive.searchable_sources(&archive_ref).expect("sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, SourceName::ResultText);
        assert_eq!(sources[0].completeness, SourceCompleteness::Incomplete);
        let mut body = String::new();
        sources[0]
            .body
            .read_to_string(&mut body)
            .expect("source body");
        assert_eq!(body, "first\nerror\nlast\n");
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[test]
    fn reopens_the_current_schema_without_migration() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("yarp/tool-calls.sqlite3");
        drop(Archive::open_path(path.clone()).expect("create"));
        let reopened = Archive::open_path(path).expect("reopen");
        assert_eq!(reopened.stats().expect("stats").calls, 0);
    }

    #[test]
    fn rejects_newer_schema_versions() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("yarp/tool-calls.sqlite3");
        fs::create_dir(path.parent().expect("parent")).expect("archive directory");
        set_file_mode(path.parent().expect("parent"), 0o700).expect("private directory");
        let connection = Connection::open(&path).expect("sqlite");
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("version");
        drop(connection);
        let error = Archive::open_path(path).err().expect("newer version error");
        assert!(error.contains("newer than supported"));
    }

    #[test]
    fn compresses_large_repetitive_payloads() {
        let (_directory, mut archive) = archive();
        let value = serde_json::json!({"output": "x".repeat(32 * 1024)});
        archive
            .begin_call(&session(), &call(), &value, &value, 20)
            .expect("begin");
        let compression: String = archive
            .connection
            .query_row("SELECT compression FROM payloads", [], |row| row.get(0))
            .expect("compression");
        assert_eq!(compression, "zstd");
    }

    #[test]
    fn keeps_small_payloads_uncompressed() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({"value": "short"}),
                &serde_json::json!({"value": "short"}),
                20,
            )
            .expect("begin");
        let compression: String = archive
            .connection
            .query_row("SELECT compression FROM payloads", [], |row| row.get(0))
            .expect("compression");
        assert_eq!(compression, "none");
    }

    #[test]
    fn synthetic_archive_stays_below_storage_budget() {
        let (_directory, mut archive) = archive();
        let value = serde_json::json!({"output": "archive fixture ".repeat(64 * 1024)});
        archive
            .begin_call(&session(), &call(), &value, &value, 20)
            .expect("begin");
        archive
            .result_before(&session(), "call-1", &value, None, 30)
            .expect("result before");
        archive
            .finish_call(&session(), "call-1", &value, false, true, 40)
            .expect("finish");
        let referenced_bytes: i64 = archive
            .connection
            .query_row(
                "SELECT sum(p.uncompressed_byte_length)
                 FROM snapshots s JOIN payloads p ON p.sha256 = s.payload_sha256",
                [],
                |row| row.get(0),
            )
            .expect("logical snapshot bytes");
        let stats = archive.stats().expect("stats");
        assert!(stats.database_bytes < u64::try_from(referenced_bytes * 3 / 10).expect("budget"));
    }

    #[test]
    fn detects_corrupt_compressed_payloads() {
        let (_directory, mut archive) = archive();
        let value = serde_json::json!({"output": "x".repeat(32 * 1024)});
        archive
            .begin_call(&session(), &call(), &value, &value, 20)
            .expect("begin");
        archive
            .connection
            .execute("UPDATE payloads SET body = x'00'", [])
            .expect("corrupt");
        let error = archive.verify().expect_err("corruption");
        assert!(error.contains("payload"));
    }

    #[test]
    fn restore_rejects_corrupt_payloads_before_writing_output() {
        let (_directory, mut archive) = archive();
        let mut shell_call = call();
        shell_call.requires_streams = true;
        archive
            .begin_call(
                &session(),
                &shell_call,
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .capture_streams(
                &ArchiveKey {
                    session: session(),
                    source_call_id: "call-1".to_owned(),
                },
                30,
                &mut Cursor::new(b"raw stdout"),
                &mut Cursor::new(b"raw stderr"),
                b"after stdout",
                b"after stderr",
            )
            .expect("streams");
        archive
            .connection
            .execute(
                "UPDATE payloads SET body = ?1
                 WHERE sha256 = (
                     SELECT payload_sha256 FROM snapshots
                     WHERE subject = 'stdout' AND stage = 'before'
                 )",
                [b"BAD stdout".as_slice()],
            )
            .expect("corrupt stdout");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = archive
            .restore_streams(
                &ArchiveKey {
                    session: session(),
                    source_call_id: "call-1".to_owned(),
                },
                &mut stdout,
                &mut stderr,
            )
            .expect_err("corrupt restore");
        assert!(error.contains("integrity verification"));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn verification_rejects_values_inserted_without_check_constraints() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .connection
            .pragma_update(None, "ignore_check_constraints", "ON")
            .expect("ignore constraints");
        archive
            .connection
            .execute(
                "INSERT INTO snapshots (
                    tool_call_id, subject, stage, media_type, captured_at_ms, payload_sha256
                 )
                 SELECT tool_call_id, 'other', stage, media_type, captured_at_ms, payload_sha256
                 FROM snapshots WHERE subject = 'input' AND stage = 'before'",
                [],
            )
            .expect("invalid snapshot");

        let report = archive.verify().expect("verify");
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("invalid subject"))
        );
    }

    #[test]
    fn concurrent_first_open_initializes_the_schema_once() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("yarp/tool-calls.sqlite3");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Archive::open_path(path)
                        .and_then(|archive| archive.stats())
                        .expect("open archive")
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().expect("join").calls, 0);
        }
        assert!(
            Archive::open_path(path)
                .expect("reopen")
                .verify()
                .expect("verify")
                .errors
                .is_empty()
        );
    }

    #[test]
    fn concurrent_writers_keep_every_call() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("yarp/tool-calls.sqlite3");
        drop(Archive::open_path(path.clone()).expect("create"));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut archive = Archive::open_path(path).expect("open writer");
                    let call = call_with_id(&format!("call-{index}"));
                    archive
                        .begin_call(
                            &session(),
                            &call,
                            &serde_json::json!({"index": index}),
                            &serde_json::json!({"index": index}),
                            20,
                        )
                        .expect("write call");
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("join");
        }
        let archive = Archive::open_path(path).expect("reopen");
        assert_eq!(archive.stats().expect("stats").calls, 8);
        assert_eq!(archive.verify().expect("verify").incomplete_calls, 8);
    }

    #[test]
    fn refuses_to_finish_executed_calls_without_required_snapshots() {
        let (_directory, mut archive) = archive();
        archive
            .begin_call(
                &session(),
                &call(),
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        let error = archive
            .finish_call(
                &session(),
                "call-1",
                &serde_json::json!({}),
                false,
                true,
                40,
            )
            .expect_err("missing pre-result");
        assert!(error.contains("missing its pre-YARP result"));
        assert_eq!(archive.stats().expect("stats").incomplete_calls, 1);
    }

    #[test]
    fn finishes_preflight_rejected_shell_calls_without_streams() {
        let (_directory, mut archive) = archive();
        let mut shell_call = call();
        shell_call.requires_streams = true;
        archive
            .begin_call(
                &session(),
                &shell_call,
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .finish_call(
                &session(),
                "call-1",
                &serde_json::json!({"error": "blocked"}),
                true,
                false,
                40,
            )
            .expect("finish preflight rejection");
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[test]
    fn requires_a_pre_result_for_executed_shell_calls() {
        let (_directory, mut archive) = archive();
        let mut shell_call = call();
        shell_call.requires_streams = true;
        archive
            .begin_call(
                &session(),
                &shell_call,
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .capture_streams(
                &ArchiveKey {
                    session: session(),
                    source_call_id: "call-1".to_owned(),
                },
                30,
                &mut Cursor::new(b"stdout"),
                &mut Cursor::new(b"stderr"),
                b"stdout",
                b"stderr",
            )
            .expect("streams");
        let error = archive
            .finish_call(
                &session(),
                "call-1",
                &serde_json::json!({}),
                false,
                true,
                40,
            )
            .expect_err("missing pre-result");
        assert!(error.contains("missing its pre-YARP result"));
    }

    #[test]
    fn requires_every_shell_stream_before_finishing() {
        let (_directory, mut archive) = archive();
        let mut shell_call = call();
        shell_call.requires_streams = true;
        archive
            .begin_call(
                &session(),
                &shell_call,
                &serde_json::json!({}),
                &serde_json::json!({}),
                20,
            )
            .expect("begin");
        archive
            .result_before(&session(), "call-1", &serde_json::json!({}), None, 30)
            .expect("before result");
        let error = archive
            .finish_call(
                &session(),
                "call-1",
                &serde_json::json!({}),
                false,
                true,
                40,
            )
            .expect_err("missing streams");
        assert!(error.contains("found 0 of 4"));
        assert_eq!(archive.stats().expect("stats").incomplete_calls, 1);
    }

    #[test]
    fn rejects_truncated_and_oversized_ingest_frames() {
        let (_directory, first_archive) = archive();
        let truncated = run_ingest_with_archive(first_archive, Cursor::new(vec![0, 1]), Vec::new())
            .expect_err("truncated length");
        assert!(truncated.contains("truncated ingest frame length"));

        let (_directory, second_archive) = archive();
        let oversized = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        let error = run_ingest_with_archive(second_archive, Cursor::new(oversized), Vec::new())
            .expect_err("oversized frame");
        assert!(error.contains("invalid ingest frame length"));
    }

    #[test]
    fn rejects_unsupported_ingest_schema_versions() {
        let (_directory, archive) = archive();
        let operation = serde_json::json!({
            "operation": "begin_call",
            "requestId": 1,
            "schemaVersion": 2,
            "session": session(),
            "call": call(),
            "inputBefore": {},
            "inputAfter": {},
            "capturedAtMs": 20
        });
        let mut output = Vec::new();
        run_ingest_with_archive(archive, Cursor::new(framed(&operation)), &mut output)
            .expect("protocol response");
        let ack: Value = serde_json::from_slice(&output).expect("ack");
        assert_eq!(ack["ok"], false);
        assert!(
            ack["error"]
                .as_str()
                .is_some_and(|error| error.contains("unsupported ingest"))
        );
    }

    #[test]
    fn ingest_protocol_handles_every_operation() {
        let (directory, archive) = archive();
        let full_output_path = directory.path().join("full-output.log");
        fs::write(&full_output_path, b"complete output").expect("full output");
        let operations = [
            serde_json::json!({
                "operation": "begin_call",
                "requestId": 1,
                "schemaVersion": 1,
                "session": session(),
                "call": call(),
                "inputBefore": {"path": "a"},
                "inputAfter": {"path": "a"},
                "capturedAtMs": 20
            }),
            serde_json::json!({
                "operation": "result_before",
                "requestId": 2,
                "schemaVersion": 1,
                "session": session(),
                "sourceCallId": "call-1",
                "result": {"content": "before"},
                "fullOutputPath": full_output_path,
                "capturedAtMs": 30
            }),
            serde_json::json!({
                "operation": "stage_result",
                "requestId": 3,
                "schemaVersion": 1,
                "session": session(),
                "sourceCallId": "call-1",
                "result": {"content": "after"},
                "isError": false,
                "capturedAtMs": 40
            }),
            serde_json::json!({
                "operation": "update_final_result",
                "requestId": 4,
                "schemaVersion": 1,
                "session": session(),
                "sourceCallId": "call-1",
                "result": {"content": "final"},
                "isError": false,
                "finishedAtMs": 50
            }),
        ];
        let input: Vec<u8> = operations.iter().flat_map(framed).collect();
        let mut output = Vec::new();
        run_ingest_with_archive(archive, Cursor::new(input), &mut output).expect("ingest");
        let acknowledgements = String::from_utf8(output).expect("UTF-8");
        assert_eq!(acknowledgements.lines().count(), 4);
        assert!(acknowledgements.contains("\"requestId\":4"));
        let reopened = Archive::open_path(directory.path().join("data/tool-calls.sqlite3"))
            .expect("reopen archive");
        let source_outputs: i64 = reopened
            .connection
            .query_row(
                "SELECT count(*) FROM snapshots WHERE subject = 'source_output'",
                [],
                |row| row.get(0),
            )
            .expect("source outputs");
        assert_eq!(source_outputs, 1);
    }

    #[cfg(unix)]
    #[test]
    fn repairs_archive_permissions_on_open() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = TempDir::new().expect("temp directory");
        let data = directory.path().join("yarp");
        let path = data.join("tool-calls.sqlite3");
        drop(Archive::open_path(path.clone()).expect("create"));
        fs::set_permissions(&data, fs::Permissions::from_mode(0o777)).expect("directory mode");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("file mode");
        let archive = Archive::open_path_with_policy(path, true).expect("repair");
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_modify_an_unrelated_public_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = TempDir::new().expect("temp directory");
        let public = directory.path().join("yarp");
        fs::create_dir(&public).expect("public directory");
        fs::set_permissions(&public, fs::Permissions::from_mode(0o777)).expect("public mode");
        let error = Archive::open_path(public.join("archive.sqlite3"))
            .err()
            .expect("insecure directory error");
        assert!(error.contains("use a private directory"));
        let mode = fs::metadata(public).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o777);
    }

    #[test]
    fn ingest_protocol_acknowledges_committed_frames() {
        let directory = TempDir::new().expect("temp directory");
        let archive = Archive::open_path(directory.path().join("yarp/tool-calls.sqlite3"))
            .expect("open archive");
        let operation = serde_json::json!({
            "operation": "begin_call",
            "requestId": 7,
            "schemaVersion": 1,
            "session": session(),
            "call": call(),
            "inputBefore": {"path": "a"},
            "inputAfter": {"path": "a"},
            "capturedAtMs": 20
        });
        let body = serde_json::to_vec(&operation).expect("json");
        let mut input = Vec::new();
        input.extend_from_slice(&(body.len() as u64).to_be_bytes());
        input.extend_from_slice(&body);
        let mut output = Vec::new();
        run_ingest_with_archive(archive, Cursor::new(input), &mut output).expect("ingest");
        let ack: Value = serde_json::from_slice(&output).expect("ack");
        assert_eq!(ack["requestId"], 7);
        assert_eq!(ack["ok"], true);
    }
}
