# Indexed output summaries implementation plan

Status: implementation and the in-place archive schema change were approved by
Onur Solmaz on 2026-08-01.

YARP should keep the exact command output locally and give the model a smaller,
command-specific summary when that summary is useful. The summary must show what
happened, preserve the evidence needed for the next action, and include one
copyable command for searching omitted output. A second command reads an exact
line range when search context is not enough.

This replaces broad head-and-tail pruning with typed summaries. It does not add
a fixed 10,000-character threshold. Small output stays exact whenever a summary
does not meet the rule's minimum savings requirements.

## Implemented result

The production matcher evaluated all 371,241 frozen shell results after the
final semantic pass. Of 66,998 eligible results, 29,439 changed and 10,353
matched an explicit pass-through rule. No result was ambiguous. YARP removed
306,302,222 of 1,445,526,406 shell-output characters, or 21.1897%. This is
92.2887% of eligible output and 15.7748% of all rendered tool output. The
registered diagnostic veto count was zero.

A private review covered the 100 largest changed results for each of the seven
original typed reducer families. The common-command expansion also reviewed 69
large direct results and 20 compound-command results from every newly active
rule in the corpus. Generic occurrences of `failure`, `panic`, `error`,
`warning`, and `test result` are rendered as `source_terms` and source-term
samples. A command-specific parser labels a line as a diagnostic only when its
output syntax provides stronger evidence. The final marker-length review kept
every diagnostic, outcome, and structure line; only bounded context, examples,
and source-term samples changed. The private cases and report remain outside
Git.

A 20-case held-out run with `openai-codex/gpt-5.6-terra` produced 20 valid first
`yarp search` commands. Nineteen cases recovered the requested evidence on the
first retrieval, and the remaining case recovered it on the second. The marker
uses `term|alternate` to demonstrate the search engine's unescaped alternation
syntax without adding another command or persistent instruction. The private
prompts, model responses, archive, and report remain outside Git.

The direct benchmark measured 137.49 MB/s with a 551,616-byte configured stream
bound. Built-in matching stayed below one microsecond at p95. Search over a 1
MiB source measured 9.391 ms p95, a 12 KiB exact read measured 3.752 ms p95, and
the one-shot 16 KiB result reducer measured 1.799 ms p95. Each latency result
uses 100 measured repetitions after warmup.

The 50,000-call migration rehearsal added about 212 database bytes per call,
kept every reference unique, and passed SQLite integrity checks. Restoring the
private pre-migration backup returned the archive to the original table shape
and `user_version = 1`.

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
- Archive, reducer, search, read, or Pi integration failure returns the
  unchanged primary output and never returns unverified archive bytes.
- The model never needs an account, session, database ID, snapshot subject,
  archive path, or Pi-specific argument to retrieve omitted output.
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

## Native tool truncation

Pi and Codex already limit large model-facing tool results. Pi's built-in limit
is 50 KiB or 2,000 lines, whichever comes first. Its tools normally keep the
head for file and search output or the tail for logs and command output. Codex
unified execution defaults to 10,000 output tokens, uses middle truncation, and
has a separate 1 MiB collection cap.

A collapsed or shortened TUI view is not proof that the model received the same
text. Display folding, model-facing truncation, output collection, and later
context compaction are separate layers.

The frozen corpus contains persisted agent tool results, so its shell-output
measurements already include much of this native truncation. YARP's 20% release
target is additional removal from those stored results, not a replacement for
native limits.

Native truncation remains the final emergency limit. YARP's job is different: it
uses command structure to remove repetitive text and retain useful evidence
before a blunt limit chooses a head, tail, or middle slice. For example, a 40
KiB test log fits below Pi's byte limit but can still be reduced to its
failures, diagnostics, and final totals.

YARP should run before native truncation whenever a safe command wrapper is
available. If a post-result reducer receives an already truncated result, it
must record that limitation, summarize only the bytes it can verify, and never
claim that its archive contains missing bytes. A documented complete source such
as Pi Bash's `fullOutputPath` may be used only after YARP captures and verifies
it through the existing `source_output/before` archive snapshot.

## Product behavior

For a selected command, YARP will:

1. Run the command once with the current child-process contract.
2. Capture the best available exact source and record whether it is complete.
3. Parse lines with a typed reducer for that command family.
4. Build a bounded summary containing outcome, diagnostics, structure, context,
   and examples.
