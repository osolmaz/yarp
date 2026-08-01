# Command-aware pruning implementation plan

Command-Aware Pruning will replace YARP's single head-and-tail policy with a compiled rule engine. Each approved command will select one bounded streaming reducer before execution. Built-in rules will be embedded in the Rust binary, while users may compile their own declarative rules into one indexed local pack without rebuilding YARP.

The feature keeps YARP's existing safety contract. Unsupported or ambiguous commands run unchanged. Child exit codes remain exact, stdout and stderr stay separate, and archive failures continue to follow `docs/tool-call-archive-spec.md`. Rules contain data only. They cannot execute code, fetch remote content, expand environment values, or change the child command.

## Current baseline

YARP currently recognizes a narrow set of developer commands and applies the same limits to every accepted stream. It keeps 160 lines from the beginning, 40 from the end, and no more than 16 KiB from one line.

The complete local corpus contains 371,241 shell results. The current policy changed 3,481 results and removed 61,814,753 net characters. This proves that bounded pruning is useful, but the narrow command set and uniform reducer leave substantial repetitive output untouched.

Command-Aware Pruning will improve coverage and reduction while retaining the current safety gates. Compression volume is an optimization metric. It does not authorize a rule to ship when the rule loses information needed to understand a result.

## Outcome

The completed feature must provide all of the following:

- Built-in rule validation and compilation during the Rust build.
- Constant-time or indexed command lookup that does not scan every rule.
- Bounded streaming reducers that never collect complete command output in memory.
- Separate success and failure policies chosen from the real child status.
- Exact stdout and stderr separation.
- Explicit pass-through guards for output that must stay complete.
- A stable source rule-pack format for user extensions.
- A `yarp rules compile` command that produces one indexed binary pack.
- Explicit runtime loading of compiled packs with no project scan, network access, watcher, daemon, or background process.
- Strict validation that rejects unknown fields, invalid paths, duplicate IDs, unsupported reducer kinds, malformed patterns, and unsafe limits.
- Synthetic fixtures, property tests, fuzz targets, offline corpus benchmarks, and reviewed paired cases.
- Clear diagnostics showing which rule matched and why a command passed through.

The first implementation will replace the current pruning path in place. It will not retain a second legacy engine or a compatibility alias.

## Core invariants

These requirements apply to every rule, reducer, source pack, compiled pack, and host integration.

1. YARP must parse the original shell command conservatively before wrapping it.
2. YARP must match the child argument vector again immediately before execution.
3. A command must pass both checks and select the same action.
4. Unknown commands must pass through unchanged.
5. An ambiguous reduction match must pass through unchanged.
6. A matching pass-through guard must override every matching reduction rule.
7. A rule must never modify the child argument vector, working directory, environment, standard input, exit code, or signal result.
8. Stdout and stderr must be captured and reduced independently. Archiving, restoration, and emission must also keep them separate.
9. Runtime memory must have a bound set by rule limits and independent of input size.
10. A short result must remain byte-for-byte exact when reduction does not meet its minimum savings requirement.
11. Built-in rule errors must fail the build.
12. External rule errors must stop that pack from participating and leave commands unchanged.
13. Rule processing must be deterministic for the same command, output bytes, exit status, engine version, and rule packs.
14. Rule files and compiled packs must never contain executable hooks.
15. YARP must not access the network or start a persistent process.

## Source pack structure

A source pack is the format people edit and share. It has one required `pack.json` file and an explicit list of rule files.

```text
example-rules/
├── pack.json
└── rules/
    ├── git/
    │   └── status.json
    └── tests/
        └── example-test.json
```

`pack.json` is the only entry point. Files not listed by the manifest are ignored. The loader does not search the directory for extra rules.

A minimal pack contains one rule:

```json
{
  "schema_version": 1,
  "id": "example-rules",
  "rules": [
    "rules/tests/example-test.json"
  ]
}
```

The source format is the stable authoring contract. A YARP release that supports source schema version 1 must either accept a valid version 1 source pack or report a specific validation error.

## Manifest fields

| Field | Required | Type | Meaning |
| --- | --- | --- | --- |
| `schema_version` | Yes | integer | Source-pack schema version. Version 1 is the only accepted value. |
| `id` | Yes | string | Stable pack identifier used in diagnostics. |
| `rules` | Yes | string array | Explicit paths to rule files in source order. |

Unknown manifest fields are errors.

### `schema_version`

`schema_version` must equal `1`. YARP must reject a missing, fractional, negative, zero, or unsupported value. The version applies to the manifest and every listed rule, so rule files do not repeat it.

### Pack `id`

A pack ID must satisfy all of these rules:

- Length from 1 through 128 ASCII bytes.
- Lowercase letters, numbers, dots, underscores, or hyphens only.
- A letter or number at both ends.
- No consecutive separators.

Valid IDs include `team-ci`, `rust-project`, and `example.tools`. Invalid IDs include `Team`, `-local`, `local-`, and `two..parts`.

### `rules`

`rules` must contain between 1 and 10,000 unique paths. Each path must:

- Use `/` as its separator.
- End in `.json`.
- Be relative to the source-pack root.
- Name a regular file.
- Stay inside the source-pack root after canonical resolution.

Absolute paths, backslashes, empty segments, `.` segments, `..` segments, NUL bytes, and symlinks are invalid. The compiler reads files in manifest order but produces a deterministic index independent of filesystem enumeration order.

The manifest has no inheritance, includes, remote references, implicit discovery, environment expansion, or merge behavior.

## Minimal reduction rule

