PRAGMA foreign_keys = ON;
PRAGMA user_version = 1;

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL,
    account TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    started_at_ms INTEGER,
    UNIQUE (agent, account, source_session_id)
);

CREATE TABLE tool_calls (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    source_call_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    working_directory TEXT,
    started_at_ms INTEGER NOT NULL,
    requires_streams INTEGER NOT NULL CHECK (requires_streams IN (0, 1)),
    finished_at_ms INTEGER,
    status TEXT NOT NULL CHECK (status IN ('started', 'finished')),
    is_error INTEGER CHECK (is_error IN (0, 1)),
    executed INTEGER CHECK (executed IN (0, 1)),
    UNIQUE (session_id, source_call_id)
);

CREATE TABLE payloads (
    sha256 BLOB PRIMARY KEY CHECK (length(sha256) = 32),
    compression TEXT NOT NULL CHECK (compression IN ('none', 'zstd')),
    uncompressed_byte_length INTEGER NOT NULL CHECK (uncompressed_byte_length >= 0),
    body BLOB NOT NULL
);

CREATE TABLE snapshots (
    tool_call_id INTEGER NOT NULL REFERENCES tool_calls(id) ON DELETE CASCADE,
    subject TEXT NOT NULL CHECK (subject IN ('input', 'result', 'source_output', 'stdout', 'stderr')),
    stage TEXT NOT NULL CHECK (stage IN ('before', 'after')),
    media_type TEXT NOT NULL,
    captured_at_ms INTEGER NOT NULL,
    payload_sha256 BLOB NOT NULL REFERENCES payloads(sha256),
    PRIMARY KEY (tool_call_id, subject, stage)
);

INSERT INTO sessions VALUES (1, 'pi', 'test', 'old-session', 1);
INSERT INTO tool_calls VALUES (
    1, 1, 'old-call', 'read', NULL, NULL, '/tmp', 2, 0,
    NULL, 'started', NULL, NULL
);
