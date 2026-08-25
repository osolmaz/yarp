# Tool-call archive implementation plan

This plan replaces YARP's competing SQLite writers with one agent-neutral local broker. The broker starts on demand, queues requests from all clients, commits compatible requests in short batches, and acknowledges each request only after durable commit.

The database format and runtime rules are defined by [Tool-call archive specification](tool-call-archive-spec.md). Indexed output and result-text recovery remain defined by [Indexed output summaries](indexed-output-summaries-implementation-plan.md) and [Recovery output](recovery-output-implementation-plan.md).

## Goal

Make local archive writes reliable and efficient when many agent sessions and shell wrappers run at the same time.

The completed design has:

- one normal SQLite writer for each canonical archive;
- one private local queue shared by all clients;
- short fixed micro-batches that reduce transaction and sync work;
- durable acknowledgement before tool execution or result shortening;
- safe replay after a lost acknowledgement;
- exact stdout, stderr, result, and recovery data;
- clear terminal failures; and
- no Pi knowledge in YARP core.

## Scope

Change YARP's generic archive protocol, broker and client processes, runtime socket and startup lock, queue and batching policy, SQLite write ownership, stream handoff, replay behavior, tests, CLI text, and canonical documentation.

Keep the Pi extension as an adapter over public Pi events. Keep the current command behavior, child exit codes, stdout and stderr streams, archive references, recovery order, pruning rules, privacy rules, and ordinary 5,120-byte text cap.

Use one private local SQLite archive as the only durable source of truth. Do not add a network listener, remote service, system service, second database, per-agent database, durable side queue, direct-write fallback, dual write, or compatibility path.

Tests and benchmarks use temporary archives, runtime directories, sockets, spool files, and processes. They never write, lock, migrate, prune, repair, or benchmark the live archive.

## Selected design

Add one `yarp archive broker` process for each canonical archive path. It starts through race-safe local activation and exits after a bounded idle period. It owns the only normal writable SQLite connection.

Keep `yarp archive ingest` as the adapter-facing framed command. Change it into a thin Rust bridge to the broker. It does not open SQLite.

Make `yarp run` a broker client for exact stream capture. Make writable maintenance commands broker clients. Use direct read-only connections for stats, verify, search, read, and restore.

The broker admits bounded requests, serves clients in round-robin order, preserves each source's operation order, prepares data before the write lock, and groups compatible ready requests into one transaction. Each request uses a savepoint. The broker sends acknowledgements only after the outer transaction commits.

Use fixed limits first. Keep micro-batching only when representative temporary-archive results show a useful improvement without a material latency or throughput cost. If batching does not meet that test, keep the broker and fair queue but commit requests one at a time.

### Batching decision evidence

The temporary-archive benchmark used 16 KiB initial-capture payloads, three repeated timing runs, and separate `strace` runs for sync calls. Each timing run sent 160 requests at 16 clients or 320 requests at 32 clients. Every serial and micro-batch request committed without an error, duplicate, or missing row.

At 16 clients, micro-batching increased median throughput from 450.44 to 552.12 requests per second, an absolute gain of 101.68 requests per second or 22.6%. Median p95 gate latency fell from 37.53 to 32.75 ms, an absolute reduction of 4.77 ms or 12.7%. In the 64-request trace run, sync calls fell from 73 to 14, a reduction of 59 or 80.8%.

At 32 clients, micro-batching increased median throughput from 473.87 to 608.03 requests per second, an absolute gain of 134.16 requests per second or 28.3%. Median p95 gate latency fell from 73.20 to 66.59 ms, an absolute reduction of 6.61 ms or 9.0%. In the 128-request trace run, sync calls fell from 140 to 14, a reduction of 126 or 90.0%.

The current direct-writer baseline at commit `1bf93b7` reached 531.64 requests per second with 33.07 ms median p95 latency at 16 clients, and 588.80 requests per second with 77.75 ms median p95 latency at 32 clients. The micro-batch broker was a practical throughput tie with the direct writer at both loads. It reduced p95 latency by 11.16 ms at 32 clients and replaced uncontrolled writer contention with one bounded queue.

The minimum worthwhile batching effect was a material sync reduction, at least 25%, with no material throughput or p95 gate-latency regression against the serial broker. Micro-batching cleared this rule at both loads. The selected implementation keeps micro-batching.

## Contracts

### Agent boundary

YARP core and the broker use generic source, call, operation, sequence, deadline, and replay identities. They do not use Pi event names, Pi session types, provider behavior, or Pi user-interface rules.

`hooks/pi` translates public Pi tool events into generic archive operations. It keeps initial execution blocking, result restoration, final-message replacement, and output-cap behavior.

