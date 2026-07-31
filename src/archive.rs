use rusqlite::blob::Blob;
use rusqlite::{
    Connection, MAIN_DB, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::{NamedTempFile, tempfile};

const SCHEMA_VERSION: i64 = 1;
const ZSTD_LEVEL: i32 = 3;
const COMPRESSION_PERCENT: u64 = 95;
const MAX_FRAME_BYTES: u64 = 256 * 1024 * 1024;
const INGEST_SCHEMA_VERSION: u32 = 1;

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
    subject        TEXT NOT NULL CHECK (subject IN ('input', 'result', 'source_output', 'stdout', 'stderr')),
    stage          TEXT NOT NULL CHECK (stage IN ('before', 'after')),
    media_type     TEXT NOT NULL,
    captured_at_ms INTEGER NOT NULL,
    payload_sha256 BLOB NOT NULL REFERENCES payloads(sha256),
    PRIMARY KEY (tool_call_id, subject, stage)
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

#[derive(Debug, Deserialize)]
#[serde(
    tag = "operation",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum IngestOperation {
    BeginCall {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        call: CallIdentity,
        input_before: Value,
        input_after: Value,
        captured_at_ms: i64,
    },
    ResultBefore {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        result: Value,
        full_output_path: Option<PathBuf>,
        captured_at_ms: i64,
    },
    FinishCall {
        request_id: u64,
        schema_version: u32,
        session: SessionIdentity,
        source_call_id: String,
        result: Value,
        is_error: bool,
        require_pre_result: bool,
        finished_at_ms: i64,
    },
}

impl IngestOperation {
    const fn request_id(&self) -> u64 {
        match self {
            Self::BeginCall { request_id, .. }
            | Self::ResultBefore { request_id, .. }
            | Self::FinishCall { request_id, .. } => *request_id,
        }
    }

    const fn schema_version(&self) -> u32 {
        match self {
            Self::BeginCall { schema_version, .. }
            | Self::ResultBefore { schema_version, .. }
            | Self::FinishCall { schema_version, .. } => *schema_version,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IngestAck<'a> {
    request_id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
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

impl Archive {
    /// Open the configured archive and initialize schema version 1 when needed.
    ///
    /// # Errors
    ///
    /// Returns an error when the path, permissions, `SQLite` database, or schema is invalid.
    pub fn open() -> Result<Self, String> {
        Self::open_path(archive_path()?)
    }

    /// Open the configured archive without performing any writes.
    ///
    /// # Errors
    ///
    /// Returns an error when the archive is missing, unreadable, or uses another schema version.
    pub fn open_read_only() -> Result<Self, String> {
        let path = archive_path()?;
        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| {
                format!(
                    "could not open archive {} read-only: {error}",
                    path.display()
                )
            })?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("could not set archive busy timeout: {error}"))?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| format!("could not read archive schema version: {error}"))?;
        if version != SCHEMA_VERSION {
            return Err(format!(
                "archive schema version {version}, expected {SCHEMA_VERSION}"
            ));
        }
        Ok(Self { connection, path })
    }

    /// Open an archive at an explicit path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path, permissions, `SQLite` database, or schema is invalid.
    pub fn open_path(path: PathBuf) -> Result<Self, String> {
        prepare_path(&path)?;
        let mut connection = Connection::open(&path)
            .map_err(|error| format!("could not open archive {}: {error}", path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("could not set archive busy timeout: {error}"))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| format!("could not enable archive foreign keys: {error}"))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| format!("could not enable archive WAL mode: {error}"))?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| format!("could not set archive sync mode: {error}"))?;

        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| format!("could not read archive schema version: {error}"))?;
        match version {
            0 => initialize_schema(&mut connection)?,
            SCHEMA_VERSION => {}
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
    ) -> Result<(), String> {
        let before = canonical_json(input_before)?;
        let after = canonical_json(input_after)?;
        let transaction = self.transaction()?;
        let session_id = ensure_session(&transaction, session)?;
        let call_id = ensure_call(&transaction, session_id, call)?;
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
            .map_err(|error| format!("could not commit tool call: {error}"))
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
                 WHERE (status = 'started' AND (finished_at_ms IS NOT NULL OR is_error IS NOT NULL OR executed IS NOT NULL))
                    OR (status = 'finished' AND (finished_at_ms IS NULL OR is_error IS NULL OR executed IS NULL))",
                "tool call(s) with inconsistent lifecycle state",
            ),
            (
                "SELECT count(*) FROM snapshots
                 WHERE subject NOT IN ('input', 'result', 'source_output', 'stdout', 'stderr')",
                "snapshot(s) with invalid subject",
            ),
            (
                "SELECT count(*) FROM snapshots WHERE stage NOT IN ('before', 'after')",
                "snapshot(s) with invalid stage",
            ),
            (
                "SELECT count(*) FROM snapshots
                 WHERE (subject IN ('input', 'result') AND media_type != 'application/json')
                    OR (subject IN ('source_output', 'stdout', 'stderr') AND media_type NOT IN ('text/plain; charset=utf-8', 'application/octet-stream'))",
                "snapshot(s) with invalid media type",
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
    run_ingest_with_archive(Archive::open()?, input, output)
}

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
        let operation: IngestOperation = serde_json::from_slice(&frame)
            .map_err(|error| format!("invalid ingest frame: {error}"))?;
        let request_id = operation.request_id();
        let result = apply_operation(&mut archive, operation);
        let error = result.as_ref().err().map(String::as_str);
        let ack = IngestAck {
            request_id,
            ok: result.is_ok(),
            error,
        };
        serde_json::to_writer(&mut output, &ack)
            .map_err(|error| format!("could not write ingest acknowledgement: {error}"))?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(|error| format!("could not flush ingest acknowledgement: {error}"))?;
    }
}

