CREATE TABLE IF NOT EXISTS schema_info (
    version INTEGER PRIMARY KEY CHECK (version > 0)
);
INSERT INTO schema_info
SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_info);

CREATE TABLE IF NOT EXISTS import_runs (
    run_key VARCHAR PRIMARY KEY,
    unix_user VARCHAR NOT NULL,
    agent VARCHAR NOT NULL,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    status VARCHAR NOT NULL CHECK (status IN ('running', 'complete', 'failed')),
    CHECK (
        (status = 'running' AND finished_at_ms IS NULL) OR
        (status <> 'running' AND finished_at_ms >= started_at_ms)
    )
);

CREATE TABLE IF NOT EXISTS source_roots (
    source_root_key VARCHAR PRIMARY KEY,
    unix_user VARCHAR NOT NULL,
    agent VARCHAR NOT NULL,
    source_kind VARCHAR NOT NULL,
    root_path VARCHAR NOT NULL,
    UNIQUE (unix_user, agent, source_kind, root_path)
);

CREATE TABLE IF NOT EXISTS source_items (
    source_item_key VARCHAR PRIMARY KEY,
    source_root_key VARCHAR NOT NULL REFERENCES source_roots(source_root_key),
    relative_path VARCHAR NOT NULL,
    adapter_version INTEGER NOT NULL CHECK (adapter_version > 0),
    device_id UBIGINT,
    inode UBIGINT,
    size_bytes UBIGINT NOT NULL,
    snapshot_mtime_ns BIGINT NOT NULL,
    imported_byte_count UBIGINT,
    prefix_sha256 BLOB CHECK (
        prefix_sha256 IS NULL OR octet_length(prefix_sha256) = 32
    ),
    last_run_key VARCHAR NOT NULL,
    status VARCHAR NOT NULL CHECK (status IN ('complete', 'deferred', 'rejected')),
    UNIQUE (source_root_key, relative_path),
    CHECK ((device_id IS NULL) = (inode IS NULL)),
    CHECK (imported_byte_count IS NULL OR imported_byte_count <= size_bytes)
);

CREATE TABLE IF NOT EXISTS sessions (
    session_key VARCHAR PRIMARY KEY,
    unix_user VARCHAR NOT NULL,
    agent VARCHAR NOT NULL,
    native_session_id VARCHAR NOT NULL,
    started_at_ms BIGINT,
    UNIQUE (unix_user, agent, native_session_id)
);

CREATE TABLE IF NOT EXISTS tool_calls (
    call_key VARCHAR PRIMARY KEY,
    session_key VARCHAR NOT NULL REFERENCES sessions(session_key),
    native_call_id VARCHAR,
    native_worker_id VARCHAR,
    called_at_ms BIGINT,
    provider VARCHAR,
    model VARCHAR,
    working_directory VARCHAR,
    tool_name VARCHAR NOT NULL,
    input_format VARCHAR NOT NULL CHECK (input_format IN ('json', 'text')),
    input_text VARCHAR NOT NULL,
    input_sha256 BLOB NOT NULL CHECK (octet_length(input_sha256) = 32)
);

CREATE TABLE IF NOT EXISTS tool_results (
    result_key VARCHAR PRIMARY KEY,
    call_key VARCHAR NOT NULL,
    returned_at_ms BIGINT,
    is_error BOOLEAN,
    output_text VARCHAR,
    output_json VARCHAR,
    result_sha256 BLOB NOT NULL CHECK (octet_length(result_sha256) = 32),
    UNIQUE (call_key, result_sha256),
    CHECK (output_text IS NOT NULL OR output_json IS NOT NULL)
);

CREATE TABLE IF NOT EXISTS observations (
    observation_key VARCHAR PRIMARY KEY,
    source_item_key VARCHAR NOT NULL,
    call_key VARCHAR REFERENCES tool_calls(call_key),
    result_key VARCHAR REFERENCES tool_results(result_key),
    record_kind VARCHAR NOT NULL CHECK (
        record_kind IN ('canonical', 'projection', 'validation')
    ),
    native_record_kind VARCHAR NOT NULL,
    sequence_number BIGINT,
    native_branch_id VARCHAR,
    is_current BOOLEAN,
    line_number UBIGINT,
    byte_offset UBIGINT,
    sqlite_rowid BIGINT,
    sqlite_blob_id VARCHAR,
    content_index UINTEGER,
    record_sha256 BLOB NOT NULL CHECK (octet_length(record_sha256) = 32),
    CHECK ((call_key IS NULL) <> (result_key IS NULL))
);

CREATE TABLE IF NOT EXISTS import_issues (
    issue_key VARCHAR PRIMARY KEY,
    run_key VARCHAR NOT NULL,
    source_item_key VARCHAR,
    severity VARCHAR NOT NULL CHECK (severity IN ('warning', 'error')),
    code VARCHAR NOT NULL,
    line_number UBIGINT,
    byte_offset UBIGINT,
    sqlite_blob_id VARCHAR,
    record_sha256 BLOB CHECK (
        record_sha256 IS NULL OR octet_length(record_sha256) = 32
    ),
    message VARCHAR NOT NULL,
    occurrence_count UBIGINT NOT NULL CHECK (occurrence_count > 0)
);

CREATE INDEX IF NOT EXISTS source_items_status_idx ON source_items(status);
CREATE INDEX IF NOT EXISTS tool_calls_session_order_idx
    ON tool_calls(session_key, called_at_ms);
CREATE INDEX IF NOT EXISTS tool_calls_native_id_idx ON tool_calls(native_call_id);
CREATE INDEX IF NOT EXISTS tool_calls_tool_name_idx ON tool_calls(tool_name);
CREATE INDEX IF NOT EXISTS tool_results_call_idx ON tool_results(call_key);
CREATE INDEX IF NOT EXISTS tool_results_error_idx ON tool_results(is_error);
CREATE INDEX IF NOT EXISTS observations_call_idx ON observations(call_key);
CREATE INDEX IF NOT EXISTS observations_result_idx ON observations(result_key);
CREATE INDEX IF NOT EXISTS import_issues_code_idx ON import_issues(severity, code);

CREATE OR REPLACE VIEW tool_call_conflicts AS
SELECT call_key, count(*) AS result_count
FROM tool_results
GROUP BY call_key
HAVING count(*) > 1;

CREATE OR REPLACE VIEW tool_calls_flat AS
SELECT
    c.*,
    r.result_key,
    r.returned_at_ms,
    r.is_error,
    r.output_text,
    r.output_json,
    (SELECT count(*) FROM observations o WHERE o.call_key = c.call_key) AS call_observations,
    (SELECT count(*) FROM observations o WHERE o.result_key = r.result_key) AS result_observations
FROM tool_calls c
LEFT JOIN tool_results r ON r.call_key = c.call_key
WHERE (SELECT count(*) FROM tool_results variants WHERE variants.call_key = c.call_key) <= 1;