### Writer boundary

The broker is the only normal process that opens the archive for writing.

This rule covers:

- initial call capture;
- result-before capture;
- result-text capture;
- provisional result staging;
- final-result update;
- call completion;
- normal and passthrough shell stream capture;
- explicit prune;
- schema initialization and supported migration; and
- any other normal archive mutation.

Read-only operations use a query-only connection. A broker failure never causes a client to open SQLite directly.

### Durability boundary

Initial capture is acknowledged only after commit. The adapter blocks tool execution until it receives that acknowledgement.

The shell runner is synchronous on stream capture. It waits for the broker to commit stdout and stderr snapshots before it emits output and exits.

Post-result operations are acknowledged only after commit. Existing raw-result and raw-stream restoration rules apply after terminal failure.

### Replay boundary

The Rust bridge owns reconnect and replay. The TypeScript adapter does not run another replay loop.

Every request keeps one end-to-end deadline through broker startup, queue wait, batching, SQLite retry, commit, reconnect, and acknowledgement. Retry does not reset the deadline.

Add one private, exhaustive `ReplayPolicy` beside `ArchiveOperation`. The policy has two values: `SafeReplay` and `UnknownOnDisconnect`. Classify every capture variant as `SafeReplay` and classify `PruneBefore` as `UnknownOnDisconnect`. Use no default match arm, so each new operation must choose a policy.

For `SafeReplay`, the Rust bridge reconnects and sends the exact request again with the same stable identity and original deadline. Existing version 1 rows and exact content checks prove whether capture committed. Identical replay succeeds without duplicate calls, snapshots, or payloads. Conflicting reuse fails. Add a named test in which `fullOutputPath` changes after an unacknowledged commit; replay must fail as a clear content conflict.

For `UnknownOnDisconnect`, the Rust bridge sends the operation once and does not reconnect or send it again after transport loss. A normal prune acknowledgement keeps the exact deleted count. A lost prune acknowledgement returns a bounded unknown outcome with no private request data and requires archive inspection before another prune. Broker rejections are terminal and are not transport-retried.

Do not add a receipt table, schema change, second store, compatibility path, second Rust client API, or TypeScript replay loop.

### Hard cutover

Remove the direct multi-writer path in the same change. Do not keep a feature flag, old socket, direct-write fallback, dual path, version 2 contract, or compatibility shim.

Old running clients must restart during the later release rollout. The implementation does not silently support old direct writers alongside the broker.

## Work

### 1. Record the current behavior

Map every writable `Archive` caller, direct `Archive::open()` call, framed ingest operation, stream-capture path, commit site, read-only command, migration path, Pi failure rule, and output-cap rule.

Record the temporary-archive lock reproduction and a benchmark baseline for the current direct-writer design. Do not point any test or benchmark at the configured live archive.

Verification:

- The inventory includes `yarp run`, ingest, prune, stats, initialization, and migration.
- A temporary lock fixture reproduces the current contention error.
- The baseline records raw results for 1, 16, and 32 clients.

### 2. Extract the generic protocol

Move framed archive operations and acknowledgements into a private agent-neutral Rust module such as `src/archive_protocol.rs`.

Keep the adapter-facing `schemaVersion: 1` frame contract. Add a separate socket handshake with schema `yarp.archive-broker.v1`, binary version, canonical archive identity, and connection correlation ID.

Add a broker envelope with stable operation identity, generic source ordering key, sequence, relative deadline, correlation ID, and one bounded operation payload. Add operations for exact and passthrough shell stream capture and writable maintenance.

Reject unknown fields, operations, schemas, identities, lengths, and sequences before allocation or queue admission. Bound all diagnostics.

Verification:

- Round-trip fixtures cover every operation and acknowledgement.
- Invalid, unknown, truncated, and oversized frames fail clearly.
- Protocol fixtures and broker modules contain no Pi types or event names.

### 3. Add the private runtime

Add a private runtime module such as `src/archive_runtime.rs`.

Derive the broker identity from a hash of the canonical archive path. Use a private directory under a valid `XDG_RUNTIME_DIR`, mode `0700`, and a Unix-domain socket, mode `0600`. When `XDG_RUNTIME_DIR` is absent, use a private user-ID-specific directory under the local temporary directory. Use the existing file-lock dependency when it fits the startup lock.

Reject symbolic links, wrong owners, broad permissions, wrong file types, unsafe stale paths, and socket paths that exceed platform limits. Check peer user IDs where supported.

Gate this implementation to supported Unix targets. Do not claim support for a platform without an equivalent secure local transport.

Verification:

- Security tests cover owner, mode, symlink, stale file, stale socket, path length, archive overrides, and multiple archives.
- Concurrent startup tests prove that one broker wins.
- The broker opens no network listener.

### 4. Add race-safe broker activation

Add a client module such as `src/archive_client.rs`.

A client first connects. On a missing or refused socket, it takes the startup lock and checks again. The broker holds a separate lifetime lock while it owns the writable archive. The winner removes only a verified stale socket with no live lifetime owner, starts the same YARP binary as `archive broker`, and waits at most 5 seconds for the ready handshake. Losing clients connect to the winner.

The broker survives the launcher. It exits after 60 idle seconds. Idle means no clients, an empty queue, and no transaction.

During exit, take the startup lock, stop accepting clients, remove the socket safely, drain admitted work, checkpoint and close the archive, and exit. A client that meets this shutdown repeats normal activation.

Verification:

- Start 32 clients through barriers and observe one broker.
- Cover winner failure before bind, slow startup, stale socket, version mismatch, launcher exit, idle cancellation by a new client, and final cleanup.
- No broker, socket, lock, or child remains after each terminal test.

### 5. Add the broker command

Add an agent-neutral `yarp archive broker` command in a module such as `src/archive_broker.rs`.

The broker owns archive open, current-schema verification, supported in-place migration, queue admission, preparation, batching, commit, checkpoint, acknowledgement, and shutdown.

It prints only bounded startup and terminal diagnostics. It does not print payloads, credentials, commands, private source IDs, or spool contents.

Verification:

- The broker holds one writable connection.
- It survives client exits and closes after the idle grace.
- Permanent database startup failure rejects clients clearly and does not create a fallback archive.

### 6. Convert ingest into a bridge

Refactor `yarp archive ingest` into a stdin-to-broker bridge. Keep its current 8-byte length prefix, JSON request, and line acknowledgement contract for the Pi adapter.

The bridge validates the frame, connects or starts the broker, adds the broker envelope, and sends one request at a time. It never opens SQLite.

On disconnect before acknowledgement, reconnect and replay an exact replay-safe capture request within its original deadline. Do not replay prune because its exact deleted count cannot be recovered from existing rows. Return a permanent broker response immediately. EOF drains or cancels bounded work and exits cleanly.

Remove the TypeScript writer restart as a replay owner. Keep TypeScript responsible for the one bounded acknowledgement envelope and adapter lifecycle.

Verification:

- The adapter frame fixtures remain valid.
- Instrumentation proves that the bridge has no writable database connection.
- Reconnect does not reset the deadline or duplicate a committed operation.
- Permanent responses are not replayed.

### 7. Route shell streams through the broker

Add generic exact-stream and passthrough-stream operations. Change `yarp run` to use the broker instead of `Archive::open()`.

Keep stdout and stderr in private `NamedTempFile` spools. Keep the files alive until acknowledgement. Send bounded path metadata rather than copying large streams into a JSON frame.

The broker opens each spool without following symbolic links and verifies regular-file type, current-user ownership, private permissions, expected identity, and bounded size. It snapshots, hashes, and compresses the file before the transaction. If repository or platform evidence shows that path handoff cannot meet these checks, use a supported safer local file handoff instead of weakening the checks.

The runner waits for durable stream acknowledgement before returning output. On terminal spool or archive failure after the child starts, it drains the child, restores exact raw bytes to their original streams, reports the archive error, and preserves the child exit code.

Verification:

- Exact binary stdout and stderr survive normal, passthrough, retry, and failure paths.
- A path replacement, symlink, wrong owner, broad mode, changed file, or oversized spool fails safely.
- `finish_call` cannot pass stream capture for the same call.
- Runner output and exit status remain unchanged after successful capture.

### 8. Add bounded fair admission

Allow one admitted request per client, 256 admitted requests globally, and at most `MAX_FRAME_BYTES` of admitted request bodies globally.

Stop reading from a client while its request is admitted. Apply socket backpressure when the queue is full. Keep the original request deadline.

Serve one head request from each client in round-robin order. Preserve each generic source's sequence. Reject sequence regression and incompatible concurrent use.

Verification:

- Tests prove request and byte bounds, stable memory use, backpressure release, deadline expiry, disconnect cleanup, round-robin service, no starvation, and per-source order.

### 9. Prepare data before writing

Split each write operation into bounded preparation and transaction-local mutation.

During preparation, validate data, create canonical JSON, compute digests, compress payloads, and snapshot approved source files. Keep prepared file data in the existing private temporary-file pattern when it does not fit the memory budget. Do not reread a changing source path during a transaction retry.

