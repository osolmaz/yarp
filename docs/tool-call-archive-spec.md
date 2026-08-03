# Tool-call archive specification

This specification defines YARP's local tool-call archive. The archive is one SQLite database at:

```text
~/.local/share/yarp/tool-calls.sqlite3
```

The database stores each tool call, its input before and after YARP processing, and its result before and after YARP processing. Shell calls may also store stdout and stderr as exact byte streams.

## Database

The archive uses SQLite format 3. YARP sets `PRAGMA user_version` to the schema version. Version 1 uses:

```sql
PRAGMA user_version = 1;

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
    subject        TEXT NOT NULL CHECK (
        subject IN (
            'input', 'result', 'result_text', 'source_output', 'stdout', 'stderr'
        )
    ),
    stage          TEXT NOT NULL CHECK (stage IN ('before', 'after')),
    media_type     TEXT NOT NULL,
    source_completeness TEXT CHECK (
        source_completeness IN ('complete', 'incomplete', 'unknown')
    ),
    captured_at_ms INTEGER NOT NULL,
    payload_sha256 BLOB NOT NULL REFERENCES payloads(sha256),
    PRIMARY KEY (tool_call_id, subject, stage),
    CHECK (
        (
            subject = 'result_text'
            AND stage = 'before'
            AND source_completeness IS NOT NULL
        )
        OR (
            subject <> 'result_text'
            AND source_completeness IS NULL
        )
    )
);

CREATE INDEX tool_calls_started_at_idx ON tool_calls(started_at_ms);
CREATE INDEX tool_calls_tool_name_idx ON tool_calls(tool_name);
CREATE INDEX snapshots_payload_idx ON snapshots(payload_sha256);
```

SQLite may create `tool-calls.sqlite3-wal` and `tool-calls.sqlite3-shm` while the archive is open. These are parts of the same logical database. A clean shutdown checkpoints committed data into `tool-calls.sqlite3`.

## Records

A session identifies one agent conversation. `agent` names the source application, starting with `pi`. `account` names the local account that owns the source data, such as `onur` or `bob`. `source_session_id` is the session identifier assigned by that agent.

A tool call identifies one invocation. `source_call_id` is the identifier assigned by the source agent. The pair `(session_id, source_call_id)` must be unique. `archive_ref` is an immutable model-facing locator in the form `yr_` plus 32 lowercase hexadecimal digits. It is generated from 128 random bits, is not reused, and is not an authorization token. `provider` and `model` record the model that emitted the call when the source exposes those values. `requires_streams` records that YARP wrapped the shell command and must capture all four stream snapshots if the call executes. `executed` distinguishes calls that reached a tool and therefore require a pre-YARP result from preflight failures that never ran.

A snapshot points to immutable bytes in `payloads`. Its `subject` says what was captured. Its `stage` says whether capture happened before or after YARP processing.

| Subject | Before | After |
| --- | --- | --- |
| `input` | Tool arguments received by YARP. | Arguments after YARP rewrites them. |
| `result` | Tool result received before YARP changes it. | Tool result returned to Pi. |
| `result_text` | Exact ordered concatenation of host text blocks saved when a typed summary needs host text or the global text cap wins. | Not used. |
| `source_output` | Exact complete output read from Pi's built-in Bash tool `fullOutputPath`. | Not used. |
| `stdout` | Exact child stdout before pruning. | Exact stdout emitted after pruning. |
| `stderr` | Exact child stderr before pruning. | Exact stderr emitted after pruning. |

Every tool call must have `input/before` and `input/after` snapshots. Every finalized call must have `result/after`. An executed call must also have `result/before`. Shell stream snapshots are additionally required when YARP executes and prunes the child process itself. Other tools may omit stream snapshots.

When YARP makes no change, the before and after snapshots point to the same payload. The archive does not duplicate the bytes.

The word `before` means before YARP processing. A source tool may already have applied its own limits before its `tool_result` event. When Pi's built-in Bash result exposes `fullOutputPath`, YARP stores the file's exact bytes as `source_output/before` in the same transaction as `result/before`. YARP ignores that field on other tools rather than treating their result metadata as a trusted local path. Otherwise, `result/before` contains the exact result exposed by the source tool.

## Payload encoding

Tool inputs and structured results use RFC 8785 canonical JSON encoded as UTF-8. Their media type is `application/json`.

Result text, source output, stdout, and stderr keep their exact bytes. Result text is the ordered concatenation of the original text blocks without inserted separators. It is always valid UTF-8 and records whether the host proved it complete, reported it truncated, or exposed unknown completeness. Other valid UTF-8 text uses `text/plain; charset=utf-8`; non-text output uses `application/octet-stream`. YARP must not normalize line endings or remove terminal control bytes before hashing a stream payload.

Wrapped `stdout/before` and `stderr/before` remain the first recovery sources even when a capped summary also has a `result_text/before` fallback. For other calls, `yarp search` and `yarp read` select `result_text/before` as the exact pre-cap visible text when it exists. Its completeness comes from the host text metadata, not from a separate full-output path. `source_output/before` is used last. This keeps capped host text searchable when a complete Bash source is binary without hiding wrapped raw streams.

`sha256` is the SHA-256 digest of the uncompressed bytes. `uncompressed_byte_length` is the length of those bytes.

YARP compresses a payload with Zstandard level 3 when the compressed body is at least 5% smaller. Otherwise it stores the original bytes with `compression = 'none'`. Readers must decompress the body before checking its length and SHA-256 digest.