5. Compare the summary with the raw output under the selected success or failure
   policy.
6. Return the raw bytes when the summary does not save enough.
7. Return the summary when it passes every guard and savings check.
8. Add one short call reference and a copyable `yarp search` example when an
   archive reference is available.
9. Let `yarp search` print copyable `yarp read` commands for exact ranges around
   returned matches.

For example, a test summary will keep failing test names, error blocks,
warnings, and final totals. It will count or omit routine passing-test progress.
A search summary will keep command diagnostics and representative matches. All
reducers track registered source terms independently. They show generic matches
as source-term samples rather than treating an arbitrary line containing the
word `error` as a typed diagnostic.

A summary may arrange sections by usefulness. It does not reorder the archived
output. Logs retain chronology, and diffs retain file and hunk order within each
displayed section.

## Non-goals

This work will not add:

- an LLM summarizer;
- network access or telemetry;
- a daemon, service, watcher, socket, or remote store;
- executable rule hooks, plugins, runtime regular expressions in rule matching,
  or replacement commands;
- a general query language, search index, ranking service, or arbitrary archive
  SQL;
- a custom Pi retrieval tool whose schema consumes model context on every turn;
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
restores the exact raw stream and preserves the child status. Wrapped streams
are complete child output and reach YARP before Pi applies its own limit.

A post-result reducer has a weaker source contract. It prefers a verified
`source_output/before` snapshot when the documented host tool provided one.
Otherwise it uses the text exposed by the host, which may already be truncated,
and commits those exact bytes as `result_text/before` only if the summary wins.
The summary and omission marker must name that source and its completeness.

The reducer does not write SQLite and does not know archive paths. It returns a
summary draft containing rendered evidence and omitted source spans. After the
archive transaction succeeds, the runner resolves the call reference and adds
the retrieval marker. No reference is shown before the exact source commits.

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
output budget. Validate `max_line_bytes` from 1 through 1,048,576,
`max_output_bytes` from 704 through 4,194,304, `min_savings_bytes` from 0 through
1,048,576, and `min_savings_basis_points` from 0 through 10,000. Reject any
combination whose calculated stream-memory bound exceeds 4 MiB.

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

### Model-facing retrieval

Expose two top-level commands:

```text
yarp search REF PATTERN
yarp read REF [SOURCE] START:END
```

`yarp search` is the normal path. `yarp read` is the exact-range escape hatch.
Do not expose the archive subcommand tree, database keys, or snapshot model in
summary text.

A generated omission marker has this shape:

```text
[yarp: omitted 2,418 lines; ref=yr_4f91d03ab8d44712a48fa8b0d671e3d2; source complete]
Search omitted output: yarp search yr_4f91d03ab8d44712a48fa8b0d671e3d2 'term|alternate'
```

The marker is emitted only when the reference resolves to the exact source used
for the summary. Without a committed source, YARP may still emit a safe summary,
but it must omit the reference and retrieval instruction.

#### Call references

`REF` is a YARP archive call reference. It is not a source agent's tool-call ID,
SQLite row ID, payload digest, path, credential, or authorization token.

Add `archive_ref TEXT NOT NULL UNIQUE` to `tool_calls`. Its external form is
`yr_` followed by 32 lowercase hexadecimal digits containing 128 random bits.
The format is deliberately boring: it is easy for models to copy, strict to
validate, and independent of every host's session and call-ID rules.

The version 1 table constraint is:

```sql
archive_ref TEXT NOT NULL UNIQUE CHECK (
    length(archive_ref) = 35
    AND substr(archive_ref, 1, 3) = 'yr_'
    AND substr(archive_ref, 4) NOT GLOB '*[^0-9a-f]*'
)
```

The snapshots subject constraint gains exactly `result_text`, plus bounded
completeness metadata for that source:

```sql
subject TEXT NOT NULL CHECK (
    subject IN (
        'input', 'result', 'result_text', 'source_output', 'stdout', 'stderr'
    )
),
source_completeness TEXT CHECK (
    source_completeness IN ('complete', 'incomplete', 'unknown')
),
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
```

Wrapped before streams and documented `source_output` snapshots are complete by
contract and need no repeated metadata. `result_text` records `complete` only
when the host proves no native truncation, `incomplete` when it reports
truncation, and `unknown` otherwise.

