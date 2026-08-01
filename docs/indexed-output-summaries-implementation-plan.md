# Indexed output summaries implementation plan

YARP should keep the exact command output locally and give the model a smaller,
command-specific summary when that summary is useful. The summary must show what
happened, preserve the evidence needed for the next action, and identify exact
archived ranges that can be read later.

This replaces broad head-and-tail pruning with typed summaries. It does not add
a fixed 10,000-character threshold. Small output stays exact whenever a summary
does not meet the rule's minimum savings requirements.

## Goal

Increase YARP's removal of stored shell-output characters from 4.6713% to at
least 20% while preserving these rules:

- Unknown, ambiguous, invalid, incompatible, or unavailable rules pass through
  unchanged.
- Structured output, exact inspection, NUL-delimited output, machine-readable
  modes, filename-only modes, count-only modes, and watch or follow modes pass
  through unchanged.
- Child arguments, environment, stdin, exit status, stdout, and stderr behavior
  do not change.
- Explicit exit codes take precedence over generic error flags.
- Short output stays byte-for-byte exact unless a summary meets both absolute
  and proportional savings requirements. The hard output cap still wins.
- Reducer memory stays bounded independently of total input size and below 4 MiB
  per stream or result.
- Archive, reducer, or Pi integration failure returns the unchanged output.
- The archive and the offline `toolcall-extractor` remain separate systems.

The working corpus target is 21%, or 303,560,545 removed shell-output
characters. The release requirement is 20%, or 289,105,282 characters. The extra
point leaves room for safety fixes found during review.

## Why this work is concentrated

The private corpus contains 717,863 tool results and 1,941,717,513 plain-text
output characters. Results with at least 10,000 characters are only 49,618
calls, or 6.91% of results, but contain 60.33% of all text. The largest 5% of
results contain 52.33% of all text.

Shell process tools account for most of the output. This means YARP can reach
the target by improving a small number of high-volume command families. It does
not need a generic fallback, arbitrary truncation, or agent-specific policies.

Search and Git diff are the first targets. Together they contain about 180
million additional removable characters over current YARP. Test, build, log,
status, and reviewed human-readable list output provide the remaining coverage.

## Product behavior

For a selected command, YARP will:

1. Run the command once with the current child-process contract.
2. Capture the exact source output.
3. Parse lines with a typed reducer for that command family.
4. Build a bounded summary containing outcome, diagnostics, structure, context,
   and examples.
5. Compare the summary with the raw output under the selected success or failure
   policy.
6. Return the raw bytes when the summary does not save enough.
7. Return the summary when it passes every guard and savings check.
8. Add exact archive ranges to omission markers when an archive reference is
   available.

For example, a test summary will keep failing test names, error blocks,
warnings, and final totals. It will count or omit routine passing-test progress.
A search summary will keep diagnostics, file groups, match counts, and
representative matches. It will not treat an arbitrary line containing the word
`error` as a diagnostic without command-specific evidence.

A summary may arrange sections by usefulness. It does not reorder the archived
output. Logs retain chronology, and diffs retain file and hunk order within each
displayed section.

## Non-goals

This work will not add:

- an LLM summarizer;
- network access or telemetry;
- a daemon, service, watcher, socket, or remote store;
- executable rule hooks, plugins, runtime regular expressions, or replacement
  commands;
- a generic unknown-command reducer;
- automatic retrieval of omitted output;
- direct writes to Pi session history;
- Pi source changes or private Pi APIs;
- separate reducer implementations for different coding agents;
- a new archive database alongside the current database.

## Architecture

### Exact source and compact result

The runner owns exact capture. Reducers own only the compact result.

```text
child stdout/stderr
        |
        +--> exact private archive snapshots
        |
        +--> typed streaming reducer --> bounded summary --> model context
```

The runner continues to spool stdout and stderr independently. It waits for the
archive transaction before emitting reduced streams. A spool or archive failure
restores the exact raw stream and preserves the child status.

The reducer does not write SQLite and does not know archive paths. It receives
an optional opaque archive reference used only to render retrieval instructions.
The archive module validates and resolves that reference.

### Typed evidence