fn apply_operation(archive: &mut Archive, operation: IngestOperation) -> Result<(), String> {
    let schema_version = operation.schema_version();
    if schema_version != INGEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported ingest schema version {schema_version}; expected {INGEST_SCHEMA_VERSION}"
        ));
    }
    match operation {
        IngestOperation::BeginCall {
            session,
            call,
            input_before,
            input_after,
            captured_at_ms,
            ..
        } => archive.begin_call(&session, &call, &input_before, &input_after, captured_at_ms),
        IngestOperation::ResultBefore {
            session,
            source_call_id,
            result,
            full_output_path,
            captured_at_ms,
            ..
        } => archive.result_before(
            &session,
            &source_call_id,
            &result,
            full_output_path.as_deref(),
            captured_at_ms,
        ),
        IngestOperation::FinishCall {
            session,
            source_call_id,
            result,
            is_error,
            require_pre_result,
            finished_at_ms,
            ..
        } => archive.finish_call(
            &session,
            &source_call_id,
            &result,
            is_error,
            require_pre_result,
            finished_at_ms,
        ),
    }
}

/// Resolve the archive path from YARP, XDG, or home-directory configuration.
///
/// # Errors
///
/// Returns an error when no archive override, XDG data path, or home directory is available.
pub fn archive_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("YARP_ARCHIVE_PATH") {
        return Ok(PathBuf::from(path));
    }
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home).join("yarp/tool-calls.sqlite3"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set and YARP_ARCHIVE_PATH was not provided".to_owned())?;
    Ok(PathBuf::from(home).join(".local/share/yarp/tool-calls.sqlite3"))
}

