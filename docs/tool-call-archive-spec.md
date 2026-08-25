# Tool-call archive specification

This specification defines YARP's local tool-call archive and its local writer protocol.

The archive is one SQLite database:

```text
~/.local/share/yarp/tool-calls.sqlite3
```

The database stores each tool call, its input before and after YARP processing, and its result before and after YARP processing. Shell calls may also store stdout and stderr as exact byte streams.

One on-demand YARP broker owns the only normal writable database connection. YARP adapters and command wrappers send agent-neutral archive operations to this broker. Read-only commands may open the database directly in read-only mode.

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

SQLite may create `tool-calls.sqlite3-wal` and `tool-calls.sqlite3-shm` while the archive is open. These files are parts of the same logical database.

## Records

A session identifies one agent conversation. `agent` names the source application. `account` names the local account that owns the source data. `source_session_id` is the session identifier assigned by that agent.

A tool call identifies one invocation. `source_call_id` is the identifier assigned by the source agent. The pair `(session_id, source_call_id)` is unique. `archive_ref` is an immutable model-facing locator in the form `yr_` plus 32 lowercase hexadecimal digits. It comes from 128 random bits, is not reused, and is not an authorization token. `provider` and `model` record the model that emitted the call when the source exposes those values. `requires_streams` records that YARP wrapped the shell command and must capture all four stream snapshots if the call executes. `executed` distinguishes calls that reached a tool from preflight failures that never ran.

A snapshot points to immutable bytes in `payloads`. Its `subject` says what was captured. Its `stage` says whether capture happened before or after YARP processing.

| Subject | Before | After |
| --- | --- | --- |
| `input` | Tool arguments received by YARP. | Arguments after YARP rewrites them. |
| `result` | Tool result received before YARP changes it. | Tool result returned to the agent. |
| `result_text` | Exact ordered host text saved when a typed summary or global cap needs a recovery source. | Not used. |
| `source_output` | Exact complete output read from an approved source output file. | Not used. |
| `stdout` | Exact child stdout before pruning. | Exact stdout emitted after pruning. |
| `stderr` | Exact child stderr before pruning. | Exact stderr emitted after pruning. |

Every tool call has `input/before` and `input/after` snapshots. Every finalized call has `result/after`. An executed call also has `result/before`. A wrapped shell call that executes has all four stdout and stderr snapshots. Other tools may omit stream snapshots.

When YARP makes no change, the before and after snapshots point to the same payload. The archive does not duplicate the bytes.

The word `before` means before YARP processing. A source tool may already have applied its own limits. For an approved source that exposes a complete output path, YARP stores that file's exact bytes as `source_output/before` in the same operation as `result/before`. YARP ignores path-shaped metadata from unapproved sources.

## Payload encoding

Tool inputs and structured results use RFC 8785 canonical JSON encoded as UTF-8. Their media type is `application/json`.

Result text, source output, stdout, and stderr keep their exact bytes. Result text is the ordered concatenation of original text blocks without inserted separators. It is valid UTF-8 and records whether the host proved it complete, reported it incomplete, or did not know. Other valid UTF-8 text uses `text/plain; charset=utf-8`. Other output uses `application/octet-stream`. YARP does not normalize line endings or remove terminal control bytes before hashing stream data.

Wrapped `stdout/before` and `stderr/before` are the first recovery sources. Other calls use `result_text/before` when it exists. `source_output/before` is the last recovery source.

`sha256` is the SHA-256 digest of the uncompressed bytes. `uncompressed_byte_length` is their byte length. YARP uses Zstandard level 3 only when compression saves enough space to justify its overhead. Content addressing lets identical snapshots share one payload row.

## Capture lifecycle

An adapter sends generic archive operations. Agent-specific event translation stays in the adapter. The broker does not know Pi event names, commands, sessions, or user-interface behavior.

Initial capture commits `input/before`, `input/after`, and the call record before tool execution. A tool does not execute until the adapter receives the durable acknowledgement.

The shell runner captures exact stdout and stderr before it returns output. It sends a generic stream-capture operation and waits for the durable acknowledgement before it emits output and exits. This ordering prevents call finalization from passing stream capture.