Replace generic retained-line decisions with a bounded evidence collector. Each
reducer emits records with an ordered class and a source span:

```rust
struct SourceSpan {
    first_line: u64,
    last_line: u64,
}

enum EvidenceClass {
    Outcome,
    Diagnostic,
    Structure,
    Context,
    Example,
    Noise,
}
```

The concrete implementation may use separate record enums for each reducer, but
every retained record must carry its source span. External rules cannot assign
numeric priorities.

The shared collector renders sections in the fixed class order. Within one class
it preserves source order unless the typed reducer documents another safe order.
It keeps bounded first and last records, bounded context around selected
diagnostics, saturating counts, and coalesced omitted ranges. It never stores an
unbounded map of files, tests, hunks, or log messages.

If one evidence class exceeds its budget, the collector retains bounded first
and last records, reports the exact omitted count, and records representative
source ranges. It must never imply that every error or match was displayed.

### Reducer families

Use one Rust implementation for each stable output shape:

- `search_summary`: ripgrep, grep, and reviewed search forms;
- `diff_summary`: human-readable Git patch output;
- `test_summary`: Cargo, npm, pnpm, Vitest, Go, pytest, and reviewed test
  runners;
- `build_summary`: compilers, package builds, and reviewed build tools;
- `log_summary`: journald, service logs, container logs, and reviewed log
  commands;
- `status_summary`: Git status, service status, and reviewed state reports;
- `list_summary`: reviewed human-readable lists where omission does not change
  an exact inspection contract;
- `line_filter`: exact literal cleanup for output with no richer stable
  structure.

Remove the generic `head_tail` reducer. Migrate every built-in rule to a typed
family or make it pass through. Keep typed behavior in Rust rather than
branching on rule IDs.

### Output policy

Replace head and tail counts with a policy that describes resource and savings
limits:

```text
max_line_bytes
max_output_bytes
min_savings_bytes
min_savings_basis_points
```

Success and failure keep separate policies. A failure normally receives a larger
output budget.

A compact result is used only when it saves at least `min_savings_bytes` and at
least `min_savings_basis_points` of the raw bytes. This removes the need for a
fixed input-length threshold. A useful short summary can still win, while an
almost identical summary remains raw.

Keep source schema version 1 and engine ABI 1. Replace the reducer and policy
contract in place. Remove the old fields and reducer names from schemas,
examples, tests, and documentation. Old source packs and compiled packs must
fail strict validation and be rewritten and recompiled. Do not add aliases or
compatibility readers.

### Command analysis

Simple commands continue through the existing rewrite and child-argument
agreement path.

Add a read-only command analysis path for results that could not be wrapped
safely. It may recognize a compound command only when all of these hold:

- every shell token is parsed without ambiguity;
- every output-producing command is allowlisted;
- setup commands such as a reviewed `cd` do not change the output type;
- every selected command maps to one compatible reducer family;
- no pass-through guard appears anywhere in the command;
- no redirect, substitution, dynamic command name, exact reader, or
  structured-output option appears;
- no unreviewed pipeline changes the final output shape.

A pass-through match vetoes the whole result. Multiple incompatible reducer
families are ambiguous and pass through. Parsing is for result classification
only; YARP never replays or changes the compound command.

### Archive retrieval

Add a bounded read command for exact archived ranges:

```text
yarp archive read \
  --archive-agent pi \
  --archive-account ACCOUNT \
  --archive-session SESSION \
  --archive-call CALL \
  --subject stdout \
  --lines 801:900
```

Supported subjects are `stdout`, `stderr`, and `result_text`. The command always
reads the `before` snapshot. It writes selected bytes to stdout and writes its
own diagnostics to stderr.

Line ranges are one-based and inclusive. Byte ranges are available for exact
non-text inspection. Exactly one range form is required. One request is capped
at 64 KiB; an oversized request fails before writing content and asks for a
smaller range. Payload decompression, byte length, and SHA-256 must verify
before any bytes are returned.

Keep `yarp archive restore` for restoring complete stdout and stderr to their
original streams. `archive read` serves a different purpose and does not replace
it.

