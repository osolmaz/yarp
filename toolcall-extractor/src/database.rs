use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use duckdb::{Connection, OptionalExt, params};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::keys;
use crate::model::{
    IssueRecord, ObservationRecord, SessionRecord, SourceItemRecord, SourceRootRecord,
    SourceStatus, ToolCallRecord, ToolResultRecord,
};
use crate::private_fs;
use crate::sink::{Checkpoint, Sink};

const SCHEMA_VERSION: i64 = 1;
const BUFFER_ROWS: usize = 1_024;
const OPEN_GROWTH_RESERVE: u64 = 1_048_576;
const ROW_GROWTH_RESERVE: u64 = 1_024;

pub struct Database {
    connection: Connection,
    path: PathBuf,
    run_key: String,
    in_transaction: bool,
    sessions: Vec<SessionRecord>,
    tool_calls: Vec<ToolCallRecord>,
    tool_results: Vec<ToolResultRecord>,
    observations: Vec<ObservationRecord>,
    issues: Vec<IssueRecord>,
    staged_growth_upper_bound: u64,
    _lock: File,
}

#[derive(Debug, Default, Serialize)]
pub struct AgentStats {
    pub sessions: i64,
    pub tool_calls: i64,
    pub tool_results: i64,
    pub errors: i64,
    pub input_characters: i64,
    pub output_characters: i64,
    pub structured_output_characters: i64,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub sessions: i64,
    pub tool_calls: i64,
    pub tool_results: i64,
    pub calls_without_results: i64,
    pub calls_with_conflicting_results: i64,
    pub errors: i64,
    pub issues: i64,
    pub input_characters: i64,
    pub output_characters: i64,
    pub structured_output_characters: i64,
    pub database_bytes: u64,
    pub by_agent: BTreeMap<String, AgentStats>,
}

#[derive(Debug, Serialize)]
pub struct Verification {
    pub schema_version: i64,
    pub orphan_calls: i64,
    pub orphan_results: i64,
    pub orphan_observations: i64,
    pub calls_without_results: i64,
    pub calls_with_conflicting_results: i64,
    pub issues: i64,
    pub private_permissions: bool,
    pub under_size_limit: bool,
}

impl Verification {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.orphan_calls == 0
            && self.orphan_results == 0
            && self.orphan_observations == 0
            && self.private_permissions
            && self.under_size_limit
    }
}

impl Database {
    pub fn open(path: &Path, unix_user: &str, agent: &str) -> Result<Self> {
        private_fs::prepare_database_path(path)?;
        let lock = private_fs::acquire_database_lock(path)?;
        private_fs::enforce_size_limit(path, OPEN_GROWTH_RESERVE)?;
        let connection = Connection::open(path)?;
        private_fs::protect_database(path)?;
        connection.execute_batch(
            "SET enable_external_access = false;
             SET autoinstall_known_extensions = false;
             SET autoload_known_extensions = false;",
        )?;
        create_schema(&connection)?;
        create_staging_tables(&connection)?;
        let started_at_ms = keys::now_ms();
        connection.execute(
            "UPDATE import_runs
             SET status = 'failed', finished_at_ms = ?
             WHERE status = 'running'",
            [started_at_ms],
        )?;
        let run_key = keys::key(&[
            b"run",
            unix_user.as_bytes(),
            agent.as_bytes(),
            &started_at_ms.to_be_bytes(),
            &std::process::id().to_be_bytes(),
        ]);
        connection.execute(
            "INSERT INTO import_runs
             (run_key, unix_user, agent, started_at_ms, status)
             VALUES (?, ?, ?, ?, 'running')",
            params![run_key, unix_user, agent, started_at_ms],
        )?;
        Ok(Self {
            connection,
            path: path.to_owned(),
            run_key,
            in_transaction: false,
            sessions: Vec::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            observations: Vec::new(),
            issues: Vec::new(),
            staged_growth_upper_bound: 0,
            _lock: lock,
        })
    }

    pub fn open_read_only(path: &Path) -> Result<Connection> {
        if !path.exists() {
            return Err(Error::InvalidSource(format!(
                "database does not exist: {}",
                path.display()
            )));
        }
        let connection = Connection::open_with_flags(
            path,
            duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
        )?;
        connection.execute_batch("SET enable_external_access = false;")?;
        Ok(connection)
    }

    fn execute_cached(&self, sql: &str, parameters: impl duckdb::Params) -> Result<()> {
        self.connection.prepare_cached(sql)?.execute(parameters)?;
        Ok(())
    }