Result capture commits the exact pre-YARP result before a typed summary or global cap can remove text. YARP stages the visible result while raw restoration is still possible. Finalization writes the final result and marks the call `finished` in one operation.

A preflight failure that never executes a tool may finish without `result/before`. An interrupted call can remain `started`. Readers treat it as incomplete.

Calls from different sources may finish in any order. Operations for one source preserve their declared sequence.

## Local broker

### Process roles

`yarp archive broker` is an ordinary on-demand YARP process. It owns the only normal writable SQLite connection for one canonical archive path. It is not an installed system or user service.

`yarp archive ingest` keeps the existing adapter-facing frame contract. It becomes a bridge to the broker and never opens SQLite for writing.

`yarp run` sends generic stream-capture operations to the broker. Writable maintenance operations, including prune, also use the broker. Stats, verify, search, read, and restore use read-only connections when they do not change state.

The broker opens the archive, applies any supported in-place migration, and verifies the writable database while it owns the startup lock. A normal client never falls back to direct writing.

### Protocol

The socket protocol uses the schema identifier `yarp.archive-broker.v1`. The adapter-facing ingest protocol keeps `schemaVersion: 1`. The two version fields have separate meanings.

A broker handshake includes the protocol schema, YARP binary version, canonical archive identity, and a connection correlation ID. A request envelope contains:

- a bounded correlation ID for the connection;
- a stable generic operation identity;
- a generic source ordering key and sequence;
- a bounded relative commit deadline;
- one supported archive operation and its bounded data.

Supported operations cover begin call, result before, result text, staged result, final result, call finish, shell stream capture, passthrough stream capture, and prune. Unknown fields, operations, schemas, identities, lengths, or sequences fail validation.

The stable operation identity comes from the source identity, source call ID, and operation kind. The ordering key is a SHA-256 digest of the source identity, so raw source IDs are not repeated in broker routing metadata. The connection correlation ID is not durable identity.

Frames are bounded before allocation. Errors are bounded and do not include archived payloads, credentials, or private command data.

### Private local IPC

The broker uses a Unix-domain socket on supported Unix systems. It opens no TCP port and makes no network request.

YARP uses a private directory under a valid `XDG_RUNTIME_DIR`. When that variable is absent, it uses a private user-ID-specific directory under the local temporary directory. The socket name contains a hash of the canonical archive path so each archive has one broker and the path stays within platform socket limits. The runtime directory uses mode `0700`, and the socket uses mode `0600`.

YARP rejects symlinks, paths owned by another user, broad permissions, wrong file types, and unsafe stale entries. The private runtime directory and socket permissions admit only the owning local user. Linux also checks the peer user ID. Platforms without an equivalent secure local IPC boundary are not supported by this broker contract.

The socket path, process arguments, logs, and errors do not contain request payloads or credentials.

### Startup and shutdown

A client first tries to connect. If no broker answers, the client takes the startup lock and checks again. The broker holds a separate lifetime lock while it owns the writable archive. The lock winner removes a stale socket only when no process holds that lifetime lock and after owner, type, and permission checks. It starts the same YARP binary as `archive broker` and waits at most 5 seconds for a versioned ready handshake. Other clients connect to the winner.

The broker survives the exit of the client that started it. It stays alive while any client is connected, a request is admitted, or a transaction is open.

The idle grace is 60 seconds. Idle means no clients, an empty queue, and no open transaction. To exit, the broker takes the startup lock, stops accepting clients, removes the socket safely, drains admitted work, closes the archive, and exits. A client that meets this shutdown retries the normal startup path.

### Admission, order, and backpressure

Each client may have one admitted request. The broker admits at most 256 requests in total. The global admitted body budget is `MAX_FRAME_BYTES`, which lets one maximum-size request make progress without admitting many such requests.

A full queue stops reading from affected sockets. The broker does not drop or acknowledge the request. The original request deadline remains in force.

The scheduler serves one head request from each client in round-robin order. It preserves each generic source's sequence and rejects a sequence regression or incompatible concurrent use.