The Pi result path needs an exact text snapshot that can be ranged without
loading the canonical result JSON. Extend the existing version 1 archive in
place with a `result_text` snapshot subject. Store it only when a shell result
has exactly one text content item. Derive it from the same `tool_result` event
and commit it atomically with `result/before`. Rebuild the version 1 `snapshots`
constraint during migration, verify the resulting schema, and keep one database.
Do not retain a reader for the superseded table shape.

This archive schema change requires explicit maintainer approval before
implementation. The rest of the reducer work can proceed without it, but
post-result summaries and `result_text` retrieval cannot ship until it is
approved.

### Pi integration

Keep the current wrapper path for simple `bash` and `exec_command` calls. It
preserves separate stdout and stderr and remains the preferred path.

For an unwrapped shell call, add a post-result path:

1. `tool_call` retains the original command in the existing active-call state.
2. Pi runs the original command unchanged.
3. `tool_result` commits `result/before`, `result_text/before`, and the
   provisional result.
4. Only after that commit, the extension invokes a one-shot Rust result reducer.
5. Rust analyzes the original command, validates rule packs, and reduces the
   exact text with the real status.
6. The extension returns a `content` patch only when Rust returns a valid
   compact result.
7. `tool_execution_end` stores the final result after every result hook.

The one-shot process uses a bounded length-framed stdin protocol so command text
and result content do not appear in process arguments. It is not a service and
retains no state after the request. The TypeScript side accepts only one shell
text content item, preserves `details`, `usage`, and `isError`, and passes
through images, multiple text items, malformed content, and unknown tool shapes.

When the archive is disabled, post-result reduction remains disabled because no
exact `result_text` recovery source exists. Direct wrapped commands continue to
use their normal reducers.

Use Pi's documented `tool_call`, `tool_result`, `tool_execution_end`,
`message_end`, `session_start`, and `session_shutdown` events. Use the
documented partial return from `tool_result`; do not edit session entries.

## Work plan

### Shared summary engine

- Add source line numbers and byte offsets to bounded line accumulation.
- Add bounded evidence records, fixed evidence classes, coalesced omission
  ranges, and deterministic rendering.
- Add absolute and proportional savings gates.
- Preserve the first observed line ending in generated text.
- Keep source-line limits ahead of ANSI stripping.
- Update memory-bound calculation and validation for every reducer family.
- Add chunk-boundary, binary, ANSI, long-line, and line-ending property tests.

### Search and diff summaries

- Parse reviewed search output forms without runtime regular expressions.
- Keep search diagnostics, bounded file groups, exact displayed match counts,
  and representative matches.
- Keep diff file headers, hunk headers, binary markers, changed-line counts, and
  bounded representative blocks.
- Preserve every current exact search and Git guard.
- Benchmark these reducers first because they provide most of the available
  savings.

### Test, build, log, status, and list summaries

- Add typed parsers and public fixtures for each stable output family.
- Keep explicit failure names, diagnostics, final summaries, and
  command-specific identifiers.
- Deduplicate only command-specific progress or repeated log records with exact
  rules.
- Preserve chronology for logs and source order within displayed diff, search,
  and list sections.
- Move every built-in rule away from `head_tail`; unsupported shapes become
  pass-through.

### Rule contract replacement

- Update the Rust model, source schema, JSON schemas, compiled format,
  build-time validation, and memory bounds.
- Regenerate built-in rules and indexes.
- Update `rules check`, `compile`, `verify`, `list`, and `explain` output.
- Update README examples and remove the old head-and-tail contract.
- Reject old source and compiled packs clearly. Do not silently reinterpret
  them.

### Archive range reading

- Add exact text and byte range readers with strict parsing and a 64 KiB request
  cap.
- Add `result_text` capture and the approved in-place schema migration.
- Include retrieval commands only when an archive key and a matching source span
  exist.
- Add pass-through guards so archive inspection output is never summarized
  recursively.
- Test pruned calls, missing calls, incomplete calls, corrupted payloads, binary
  payloads, line endings, and concurrent archive writers.

### Compound-result support

- Add conservative command-graph classification in Rust.
- Add the one-shot framed result-reduction command.
- Add the thin Pi client and return only documented `tool_result` patches.
- Preserve result metadata and explicit exit-code precedence.
- Pass through mixed reducer families, multiple text parts, structured content,
  archive opt-out, and every protocol failure.