## Write lifecycle

YARP observes `tool_execution_start` to retain arguments in memory for calls rejected before `tool_call`. At `tool_call`, YARP writes the session, the call with `status = 'started'`, and both input snapshots in one transaction. The transaction must commit before tool execution begins. A call rejected before `tool_call` never executes, so YARP writes its unchanged input snapshots with its final preflight result at `tool_execution_end`.

The shell runner writes its before and after stream snapshots in one transaction before it returns. The `tool_result` hook writes `result/before`, then invokes the bounded typed reducer only for one safe shell text result. If that summary wins and no complete `source_output` exists, it commits the exact host text as `result_text/before` before returning the summary.

After typed processing, the hook measures the UTF-8 bytes across every remaining text block. An oversized typed summary is shortened against exact raw streams, `source_output`, or `result_text` already committed for that summary. Before capping a wrapped summary, the hook also commits its exact visible text as a fallback; source selection still keeps the original `stdout` and `stderr` first. Other text over the configured global budget is shortened only after its exact ordered concatenation commits as `result_text/before`. The visible result keeps UTF-8-safe beginning and ending content plus a bounded recovery marker. Image blocks remain unchanged and are excluded from the text-byte budget. Archive failure leaves the pre-cap result visible.

The hook then stages a provisional `result/after` and `is_error` while it can still restore raw output on failure. The call remains `started`, so a crash or later reconciliation failure is visible as an incomplete call. After all result hooks run, `tool_execution_end` replaces the provisional result and atomically writes its final metadata with `status = 'finished'`. If this final transaction fails for a wrapped shell call, YARP restores its raw streams and replaces the public tool-result message at `message_end`. Completion verifies that `result/before` exists and that executed wrapped shell calls have all four stream snapshots.

Pi can reject or block a call before tool execution without emitting `tool_result`, and validation can reject it before `tool_call`. For those preflight failures, `tool_execution_end` records the error result and finishes the call without requiring `result/before`. The archive does not infer a rejection or cancellation category from result text.

An interrupted process can leave a call in `started`. Readers must treat this as an incomplete call. A missing finish time does not describe a completed call with empty output.

Parallel tool calls may finish in any order. Writers use the source call ID for correlation and must not depend on event order.

## SQLite settings

Writers use these settings:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA auto_vacuum = INCREMENTAL;
```

Schema creation and migration run inside `BEGIN EXCLUSIVE`. Normal writes use short transactions. Writers retry `SQLITE_BUSY` within the configured busy timeout and must not retry malformed data or constraint failures.

## Filesystem permissions

YARP creates `~/.local/share/yarp` with mode `0700` and the database with mode `0600` on POSIX systems. YARP narrows broader permissions on that known default directory and its database before opening them. An explicit override must already use a private directory; YARP never changes an existing override directory. Failure to establish private permissions is an error.

The archive can contain commands, source code, file contents, environment-derived values, and secrets printed by tools. YARP never uploads, syncs, or serves this database.

## Failure behavior

YARP must never hide an archive failure.

If the initial tool-call transaction cannot commit after one writer restart and bounded retry, YARP blocks the call. This preserves the guarantee that an executed call has an archived input.

If a result transaction fails after the tool has executed, YARP leaves the call in `started` and reports the archive failure clearly. Non-shell tools keep the unchanged result already exposed by Pi. Wrapped shell tools restore the committed raw streams and return them instead of the pruned result. If the runner's raw spool fails after the child starts, it drains the child, emits the exact raw bytes on their original streams, reports the archive error, and preserves the child status.

A process crash, full disk, invalid database, unsupported schema version, failed integrity check, or permission error is fatal to capture. YARP must not create a second database or fall back to per-call files.

## Validation

`yarp archive verify` checks:

- SQLite integrity and foreign keys.
- The supported `user_version`.
- Allowed lifecycle and snapshot values.
- Unique well-formed archive references and valid result-text completeness.
- Payload decompression, byte length, and SHA-256 digest.
- Filesystem permissions.
- Incomplete calls.
- Missing required snapshots.
- Unreferenced payloads.

The command exits nonzero for corruption, unsupported versions, bad permissions, invalid payloads, or missing required snapshots. Incomplete calls are reported separately so an interrupted process does not look like database corruption.

## Retention

Version 1 does not delete calls automatically. YARP reports database size through `yarp archive stats`. An explicit prune command may delete finished calls in age order, remove payloads that no remaining snapshot references, and run incremental vacuuming.

YARP must not delete incomplete calls or the existing `~/.local/share/yarp/tee` directory. The old tee files are outside this format.

## Migrations

YARP accepts databases with `user_version = 1`. A newer binary may migrate an older supported version inside one exclusive transaction after creating a SQLite backup. It must reject a database with a newer version.

Migrations update the existing contract in place. The indexed-output migration keeps `user_version = 1`, creates and verifies a private pre-migration SQLite backup, backfills stable references, rebuilds the constrained call and snapshot tables, and verifies row counts, foreign keys, references, and integrity before commit. YARP does not create a parallel database version or keep fallback readers for superseded schemas.

## Boundaries

Version 1 records final tool calls and results. It does not record provider request bodies, model reasoning, user prompts, streaming tool updates, credentials from agent configuration, or shell input sent after process start unless that input is itself a tool call.

The SQLite database is the source of truth. Explicit exports may use JSONL or Parquet. DuckDB may query those exports or a read-only database copy. YARP does not write any of them alongside normal capture.