The following rule recognizes `example-test`, removes routine progress lines, and keeps larger sections when the command fails.

```json
{
  "id": "tests/example-test",
  "match": {
    "program": ["example-test"]
  },
  "action": "reduce",
  "reducer": {
    "kind": "line_filter",
    "strip_ansi": true,
    "drop": [
      {
        "kind": "prefix",
        "value": "progress:",
        "trim": "start"
      }
    ]
  },
  "success": {
    "head_lines": 12,
    "tail_lines": 10,
    "max_line_bytes": 16384,
    "max_output_bytes": 16384,
    "min_savings_bytes": 120
  },
  "failure": {
    "head_lines": 18,
    "tail_lines": 18,
    "max_line_bytes": 16384,
    "max_output_bytes": 32768,
    "min_savings_bytes": 120
  }
}
```

## Rule fields

| Field | Required | Type | Meaning |
| --- | --- | --- | --- |
| `id` | Yes | string | Unique rule identifier. |
| `match` | Yes | object | Parsed argument conditions. |
| `action` | Yes | enum | `reduce` or `passthrough`. |
| `reducer` | Conditional | object | Reduction behavior for a `reduce` rule. |
| `success` | Conditional | object | Limits for exit status zero. |
| `failure` | Conditional | object | Limits for nonzero or signal exit. |

Unknown fields are errors at every object level.

### Rule `id`

A rule ID must contain 1 through 128 lowercase ASCII bytes. It may use letters, numbers, dots, underscores, hyphens, and `/`. A slash separates categories such as `tests/example-test`. Empty segments, consecutive separators, and separators at either end are invalid.

Rule IDs must be unique across built-ins and every configured external pack. A duplicate ID disables the conflicting external pack. Duplicate built-in IDs fail the build.

### `action`

`action` accepts two values.

| Value | Behavior |
| --- | --- |
| `reduce` | Run the declared reducer under the success or failure limits. |
| `passthrough` | Preserve the stream exactly and block broader reduction rules. |

A `reduce` rule requires `reducer`, `success`, and `failure`. A `passthrough` rule forbids those fields.

Pass-through rules protect exact inspections and unusual command forms. If a pass-through rule and one or more reduction rules match the same command, pass-through wins. More than one matching reduction rule is an ambiguity, so the command runs unchanged.

The first version has no numeric priority. Rules must describe disjoint command forms. This avoids magic ordering values and makes accidental overlap fail safely.

## Command matching

Rules match parsed arguments. They never search unparsed shell source text for a command name.

```json
{
  "match": {
    "program": ["npm", "pnpm"],
    "argv_prefix": ["run", "test"],
    "argv_contains_all": ["--reporter=dot"]
  }
}
```

| Field | Required | Default | Meaning |
| --- | --- | --- | --- |
| `program` | Yes | None | Accepted executable names. |
| `argv_prefix` | No | `[]` | Exact argument sequence after the executable. |
| `argv_contains_all` | No | `[]` | Tokens that must all appear after the executable. |

Unknown matcher fields are errors.

### Program names