    fn flush_buffers(&mut self) -> Result<()> {
        let growth = estimate_sessions(&self.sessions)
            .saturating_add(estimate_calls(&self.tool_calls))
            .saturating_add(estimate_results(&self.tool_results))
            .saturating_add(estimate_observations(&self.observations))
            .saturating_add(estimate_issues(&self.issues));
        private_fs::enforce_size_limit(&self.path, growth)?;
        append_sessions(&self.connection, std::mem::take(&mut self.sessions))?;
        append_calls(&self.connection, std::mem::take(&mut self.tool_calls))?;
        append_results(&self.connection, std::mem::take(&mut self.tool_results))?;
        append_observations(&self.connection, std::mem::take(&mut self.observations))?;
        append_issues(
            &self.connection,
            &self.run_key,
            std::mem::take(&mut self.issues),
        )?;
        self.staged_growth_upper_bound = self.staged_growth_upper_bound.saturating_add(growth);
        Ok(())
    }

    fn merge_staging(&self) -> Result<()> {
        self.connection.execute_batch(
            "INSERT OR IGNORE INTO sessions
             SELECT session_key, unix_user, agent, native_session_id, started_at_ms
             FROM (
                 SELECT *, row_number() OVER (PARTITION BY session_key) AS duplicate_number
                 FROM staging_sessions
             ) WHERE duplicate_number = 1;

             INSERT OR IGNORE INTO tool_calls
             SELECT call_key, session_key, native_call_id, native_worker_id, called_at_ms,
                    provider, model, working_directory, tool_name, input_format, input_text,
                    input_sha256
             FROM (
                 SELECT *, row_number() OVER (PARTITION BY call_key) AS duplicate_number
                 FROM staging_tool_calls
             ) WHERE duplicate_number = 1;

             INSERT OR IGNORE INTO tool_results
             SELECT result_key, call_key, returned_at_ms, is_error, output_text, output_json,
                    result_sha256
             FROM (
                 SELECT *, row_number() OVER (PARTITION BY result_key) AS duplicate_number
                 FROM staging_tool_results
             ) WHERE duplicate_number = 1;

             INSERT OR IGNORE INTO observations
             SELECT observation_key, source_item_key, call_key, result_key, record_kind,
                    native_record_kind, sequence_number, native_branch_id, is_current,
                    line_number, byte_offset, sqlite_rowid, sqlite_blob_id, content_index,
                    record_sha256
             FROM (
                 SELECT *, row_number() OVER (PARTITION BY observation_key) AS duplicate_number
                 FROM staging_observations
             ) WHERE duplicate_number = 1;

             INSERT OR IGNORE INTO import_issues
             SELECT issue_key, run_key, source_item_key, severity, code, line_number,
                    byte_offset, sqlite_blob_id, record_sha256, message, occurrence_count
             FROM staging_issues;

             DELETE FROM staging_sessions;
             DELETE FROM staging_tool_calls;
             DELETE FROM staging_tool_results;
             DELETE FROM staging_observations;
             DELETE FROM staging_issues;",
        )?;
        Ok(())
    }

    fn clear_buffers(&mut self) {
        self.sessions.clear();
        self.tool_calls.clear();
        self.tool_results.clear();
        self.observations.clear();
        self.issues.clear();
        self.staged_growth_upper_bound = 0;
    }