### Prepared data and stream files

The broker validates JSON, creates canonical bytes, computes hashes, compresses payloads, and snapshots approved files before it takes the SQLite write lock.

The shell runner keeps its private temporary stdout and stderr files alive until the broker acknowledges stream capture. It sends their paths through the generic stream-capture operation instead of copying large streams into an ordinary JSON frame. The broker opens each path without following a symbolic link and checks that it is a regular file owned by the current user, has private permissions, stays within the request limits, and still refers to the expected file. It snapshots the bytes before the transaction. The runner does not emit output before this acknowledgement.

Approved source output paths follow the same bounded and no-follow rules. A replay that sees changed file bytes conflicts with the committed content and fails clearly.

Temporary files are not a second durable queue or source of truth. SQLite remains the only durable record.

### Batching and commit

A ready request may wait at most 2 milliseconds for a compatible batch. A batch has at most 32 requests and at most 8 MiB of prepared payload. A larger legal request runs alone.

The broker applies requests strictly in order inside one outer immediate transaction. Each request runs under its own savepoint. If one request fails with a request-local permanent error, the broker rolls that savepoint back completely before it starts the next request. Shared payload deduplication and foreign keys must remain valid after this rollback.

The broker commits the outer transaction once. It sends success and request-local failure acknowledgements only after that commit. If outer transaction start or commit fails, no request receives success.

A permanent outer failure applies to every request in the batch. YARP returns the original bounded cause. It retries only typed SQLite busy or locked outcomes within the request deadline. It does not retry malformed data, conflicts, corruption, schema errors, permission errors, disk-full errors, other I/O errors, protocol errors, authentication errors, or invariant failures.

### Acknowledgement and replay

The Rust bridge owns reconnect and replay. The TypeScript adapter does not run a second archive replay policy.

A request has one end-to-end deadline. Reconnect, broker startup, queue wait, batching, SQLite retry, commit, and acknowledgement share that deadline. A retry never resets it.

An acknowledgement means the outer transaction committed. After a transport failure, the bridge follows one private, exhaustive `ReplayPolicy` on `ArchiveOperation`. The policy has two values: `SafeReplay` and `UnknownOnDisconnect`. Every capture variant is `SafeReplay`. `PruneBefore` is `UnknownOnDisconnect`. The match has no default arm, so each new operation must choose a policy.

For `SafeReplay`, the bridge reconnects and sends the exact capture request again with the same stable operation identity and the original deadline. Existing version 1 rows and exact content checks prove committed capture outcomes. Identical replay returns the same archive reference or success without duplicate calls, snapshots, or payloads. Reuse of an identity with different content fails.

For `UnknownOnDisconnect`, the bridge does not reconnect or send the operation again. A successful prune acknowledgement still returns the exact number of deleted calls. If that acknowledgement is lost, existing rows cannot recover the original count. The bridge reports a bounded unknown outcome that contains no private request data and tells the operator to inspect the archive before another prune.

A broker rejection is terminal and is not transport-retried. The archive does not add an operation receipt table.

The in-memory queue is not durable. A broker crash rolls back its open transaction and loses queued work. Clients replay only replay-safe unacknowledged capture requests. SQLite and stable archive identities decide whether prior capture work committed.

## SQLite settings