`program` must contain between 1 and 32 unique names. A name must contain 1 through 128 ASCII bytes and must not contain whitespace, `/`, `\`, a NUL byte, or shell metacharacters.

Matching is exact and case-sensitive. Version 1 does not reduce commands invoked through an absolute executable path, an alias, or an unknown wrapper. Supporting one of those forms requires a separate reviewed matcher capability.

### Argument values

Each argument condition contains at most 64 unique tokens. A token may contain up to 1,024 Unicode scalar values and must not contain a NUL byte.

`argv_prefix` starts at argument index 1. An empty prefix matches every argument list for the selected program. `argv_contains_all` checks complete tokens after the executable and does not perform substring matching.

Multiple alternatives should use separate rules. Version 1 does not provide OR groups, negative matchers, regular expressions, shell fragments, or arbitrary boolean expressions in command matching.

### Shell source checks

The existing conservative shell parser remains the first gate. It rejects control operators, redirections, command substitutions, variable expansion outside single quotes, comments, embedded newlines, unmatched quotes, and incomplete escapes.

The rewrite path uses the parsed words only to decide whether to emit a wrapper command. The runner receives the operating system argument vector and repeats rule selection before starting the child. If the source-text selection and child-argv selection disagree, YARP runs the child with pass-through output.

## Reducer kinds

The rule format has a closed set of reducer kinds. Version 1 supports:

| Kind | Location | Purpose |
| --- | --- | --- |
| `head_tail` | Declarative engine | Keep bounded beginning and ending sections. |
| `line_filter` | Declarative engine | Filter lines before bounded retention. |
| `cargo_test` | Typed Rust module | Parse Cargo test output. |
| `git_diff` | Typed Rust module | Keep patch structure and bounded changed-line context. |
| `git_status` | Typed Rust module | Summarize repository status groups. |
| `search` | Typed Rust module | Bound search matches, including very long single lines. |

A rule chooses a reducer kind. Reducer selection must not depend on the rule ID. Adding a reducer kind requires Rust code, an enum variant, source-schema support, build-time validation, fixtures, fuzz coverage, and a YARP release.

The specialized kinds accept only `kind` in source schema version 1. Their behavior belongs to typed Rust modules. Open-ended rule fields are forbidden.

### `head_tail`

A head-tail reducer has this shape:

```json
{
  "reducer": {
    "kind": "head_tail"
  }
}
```

It retains lines under the selected output policy and inserts one omission marker when it removes bytes or lines.

### `line_filter`

A line-filter reducer may remove ANSI control sequences, drop matching lines, or keep only matching lines.

```json
{
  "reducer": {
    "kind": "line_filter",
    "strip_ansi": true,
    "drop": [
      {
        "kind": "prefix",
        "value": "Compiling ",
        "trim": "start"
      }
    ],
    "keep": [
      {
        "kind": "contains",
        "value": "test result:"
      }
    ]
  }
}
```

| Field | Required | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | Yes | None | Must equal `line_filter`. |
| `strip_ansi` | No | `false` | Remove recognized ANSI terminal sequences before matching and retention. |
| `drop` | No | `[]` | Remove lines that match any listed pattern. |
| `keep` | No | `[]` | Retain only lines that match at least one listed pattern. |

A reducer may have at most 256 combined `drop` and `keep` patterns. A line is first checked against `drop`. A dropped line never reaches `keep`. When `keep` is nonempty, every remaining line that does not match it is omitted. The reducer then applies the selected head, tail, line-byte, and total-byte limits.

Version 1 does not support declarative counters, adjacent deduplication, output templates, replacement strings, or executable callbacks. Typed reducers may emit fixed factual summaries when their tests define the exact format.

## Line patterns

A line pattern has the following fields:

| Field | Required | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | Yes | None | One of `exact`/`prefix`/`suffix`/`contains`. |
| `value` | Yes | None | Nonempty byte sequence encoded as a JSON string. |
| `case` | No | `sensitive` | `sensitive` or `ascii_insensitive`. |
| `trim` | No | `none` | `none`, `start`, or `both`. |

`value` must contain 1 through 4,096 UTF-8 bytes and no NUL byte. Literal matching operates on bytes. ASCII-insensitive matching folds only bytes in `A` through `Z`; it does not perform locale or Unicode case conversion.

`trim` removes ASCII spaces and tabs from the selected edge before matching. It does not change the retained output bytes.

Version 1 deliberately omits regular expressions. Exact, prefix, suffix, and contains patterns cover routine progress filtering with predictable linear work. Output needing richer parsing belongs in a typed reducer.

## Output policy

Every reduction rule has a success policy and a failure policy.

```json
{
  "success": {
    "head_lines": 12,
    "tail_lines": 10,
    "max_line_bytes": 16384,
    "max_output_bytes": 16384,
    "min_savings_bytes": 120
  }
}
```

| Field | Range | Meaning |
| --- | ---: | --- |
| `head_lines` | 0 to 10,000 | Maximum retained lines from the beginning. |
| `tail_lines` | 0 to 10,000 | Maximum retained lines from the end. |
| `max_line_bytes` | 256 to 1,048,576 | Maximum retained bytes from one line. |
| `max_output_bytes` | 1,024 to 16,777,216 | Hard limit for the rendered stream. |
| `min_savings_bytes` | 0 to 1,048,576 | Minimum required reduction before replacing short raw output. |

All fields are required. Values must be integers. Unknown fields are errors.

The total output includes retained bytes, fixed summaries, line-truncation markers, and the omission marker. The compiler rejects a policy whose fixed output can consume the full budget before any source bytes are retained.

Failure limits should usually be at least as large as success limits. A smaller failure limit requires a dedicated fixture that proves the expected failure evidence remains present. The validator reports this case prominently even when the rule is otherwise valid.

### Minimum savings

YARP keeps a short raw stream in a bounded pass-through buffer. The buffer may grow only to:

```text
max_output_bytes + min_savings_bytes
```

At finish time, YARP emits the reduced form only when it saves at least `min_savings_bytes`. Otherwise it emits the exact buffered stream.

Once raw input exceeds that threshold, the raw pass-through buffer is discarded. A valid reducer must then produce output within `max_output_bytes`, which guarantees the required savings. Archive capture remains separate and may still hold the exact raw stream in its private spool.

This gate prevents markers and summaries from expanding short output.

## Stream processing order

The declarative engine processes each stream in this order:

1. Read a bounded byte chunk.
2. Split complete lines without assuming UTF-8.
3. Apply the per-line byte limit.
4. Remove recognized ANSI sequences when requested.
5. Evaluate `drop` patterns.
6. Evaluate `keep` patterns when present.
7. Feed retained lines into the bounded head and tail state.
8. Count omitted source lines and omitted source bytes.
9. Choose the success or failure policy after the child exits.
10. Render retained bytes and one bounded omission marker.
11. Apply the minimum-savings decision.

Typed reducers use the same stream and budget components, including marker handling and the minimum-savings gate. A typed reducer may classify lines or keep small parser state, but it may not bypass the hard byte limit.

## Byte and line behavior

YARP treats child streams as bytes. Pass-through output is byte-for-byte exact. Reduced output follows these rules:

- Retained line bytes keep their original line endings.
- A synthesized marker uses the first observed line-ending style, or `\n` when no ending was observed.
- A line longer than `max_line_bytes` keeps a bounded prefix and receives a line-truncation marker.
- Invalid UTF-8 is valid stream data and remains eligible for literal byte matching.
- ANSI removal is a bounded byte-state machine and must handle an escape sequence split across read chunks.
- A reducer must not normalize tabs, spaces, Unicode, or line endings.
- Empty stdout and empty stderr remain empty.

A one-line result is still subject to `max_line_bytes` and `max_output_bytes`. Typed search reducers should retain useful match boundaries even when the result has one line.

## Success and failure selection

The reducer learns the final child status only after output capture. It therefore retains enough bounded state for both policies while the process runs. The implementation may share state by using the larger head/tail/line/byte requirements, then render the selected policy at finish time.

Exit status zero selects `success`. A nonzero code or signal-derived status selects `failure`. YARP preserves the original status after rendering.

A policy may request more failure context without writing raw output to disk. A rule that requires complete failure output should use a pass-through guard for that command form.

## Compiled pack

`yarp rules compile` converts a source pack into one local `.yrp` file. The `.yrp` file is a derived index. Authors edit the source pack.

```sh
yarp rules check ./example-rules
yarp rules compile ./example-rules --output ./example-rules.yrp
yarp rules verify ./example-rules.yrp
```

Compilation must be deterministic. The same source bytes and compiler version must produce the same pack bytes.

### Source digest

The compiler computes one SHA-256 digest over a domain-separated sequence containing:

1. The exact `pack.json` bytes.
2. Each listed relative path in manifest order.
3. The exact bytes of each listed rule file.

Lengths precede every path and file body. No newline or JSON normalization occurs. A path reorder or whitespace change therefore changes the digest.

### Binary header

The header section contains:

- Eight magic bytes identifying a YARP rule pack.
- A compiled-format version.
- Minimum and maximum supported engine ABI versions.
- Header length.
- Source-pack ID length and bytes.
- Source digest.
- Rule count.
- Program-index offset and length.
- Rule-record offset and length.
- Header-and-index SHA-256 digest.

The digest is computed with its own 32-byte field set to zero. Integers use fixed-width little-endian encoding. Every offset and length is checked for overflow, file bounds, overlap, alignment requirements, and configured limits before YARP allocates or seeks.

### Program index

The program index is sorted by exact program bytes. One program entry points to a contiguous candidate list. A rule with several program names creates one candidate reference under each name without duplicating its rule record.

Candidate entries contain only the data needed to reject most rules quickly:

- Rule-record offset and length.
- Rule-record digest.
- Action.
- Argument-prefix length.
- Number of required argument tokens.

The runtime uses binary search to locate one program bucket. It reads only that bucket and the candidate rule records needed for the supplied argv. Total rule count does not affect line processing.

### Rule records

A rule record contains the rule ID, action, matcher, reducer kind and settings, plus success and failure policies. Human descriptions and source paths do not enter runtime records.

Each record has its own SHA-256 digest. The runtime verifies the header and index plus every selected record before use. `yarp rules verify` reads and verifies the whole pack.

### Limits

A source or compiled pack must stay within these limits:

| Item | Limit |
| --- | ---: |
| Rules | 10,000 |
| Source files | 10,001 including the manifest |
| One source file | 64 KiB |
| Combined source bytes | 32 MiB |
| Compiled pack | 64 MiB |
| Programs per rule | 32 |
| Argument tokens per matcher field | 64 |
| Line patterns per rule | 256 |

The compiler checks the conservative compiled-size upper bound before writing. The runtime checks the actual file size before reading its header.

### Writes and permissions

Compilation writes a temporary file beside the requested output, flushes it, syncs it, and renames it atomically. A new file uses mode `0600` on POSIX systems under `umask 077`. YARP does not change an existing parent directory's permissions.

YARP writes a compiled pack only after an explicit `rules compile` command. It does not create an automatic cache.

## Built-in rule compilation

Built-in source rules live in the repository and use the same source schema as external rules. The Rust build validates them and generates a static registry in `OUT_DIR`.

The build must fail for:

- Invalid JSON or unknown fields.
- Invalid IDs, paths, names, patterns, or limits.
- Duplicate pack or rule IDs.
- Unsupported reducer kinds.
- Missing fixture coverage.
- Exact duplicate matchers.
- A known matcher overlap that can select two reduction rules.
- A pass-through rule that has reducer fields.
- A reduction rule missing either output policy.
- Generated output that differs across identical builds.

The generated Rust contains sorted static matcher and rule tables. Runtime code does not parse built-in JSON, walk directories, or compile patterns.

The build script must emit `cargo:rerun-if-changed` for the manifest, every listed rule, every fixture, and the source schema. It must not access the network or read files outside the repository rule roots.

## External pack loading

Built-in rules are always available without file I/O. External packs participate only when the caller supplies them explicitly.

The Rust CLI accepts repeated `--rule-pack PATH` arguments. It may also read `YARP_RULE_PACKS` through the operating system path-list parser. An empty path entry is invalid.

The direct Rust command never searches the current directory for a pack. The Pi extension may pass the conventional project pack `.yarp/rules.yrp` only when all of these conditions hold:

- Pi reports that the project is trusted.
- The path is a regular file beneath the trusted project root.
- The path does not resolve through a symlink outside that root.
- The file passes header, ABI, and size checks, along with index and selected-record checks.

Pack IDs and rule IDs must be unique across all loaded packs. A conflicting external pack is disabled as a whole. Built-in rules remain available.

### Rewrite and run agreement

`yarp rewrite` and `yarp run` are separate processes. A compiled pack could change between them.

The rewritten command therefore carries each external pack path and source digest. Before spawning the child, `yarp run` reopens the packs, verifies those digests, and repeats rule selection against the child argv. A missing, changed, incompatible, or corrupt pack causes pass-through execution. It must never cause YARP to apply a different rule silently.

The wrapper command must shell-quote pack paths, digests, archive identifiers, and every YARP-owned argument. The original command text remains unchanged after `--`.

## Registry and matcher implementation

The rule registry has two sources:

```rust
struct RuleRegistry {
    builtin: &'static BuiltinRegistry,
    external: Vec<CompiledPack>,
}
```

`BuiltinRegistry` points to generated static data. `CompiledPack` holds an open file and validated header plus the small program index needed for lookup. It does not load every rule record.

Selection follows a fixed algorithm:

1. Find exact program buckets in built-in and external indexes.
2. Test `argv_prefix` and `argv_contains_all` for each candidate.
3. Collect matching pass-through rules.
4. Collect matching reduction rules.
5. Return pass-through when any pass-through rule matched.
6. Return the one reduction rule when exactly one matched.
7. Return ambiguity when several reduction rules matched.
8. Return unsupported when none matched.

The program index uses sorted slices and binary search. Candidate lists remain small because they contain rules for one executable only. The matcher performs no output inspection and allocates no strings in the built-in-only path.

## Reducer implementation

Reducers implement one internal byte-stream contract:

```rust
trait StreamReducer {
    fn push(&mut self, chunk: &[u8]);
    fn finish(self, status: ChildStatus) -> ReducedStream;
}
```

The real trait may return bounded errors during initialization, but `push` should be total after the child starts. A malformed output byte sequence is data and must not become a reducer error.

Static dispatch is preferred. A `ReducerState` enum may hold every built-in reducer state and use an exhaustive match to call it. YARP must not use a rule-ID chain or load reducer code dynamically.

Shared components should include:

- A chunked line splitter.
- An ANSI state machine.
- Literal line-pattern matching.
- A bounded head buffer.
- A bounded tail ring.
- A short raw pass-through buffer.
- Byte and line counters.
- Omission and line-truncation markers.
- Budget-aware rendering.

The implementation should reuse allocated buffers and avoid one allocation per input line. It must keep no state proportional to total input bytes or total input lines.

## Process runner

The runner keeps the current process model. It spawns the child directly from argv, inherits stdin, and captures stdout and stderr on separate threads so one full pipe cannot block the other.

Each capture thread owns one reducer state and one optional archive spool. The parent waits for the child, converts signal termination to the existing status convention, then asks each reducer to render with the real status.

The runner must preserve these behaviors:

- Child stdin remains inherited.
- Stdout bytes return only on stdout.
- Stderr bytes return only on stderr.
- The child exit code or signal-derived code remains unchanged.
- Archive spool failure restores exact raw bytes to the original stream.
- Archive transaction failure restores exact raw streams for wrapped commands.
- An output write failure is reported and never converted into child success.

A reducer initialization failure before spawn causes pass-through execution. Reducers must not have recoverable parse failures after spawn. Specialized parsers should fall back internally to bounded head-tail behavior when an output form is unknown.

## Archive integration

Command-aware output requires no archive schema change. Existing snapshots retain the same meaning:

- `stdout/before` and `stderr/before` contain exact child bytes.
- `stdout/after` and `stderr/after` contain the selected reducer output.
- `result/before` contains the Pi result before YARP processing.
- `result/after` contains the final result returned to Pi.

The archive remains independent from rule-pack storage. Compiled packs do not enter SQLite. Archive verification does not require the rule pack that produced an old result.

An optional future archive field could record a rule ID, but version 1 does not need it. The compiled output and before/after snapshots already provide the required runtime record. Any archive schema addition requires a separate specification update and explicit approval.

## Pi integration

The Pi extension remains thin. It uses the documented `tool_call` event to ask the Rust binary whether a supported shell command should be wrapped. The Rust binary owns command matching, pack validation, process execution, stream reduction, and status preservation.

The extension may discover `.yarp/rules.yrp` only after `ctx.isProjectTrusted()` returns true. It passes the resolved path explicitly to `yarp rewrite`; the Rust command performs its own path and pack checks.

The extension must continue to fail open when rewrite exits nonzero, times out, is killed, prints no wrapper command, or reports an incompatible external pack. Existing archive lifecycle hooks remain unchanged.

Contract impact:

- **Session state:** no new custom entries, fields, labels, or session mutations. Normal Pi records still contain the command and result that actually ran.
- **Other persistent data:** an external `.yrp` file exists only when a user explicitly runs `yarp rules compile`. YARP creates no automatic cache.
- **Pi internals:** none.
- **Public Pi API:** documented `tool_call`, tool execution, `tool_result`, message, session lifecycle, `ctx.isProjectTrusted()`, and `pi.exec()` APIs only.

## Command line

The feature adds these commands:

```text
yarp rules check <source-pack>
yarp rules compile <source-pack> --output <compiled-pack>
yarp rules verify <compiled-pack>
yarp rules list [--rule-pack <path>]...
yarp rules explain [--rule-pack <path>]... -- <command> [arguments...]
```

### `rules check`

`check` validates the manifest, every rule, path containment, matcher conflicts, reducer fields, policies, and fixture references. It prints no source output because source packs contain configuration only.

The command exits nonzero on every schema or semantic error. Warnings are reserved for unusual but valid choices such as a smaller failure budget.

### `rules compile`

`compile` performs all `check` work, builds the deterministic program index and rule records, verifies the completed bytes, and writes the output atomically.

The output path is required. YARP does not choose or persist a cache location.

### `rules verify`

`verify` checks the complete compiled pack, including size, magic, format version, ABI range, offsets, lengths, sort order, duplicate IDs, index references, and all digests.

### `rules list`

`list` prints pack IDs, rule IDs, actions, reducer kinds, and command matchers. It does not execute commands or inspect output.

### `rules explain`

`explain` selects a rule for the supplied argv and prints:

- Pack and rule ID.
- Selected action.
- Reducer kind and output limits when applicable.
- Matching fields.
- Any ambiguity or pass-through reason.

It does not run the child. A machine-readable JSON mode should use a stable checked schema so tests and editor tooling can consume it.

## Diagnostics

Normal wrapped commands should print no rule diagnostics. Diagnostics belong in explicit rule commands, test failures, or extension warnings about disabled packs.

Errors must name the pack and bounded source location without printing unrelated file content. Examples include:

```text
rules/tests/example-test.json: reducer.drop[2].kind: unknown value
rules/git/status.json: success.max_output_bytes: must be at least 1024
example-rules.yrp: engine ABI 4 is not supported by this YARP build
```

A runtime ambiguity should identify matching rule IDs in `rules explain`. During normal command execution it should quietly pass through unless an explicit debug setting is active.

## Rule fixtures

Every built-in rule requires committed synthetic fixtures. External pack authors may include fixtures and should use the same format, but runtime loading ignores them.

A fixture records:

- A unique fixture ID.
- The expected rule ID or pass-through outcome.
- Exact argv.
- Exit status.
- Input stdout bytes.
- Input stderr bytes.
- Expected stdout bytes.
- Expected stderr bytes.
- Expected omission and truncation counts.

Binary bytes use base64 with an explicit encoding field. Text fixtures use UTF-8 strings. Unknown fixture fields are errors.

Each built-in reduction rule must cover at least:

- One positive command match.
- One nearby negative match.
- One short exact pass-through result.
- One successful reduction.
- One failed-command reduction.
- One important middle line.
- One very long line.
- Empty stdout and stderr.
- Separate stdout and stderr content.
- Invalid UTF-8 when the command can emit arbitrary bytes.

Pass-through guards need a positive guard case and proof that a broader reduction rule would otherwise match.

## Rule approval

A new rule moves through three states outside the runtime format:

1. **Observed:** corpus evidence identifies repetitive output or a safety gap.
2. **Recommended:** synthetic fixtures and reviewed paired cases support the proposed behavior.
3. **Approved:** a maintainer explicitly accepts the named rule for built-in runtime use.

Only approved rules enter the built-in manifest. Source files do not carry a mutable approval flag, and the runtime never promotes a rule from benchmark results.

Compression reports must show original and final characters, changed-result counts, absolute savings, and the share of all shell output. Reviewers must also inspect whether the reduced result preserves the information needed for the next action.

## Initial rule set

The first release should preserve current command coverage before adding new reducers. The sequence should be:

1. Express the current allowlist as built-in rules using `head_tail` with the current 160-line head, 40-line tail, and 16 KiB line limit.
2. Add explicit pass-through guards for exact content inspection forms.
3. Replace eligible generic rules with typed reducers for Cargo tests, Git status, Git diff, and routine search results.
4. Add tested reducers for pytest, Go tests, .NET build and test output, and approved package-manager scripts.
5. Consider additional command families only after command safety review and paired output review.

The generic unknown-command fallback remains absent. Expanding the reducer catalog does not weaken command matching.

## Offline evaluation

`toolcall-extractor benchmark-yarp` must call the same matcher and reducer library used by the live runner. It must not retain a second pruning implementation.

The benchmark reads recorded commands, status metadata when available, and separate output streams when available. Results with uncertain command or status information must be labeled. The report should include:

- Total shell results and characters.
- Supported, pass-through, ambiguous, and unsupported counts.
- Changed-result count.
- Net and gross bytes and characters removed.
- Reduction by rule, command family, agent, and success class.
- Results that grew and the number of added bytes.
- Results that hit line or byte limits.
- Rule conflicts and disabled external packs.
- Processing throughput and peak memory.

Real commands and outputs stay in the private database. Committed reports use aggregates and sanitized structural examples only.

## Performance contract

The implementation must measure performance before enabling the new engine by default. Benchmarks run on the same machine against the current pruner and the new engine.

### Matching

Measure these cases independently:

- Built-ins only.
- One warm external pack with 1,000 rules.
- One cold external pack with 1,000 rules.
- Several packs totaling 10,000 rules.
- Program buckets containing 1/10/100/1,000 candidates.

The initial acceptance targets are:

- Built-in match p95 below 100 microseconds, excluding process startup.
- Warm external pack open-and-match p95 at or below 5 milliseconds for 1,000 validated rules.
- Match work proportional only to the selected program bucket.
- No allocations in the built-in lookup path after registry construction.

The 5 millisecond threshold is a proposed operational limit. If measurements show that it is too strict or too loose, the report must state the absolute effect before the threshold changes.

### Stream reduction

Measure 1 MiB, 100 MiB, and 1 GiB synthetic streams with short lines, long lines, ANSI sequences, invalid UTF-8, and high filter-match rates.

The new engine must:

- Keep memory within the configured stream budgets plus a documented fixed overhead.
- Avoid throughput more than 10% below the current head-tail reducer on equivalent work.
- Keep rule matching outside the per-line loop.
- Avoid work proportional to rules for each output chunk.
- Report median and p95 timing plus peak resident memory across repeated runs.

A specialized reducer may be slower than head-tail when it removes substantially more output, but the report must show the absolute CPU cost and saved bytes. An unmeasured metric lead cannot authorize added runtime complexity.

### Process model

Version 1 uses one short-lived YARP wrapper per command. It does not add a server, daemon, socket, lock service, watcher, or resident worker.

A persistent process may be reconsidered only if the external-pack path exceeds 5 milliseconds at p95 under a warm filesystem cache and that delay materially affects real command wall time. Any such proposal requires a separate design covering authentication, permissions, lifecycle, and version agreement. It must also cover crashes and upgrades plus fallback behavior. It is outside this plan.

## Memory contract

For each stream, retained memory must be bounded by a function of the selected policies:

```text
short raw buffer
+ largest head budget
+ largest tail budget
+ largest line buffer
+ fixed reducer state
+ fixed rendering reserve
```

The implementation must calculate a conservative upper bound before starting the child. It must reject a rule whose bound overflows `usize` or exceeds the engine's configured per-stream maximum.

The runner handles stdout and stderr concurrently, so process-level calculations must include both streams and optional archive spool buffers. Disk-backed archive spools do not count as retained output memory, but their small I/O buffers do.

## Security and trust

Rule source and compiled packs are untrusted input until validation finishes.

The implementation must enforce these boundaries:

- No executable code, dynamic library, script hook, shell fragment, or replacement command in a rule.
- No network URL or remote include.
- No environment interpolation.
- No absolute or escaping source paths.
- No source symlink traversal.
- No automatic project discovery in the Rust CLI.
- No project pack use before Pi trust.
- No unchecked offset, length, integer conversion, or allocation from compiled bytes.
- No unsafe Rust.
- No backtracking regular-expression engine because version 1 has no regex field.
- No command selection from output text.
- No output merging between stdout and stderr.

A malicious rule can request the largest allowed output budget but cannot execute code or widen command matching beyond its declared program and argv conditions. A rule pack cannot add shell control syntax because the shell parser remains a separate gate.

## Repository layout

The implementation should use this layout:

```text
yarp/
├── rules/
│   ├── pack.json
│   ├── schema/
│   │   ├── pack.schema.json
│   │   ├── rule.schema.json
│   │   └── fixture.schema.json
│   ├── builtin/
│   │   ├── git/
│   │   ├── tests/
│   │   └── search/
│   └── fixtures/
├── src/
│   ├── rules/
│   │   ├── mod.rs
│   │   ├── model.rs
│   │   ├── source.rs
│   │   ├── compiler.rs
│   │   ├── pack.rs
│   │   ├── registry.rs
│   │   └── matcher.rs
│   ├── reducers/
│   │   ├── mod.rs
│   │   ├── common.rs
│   │   ├── head_tail.rs
│   │   ├── line_filter.rs
│   │   ├── cargo_test.rs
│   │   ├── git_diff.rs
│   │   ├── git_status.rs
│   │   └── search.rs
│   ├── rewrite.rs
│   └── runner.rs
├── hooks/pi/
├── toolcall-extractor/
└── docs/
    └── command-aware-pruning-implementation-plan.md