    fn quarantine_orphan_results(&self) -> Result<()> {
        // Keep ordinary orphans so a call appended later can resolve them. A result directly
        // after a malformed record has bounded source evidence that its call was unrecoverable.
        self.connection.execute_batch(
            "BEGIN TRANSACTION;
             CREATE OR REPLACE TEMP TABLE orphan_result_keys AS
             SELECT DISTINCT r.result_key
             FROM tool_results r
             LEFT JOIN tool_calls c ON c.call_key = r.call_key
             JOIN observations o ON o.result_key = r.result_key
             JOIN import_issues i ON i.source_item_key = o.source_item_key
                                 AND i.code = 'malformed_jsonl'
                                 AND i.line_number + 1 = o.line_number
             WHERE c.call_key IS NULL;",
        )?;
        let result = (|| -> Result<()> {
            let mut statement = self.connection.prepare(
                "SELECT k.result_key, o.source_item_key, o.line_number, o.byte_offset,
                        o.sqlite_blob_id, o.record_sha256
                 FROM orphan_result_keys k
                 LEFT JOIN observations o ON o.result_key = k.result_key
                 QUALIFY row_number() OVER (
                     PARTITION BY k.result_key
                     ORDER BY o.source_item_key, o.line_number, o.byte_offset
                 ) = 1
                 ORDER BY k.result_key",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<u64>>(2)?,
                    row.get::<_, Option<u64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            })?;
            let mut issues = Vec::new();
            for row in rows {
                issues.push(row?);
            }
            drop(statement);

            for (result_key, source_item_key, line_number, byte_offset, sqlite_blob_id, hash) in
                issues
            {
                let issue_key = keys::key(&[
                    b"issue",
                    source_item_key.as_deref().unwrap_or_default().as_bytes(),
                    b"result_without_call",
                    result_key.as_bytes(),
                ]);
                self.execute_cached(
                    "INSERT INTO import_issues
                     (issue_key, run_key, source_item_key, severity, code, line_number,
                      byte_offset, sqlite_blob_id, record_sha256, message, occurrence_count)
                     VALUES (?, ?, ?, 'warning', 'result_without_call', ?, ?, ?, ?,
                             'tool result follows a malformed record and has no defensible matching call', 1)
                     ON CONFLICT (issue_key) DO UPDATE SET
                       run_key = excluded.run_key,
                       source_item_key = excluded.source_item_key,
                       line_number = excluded.line_number,
                       byte_offset = excluded.byte_offset,
                       sqlite_blob_id = excluded.sqlite_blob_id,
                       record_sha256 = excluded.record_sha256,
                       message = excluded.message,
                       occurrence_count = excluded.occurrence_count",
                    params![
                        issue_key,
                        self.run_key,
                        source_item_key,
                        line_number,
                        byte_offset,
                        sqlite_blob_id,
                        hash
                    ],
                )?;
            }
            self.connection.execute_batch(
                "DELETE FROM observations
                 WHERE result_key IN (SELECT result_key FROM orphan_result_keys);
                 DELETE FROM tool_results
                 WHERE result_key IN (SELECT result_key FROM orphan_result_keys);
                 DROP TABLE orphan_result_keys;",
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => self.connection.execute_batch("COMMIT;").map_err(Into::into),
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    pub fn finish(&mut self, success: bool) -> Result<()> {
        if self.in_transaction {
            self.rollback_source()?;
        }
        if success {
            self.quarantine_orphan_results()?;
        }
        let status = if success { "complete" } else { "failed" };
        self.execute_cached(
            "UPDATE import_runs SET status = ?, finished_at_ms = ? WHERE run_key = ?",
            params![status, keys::now_ms(), self.run_key],
        )?;
        self.connection.execute_batch("CHECKPOINT;")?;
        private_fs::protect_database(&self.path)?;
        private_fs::enforce_size_limit(&self.path, 0)
    }

    pub fn stats(path: &Path) -> Result<Stats> {
        let connection = Self::open_read_only(path)?;
        let scalar = |sql: &str| -> Result<i64> {
            connection
                .query_row(sql, [], |row| row.get(0))
                .map_err(Into::into)
        };
        Ok(Stats {
            sessions: scalar("SELECT count(*) FROM sessions")?,
            tool_calls: scalar("SELECT count(*) FROM tool_calls")?,
            tool_results: scalar("SELECT count(*) FROM tool_results")?,
            calls_without_results: scalar(
                "SELECT count(*) FROM tool_calls c
                 WHERE NOT EXISTS (SELECT 1 FROM tool_results r WHERE r.call_key = c.call_key)",
            )?,
            calls_with_conflicting_results: scalar(
                "SELECT count(*) FROM (
                     SELECT call_key FROM tool_results GROUP BY call_key HAVING count(*) > 1
                 )",
            )?,
            errors: scalar("SELECT count(*) FROM tool_results WHERE is_error = true")?,
            issues: scalar("SELECT coalesce(sum(occurrence_count), 0) FROM import_issues")?,
            input_characters: scalar(
                "SELECT coalesce(sum(length(input_text)), 0) FROM tool_calls",
            )?,
            output_characters: scalar(
                "SELECT coalesce(sum(length(output_text)), 0) FROM tool_results",
            )?,
            structured_output_characters: scalar(
                "SELECT coalesce(sum(length(output_json)), 0) FROM tool_results",
            )?,
            database_bytes: private_fs::data_directory_bytes(path)?,
            by_agent: agent_stats(&connection)?,
        })
    }

    pub fn verify(path: &Path) -> Result<Verification> {
        let connection = Self::open_read_only(path)?;
        let scalar = |sql: &str| -> Result<i64> {
            connection
                .query_row(sql, [], |row| row.get(0))
                .map_err(Into::into)
        };
        let schema_version = scalar("SELECT version FROM schema_info")?;
        let private_permissions = private_fs::verify_private(path).is_ok();
        let under_size_limit =
            private_fs::data_directory_bytes(path)? <= private_fs::MAX_DATA_BYTES;
        Ok(Verification {
            schema_version,
            orphan_calls: scalar(
                "SELECT count(*) FROM tool_calls c LEFT JOIN sessions s
                 ON s.session_key = c.session_key WHERE s.session_key IS NULL",
            )?,
            orphan_results: scalar(
                "SELECT count(*) FROM tool_results r LEFT JOIN tool_calls c
                 ON c.call_key = r.call_key WHERE c.call_key IS NULL",
            )?,
            orphan_observations: scalar(
                "SELECT count(*) FROM observations o
                 LEFT JOIN source_items s ON s.source_item_key = o.source_item_key
                 LEFT JOIN tool_calls c ON c.call_key = o.call_key
                 LEFT JOIN tool_results r ON r.result_key = o.result_key
                 WHERE s.source_item_key IS NULL
                    OR (o.call_key IS NOT NULL AND c.call_key IS NULL)
                    OR (o.result_key IS NOT NULL AND r.result_key IS NULL)",
            )?,
            calls_without_results: scalar(
                "SELECT count(*) FROM tool_calls c
                 WHERE NOT EXISTS (SELECT 1 FROM tool_results r WHERE r.call_key = c.call_key)",
            )?,
            calls_with_conflicting_results: scalar(
                "SELECT count(*) FROM (
                     SELECT call_key FROM tool_results GROUP BY call_key HAVING count(*) > 1
                 )",
            )?,
            issues: scalar("SELECT coalesce(sum(occurrence_count), 0) FROM import_issues")?,
            private_permissions,
            under_size_limit,
        })
    }

    pub fn print_issues(path: &Path) -> Result<()> {
        let connection = Self::open_read_only(path)?;
        let mut statement = connection.prepare(
            "SELECT r.agent, s.relative_path, i.line_number, i.byte_offset,
                    i.sqlite_blob_id, i.severity, i.code, i.message, i.occurrence_count
             FROM import_issues i
             LEFT JOIN source_items s ON s.source_item_key = i.source_item_key
             LEFT JOIN source_roots r ON r.source_root_key = s.source_root_key
             ORDER BY i.severity DESC, r.agent, s.relative_path, i.line_number
             LIMIT 1000",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(serde_json::json!({
                "agent": row.get::<_, Option<String>>(0)?,
                "source": row.get::<_, Option<String>>(1)?,
                "line": row.get::<_, Option<u64>>(2)?,
                "byte_offset": row.get::<_, Option<u64>>(3)?,
                "sqlite_blob_id": row.get::<_, Option<String>>(4)?,
                "severity": row.get::<_, String>(5)?,
                "code": row.get::<_, String>(6)?,
                "message": row.get::<_, String>(7)?,
                "occurrences": row.get::<_, u64>(8)?,
            }))
        })?;
        for row in rows {
            println!("{}", serde_json::to_string(&row?)?);
        }
        Ok(())
    }
}