Generate references with SQLite `randomblob(16)` inside the call-start
transaction. Retry a unique collision at most three times, then fail the initial
archive transaction. An idempotent retry of an existing
`(session_id, source_call_id)` reuses its committed reference; it never
generates a second identity for the same call. The reference is immutable for
the life of the call and is removed only when explicit archive pruning deletes
that call. It is not reused. Possessing a reference does not bypass filesystem
permissions or grant access to another user's archive.

Migrate the existing version 1 archive in place:

1. Create the normal pre-migration backup.
2. Add a nullable `archive_ref` column.
3. Backfill every existing call with a unique value.
4. Rebuild `tool_calls` with `NOT NULL`, format checks, and a unique constraint.
5. Recreate indexes and foreign keys.
6. Verify row counts, foreign keys, archive references, payload references, and
   SQLite integrity before commit.
7. Keep `PRAGMA user_version = 1`; do not add a parallel schema or fallback
   reader.

The extension and runner continue to correlate writes with the full internal
archive key. The short reference is only for explicit read-only retrieval. It
resolves without ambient session environment, working directory, account flags,
or host-specific state, so the same command works after resume and from every
supported host adapter.

#### Canonical source selection

A call reference can own more than one snapshot. Search chooses sources without
asking the model for a subject:

1. For a wrapped call, search `stdout/before` and `stderr/before` separately.
2. Otherwise, prefer a verified `source_output/before` produced by a documented
   host full-output path.
3. Otherwise, search `result_text/before`.
4. If none exists, report that the call has no searchable text.

Never search `input`, canonical result JSON, after snapshots, or duplicate views
of the same source. Search output labels `stdout`, `stderr`, `source_output`, or
`result_text`. Wrapped streams stay separate. If the source is only the
host-exposed result and native truncation is known or cannot be ruled out, print
`source incomplete` or `source completeness unknown` instead of
`source complete`.

#### Search syntax

The default form follows the part of `rg` that models already know:

```text
yarp search REF PATTERN
yarp search REF -e PATTERN [-e PATTERN ...]
```

Support these options and no others in the first implementation:

| Option                     | Meaning                                      |
| -------------------------- | -------------------------------------------- |
| `-e`, `--regexp PATTERN`   | Add an alternative pattern.                  |
| `-F`, `--fixed-strings`    | Treat patterns as literals.                  |
| `-i`, `--ignore-case`      | Use ASCII case-insensitive matching.         |
| `-w`, `--word-regexp`      | Require ASCII word boundaries.                |
| `-v`, `--invert-match`     | Select nonmatching lines.                    |
| `-A`, `--after-context N`  | Show lines after a selected line.            |
| `-B`, `--before-context N` | Show lines before a selected line.           |
| `-C`, `--context N`        | Set both context counts.                     |
| `-m`, `--max-count N`      | Show at most N selected lines per source.    |
| `--`                       | End option parsing.                          |

Accept options before or after `REF` and the positional pattern. A pattern that
starts with `-` requires `-e` or `--`. One positional pattern and repeated `-e`
are mutually exclusive. Repeated `-e` patterns use OR semantics, matching `rg`.

Defaults are two context lines and 20 selected lines per source. Accept context
counts from 0 through 50 and match counts from 1 through 100 as canonical
unsigned decimal values. Reject duplicates that conflict, missing values,
overflow, signs, whitespace-padded numbers, and unknown options. Context is
coalesced when ranges overlap. Results keep source order, use one-based line
numbers, and separate disjoint groups with `--`. No-match behavior follows `rg`:
exit 1 with an explicit `No matches` message; matches exit 0; invalid arguments,
invalid patterns, missing references, corruption, and I/O failures exit nonzero
with a YARP diagnostic.

Use the pinned Rust `regex` crate with its linear-time engine and explicit
compiled-size limits. The standard library has no regex engine, so this is the
one justified new runtime dependency. Disable unneeded crate features, keep the
lockfile exact, and include it in normal dependency audit checks.

Match one source line at a time; do not add multiline mode, look-around,
backreferences, replacement, captures in output, file globs, recursion, or
executable predicates. Bound one pattern to 1,024 UTF-8 bytes, allow at most
eight patterns, cap the compiled regex size, and keep total regex state inside
the 4 MiB result-query memory limit. `-F` remains available when exact text is
easier and cheaper.