fn initialize_schema(connection: &mut Connection) -> Result<(), String> {
    connection
        .pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .map_err(|error| format!("could not enable incremental auto-vacuum: {error}"))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .map_err(|error| format!("could not start archive migration: {error}"))?;
    transaction
        .execute_batch(SCHEMA)
        .map_err(|error| format!("could not create archive schema: {error}"))?;
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|error| format!("could not set archive schema version: {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("could not commit archive schema: {error}"))
}

fn ensure_session(transaction: &Transaction<'_>, session: &SessionIdentity) -> Result<i64, String> {
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
    transaction: &Transaction<'_>,
    session_id: i64,
    call: &CallIdentity,
) -> Result<i64, String> {
    transaction
        .execute(
            "INSERT INTO tool_calls (
                session_id, source_call_id, tool_name, provider, model,
                working_directory, started_at_ms, requires_streams, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'started')
             ON CONFLICT(session_id, source_call_id) DO NOTHING",
            params![
                session_id,
                call.source_call_id,
                call.tool_name,
                call.provider,
                call.model,
                call.working_directory,
                call.started_at_ms,
                i64::from(call.requires_streams)
            ],
        )
        .map_err(|error| format!("could not store tool call: {error}"))?;
    let stored = transaction
        .query_row(
            "SELECT id, tool_name, provider, model, working_directory, started_at_ms, requires_streams
             FROM tool_calls WHERE session_id = ?1 AND source_call_id = ?2",
            params![session_id, call.source_call_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .map_err(|error| format!("could not find stored tool call: {error}"))?;
    if stored.1 != call.tool_name
        || stored.2 != call.provider
        || stored.3 != call.model
        || stored.4 != call.working_directory
        || stored.5 != call.started_at_ms
        || stored.6 != call.requires_streams
    {
        return Err(format!(
            "tool call {} was already stored with different metadata",
            call.source_call_id
        ));
    }
    Ok(stored.0)
}

fn find_call(
    transaction: &Transaction<'_>,
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

fn validate_completion(
    transaction: &Transaction<'_>,
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
    transaction: &Transaction<'_>,
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

fn insert_snapshot_reader(
    transaction: &Transaction<'_>,
    call_id: i64,
    subject: &str,
    stage: &str,
    media_type: &str,
    captured_at_ms: i64,
    reader: &mut (impl Read + Seek),
) -> Result<(), String> {
    let sha = insert_payload(transaction, reader)?;
    let inserted = transaction
        .execute(
            "INSERT INTO snapshots (
                tool_call_id, subject, stage, media_type, captured_at_ms, payload_sha256
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(tool_call_id, subject, stage) DO NOTHING",
            params![call_id, subject, stage, media_type, captured_at_ms, sha],
        )
        .map_err(|error| format!("could not store {subject}/{stage} snapshot: {error}"))?;
    if inserted == 0 {
        let existing: Vec<u8> = transaction
            .query_row(
                "SELECT payload_sha256 FROM snapshots WHERE tool_call_id = ?1 AND subject = ?2 AND stage = ?3",
                params![call_id, subject, stage],
                |row| row.get(0),
            )
            .map_err(|error| format!("could not check existing snapshot: {error}"))?;
        if existing != sha {
            return Err(format!(
                "snapshot {subject}/{stage} already exists with different content"
            ));
        }
    }
    Ok(())
}

fn insert_payload(
    transaction: &Transaction<'_>,
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

fn prepare_path(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("archive path {} has no parent", path.display()))?;
    let parent_existed = parent.exists();
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create archive directory {}: {error}",
            parent.display()
        )
    })?;
    if !parent_existed || parent.file_name().is_some_and(|name| name == "yarp") {
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
            "archive directory {} has mode {mode:o}; use a private directory or a directory named yarp",
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
        archive
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
    }

    #[test]
    fn stores_exact_source_full_output_with_the_result() {
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
                "operation": "finish_call",
                "requestId": 3,
                "schemaVersion": 1,
                "session": session(),
                "sourceCallId": "call-1",
                "result": {"content": "after"},
                "isError": false,
                "requirePreResult": true,
                "finishedAtMs": 40
            }),
        ];
        let input: Vec<u8> = operations.iter().flat_map(framed).collect();
        let mut output = Vec::new();
        run_ingest_with_archive(archive, Cursor::new(input), &mut output).expect("ingest");
        let acknowledgements = String::from_utf8(output).expect("UTF-8");
        assert_eq!(acknowledgements.lines().count(), 3);
        assert!(acknowledgements.contains("\"requestId\":3"));
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
        let archive = Archive::open_path(path).expect("repair");
        assert!(archive.verify().expect("verify").errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_modify_an_unrelated_public_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = TempDir::new().expect("temp directory");
        let public = directory.path().join("public");
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
