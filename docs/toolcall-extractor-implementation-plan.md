# `toolcall-extractor` implementation plan

`toolcall-extractor` will be an offline command in the YARP repository. It will read existing Pi, Codex, Claude Code, and Cursor session stores and write normalized tool calls and results to one local DuckDB database.

The extractor will never run from the Pi extension or the `yarp` pruning command. It will not intercept new calls, change an agent session, or add background logging. The earlier live archive design is removed.

## Outcome

The first release will create a queryable local dataset from source files that already exist on this machine. It must:

- Preserve tool inputs, rendered results, useful structured result details, native IDs, and source locations.
- Keep distinct result variants when source records disagree.
- Resume after interruption without duplicating calls.
- Read live JSONL and SQLite/WAL sources without writing to them.
- Require an explicit Unix-user label for every import.
- Keep directories at mode `0700` and files at mode `0600`.
- Avoid network access, agent credentials, configuration files, shell history, debug logs, and unrelated blobs.
- Stop before files under its data directory reach 10,000,000,000 bytes.

The default database path will be:

```text
~/.local/share/toolcall-extractor/toolcalls.duckdb
```

The repository will contain synthetic fixtures only. Real session data, generated reports, and DuckDB files stay outside Git.

## Repository layout

The extractor will be a separate Rust package so its DuckDB and SQLite dependencies, along with its JSON and protobuf parsers, do not become dependencies of `yarp`.

```text
yarp/
├── src/                         # Existing YARP command
├── hooks/pi/                    # Existing Pi extension
├── toolcall-extractor/
│   ├── Cargo.toml
│   ├── src/
│   │   ├── adapters/
│   │   │   ├── pi.rs
│   │   │   ├── codex.rs
│   │   │   ├── claude.rs
│   │   │   └── cursor.rs
│   │   ├── database.rs
│   │   ├── model.rs
│   │   ├── snapshot.rs
│   │   ├── verify.rs
│   │   └── main.rs
│   └── tests/fixtures/          # Small synthetic and sanitized fixtures
└── docs/
    └── toolcall-extractor-implementation-plan.md
```

The root manifest will become a Cargo workspace containing `yarp-cli` and `toolcall-extractor`. Each package will keep its own dependency boundary and binary.

## Command line

Each extraction command handles one agent and one explicitly named Unix user. Source paths are required. This prevents an accidental scan of every home directory.

```text
toolcall-extractor extract pi \
  --unix-user onur \
  --sessions /home/onur/.pi/agent/sessions

toolcall-extractor extract codex \
  --unix-user onur \
  --sessions /home/onur/.codex/sessions \
  --state-db /home/onur/.codex/state_5.sqlite

toolcall-extractor extract claude \
  --unix-user onur \
  --projects /home/onur/.claude/projects

toolcall-extractor extract cursor \
  --unix-user onur \
  --chats /home/onur/.cursor/chats \
  --acp-sessions /home/onur/.cursor/acp-sessions \
  --projects /home/onur/.cursor/projects
```

All commands accept `--database`. The remaining commands operate on an existing database:

```text
toolcall-extractor stats
toolcall-extractor issues
toolcall-extractor verify
```

`stats` and `issues` print counts and source locations. They never print tool inputs or results. `verify` checks the database schema, key relationships, source checkpoints, result conflicts, and file permissions.

Bob's data is a separate import phase. The implementation must not grant Onur access to Bob's session directories or run the whole writer as root. After explicit approval, a narrow reader can run as Bob and send normalized framed records directly to an Onur-owned writer over a pipe. That path must not create an intermediate export.

## Source rules

The extractor will use source-specific adapters because the four agents do not share one storage format.

| Agent | Tool-call source | Supporting source |
| --- | --- | --- |
| Pi | Version 3 session tree JSONL | Session header and assistant metadata in the same file |
| Codex | Rollout JSONL | `state_5.sqlite` for enumeration and thread metadata |
| Claude Code | Project and subagent JSONL | Explicitly referenced `tool-results/` files |
| Cursor | Chat and ACP `store.db` blobs | Latest protobuf root and native chat transcript JSONL |

An adapter must reject an unknown source version or schema. It records a bounded issue and continues with other source items. It must never guess at an unknown record shape.