`-v` is safe because this command reads an immutable source and still obeys
match, context, and byte limits. It does not change the stored source or the
primary summary.

#### Search output

Search begins with a machine-generated header that states:

- the call reference;
- the source name;
- whether that source is complete;
- total selected lines;
- displayed selected lines;
- omitted selected lines;
- the active context and match limits.

Every displayed selected line includes its source name and source line number.
Use `:` between a source, selected line number, and selected text, and `-` for a
context line, matching familiar `rg -n -C` output. A complete result looks like:

```text
[yarp search: ref=yr_4f91d03ab8d44712a48fa8b0d671e3d2]
[source=stderr complete=true matches=17 showing=2 context=2]
stderr-116-before context
stderr-117-before context
stderr:118:error[E0308]: mismatched types
stderr-119-after context
stderr-120-after context
--
stderr:904:error[E0308]: expected String
[yarp search: 15 selected lines omitted]
Read exact context: yarp read yr_4f91d03ab8d44712a48fa8b0d671e3d2 stderr 116:120
Read exact context: yarp read yr_4f91d03ab8d44712a48fa8b0d671e3d2 stderr 902:906
```

The footer prints one ready-to-run exact read command for each displayed group.

Do not print archive paths, internal IDs, payload hashes, SQL values, or
unrequested result metadata.

#### Model usability

The omission marker is the just-in-time instruction surface. Do not add a
persistent system-prompt paragraph, skill requirement, custom tool schema, or
host-specific tutorial. The marker contains exactly one valid command and only
appears when retrieval is possible.

`yarp search --help` must fit within 2 KiB and lead with these examples:

```text
yarp search REF 'error|FAILED'
yarp search REF 'literal text' -F -i
yarp search REF 'warning' -v -C 3 -m 20
yarp read REF stdout 118:130
```

Usage errors must print one corrected command shape. Search output must print
copyable exact-read commands rather than asking the model to construct ranges or
source names. Stable command names, option names, defaults, headers, and error
phrasing are part of the model-facing contract and require fixture tests.

Add a bounded model usability evaluation before release. For each active model
family, present 20 held-out shortened results with only the generated marker and
a task that requires omitted evidence. At least 19 of 20 first retrieval
attempts must use a syntactically valid `yarp search` command, and every model
must recover the requested evidence within two retrieval calls. Report raw
counts by model. A failure blocks release even when the character target passes.

#### Exact reads

`yarp read` accepts a one-based inclusive line range:

```text
yarp read REF 118:130
yarp read REF stdout 118:130
```

The short form is valid only when canonical source selection yields one source.
When a wrapped call has both stdout and stderr, the source argument is required.
Valid source names are exactly those printed by `yarp search`.

Add an explicit byte form for binary or byte-exact inspection:

```text
yarp read REF SOURCE --bytes START:END
```

Byte offsets are zero-based with an exclusive end. Line and byte forms are
mutually exclusive. Text reads preserve exact bytes and line endings; generated
headers and diagnostics go to stderr so stdout contains only requested source
bytes. Binary data is never inserted into a generated summary or search result.

#### Query bounds and verification

Search and read have a 32 KiB hard stdout cap so their results stay below Pi's
50 KiB native tool cap and normal Codex output limits with room for metadata. An
exact read that would exceed the cap fails before writing stdout and prints a
smaller suggested range. Search reduces displayed groups until the whole result
fits, then reports the omitted selected-line count.

Archive payloads are read through incremental SQLite blob I/O and incremental
Zstandard decompression. YARP scans the complete selected source, verifies its
uncompressed length and SHA-256, and retains only bounded matches and context.
It emits no source bytes until verification succeeds. A checksum, decompression,
range, schema, permission, or source-selection error returns no content.

A query source line is capped at 1 MiB before regex matching. Encountering a
longer line aborts the search before output and recommends a bounded byte read;
YARP must not silently skip an unsearched part of a line or call the match count
complete. Exact line reads containing one line larger than 32 KiB also fail
before output and recommend the byte form.

Do not build a persistent search index. Current source sizes are cheap to scan,
and on-demand scanning avoids another schema, stale index repair, and retained
plaintext. Add an index only after measured read latency exceeds the release
budget on real archives.