fn agent_stats(connection: &Connection) -> Result<BTreeMap<String, AgentStats>> {
    let mut statement = connection.prepare(
        "SELECT a.agent,
                (SELECT count(*) FROM sessions s WHERE s.agent = a.agent),
                (SELECT count(*) FROM tool_calls c JOIN sessions s ON s.session_key = c.session_key
                 WHERE s.agent = a.agent),
                (SELECT count(*) FROM tool_results r JOIN tool_calls c ON c.call_key = r.call_key
                 JOIN sessions s ON s.session_key = c.session_key WHERE s.agent = a.agent),
                (SELECT count(*) FROM tool_results r JOIN tool_calls c ON c.call_key = r.call_key
                 JOIN sessions s ON s.session_key = c.session_key
                 WHERE s.agent = a.agent AND r.is_error = true),
                (SELECT coalesce(sum(length(c.input_text)), 0) FROM tool_calls c
                 JOIN sessions s ON s.session_key = c.session_key WHERE s.agent = a.agent),
                (SELECT coalesce(sum(length(r.output_text)), 0) FROM tool_results r
                 JOIN tool_calls c ON c.call_key = r.call_key
                 JOIN sessions s ON s.session_key = c.session_key WHERE s.agent = a.agent),
                (SELECT coalesce(sum(length(r.output_json)), 0) FROM tool_results r
                 JOIN tool_calls c ON c.call_key = r.call_key
                 JOIN sessions s ON s.session_key = c.session_key WHERE s.agent = a.agent)
         FROM (SELECT DISTINCT agent FROM sessions) a ORDER BY a.agent",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            AgentStats {
                sessions: row.get(1)?,
                tool_calls: row.get(2)?,
                tool_results: row.get(3)?,
                errors: row.get(4)?,
                input_characters: row.get(5)?,
                output_characters: row.get(6)?,
                structured_output_characters: row.get(7)?,
            },
        ))
    })?;
    let mut stats = BTreeMap::new();
    for row in rows {
        let (agent, values) = row?;
        stats.insert(agent, values);
    }
    Ok(stats)
}