The extractor excludes user prompts, assistant prose, reasoning, provider request bodies, agent settings, authentication files, shell snapshots, generic command histories, task state, debug output, worker logs, and Cursor blobs that do not pass the strict tool-call or tool-result shape checks.

Tool inputs and outputs can themselves contain secrets printed or read by a tool. The extractor preserves those values because removing them would change the record. The database remains private and local, and summary commands never print content.

## Draft DuckDB schema

Schema version 1 will use the tables below. JSON values are validated and stored as UTF-8 text so reading the database does not depend on DuckDB extension downloads. Hash columns contain SHA-256 bytes.

```sql
CREATE TABLE schema_info (
    version INTEGER PRIMARY KEY CHECK (version > 0)
);

CREATE TABLE import_runs (
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

CREATE TABLE source_roots (
    source_root_key VARCHAR PRIMARY KEY,
    unix_user VARCHAR NOT NULL,
    agent VARCHAR NOT NULL,
    source_kind VARCHAR NOT NULL,
    root_path VARCHAR NOT NULL,
    UNIQUE (unix_user, agent, source_kind, root_path)
);

CREATE TABLE source_items (
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
    last_run_key VARCHAR NOT NULL REFERENCES import_runs(run_key),
    status VARCHAR NOT NULL CHECK (status IN ('complete', 'deferred', 'rejected')),
    UNIQUE (source_root_key, relative_path),
    CHECK ((device_id IS NULL) = (inode IS NULL)),
    CHECK (imported_byte_count IS NULL OR imported_byte_count <= size_bytes)
);

CREATE TABLE sessions (
    session_key VARCHAR PRIMARY KEY,
    unix_user VARCHAR NOT NULL,
    agent VARCHAR NOT NULL,
    native_session_id VARCHAR NOT NULL,
    started_at_ms BIGINT,
    UNIQUE (unix_user, agent, native_session_id)
);

CREATE TABLE tool_calls (
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

CREATE TABLE tool_results (
    result_key VARCHAR PRIMARY KEY,
    call_key VARCHAR NOT NULL REFERENCES tool_calls(call_key),
    returned_at_ms BIGINT,
    is_error BOOLEAN,
    output_text VARCHAR,
    output_json VARCHAR,
    result_sha256 BLOB NOT NULL CHECK (octet_length(result_sha256) = 32),
    UNIQUE (call_key, result_sha256),
    CHECK (output_text IS NOT NULL OR output_json IS NOT NULL)
);

CREATE TABLE observations (
    observation_key VARCHAR PRIMARY KEY,
    source_item_key VARCHAR NOT NULL REFERENCES source_items(source_item_key),
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

CREATE TABLE import_issues (
    issue_key VARCHAR PRIMARY KEY,
    run_key VARCHAR NOT NULL REFERENCES import_runs(run_key),
    source_item_key VARCHAR REFERENCES source_items(source_item_key),
    severity VARCHAR NOT NULL CHECK (severity IN ('warning', 'error')),
    code VARCHAR NOT NULL,
    line_number UBIGINT,
    byte_offset UBIGINT,
    sqlite_blob_id VARCHAR,
    record_sha256 BLOB CHECK (
        record_sha256 IS NULL OR octet_length(record_sha256) = 32
    ),
    message VARCHAR NOT NULL,
    occurrence_count UBIGINT NOT NULL
);
```

`input_text` contains canonical JSON when `input_format = 'json'` and exact source text otherwise. `output_text` holds rendered output. `output_json` holds allowlisted structured output, such as Pi result details, Claude's `toolUseResult`, or Cursor's high-level result. An adapter removes a rendered-text copy from `output_json` when the same bytes already appear in `output_text`.

`source_kind` identifies roots with different roles under one agent, including Cursor chats, Cursor ACP sessions, Cursor project transcripts, Codex rollouts, and Codex state. `adapter_version` records the parser contract used for the current checkpoint. A newer adapter invalidates an older checkpoint and reparses that source item.

Source order, branch identity, native record type, and Cursor current-state membership belong to observations because different source projections of one call can disagree. `is_current` is true for a record reachable from the latest Cursor root, false for retained historical state, and null when the source does not define reachability.

The implementation will add indexes for session order, native IDs, tool names, call times, and errors after measuring the pilot queries. Unresolved issues also get an index. Two views cover common queries. `tool_calls_flat` exposes calls with zero or one distinct result, while `tool_call_conflicts` exposes calls with more than one result variant. The views derive observation counts and never choose one conflicting result automatically.