Keep `yarp archive restore` as the operator command for restoring complete
stdout and stderr to their original streams. The model-facing commands do not
replace archive verification, statistics, pruning, or full restore.

#### Result text snapshots

The Pi result path needs exact text bytes without loading canonical result JSON.
Extend the existing version 1 archive in place with a `result_text` snapshot
subject. It is a recovery source, not a second copy of every shell result. Store
it only when a post-result summary actually wins, the shell result has exactly
one text content item, and no complete stdout, stderr, or `source_output`
snapshot can recover the summarized bytes. Derive it from the same `tool_result`
event and commit it before returning the compact result. Rebuild the version 1
`snapshots` constraint during migration, verify the resulting schema, and keep
one database. Do not retain a reader for the superseded table shape.

The `archive_ref`, `result_text`, and `source_completeness` changes share one
reviewed in-place migration. The maintainer explicitly approved this persistent
schema change for the end-to-end implementation.

### Pi integration

Keep the current wrapper path for simple `bash` and `exec_command` calls. It
preserves separate stdout and stderr and remains the preferred path.

For an unwrapped shell call, add a post-result path:

1. `tool_call` retains the original command in the existing active-call state.
2. Pi runs the original command unchanged.
3. `tool_result` commits `result/before`, any documented `source_output/before`,
   and the provisional raw result.
4. Only after that commit, the extension invokes a one-shot Rust result reducer.
5. Rust prefers a verified complete `source_output` and otherwise uses the exact
   host-exposed text supplied through the framed request with its truncation
   state.
6. Rust analyzes the original command, validates rule packs, and reduces the
   selected source with the real status.
7. When the compact result wins and no complete source snapshot exists, YARP
   commits `result_text/before` before adding a retrieval reference.
8. The extension returns a `content` patch only after the chosen recovery source
   and call reference commit.
9. `tool_execution_end` stores the final result after every result hook.

The one-shot process uses a bounded length-framed stdin protocol so command
text, source completeness, native truncation metadata, and result content do not
appear in process arguments. The compact response contains a summary draft; Rust
adds the committed call reference and exact retrieval instruction before the
extension returns the `content` patch. It is not a service and retains no state
after the request. The TypeScript side accepts only one shell text content item,
preserves `details`, `usage`, and `isError`, and passes through images, multiple
text items, malformed content, and unknown tool shapes.

When the archive is disabled, post-result reduction remains disabled because no
exact `result_text` recovery source exists. Direct wrapped commands continue to
use their normal reducers.

Use Pi's documented `tool_call`, `tool_result`, `tool_execution_end`,
`message_end`, `session_start`, and `session_shutdown` events. Use the
documented partial return from `tool_result`; do not edit session entries.

## Security and privacy

Archive references are locators, not access-control capabilities. Every command
still opens the one private local archive under its normal POSIX permission
checks. Resolve references with parameterized SQLite queries. Never accept a
path, database override, account, session, snapshot ID, or SQL fragment from the
model-facing commands.

Run search inside one read transaction. This pins a consistent set of call and
payload rows while explicit pruning or another writer proceeds through WAL.
Verify the selected reference still points to the same snapshots before
returning output.

Search valid UTF-8 text only. Strip ANSI terminal sequences with the same
bounded state machine used by reducers, render other unsafe control bytes
visibly, and match the normalized line that is shown to the model. Keep source
line numbers bound to raw archived lines. `yarp read` remains exact and may
return control or binary bytes only when explicitly requested.

A query pattern and its matched output may contain secrets. Pi archives the
`yarp search` or `yarp read` tool call under the normal archive policy. YARP
does not write query logs elsewhere, collect metrics remotely, place plaintext
mirrors on disk, or include source content in diagnostics.

Search results are untrusted tool output. Generated headers and footers use
fixed text and cannot be influenced by source lines. Source text is displayed
inside clearly delimited source groups. YARP does not claim that matching text
is safe instructions for the model.

## Version agreement and activation

The Rust binary, rule-pack contract, Pi extension, and archive schema change in
one release. At `session_start`, the Pi package compares its exact package
version with `yarp --version`. A mismatch disables archive capture, rewriting,
post-result summaries, and model-facing retrieval for that session, preserves
original tool behavior, and prints one clear warning. Do not guess capabilities
from failed commands or keep version-specific compatibility branches.