- Test parallel tool calls and out-of-order completion.

### Corpus tuning

- Extend `toolcall-extractor benchmark-yarp` to report each reducer family,
  result status, output-size band, changed-result count, character removal, and
  diagnostic retention.
- Tune typed reducer code and built-in output policies against the frozen
  corpus.
- Review the 100 largest changed results in every reducer family.
- Keep private transcripts, databases, samples, and reports outside Git.
- Stop adding command coverage when the production engine clears the working
  target without violating a safety gate.

## Verification

### Required behavior tests

- Short output remains byte-for-byte exact when savings gates do not pass.
- The hard output cap always wins.
- Success and failure select from the real child status.
- Explicit stored exit codes override generic error fields.
- Stdout and stderr reducers have independent state.
- Every omitted range points to the exact archived source bytes.
- A retrieval request returns exact bytes or returns no content and a nonzero
  status.
- Archive failure restores raw output.
- Unknown, ambiguous, mixed, invalid, and guarded commands remain unchanged.
- Compiled-pack source and file digests are rechecked before use.
- Reducer output is independent of input chunk boundaries.
- Per-stream and per-result memory remains below 4 MiB.

### Corpus release gates

Run the production matcher and reducers over all 371,241 frozen shell results
and report raw counts.

- Remove at least 289,105,282 of 1,445,526,406 shell-output characters.
- Use 303,560,545 removed characters as the development target.
- Change zero current pass-through results.
- Produce zero ambiguous reductions.
- Add no result that loses every registered failure, panic, error, warning, or
  final test-summary line.
- Keep exact, structured, filename-only, count-only, NUL-delimited, and watch or
  follow cases unchanged.
- Report both shell-output reduction and reduction across all rendered tool
  output.
- Report actual token counts for the active model families as a secondary
  metric; runtime limits remain byte based and deterministic.

The corpus is a census of the stored data, so it has no sampling interval. It
may not represent future command use. Manual review and public fixtures remain
release vetoes even when the numeric target passes.

### Performance gates

- Keep built-in matching below 1 ms p95.
- Keep one stream's configured memory below 4 MiB.
- Keep direct reduction throughput above 100 MB/s on the existing benchmark
  machine.
- Keep one-shot post-result reducer startup and reduction below 20 ms p95 for a
  16 KiB result after warmup.
- Keep archive range reads below 20 ms p95 for a 64 KiB range after warmup.
- Run at least 100 measured repetitions and report median, p95, and maximum.

### Repository gates

Before completion, run:

```text
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

Exercise the extension with a temporary `pi -e` path and then reload it. The
live test must cover one unchanged short result, one indexed search summary, one
failed test summary, one exact range retrieval, one compound result, one archive
failure, and one parallel pair.

## Pi contract

- **Session state:** Pi appends its normal tool-result entry containing the
  compact text returned by the documented hook. YARP does not append, rewrite,
  or migrate Pi session entries directly.
- **Other persistent data:** the existing YARP archive gains the proposed
  `result_text` snapshot subject in place. No second database or sidecar is
  added. This change needs explicit maintainer approval before implementation.
- **Pi internals:** none.
- **Public API:** documented `tool_call`, `tool_result`, `tool_execution_end`,
  `message_end`, `session_start`, and `session_shutdown` events and their
  documented return values.

## Completion criteria

The work is complete when:

- every built-in reduction rule uses a typed summary or passes through;
- generic head-and-tail pruning is removed;
- compact results use absolute and proportional savings gates instead of a fixed
  input threshold;
- omission markers identify exact, bounded archive ranges when recovery is
  available;
- direct wrapped commands preserve stream separation and status;
- safe compound results can be reduced without changing or replaying their
  commands;
- archive, protocol, reducer, and rule failures return unchanged output;
- the production corpus benchmark clears 20% and every semantic gate;
- the README, schemas, CLI help, archive specification, and implementation
  documents match the shipped behavior;
- no Pi source, private API, session schema, daemon, network path, or executable
  rule mechanism is introduced.