## Identity and pairing

Native tool-call IDs are provenance. They are not universal primary keys. Pi has reused a tool-call ID within one session, Codex has duplicate projections, and copied histories can repeat a call in another file.

Each adapter will generate `call_key` from the strongest stable source identity it has:

- Pi uses the session entry ID, content index, and native tool-call ID.
- Codex uses the canonical thread, native call ID, and response-item identity. Event projections map to that call.
- Claude Code uses the session, message UUID, content index, and tool-use ID. Main and sidechain projections, including subagents, can map to one call.
- Cursor uses the session, tool-call ID, message or blob identity, and normalized call fingerprint. Transcript and SQLite observations map to one call.

Reimporting the same source occurrence is idempotent. Distinct projections add observations, and the query views derive their counts. Distinct results for one call remain separate `tool_results` rows. Calls without a result and results without a defensible call match remain visible as issues.

Pairing uses native IDs only within the adapter's source context. Time or nearest-row heuristics may rank candidates for an issue report, but they cannot create a pair.

## Stable reads

JSONL adapters will work in byte offsets. Before reading, they record the file identity and size plus its nanosecond modification time. They read only complete lines ending at or before the starting size. A trailing partial line is deferred. After reading, they check the file identity and size again. The committed `imported_byte_count` is the number of verified prefix bytes, not a row count or SQLite cursor.

An append during extraction is safe because the new suffix waits for the next run. A truncation, replacement, or in-place change rejects the checkpoint. The next run restarts that source item from a verified prefix. Interior malformed records create issues and parsing continues at the next physical line. The extractor never repairs or rewrites source JSONL.

SQLite adapters open databases with `mode=ro`, set `PRAGMA query_only = ON`, and read inside one transaction. They must keep WAL visibility, so they cannot use `immutable=1` when a WAL exists. Queries name each accepted table and column. The extractor never uses `SELECT *` against agent databases.

The extractor accepts regular files beneath an explicitly supplied root. It rejects path escapes and symlinks that resolve outside that root. It does not change source permissions, timestamps, or contents.

## Adapter work

### Pi

The Pi adapter will parse version 3 tree JSONL. It pairs assistant `toolCall.id` values with `toolResult.toolCallId`, keeps entry IDs and parent IDs, and records provider and model values from the assistant message that emitted the call. It scans every branch. Compaction payloads are metadata and are not recursively parsed as new calls.

Malformed physical lines are recorded and skipped. A result without a call and a call without a result remain in the dataset as issues. Structured `details` are preserved without duplicating rendered text.

This work reads Pi's existing state. It changes no Pi session entry, persistent schema, setting, public API, internal API, or runtime behavior.

### Codex

The Codex adapter will open `state_5.sqlite` for thread enumeration and metadata, then parse each referenced rollout JSONL. It will also scan the sessions root for rollouts missing from SQLite. Rollout JSONL remains the source for calls and results.

The first `session_meta` record identifies the rollout. Later session metadata can be copied history and does not create a second owner for the file. The adapter handles function calls, custom tool calls, tool-search records, and their outputs. Command and patch completion records are projections, as are MCP and web-search completion records. They enrich or recover a call after native-ID deduplication.

Malformed lines are reported and skipped. Logs and goals are outside this adapter. It also excludes memories, diagnostics, and authentication state.

### Claude Code

The Claude adapter will scan main transcripts and sidechains, including `subagents/*.jsonl`. It pairs `tool_use.id` with `tool_result.tool_use_id` and keeps message UUID, parent UUID, session ID, and agent ID as provenance.

The top-level `toolUseResult` object is preserved as structured details when it adds information beyond rendered content. Identical duplicate result rows collapse into one result with multiple observations. Referenced `tool-results/` files are read only when a parsed result names the exact file. Other files under `~/.claude` are outside the source root.

### Cursor

The Cursor adapter will open each chat and ACP `store.db` in a read-only transaction. It will accept JSON blobs only when the full message and content-block shape matches a Cursor tool call or result. Generic JSON blobs are ignored.

A streaming allowlist reader will take the session ID, latest root blob ID, and timestamps from metadata. It also accepts the mode and subagent relation. It will skip encryption material and unknown fields without retaining or logging their values.