The first archive open by the new binary performs the approved in-place schema
migration under an exclusive transaction after creating and verifying the
backup. Refuse migration when the backup cannot be created, free space is
insufficient, the schema is unexpected, or integrity checks fail. Do not start
Pi tool capture until migration commits.

The final feature is enabled by default after all release gates pass. Do not
ship a permanent experimental setting or separate old and new modes. When the
archive is explicitly disabled, direct typed reducers may still produce
self-contained summaries, but they omit references; post-result summaries and
retrieval remain unavailable.

Rolling back requires stopping Pi, restoring the pre-migration database backup,
and reinstalling the previous matching Rust and Pi package versions. Do not add
runtime downgrade readers. Document that calls captured after migration are not
present in the restored backup and must be explicitly exported first if they
need to be retained.

## Delivery sequence

Implement and verify the work in this dependency order:

1. Add the bounded evidence collector and typed reducers without changing the
   archive or enabling new output.
2. Replace the rule and output-policy contract, migrate built-ins, and prove the
   direct-runner safety and corpus gates.
3. Add the reviewed archive schema migration, stable references, and
   `result_text` snapshots.
4. Add streaming archive source verification and canonical source selection.
5. Add `yarp search`, then `yarp read`, with CLI, security, model usability, and
   performance tests.
6. Add retrieval markers to direct wrapped summaries only after archive commit.
7. Add the conservative post-result classifier and one-shot Pi result reducer.
8. Run the full corpus, manual semantic review, live Pi canaries, model
   usability evaluation, migration rehearsal, and rollback rehearsal.
9. Update the archive specification, README, CLI help, rule schemas, examples,
   and release notes in the same release.

Commit coherent working slices, but do not publish a release where summaries
advertise references before search and read are available. Do not activate the
post-result path before `result_text`, completeness reporting, and failure
restoration pass their live canaries.

## Work plan

### Code boundaries

Keep responsibilities in these modules:

- `rule-pack/src/model.rs`, `source.rs`, `compiled.rs`, and `validation.rs` own
  the strict reducer and output-policy contract.
- `src/reducers/evidence.rs` owns bounded evidence classes, source spans,
  omission ranges, and rendering.
- `src/reducers/search.rs`, `diff.rs`, `test.rs`, `build.rs`, `log.rs`,
  `status.rs`, and `list.rs` own typed parsing. Do not put command-ID branches
  in the shared collector.
- `src/runner.rs` owns child execution, stream spools, real status, archive
  commit ordering, summary selection, and final emission.
- `src/archive.rs` owns schema migration, references, payload verification,
  source lookup, pruning, and exact snapshot reads.
- A new `src/archive_query.rs` owns model-facing search and range-read parsing,
  bounded matching, context collection, and rendering. It receives verified
  source readers from `archive.rs` and never opens SQLite itself.
- `src/lib.rs` owns top-level `search` and `read` CLI dispatch and stable exit
  codes.
- `src/rewrite/` owns conservative shell analysis. It does not read outputs or
  archives.
- `hooks/pi/archive-client.ts` owns archive operations. A new checked Pi client
  module owns the one-shot framed result reducer.
- `hooks/pi/yarp.ts` remains orchestration only: correlate calls, invoke Rust,
  and return documented Pi patches.
- `toolcall-extractor/src/benchmark.rs` owns private corpus measurements and has
  no live-runtime dependency.

Every new external value is checked before use. TypeScript keeps strict unknown
validation and no explicit `any`; Rust keeps `#![forbid(unsafe_code)]` and no
unchecked casts.

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

### Call references and model retrieval

- Add strict `archive_ref` creation, migration, lookup, uniqueness, format, and
  pruning behavior.
- Add on-demand `result_text` capture in the same approved in-place schema
  migration, only for winning post-result summaries without another complete
  source.
- Add canonical source selection without duplicate snapshots or merged streams.
- Implement top-level `yarp search` with the reviewed `rg`-style subset.
- Implement top-level `yarp read` for exact bounded line and byte ranges.
- Use streaming payload verification and bounded regex, match, and context
  state.
- Include one search instruction only after the referenced source commits.
- Print exact read commands from search results so the model never constructs a
  source selector or range syntax without an example.
- Add pass-through guards so `yarp search`, `yarp read`, and archive inspection
  output are never summarized recursively.