The broker uses:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 250;
PRAGMA auto_vacuum = INCREMENTAL;
```

Schema creation and migration use `BEGIN EXCLUSIVE`. Normal batches use short immediate transactions. Expensive validation, hashing, compression, and file reads happen before the transaction.

The long-lived broker uses a bounded WAL checkpoint policy. It does not checkpoint inside an active request transaction. Idle shutdown performs a clean bounded checkpoint before closing. Checkpoint failure stays visible and does not make an uncommitted request look committed.

## Filesystem permissions

YARP creates `~/.local/share/yarp` with mode `0700` and the database with mode `0600` on POSIX systems. YARP narrows broader permissions on that known default directory and its database before opening them. An explicit override already uses a private directory; YARP does not change an existing override directory. Failure to establish private permissions is an error.

The archive can contain commands, source code, file contents, environment-derived values, and secrets printed by tools. YARP never uploads, syncs, serves, or logs this data.

## Failure behavior

YARP never hides an archive failure.

If initial capture cannot commit after bounded startup, reconnect, and retry, YARP blocks the call. This preserves the rule that an executed call has archived input.

If a result operation fails after execution, YARP leaves the call incomplete and reports the archive failure. Non-shell tools keep the unchanged result already exposed by the agent. Wrapped shell tools restore the committed raw streams and return them instead of the pruned result.

If the runner's spool or stream-capture acknowledgement fails after the child starts, the runner drains the child, emits the exact raw bytes on their original streams, reports the archive error, and preserves the child status.

A broker crash, full disk, invalid database, unsupported schema, failed integrity check, unsafe runtime path, authentication failure, or permission error is terminal for the affected capture. YARP does not create another database, write a fallback file, or open SQLite directly from the client.

## Output recovery and cap

YARP commits an exact `result_text`, typed-summary recovery source, source output, or raw stream source before it shortens visible text.

After command-aware processing, YARP applies the global text cap across the remaining text blocks. The default ordinary cap is 5,120 UTF-8 bytes. The visible result keeps bounded UTF-8-safe beginning and ending content plus a valid `yarp search` marker. Image blocks do not count toward this text budget.

Archive failure leaves the pre-cap result visible. Direct recovery commands use their own bounds and do not receive another outer cap marker.

## Commands

`yarp archive stats` reports safe counts and sizes through a read-only connection. It does not print archived content.

`yarp archive verify` checks the database through a read-only connection.

`yarp archive prune --before <UTC timestamp>` sends a writable maintenance request to the broker. Prune deletes only finished calls older than the requested time, removes unreferenced payloads, and runs incremental vacuuming. Version 1 does not delete calls automatically.

`yarp archive ingest` is the framed adapter bridge. `yarp archive broker` is the on-demand local writer process.

## Validation

`yarp archive verify` checks:

- SQLite integrity and foreign keys;
- the supported `user_version`;
- allowed lifecycle and snapshot values;
- unique well-formed archive references and valid result-text completeness;
- payload decompression, byte length, and SHA-256 digest;
- filesystem permissions;
- incomplete calls;
- missing required snapshots; and
- unreferenced payloads.

The command exits nonzero for corruption, unsupported versions, bad permissions, invalid payloads, or missing required snapshots. It reports incomplete calls separately.

Broker validation also covers:

- private runtime directory and socket permissions;
- same-user peers;
- protocol and binary versions;
- frame, queue, batch, and deadline bounds;
- source order;
- identical and conflicting replay;
- startup and idle-shutdown races;
- savepoint isolation and outer commit failure;
- broker and client crashes; and
- absence of direct writable client paths.

## Retention

Version 1 does not delete calls automatically. Explicit prune is the only normal retention write. YARP does not delete incomplete calls or the existing `~/.local/share/yarp/tee` directory. The old tee files are outside this format.

## Migrations

YARP accepts databases with `user_version = 1`. The broker may migrate an older supported version inside one exclusive transaction after creating and verifying a private SQLite backup. It rejects a newer version.

Migrations update version 1 in place. YARP does not create a parallel database version, dual reader, dual writer, fallback reader, or compatibility service.

Existing version 1 rows provide replay evidence. This broker design does not add a receipt table.

## Boundaries

YARP core and the broker are agent-neutral. Agent adapters translate public agent events into generic archive operations. Pi-specific types and lifecycle rules stay in `hooks/pi`.

Version 1 does not record provider request bodies, model reasoning, user prompts, streaming tool updates, credentials from agent configuration, or shell input sent after process start unless that input is a tool call.

The SQLite database is the only durable source of truth. YARP does not add a second database, per-agent archive, durable side queue, fallback writer, network service, telemetry, or installed system or user service.

This contract covers one local user and supported local Unix filesystems. It does not claim remote access, distributed operation, perfect operating-system fairness, or safety on a broken or network filesystem.