Keep transaction closures limited to database work. Keep lifecycle and content checks inside the transaction where they depend on current rows.

Verification:

- Transaction instrumentation observes no source file read or compression.
- A source file change after preparation cannot change committed bytes.
- Temporary files are private and removed on success, failure, cancellation, and crash recovery.

### 10. Add micro-batches and savepoints

Use fixed first-version limits:

- at most 2 milliseconds of batch wait;
- at most 32 requests; and
- at most 8 MiB of prepared payload.

Run a larger legal request alone.

Start one immediate outer transaction. Apply requests strictly one at a time. Give each request a savepoint. On a request-local failure, roll back that savepoint fully before the next request starts. This rule protects shared session rows, payload deduplication, and foreign keys.

Commit the outer transaction once. Send success and request-local error acknowledgements only after that commit.

If outer transaction start or commit fails, send no success. A permanent outer failure applies to all requests in the batch. Retry only typed SQLite busy or locked outcomes within the shared request deadlines. Preserve the original terminal cause.

Verification:

- Inject failures at begin, savepoint start, request body, savepoint release, and outer commit.
- Test two requests with the same payload where the first fails after inserting it and the second still commits correctly.
- Prove no acknowledgement before commit, no partial rows, no duplicate operations, and exact failure assignment.

### 11. Move every write behind the broker

Refactor archive methods so the broker can apply prepared operations inside a caller-owned transaction.

Route begin, result-before, result-text, stage, finish, final update, stream capture, passthrough capture, prune, and all other normal mutations through the broker.

Run database initialization and migration at broker startup while holding the startup lock. Change stats to read-only. Keep verify, search, read, restore, and other factual readers read-only.

Delete normal direct writable archive helpers from client, runner, ingest, and maintenance paths.

Verification:

- Static searches and instrumentation find one normal writable opener: the broker.
- A failed broker causes a clear error and no direct-write fallback.
- Fixture rows, hashes, references, lifecycle state, pruning, and verification match the existing contract.

### 12. Apply replay policy with existing rows

Audit each operation against current unique keys and content checks. Keep the current session upsert, call verify-or-conflict behavior, snapshot conflict checks, and final-result replay checks.

Add the private exhaustive `ReplayPolicy` to `ArchiveOperation`. The Rust bridge uses it only after a transport failure. It retries `SafeReplay` capture operations within the unchanged deadline. It sends `PruneBefore` once and returns `UnknownOnDisconnect` when its acknowledgement is lost.

Do not add an operation receipt table. Do not claim more than existing durable rows prove.

Verification:

- A classification test covers every `ArchiveOperation` variant and proves that prune is the only `UnknownOnDisconnect` operation.
- Each capture operation survives commit followed by acknowledgement loss and reuses the same stable identity and original deadline.
- Identical capture replay returns the same result with unchanged row counts.
- One changed byte or metadata field returns a permanent conflict.
- A changed `fullOutputPath` replay is a named conflict test.
- A normal prune acknowledgement returns the committed deleted count.
- A lost prune acknowledgement sends one prune request and returns bounded private-safe inspection guidance.
- A broker rejection receives no transport replay.
- TypeScript sends once and does not restart or replay archive transport.

### 13. Define crash and shutdown behavior

An accepted but unacknowledged capture request remains the client's responsibility. A broker crash rolls back the active transaction and loses queued memory. The bridge reconnects and replays a replay-safe capture request until the original deadline. A lost prune acknowledgement produces a truthful unknown-outcome error instead of an automatic replay.

A client disconnect removes queued work that has not started. Active work may commit; replay identity resolves the result.

When database safety is uncertain, the broker stops accepting writes, closes clients with one bounded cause, checkpoints when safe, and exits. It does not reinterpret a permanent failure as contention.

Verification:

- Kill the broker before begin, during preparation, during request mutation, after commit before acknowledgement, and during idle shutdown.
- Kill a client while queued and active.
- Every case ends as a proved commit, a safe exact replay, or a clear terminal failure.
- Archive integrity and foreign keys pass after every case.

### 14. Keep the Pi adapter boundary

Keep Pi event translation in `hooks/pi/yarp.ts`. Keep the extension on public Pi APIs.

Keep initial capture execution-blocking. Keep post-result terminal failures visible. Keep raw-result and raw-stream restoration, child exit codes, stream separation, command rewrite failure behavior, and final message replacement.

Keep the exact recovery-source rule. Apply the ordinary global cap at 5,120 UTF-8 bytes only after an exact recovery source commits.

Verification:

- No Pi type or event name enters broker protocol fixtures or Rust broker modules.
- Initial failure blocks execution.
- Post-result failure keeps or restores the required raw output.
- The cap stays exactly 5,120 bytes and leaves a valid recovery reference.