```

The root `yarp-cli` library remains the only live pruning implementation. `toolcall-extractor` depends on that library for benchmarking. A separate rule-engine crate should be added only if another production consumer needs an independent release boundary.

## Implementation stages

### Baseline and harness

Freeze the current rewrite and runner behavior in tests, together with archive and benchmark behavior. Add criterion-free Rust microbench binaries or stable test harnesses that can measure matcher time, stream throughput, and peak retained bytes without adding a runtime benchmark dependency.

Record current results on the synthetic benchmark matrix and the private corpus. These measurements are the baseline for later stages.

### Source models and validation

Add strict Serde models with `deny_unknown_fields` at every object level. Implement ID and path validation, together with token, pattern, and limit checks. Add source-pack containment checks and deterministic source hashing.

Create JSON Schemas that match the Rust validator. Add tests proving that unknown fields fail in manifest and rule objects, including matcher, reducer, pattern, and policy objects. Review the schema manually after Schemator suggestions; automated simplification does not decide product semantics.

### Built-in registry

Move the existing hard-coded allowlist into source rules using `head_tail`. Add build-time generation of static sorted tables and compile-fail tests for malformed built-ins.

At the end of this stage, live behavior must match the current release on every existing fixture. There is one runtime engine and no legacy branch.

### Streaming core

Extract the line splitter, bounded head, tail ring, short raw buffer, marker renderer, and budget accounting from `runner.rs`. Keep stdout and stderr capture independent.

Add `ReducerState` and the shared reducer contract. Implement `head_tail` first and prove byte-for-byte parity for current accepted commands.

### Declarative filtering

Implement literal patterns, ANSI stripping, and `line_filter`. Add fixture-driven source compilation and runtime records. Prove that pattern count affects only the selected rule.

### Typed reducers

Implement one typed reducer at a time. Each reducer begins as a candidate with synthetic fixtures and offline paired evidence. Add it to the built-in pack only after explicit approval.

Typed reducers share budget and rendering components. They must fall back internally to bounded head-tail behavior when they encounter an unknown output form.

### Compiled external packs

Implement deterministic serialization, full verification, indexed reads, explicit CLI pack paths, and digest agreement between rewrite and run. Add corrupt-header/index/record tests, size-limit and ABI tests, plus changed-between-process tests.

No automatic cache or project scan is added in this stage.

### Pi project packs

Update the extension to detect the conventional project pack only after project trust. Preserve the two-second rewrite timeout and fail-open behavior. Add TypeScript tests for untrusted projects, missing packs, invalid packs, changed packs, disabled YARP, and both supported shell tool names.

### Offline benchmark integration

Replace the extractor's generic `prune_bytes` benchmark call with command-aware selection and reduction. Add aggregate reports by rule and safety outcome. Verify the complete private database without committing content.

### Documentation and release

Update the README with rule authoring, compilation, explicit loading, and project trust. Document diagnostics and limits in the same change. Add examples using synthetic commands only. Run every repository check before release.

## Test matrix

### Source-pack tests

- Minimal valid pack.
- Maximum valid counts and lengths.
- Unknown field at every object depth.
- Invalid JSON and duplicate JSON keys at every object depth.
- Missing, repeated, escaping, absolute, or symlinked paths.
- Duplicate pack and rule IDs.
- Unsupported schema versions.
- Empty or oversized source files and packs.
- Deterministic source digest.

### Matcher tests

- Exact program and argument-prefix matches.
- Required argument tokens in different positions.
- Quoted arguments and escaped spaces accepted by the conservative parser.
- Pipelines, redirects, substitutions, and expansions rejected, along with comments and compound commands.
- Source-text and child-argv disagreement.
- Pass-through guard over reduction.
- Multiple reduction matches producing ambiguity.
- Unknown program pass-through.
- A 10,000-rule registry selecting from one small program bucket.

### Reducer tests

- Empty and short streams, boundary-sized streams, and very large streams.
- Success and failure policy selection.
- Head-only/tail-only/zero-line policies.
- Total output byte enforcement.
- Very long terminated and unterminated lines.
- CRLF, LF, mixed endings, and no final newline.
- ANSI sequences split across chunks.
- Invalid UTF-8 and NUL bytes.
- Literal matching modes and ASCII-only case folding.
- Drop before keep ordering.
- Minimum-savings exact pass-through.
- Omission counts and marker budget.
- Deterministic output under different read chunk sizes.

### Runner tests

- Exact child exit codes.
- Signal-derived status.
- Separate stdout and stderr with concurrent large writes.
- Inherited stdin.
- Child spawn failure.
- Output write failure.
- Archive spool failure and raw restoration.
- Archive transaction failure and raw restoration.
- Pack validation failure before spawn.
- Specialized reducer fallback.

### Compiled-pack tests

- Deterministic pack bytes.
- Every header and index boundary.
- Truncated files at every structural section.
- Integer overflow and overlapping sections.
- Wrong magic, format, ABI, digest, sort order, or record reference.
- Duplicate IDs across several external packs.
- Pack replacement between rewrite and run.
- Warm and cold lookup with 1,000 rules.
- Full verification with 10,000 rules.

### Pi extension tests

- Trusted and untrusted project roots.
- Project pack inside and outside the root.
- Symlink escape.
- Rewrite timeout or nonzero exit.
- Pack path and digest quoting.
- `bash` and `exec_command` bindings.
- Disabled pruning with archive still active.
- No session entries or Pi internals changed.

## Property and fuzz testing

Property tests should generate rule models, argv lists, byte streams, chunk boundaries, and output policies. They must prove:

- Rendered output never exceeds `max_output_bytes` for a valid reduction rule.
- Memory accounting never exceeds its declared bound.
- Short unchanged output remains exact.
- Different chunk boundaries produce identical output.
- Pass-through output remains exact.
- Rule selection is deterministic.
- A pass-through guard always wins.
- Ambiguity never selects an arbitrary reduction rule.

Fuzz targets should cover the shell parser, source JSON dispatcher, compiled-pack reader, matcher, line splitter, ANSI state machine, and every typed reducer. Fuzzing is useful during development and security investigation. It does not become part of the normal completion checks unless explicitly requested.

## Quality gates

Normal completion must run every command required by `AGENTS.md`, including Rust formatting, checking, Clippy, tests, coverage, audits, Pi type checking and tests, Slophammer, and `git diff --check`.

Additional gates for this feature are:

- Built-in source compilation produces no warnings.
- Every built-in rule has fixture coverage.
- The generated registry is deterministic.
- JSON Schema and Serde validation agree on the committed valid and invalid corpus.
- The 1,000-rule warm external-pack benchmark meets the 5 millisecond p95 target or has an approved exception backed by raw measurements.
- Stream memory remains bounded under a 1 GiB input.
- Offline corpus verification reports no new orphan or integrity errors.
- Paired semantic review finds no release-blocking information loss in approved rules.
- No command, transcript, output, private database, or generated real-data report enters Git.

## Boundaries

Version 1 does not provide:

- A generic reducer for unknown commands.
- Dynamic Rust, native, WebAssembly, JavaScript, or shell plugins.
- Remote rule registries or downloads.
- Automatic source-pack compilation.
- Automatic cache writes.
- A daemon, server, socket, watcher, or resident worker.
- Rule inheritance, deep merge, aliases, or compatibility readers.
- Runtime regular expressions.
- Command rewriting defined by rule files.
- Combined stdout and stderr reduction.
- Automatic promotion from benchmark results.
- An archive schema change.

Future work must start from measured need. A new feature should not widen these boundaries merely because the source schema has room to grow.

## Completion criteria

Command-Aware Pruning is complete when:

- The current hard-coded allowlist has been replaced by validated built-in rules.
- Built-in rules are embedded and require no runtime file parsing.
- A user can validate and compile a source pack without rebuilding YARP.
- A 1,000-rule compiled pack loads and matches within the approved performance limit.
- Runtime matching reads one program bucket and does not scan the full registry.
- Every reducer uses bounded streaming state.
- Success and failure use the actual child status.
- Stdout, stderr, exit codes, and signals preserve their contracts. Stdin and archive restoration do too.
- Unknown, ambiguous, invalid, changed, or incompatible rules produce pass-through behavior.
- Exact inspection guards work before broad reduction rules.
- The Pi extension uses public APIs and respects project trust.
- The extractor calls the production engine directly for benchmarks.
- Synthetic tests, property tests, approved fuzz investigations, corpus verification, performance checks, and repository quality gates pass.
- Documentation explains the source format, compiled format, commands, safety behavior, extension path, and operational limits.

If command safety cannot be proved, output structure cannot be parsed with bounded state, or reviewed examples lose required evidence, that rule remains outside the built-in manifest. The rest of the engine may proceed without it.