The adapter will decode the latest `agent.v1.ConversationStateStructure` using a checked-in protobuf definition tied to the supported Cursor storage revision. Observations reachable from that root get `is_current = true`. Validated history absent from the root gets `is_current = false`. A missing root or unsupported protobuf version leaves `is_current` null. Otherwise valid call/result pairs remain available.

Native project transcript JSONL supplies call order and validation for chat sessions. It does not create extra calls because it omits call IDs and results. Transcript call sequences must match the SQLite calls before their order is accepted. ACP order comes from the decoded conversation graph, never SQLite row order alone.

## Size and privacy controls

The extractor creates its data directory with mode `0700` and database files with mode `0600`. It starts with `umask 077` and repairs broader permissions only on files it owns inside its own data directory.

Each write batch checks the current database, WAL, temporary files, and a conservative upper bound for the next batch. The transaction is refused if those files could cross 10,000,000,000 bytes. No source data is deleted or changed when this happens.

DuckDB external access and automatic extension installation stay disabled. The extractor has no network code. It stores no raw transcript rows, provider payloads, Cursor protobuf blobs, or malformed records. Provenance consists of hashes and bounded source locations.

## Implementation stages

### Package and database

Create the workspace package, command parser, and private data-directory handling. Then add schema creation, migrations, `stats`, and `verify`. Add deterministic key and canonical-JSON helpers. Finish the Schemator review and manual product pass before freezing version 1.

### JSONL adapters

Implement the shared stable JSONL reader, followed by the Pi and Codex adapters. Add Claude Code after the shared pairing behavior is tested. This stage includes malformed-line handling, checkpoints, source observations, native-ID pairing, and conflict retention.

### Cursor adapter

Implement read-only SQLite snapshots, strict blob classification, protobuf graph traversal, transcript validation, current-versus-historical tags, and ACP subagent handling. Unknown protobuf data must fail at the source-item boundary without falling back to row-order guesses.

### Incremental imports

Add restart tests, append-only resume, source replacement detection, idempotent re-import, duplicate projection handling, and bounded issue aggregation. Add the approved Bob reader/writer pipe only after the Onur import path passes verification.

### Pilot and full import

Run an Onur-only pilot with a bounded sample from all four agents. Report source bytes read, calls, distinct results, incomplete pairs, conflicts, malformed records, elapsed time, peak temporary space, and DuckDB bytes. Reports contain counts and locations only.

After the pilot passes, run the full Onur import and compare adapter counts with the audit baseline. A full Bob import requires explicit approval for the `unix_user = 'bob'` label and cross-user transfer. The final run must verify permissions, database relationships, source coverage, and the 10 GB limit.

## Tests

Committed tests use synthetic or manually sanitized data. They cover:

- Each supported call and result shape.
- Branching, sidechains, rewinds, subagents, and copied histories.
- Duplicate projections and conflicting results.
- Malformed interior records and truncated final lines.
- JSONL appends and source replacement or truncation during extraction.
- SQLite WAL visibility and concurrent source writes.
- Cursor protobuf roots and missing roots, including unknown fields.
- Interrupted imports and idempotent resume.
- Permission failures and path escapes, including the 10 GB stop.
- Proof that excluded records never enter DuckDB.

Property tests will exercise framing, source-location decoding, canonical JSON, and deterministic keys. Fuzz targets are useful for JSONL record dispatch and Cursor blob classification, but they do not run as part of normal completion checks.

The workspace keeps at least 85% Rust line coverage. Normal completion runs every command in `AGENTS.md`, including formatting, Clippy, tests, and coverage. Audits and Slophammer run before `git diff --check`.

## Completion criteria

The work is complete when:

- The four adapters import their audited source formats without changing source state.
- A second unchanged run inserts no new calls or results.
- Interrupted imports resume from verified checkpoints.
- Native IDs, branches, subagents, Cursor current-state membership, and source locations remain queryable.
- Every result is paired by source evidence or left visibly unmatched.
- Conflicting results remain distinct.
- Excluded files and records are absent.
- The three summary and verification commands never print tool content.
- All extractor files remain private and below the size limit.
- YARP pruning and its Pi extension still keep no command history.
- Repository checks pass with synthetic fixtures.

If an agent format cannot be identified, a stable snapshot cannot be obtained, or pairing requires a guess, the extractor records the blocker and stops that source item. It continues only after the adapter has defensible evidence for the next step.
