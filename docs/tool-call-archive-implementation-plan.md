# Tool-call archive implementation plan

YARP will record every Pi tool call in one local SQLite database. Each record will contain the tool input and result before and after YARP processing. The database format is defined in [tool-call-archive-spec.md](tool-call-archive-spec.md).

The current release prunes supported shell output in memory and keeps no history. This work adds local persistence without adding messages or entries to Pi sessions.

## Scope

The first implementation covers every Pi `tool_call` and `tool_result` event. Supported shell commands also record exact stdout and stderr before and after pruning. Tools that YARP does not change store identical before and after snapshots through one deduplicated payload.

The schema includes `agent` and `account` so later read-only importers can add locally stored calls from Codex and Claude as well as Cursor. Those importers are separate work. This implementation does not inspect another agent's credentials or copy data between user accounts.

Existing files under `~/.local/share/yarp/tee` stay untouched. There is no old YARP archive schema to migrate.

## Design

The Rust crate owns SQLite access and migrations. It also owns compression, content hashing, and archive verification. This keeps the database rules in one implementation.

The Pi extension starts a session-scoped `yarp archive ingest` child process during `session_start`. It sends bounded framed messages to that process and closes it during `session_shutdown`. The child is part of the Pi session and is never installed as a system or user service.

`yarp run` uses the same Rust archive module when it captures a supported shell command. SQLite WAL mode coordinates the ingest process and any concurrent command wrappers. All writers use short transactions and the source tool-call ID as the correlation key.

The extension continues to use Pi's public APIs. It reads `toolCallId`, tool name, input, session ID, and the active provider/model from documented event and context fields. It does not append custom session entries or use Pi internals.

## Work

### Archive foundation

Add a Rust archive module with the version 1 schema and migrations. The module will:

- Resolve `~/.local/share/yarp/tool-calls.sqlite3` with an environment override for tests.
- Create the data directory and database with private permissions.
- Apply the required SQLite settings.
- Encode RFC 8785 JSON and exact byte payloads.
- Hash uncompressed bytes with SHA-256.
- Compress worthwhile payloads with Zstandard level 3.
- Insert payloads by digest and reuse existing rows.
- Expose transactional operations for sessions, calls, snapshots, and terminal updates.

Add `yarp archive verify` and `yarp archive stats` before enabling automatic capture. `verify` must distinguish incomplete calls from corruption.

### Ingest process

Add `yarp archive ingest`, a long-lived stdin reader used by the Pi extension. Each operation is one JSON object preceded by an unsigned 8-byte big-endian length. Frames are capped at 256 MiB. The protocol is internal to the matching YARP binary and Pi package revision.

The process acknowledges each committed operation with one JSON line. It rejects malformed frames, oversized frames, unknown operations, unsupported schema versions, and truncated input. It stops cleanly at EOF after flushing committed work.

The extension will serialize requests through one queue. If the child exits, the extension restarts it once and retries the unacknowledged request. It must not retry a request that was acknowledged.

### Pi call capture

Extend the Pi package with one in-memory call map keyed by `toolCallId`.

At `tool_call`, capture the original input, run the existing YARP rewrite decision, capture the resulting input, and commit both snapshots before execution. Calls outside the rewrite allowlist still get archived.

Use `tool_result` to capture the result before YARP's result handler changes it. Use `tool_execution_end` to capture the finalized result for every call and mark the row as finished. Pi emits this final event even when preflight rejects or blocks a call and skips `tool_result`. A shell command handled by `yarp run` uses its raw stdout and stderr snapshots as the pre-pruning record. Record `isError` from the event without classifying errors from their text.

Exercise parallel tool mode in tests. Sibling calls can finish in a different order from their source order. Add duplicate prevention because both result hooks can observe one call.

### Shell stream capture

Pass the archive session and call identifiers to `yarp run` without placing payload data in shell arguments. The Rust runner will write raw stdout and stderr snapshots before rendering its bounded output, then write the exact emitted streams as after snapshots.

Preserve child exit codes and stream separation. Archive writes must not make stdout or stderr visible on the other stream. A database failure after command execution returns the unpruned stream and a clear archive error.

Long-running and interactive behavior needs an explicit regression test because the current runner buffers supported commands until exit. This archive work must not make that behavior worse. Any later streaming change belongs in a separate change.

### Commands and configuration

Add these commands:

```text
yarp archive stats
yarp archive verify
yarp archive prune --before <UTC timestamp>
```

`stats` reports call counts, incomplete calls, logical payload bytes, stored payload bytes, database bytes, and oldest and newest call times. It does not print tool content.

`prune` is explicit. It deletes only finished calls older than the requested time, removes unreferenced payloads, and runs incremental vacuuming. Version 1 performs no automatic deletion.

Keep `YARP_DISABLED=1` limited to pruning and rewriting. Add `YARP_ARCHIVE_DISABLED=1` as an explicit capture opt-out. Normal operation enables the archive.

### Documentation

Update the README when capture ships. The current README must continue to say that released YARP does not store output until the implementation is complete.

Document the database path, permissions, size reporting, opt-out flag, verification command, and the meaning of before and after. State that the archive may contain secrets printed by tools.

## Verification

Add tests at the Rust and Pi boundaries. Cover the CLI and a live Pi session within those suites.

The Rust suite will cover schema creation, idempotent migration, content deduplication, compression fallback, exact binary streams, permission repair, foreign keys, concurrent writers, busy retries, interrupted calls, corruption detection, explicit pruning, and orphan cleanup.

The Pi suite will cover every tool name, unchanged calls, rewritten shell inputs, both result hooks, blocked calls, errors, cancellation, parallel completion, reload, session replacement, ingest restart, duplicate acknowledgements, and archive opt-out.

Run crash tests that send `SIGKILL` during input capture and result capture. After restart, `PRAGMA integrity_check` must pass. A call may remain `started`, but committed snapshots must verify.

Use a fixed fixture of 812 representative local Pi tool results for storage checks. The current measurement is 7,479,585 uncompressed bytes and 1,269,835 Zstandard-compressed bytes. The fixture is private test input and must not be committed. A synthetic public fixture will enforce that the database stays below 30% of its uncompressed before-and-after payload size.

Measure archive overhead on the local NVMe filesystem after warmup. For a 16 KiB input, the 95th-percentile pre-execution commit must stay below 10 ms. For a 1 MiB result, the 95th-percentile final transaction must stay below 100 ms. Report raw timings and at least 100 repetitions. If these limits fail, keep capture disabled and investigate before release.

Run the repository checks in `AGENTS.md`, then test the package with `pi -e` and `/reload`. A live test must show one short unmodified tool, one pruned shell command, one failed command, and one parallel pair in `yarp archive verify` with matching before and after snapshots.

## Completion criteria

The work is complete when:

- Every Pi tool call creates a version 1 archive row before execution.
- Changed and unchanged inputs and results have correct snapshot references.
- Supported shell output has exact before and after stream snapshots.
- No omitted raw bytes are lost when capture succeeds.
- Archive failures are visible and initial capture failure blocks execution.
- SQLite integrity survives the crash and concurrency tests.
- The archive stays within the performance and storage budgets.
- The README and CLI help match the shipped behavior.
- no Pi session state, Pi persistent schema, or internal API changes are introduced.

If Pi's public events cannot expose a required pre-YARP value, stop and record the missing public capability. Do not patch Pi internals or silently weaken the meaning of `before`.