### 15. Control WAL growth

Keep WAL mode, `synchronous = FULL`, foreign keys, and short transactions.

Use a bounded checkpoint policy for the long-lived broker. Do not checkpoint inside an active request transaction. Run a clean bounded checkpoint during idle shutdown.

Verification:

- Sustained temporary load keeps WAL growth within the documented policy.
- Checkpoint failure is visible and does not change request commit truth.
- Readers continue while the broker writes.

### 16. Measure capacity and practical value

Use a deterministic temporary-archive harness with 1, 16, and 32 concurrent bridge clients. Run the current direct-writer baseline, a serial broker mode, and the micro-batch broker with representative bounded payloads and the normal call operation sequence.

Report:

- raw request and byte counts;
- elapsed time and throughput;
- p95 initial-capture gate latency;
- queue delay;
- transaction or fsync count;
- batch size;
- errors;
- duplicate operations; and
- missing operations.

Correctness requires no lost, duplicate, or contention-failed operation.

Use the practical-significance rule before retaining batching. At 16 and 32 clients, batching must materially reduce fsyncs per request without a material p95 gate-latency or throughput regression against the serial broker. Report absolute effects and repeated-run ranges. Treat uncertain or immaterial results as a tie and keep the simpler serial broker.

The broker itself must also improve the relevant failure and capacity results over the current direct-writer baseline. Do not ship batching because it wins only a proxy metric.

### 17. Update documentation and help

Update this plan, the archive specification, README, Pi adapter guide, CLI help, and protocol documentation.

Document agent neutrality, local IPC, permissions, startup, fixed limits, fair queueing, ordering, batching, savepoints, durable acknowledgement, replay, deadlines, stream handoff, failures, WAL checkpoints, idle exit, Unix support, hard cutover, and operator recovery.

Do not include private paths, payloads, credentials, or live archive data in examples.

### 18. Validate the change

Run:

```sh
cargo fmt --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo llvm-cov --workspace --all-targets --fail-under-lines 85
cargo audit --deny warnings
npm audit
npm run typecheck:pi
npm run test:pi
slophammer-rs check .
git diff --check
```

Also inspect the complete patch for live archive paths, credentials, private payloads, generated archives, sockets, temporary files, process leaks, direct writable clients, fallback writers, and unrelated changes.

Do not run mutation testing unless the user requests it or test evidence shows a specific test-strength problem.

## Later rollout

Release, publication, installation, OnurPi changes, and session restarts are separate work.

After separate approval:

1. Release the reviewed Yarp version under the repository's pre-1.0 version rules.
2. Update the OnurPi working tree with `git pull --ff-only`.
3. Pin the reviewed immutable Yarp source and matching binary version in `packages/yarp`.
4. Update the upstream record and package tests.
5. Run OnurPi checks and install the matching binary and package.
6. Restart old Pi sessions so no direct writer remains.
7. Run a multi-client canary against a temporary archive.
8. Run `yarp archive verify` against the live archive in read-only mode.

Do not use the live archive for a lock, load, migration, prune, repair, or benchmark test.

## Completion criteria

The implementation is complete when:

- one on-demand broker owns every normal writable archive connection;
- YARP core and its protocol contain no Pi-specific behavior;
- initial capture commits before execution;
- shell stream capture commits before the runner emits output;
- post-result recovery and terminal failure rules still hold;
- bounded fair admission and source ordering pass deterministic tests;
- a valid batch isolates request-local failure and acknowledges only after outer commit;
- replay after a lost capture acknowledgement creates no duplicate data, and lost prune acknowledgement stays explicitly unresolved;
- broker and client crashes have deterministic outcomes;
- no direct-write fallback or compatibility path remains;
- the selected serial or micro-batch policy passes the practical-significance review;
- the 5,120-byte cap and exact recovery source remain correct;
- canonical documentation and CLI help match the implementation;
- every required repository check passes;
- Pi Reviewer reports no P0 or P1 finding;
- CI is green; and
- the approved pull request is merged.

## Boundaries

This work does not change Pi core, pi-workflows, provider code, credentials, another agent repository, or an external service.

It does not add TCP, HTTP, WebSocket, telemetry, remote access, systemd, launchd, another installed service, another database, a durable queue, a fallback file, or a compatibility writer.

It does not change command rewrite meaning, child exit codes, stream placement, archive references, search and read recovery, pruning meaning, or the ordinary output cap.

It does not claim perfect operating-system fairness, instant cancellation, remote-filesystem safety, or exactly-once behavior beyond the durable identities and content checks proved by tests.