impl Sink for Database {
    fn checkpoint(&self, source_item_key: &str) -> Result<Option<Checkpoint>> {
        self.connection
            .query_row(
                "SELECT adapter_version, device_id, inode, size_bytes, snapshot_mtime_ns,
                        imported_byte_count, prefix_sha256, status
                 FROM source_items WHERE source_item_key = ?",
                [source_item_key],
                |row| {
                    Ok(Checkpoint {
                        adapter_version: row.get(0)?,
                        device_id: row.get(1)?,
                        inode: row.get(2)?,
                        size_bytes: row.get(3)?,
                        snapshot_mtime_ns: row.get(4)?,
                        imported_byte_count: row.get(5)?,
                        prefix_sha256: row.get(6)?,
                        status: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    fn resolve_call(&self, session_key: &str, native_call_id: &str) -> Result<Option<String>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT call_key FROM tool_calls
             WHERE session_key = ? AND native_call_id = ?
             ORDER BY called_at_ms DESC NULLS LAST, call_key
             LIMIT 2",
        )?;
        let mut rows = statement.query(params![session_key, native_call_id])?;
        let first = rows
            .next()?
            .map(|row| row.get::<_, String>(0))
            .transpose()?;
        if first.is_some() && rows.next()?.is_some() {
            return Ok(None);
        }
        Ok(first)
    }

    fn begin_source(&mut self) -> Result<()> {
        if self.in_transaction {
            return Err(Error::InvalidSource(
                "source transaction already active".to_owned(),
            ));
        }
        self.connection.execute_batch("BEGIN TRANSACTION;")?;
        self.in_transaction = true;
        Ok(())
    }

    fn commit_source(&mut self) -> Result<()> {
        if !self.in_transaction {
            return Ok(());
        }
        self.flush_buffers()?;
        private_fs::enforce_size_limit(&self.path, self.staged_growth_upper_bound)?;
        self.merge_staging()?;
        if private_fs::enforce_size_limit(&self.path, 0).is_err() {
            self.connection.execute_batch("ROLLBACK;")?;
            self.in_transaction = false;
            return Err(Error::SizeLimit);
        }
        self.connection.execute_batch("COMMIT;")?;
        self.in_transaction = false;
        self.staged_growth_upper_bound = 0;
        Ok(())
    }

    fn rollback_source(&mut self) -> Result<()> {
        self.clear_buffers();
        if self.in_transaction {
            self.connection.execute_batch("ROLLBACK;")?;
            self.in_transaction = false;
        }
        Ok(())
    }

    fn reset_source(&mut self, source_item_key: &str) -> Result<()> {
        self.execute_cached(
            "CREATE OR REPLACE TEMP TABLE reset_result_keys AS
             SELECT result_key FROM observations
             WHERE source_item_key = ? AND result_key IS NOT NULL",
            [source_item_key],
        )?;
        self.execute_cached(
            "CREATE OR REPLACE TEMP TABLE reset_call_keys AS
             SELECT call_key FROM observations
             WHERE source_item_key = ? AND call_key IS NOT NULL
             UNION
             SELECT r.call_key FROM tool_results r
             JOIN reset_result_keys k ON k.result_key = r.result_key",
            [source_item_key],
        )?;
        self.execute_cached(
            "DELETE FROM observations WHERE source_item_key = ?",
            [source_item_key],
        )?;
        self.execute_cached(
            "DELETE FROM import_issues WHERE source_item_key = ?",
            [source_item_key],
        )?;
        self.connection.execute_batch(
            "DELETE FROM tool_results r
             WHERE r.result_key IN (SELECT result_key FROM reset_result_keys)
               AND NOT EXISTS (
                   SELECT 1 FROM observations o WHERE o.result_key = r.result_key
               );
             DELETE FROM tool_calls c
             WHERE c.call_key IN (SELECT call_key FROM reset_call_keys)
               AND NOT EXISTS (
                   SELECT 1 FROM observations o WHERE o.call_key = c.call_key
               )
               AND NOT EXISTS (
                   SELECT 1 FROM tool_results r WHERE r.call_key = c.call_key
               );
             DROP TABLE reset_call_keys;
             DROP TABLE reset_result_keys;",
        )?;
        Ok(())
    }

    fn source_root(&mut self, record: &SourceRootRecord) -> Result<()> {
        private_fs::enforce_size_limit(&self.path, estimate_source_root(record))?;
        self.execute_cached(
            "INSERT INTO source_roots
             (source_root_key, unix_user, agent, source_kind, root_path)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (source_root_key) DO NOTHING",
            params![
                record.source_root_key,
                record.unix_user,
                record.agent,
                record.source_kind,
                record.root_path
            ],
        )?;
        Ok(())
    }

    fn source_item(&mut self, record: &SourceItemRecord) -> Result<()> {
        private_fs::enforce_size_limit(&self.path, estimate_source_item(record))?;
        if matches!(record.status, SourceStatus::Deferred)
            && record.imported_byte_count == Some(0)
            && let Some(checkpoint) = self.checkpoint(&record.source_item_key)?
        {
            let unchanged = checkpoint.adapter_version == record.adapter_version
                && checkpoint.device_id == record.device_id
                && checkpoint.inode == record.inode
                && checkpoint.size_bytes == record.size_bytes
                && checkpoint.snapshot_mtime_ns == record.snapshot_mtime_ns
                && checkpoint.imported_byte_count == Some(record.size_bytes)
                && checkpoint.status == "complete";
            if !unchanged {
                self.reset_source(&record.source_item_key)?;
            }
        }
        self.execute_cached(
            "INSERT INTO source_items
             (source_item_key, source_root_key, relative_path, adapter_version,
              device_id, inode, size_bytes, snapshot_mtime_ns, imported_byte_count,
              prefix_sha256, last_run_key, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (source_item_key) DO UPDATE SET
               source_root_key = excluded.source_root_key,
               relative_path = excluded.relative_path,
               adapter_version = excluded.adapter_version,
               device_id = excluded.device_id,
               inode = excluded.inode,
               size_bytes = excluded.size_bytes,
               snapshot_mtime_ns = excluded.snapshot_mtime_ns,
               imported_byte_count = excluded.imported_byte_count,
               prefix_sha256 = excluded.prefix_sha256,
               last_run_key = excluded.last_run_key,
               status = excluded.status",
            params![
                record.source_item_key,
                record.source_root_key,
                record.relative_path,
                record.adapter_version,
                record.device_id,
                record.inode,
                record.size_bytes,
                record.snapshot_mtime_ns,
                record.imported_byte_count,
                record.prefix_sha256,
                self.run_key,
                record.status.as_str()
            ],
        )?;
        Ok(())
    }

    fn session(&mut self, record: &SessionRecord) -> Result<()> {
        self.sessions.push(record.clone());
        if self.sessions.len() >= BUFFER_ROWS {
            let growth = estimate_sessions(&self.sessions);
            private_fs::enforce_size_limit(&self.path, growth)?;
            append_sessions(&self.connection, std::mem::take(&mut self.sessions))?;
            self.staged_growth_upper_bound = self.staged_growth_upper_bound.saturating_add(growth);
        }
        Ok(())
    }

    fn tool_call(&mut self, record: &ToolCallRecord) -> Result<()> {
        self.tool_calls.push(record.clone());
        if self.tool_calls.len() >= BUFFER_ROWS {
            let growth = estimate_calls(&self.tool_calls);
            private_fs::enforce_size_limit(&self.path, growth)?;
            append_calls(&self.connection, std::mem::take(&mut self.tool_calls))?;
            self.staged_growth_upper_bound = self.staged_growth_upper_bound.saturating_add(growth);
        }
        Ok(())
    }

    fn tool_result(&mut self, record: &ToolResultRecord) -> Result<()> {
        self.tool_results.push(record.clone());
        if self.tool_results.len() >= BUFFER_ROWS {
            let growth = estimate_results(&self.tool_results);
            private_fs::enforce_size_limit(&self.path, growth)?;
            append_results(&self.connection, std::mem::take(&mut self.tool_results))?;
            self.staged_growth_upper_bound = self.staged_growth_upper_bound.saturating_add(growth);
        }
        Ok(())
    }

    fn observation(&mut self, record: &ObservationRecord) -> Result<()> {
        self.observations.push(record.clone());
        if self.observations.len() >= BUFFER_ROWS {
            let growth = estimate_observations(&self.observations);
            private_fs::enforce_size_limit(&self.path, growth)?;
            append_observations(&self.connection, std::mem::take(&mut self.observations))?;
            self.staged_growth_upper_bound = self.staged_growth_upper_bound.saturating_add(growth);
        }
        Ok(())
    }

    fn issue(&mut self, record: &IssueRecord) -> Result<()> {
        self.issues.push(record.clone());
        if self.issues.len() >= BUFFER_ROWS {
            let growth = estimate_issues(&self.issues);
            private_fs::enforce_size_limit(&self.path, growth)?;
            append_issues(
                &self.connection,
                &self.run_key,
                std::mem::take(&mut self.issues),
            )?;
            self.staged_growth_upper_bound = self.staged_growth_upper_bound.saturating_add(growth);
        }
        Ok(())
    }
}

fn estimated_growth(payload_bytes: u64, rows: usize) -> u64 {
    payload_bytes.saturating_mul(2).saturating_add(
        u64::try_from(rows)
            .unwrap_or(u64::MAX)
            .saturating_mul(ROW_GROWTH_RESERVE),
    )
}

fn text_bytes(value: &str) -> u64 {
    u64::try_from(value.len()).unwrap_or(u64::MAX)
}

fn optional_text_bytes(value: Option<&String>) -> u64 {
    value.map_or(0, |text| text_bytes(text))
}

fn estimate_source_root(record: &SourceRootRecord) -> u64 {
    estimated_growth(
        text_bytes(&record.source_root_key)
            .saturating_add(text_bytes(&record.unix_user))
            .saturating_add(text_bytes(&record.agent))
            .saturating_add(text_bytes(&record.source_kind))
            .saturating_add(text_bytes(&record.root_path)),
        1,
    )
}

fn estimate_source_item(record: &SourceItemRecord) -> u64 {
    estimated_growth(
        text_bytes(&record.source_item_key)
            .saturating_add(text_bytes(&record.source_root_key))
            .saturating_add(text_bytes(&record.relative_path))
            .saturating_add(
                record
                    .prefix_sha256
                    .as_ref()
                    .map_or(0, |value| u64::try_from(value.len()).unwrap_or(u64::MAX)),
            ),
        1,
    )
}

fn estimate_sessions(records: &[SessionRecord]) -> u64 {
    let payload = records.iter().fold(0_u64, |total, record| {
        total
            .saturating_add(text_bytes(&record.session_key))
            .saturating_add(text_bytes(&record.unix_user))
            .saturating_add(text_bytes(&record.agent))
            .saturating_add(text_bytes(&record.native_session_id))
    });
    estimated_growth(payload, records.len())
}

fn estimate_calls(records: &[ToolCallRecord]) -> u64 {
    let payload = records.iter().fold(0_u64, |total, record| {
        total
            .saturating_add(text_bytes(&record.call_key))
            .saturating_add(text_bytes(&record.session_key))
            .saturating_add(optional_text_bytes(record.native_call_id.as_ref()))
            .saturating_add(optional_text_bytes(record.native_worker_id.as_ref()))
            .saturating_add(optional_text_bytes(record.provider.as_ref()))
            .saturating_add(optional_text_bytes(record.model.as_ref()))
            .saturating_add(optional_text_bytes(record.working_directory.as_ref()))
            .saturating_add(text_bytes(&record.tool_name))
            .saturating_add(text_bytes(&record.input_text))
            .saturating_add(u64::try_from(record.input_sha256.len()).unwrap_or(u64::MAX))
    });
    estimated_growth(payload, records.len())
}

fn estimate_results(records: &[ToolResultRecord]) -> u64 {
    let payload = records.iter().fold(0_u64, |total, record| {
        total
            .saturating_add(text_bytes(&record.result_key))
            .saturating_add(text_bytes(&record.call_key))
            .saturating_add(optional_text_bytes(record.output_text.as_ref()))
            .saturating_add(optional_text_bytes(record.output_json.as_ref()))
            .saturating_add(u64::try_from(record.result_sha256.len()).unwrap_or(u64::MAX))
    });
    estimated_growth(payload, records.len())
}

fn estimate_observations(records: &[ObservationRecord]) -> u64 {
    let payload = records.iter().fold(0_u64, |total, record| {
        total
            .saturating_add(text_bytes(&record.observation_key))
            .saturating_add(text_bytes(&record.source_item_key))
            .saturating_add(optional_text_bytes(record.call_key.as_ref()))
            .saturating_add(optional_text_bytes(record.result_key.as_ref()))
            .saturating_add(text_bytes(&record.native_record_kind))
            .saturating_add(optional_text_bytes(record.native_branch_id.as_ref()))
            .saturating_add(optional_text_bytes(record.sqlite_blob_id.as_ref()))
            .saturating_add(u64::try_from(record.record_sha256.len()).unwrap_or(u64::MAX))
    });
    estimated_growth(payload, records.len())
}

fn estimate_issues(records: &[IssueRecord]) -> u64 {
    let payload = records.iter().fold(0_u64, |total, record| {
        total
            .saturating_add(text_bytes(&record.issue_key))
            .saturating_add(optional_text_bytes(record.source_item_key.as_ref()))
            .saturating_add(text_bytes(&record.code))
            .saturating_add(optional_text_bytes(record.sqlite_blob_id.as_ref()))
            .saturating_add(
                record
                    .record_sha256
                    .as_ref()
                    .map_or(0, |value| u64::try_from(value.len()).unwrap_or(u64::MAX)),
            )
            .saturating_add(text_bytes(&record.message))
    });
    estimated_growth(payload, records.len())
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(include_str!("schema.sql"))?;
    let version: i64 =
        connection.query_row("SELECT version FROM schema_info", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(Error::InvalidSource(format!(
            "unsupported schema version {version}"
        )));
    }
    Ok(())
}

fn create_staging_tables(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TEMP TABLE staging_sessions AS SELECT * FROM sessions WHERE false;
         CREATE TEMP TABLE staging_tool_calls AS SELECT * FROM tool_calls WHERE false;
         CREATE TEMP TABLE staging_tool_results AS SELECT * FROM tool_results WHERE false;
         CREATE TEMP TABLE staging_observations AS SELECT * FROM observations WHERE false;
         CREATE TEMP TABLE staging_issues AS SELECT * FROM import_issues WHERE false;",
    )?;
    Ok(())
}

fn append_sessions(connection: &Connection, records: Vec<SessionRecord>) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut appender = connection.appender("staging_sessions")?;
    for record in records {
        appender.append_row(params![
            record.session_key,
            record.unix_user,
            record.agent,
            record.native_session_id,
            record.started_at_ms
        ])?;
    }
    appender.flush()?;
    Ok(())
}

fn append_calls(connection: &Connection, records: Vec<ToolCallRecord>) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut appender = connection.appender("staging_tool_calls")?;
    for record in records {
        appender.append_row(params![
            record.call_key,
            record.session_key,
            record.native_call_id,
            record.native_worker_id,
            record.called_at_ms,
            record.provider,
            record.model,
            record.working_directory,
            record.tool_name,
            record.input_format.as_str(),
            record.input_text,
            record.input_sha256
        ])?;
    }
    appender.flush()?;
    Ok(())
}

fn append_results(connection: &Connection, records: Vec<ToolResultRecord>) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut appender = connection.appender("staging_tool_results")?;
    for record in records {
        appender.append_row(params![
            record.result_key,
            record.call_key,
            record.returned_at_ms,
            record.is_error,
            record.output_text,
            record.output_json,
            record.result_sha256
        ])?;
    }
    appender.flush()?;
    Ok(())
}