- Test old and new calls, missing and pruned references, incomplete calls,
  malformed references, regex limits, no matches, invert matching, overlapping
  context, multiple sources, corrupted payloads, binary payloads, line endings,
  output caps, concurrent archive writers, and native host truncation.

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
- Every omission marker names a committed call reference or contains no
  retrieval instruction.
- Every search match and read range points to the exact archived source bytes.
- A retrieval request returns verified bounded bytes or returns no content and a
  nonzero status.
- Models can copy the generated `yarp search` and `yarp read` commands without
  knowing archive or Pi internals.
- Held-out model usability checks meet the 19-of-20 valid first-search gate for
  every active model family and recover requested evidence within two calls.
- Archive failure restores raw output.
- Unknown, ambiguous, mixed, invalid, and guarded commands remain unchanged.
- Compiled-pack source and file digests are rechecked before use.
- Reducer output is independent of input chunk boundaries.
- Per-stream and per-result memory remains below 4 MiB.

### Corpus release gates

Run the production matcher and reducers over all 371,241 frozen shell results
and report raw counts. Treat these as incremental savings after the native
truncation already present in persisted agent results.

- Remove at least 289,105,282 of 1,445,526,406 shell-output characters.
- Count every generated heading, omission marker, call reference, and retrieval
  instruction in final output size; do not report metadata-free savings.
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
- Report follow-up search and read calls separately from primary summary
  savings; do not assume that every omission causes a retrieval.

The corpus is a census of the stored data, so it has no sampling interval. It
may not represent future command use. Manual review and public fixtures remain
release vetoes even when the numeric target passes.

### Storage and migration gates

- Rehearse migration from an untouched version 1 fixture and a copy of the live
  archive.
- Verify every old call receives one unique immutable reference and every old
  snapshot remains byte-identical.
- Keep reference-column and index growth below 256 database bytes per existing
  call, excluding the explicit migration backup.
- Store no `result_text` snapshot for unchanged results or calls with a complete
  stream or `source_output` recovery source.
- Verify result-text compression uses the existing payload policy and does not
  create an uncompressed mirror.
- Run migration interruption tests before table rebuild, during backfill, during
  index creation, and before commit; each interruption must leave either the old
  verified database or the new verified database.
- Rehearse the documented restore of the pre-migration backup with the previous
  binary and Pi package.

### Performance gates

- Keep built-in matching below 1 ms p95.
- Keep one stream's configured memory below 4 MiB.
- Keep direct reduction throughput above 100 MB/s on the existing benchmark
  machine.
- Keep one-shot post-result reducer startup and reduction below 20 ms p95 for a
  16 KiB result after warmup.
- Keep `yarp search` below 20 ms p95 for a 1 MiB compressed source with 20
  displayed matches after warmup.
- Keep `yarp read` below 20 ms p95 for a 32 KiB range after warmup.
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
failed test summary, one copied `yarp search` command, one copied `yarp read`
command, one no-match query, one compound result, one archive failure, and one
parallel pair.

## Pi contract

- **Session state:** Pi appends its normal tool-result entry containing the
  compact text returned by the documented hook. YARP does not append, rewrite,
  or migrate Pi session entries directly.
- **Other persistent data:** the existing YARP archive gains
  `tool_calls.archive_ref`, the `result_text` snapshot subject, and its
  bounded `source_completeness` value in place. No second database, search
  index, plaintext mirror, or sidecar is added. The maintainer explicitly
  approved this in-place change for this implementation.
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
- omission markers contain one short committed call reference and one copyable
  `yarp search` command when recovery is available;
- `yarp search` provides bounded `rg`-style retrieval and prints exact
  `yarp read` commands without exposing archive internals;
- the generated marker and short help text pass the held-out model usability
  gate;
- search and read state whether the archived source was complete;
- direct wrapped commands preserve stream separation and status;
- native Pi and Codex limits remain final safety nets rather than YARP's summary
  policy;
- safe compound results can be reduced without changing or replaying their
  commands;
- archive, protocol, reducer, and rule failures return unchanged output;
- the production corpus benchmark clears 20% and every semantic gate;
- the README, schemas, CLI help, archive specification, and implementation
  documents match the shipped behavior;
- no Pi source, private API, session schema, daemon, network path, or executable
  rule mechanism is introduced.