fn append_observations(connection: &Connection, records: Vec<ObservationRecord>) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut appender = connection.appender("staging_observations")?;
    for record in records {
        appender.append_row(params![
            record.observation_key,
            record.source_item_key,
            record.call_key,
            record.result_key,
            record.record_kind.as_str(),
            record.native_record_kind,
            record.sequence_number,
            record.native_branch_id,
            record.is_current,
            record.line_number,
            record.byte_offset,
            record.sqlite_rowid,
            record.sqlite_blob_id,
            record.content_index,
            record.record_sha256
        ])?;
    }
    appender.flush()?;
    Ok(())
}

fn append_issues(connection: &Connection, run_key: &str, records: Vec<IssueRecord>) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut appender = connection.appender("staging_issues")?;
    for record in records {
        let issue_key = record.issue_key.clone();
        appender.append_row(params![
            issue_key,
            run_key,
            record.source_item_key,
            record.severity.as_str(),
            record.code,
            record.line_number,
            record.byte_offset,
            record.sqlite_blob_id,
            record.record_sha256,
            keys::bounded_message(&record.message),
            record.occurrence_count
        ])?;
    }
    appender.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn rejects_database_open_before_crossing_the_size_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let directory = temp.path().join("data");
        fs::create_dir(&directory).expect("data directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("private data directory");
        fs::File::create(directory.join("reserved"))
            .expect("reserved file")
            .set_len(private_fs::MAX_DATA_BYTES)
            .expect("sparse size");
        let path = directory.join("toolcalls.duckdb");
        assert!(matches!(
            Database::open(&path, "test", "pi"),
            Err(Error::SizeLimit)
        ));
    }
}
