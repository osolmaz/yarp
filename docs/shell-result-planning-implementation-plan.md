# Shell result planning implementation plan

## Purpose

YARP should understand the visible output of safe shell pipelines and command chains before selecting a typed reducer. The command must run exactly as written. YARP will archive the exact result first, then reduce it only when every possible visible output has one compatible type.

This work also improves evidence selection inside the existing reducers. Large successful results should keep the smallest useful summary. Failures and diagnostics should receive more space. Every changed result must retain an exact recovery path.

## Implementation status

The implementation is complete. It uses pinned `tree-sitter` 0.25.10 and `tree-sitter-bash` 0.25.1, both under the MIT license. Their build scripts compile the published generated C parsers with the `cc` crate and run no downloaded or repository-provided executable.

The shared parser, result planner, transform action, six initial transforms, privacy-safe syntax report, ranked evidence selector, parser robustness harness, and performance benchmarks are in place. The production success budget is 512 bytes. A 384-byte budget removed about 0.9 million more characters than 512 bytes but dropped every immediate example from a representative large successful search summary. The 512-byte form retained three diverse examples and exact recovery. A 704-byte budget removed about 0.8 million fewer characters than 512 bytes without improving a mandatory safety gate. These differences are below the one-percentage-point release threshold, so the middle budget is the safer practical choice.

## Measured baseline

The private corpus contains 371,241 shell results and 1,445,526,406 output characters. Current typed summaries change 29,439 results and remove 306,302,222 characters, which is 21.1897% of shell output.

Shell operators and syntax account for a large part of the unchanged output. The full census is recorded in [Reduction ceiling analysis](reduction-ceiling-analysis.md). The largest forms are pipelines, `&&` chains, redirections, semicolon chains, multiline input, parameter expansion, and command substitution. Counts overlap because one command can contain several forms.

Before this implementation, the classifier accepted simple `;`, `&&`, and `||` chains when every output command selected the same reducer family. It selected 3,958 compound results. Of those, 1,486 changed and removed 11,782,428 characters. Pipelines passed through.

The finished planner selects 16,945 compound results. Of those, 10,093 change and remove 82,731,646 characters. The full candidate changes 37,466 shell results and removes 375,126,127 characters, or 25.9508% of shell output.

## Required result

The finished implementation must provide all of the following behavior:

- One shell parser serves simple commands, command chains, and pipelines.
- The parser reads syntax without executing commands or expanding shell values.
- A planner determines the reducer type for every possible visible output path.
- Reviewed line-preserving commands can carry a reducer type through a pipeline.
- Any guard, unknown stage, mixed reducer family, or uncertain parse preserves the original result.
- Pipeline commands run unchanged. YARP does not wrap individual stages.
- Exact archived output remains available through `yarp search` and `yarp read`.
- Reducers choose retained evidence according to its value and the result status.
- Memory remains bounded independently of command output size.
- The existing throughput and query latency gates continue to pass.

## Boundaries

This work may change YARP's shell parser, rule schema, result planner, typed reducers, fixtures, offline analyzer, and Pi package tests.

The implementation must not evaluate shell expansions, resolve aliases, source shell startup files, inspect shell history, or execute a command during classification. It must not add a daemon, watcher, network request, plugin hook, replacement shell, or generic fallback summary.

Loops, functions, background jobs, and nested shells pass through in the first implementation. Process substitution and heredocs also pass through. The parser rejects every recovered tree. Parameter expansion and command substitution stay closed unless a later review proves that a narrow form cannot affect command or output classification.

Exact-text inspection, structured output, NUL-delimited output, count-only output, digests, machine-readable formats, watch modes, and follow modes keep their current pass-through behavior.

## Parser

Use exact dependencies on `tree-sitter = "=0.25.10"` and `tree-sitter-bash = "=0.25.1"`. The implementation review covered their MIT licenses, generated C parsers, build scripts, and transitive dependencies. The pull request records the same dependency review.

Move shell parsing out of `src/rewrite.rs` into a dedicated `src/shell/` module. The module should contain parsing, literal-word decoding, stream analysis, and result planning. Both pre-execution rewriting and post-result reduction must use this parser.

The parser accepts a tree only when all of these checks pass:

- The root covers the full source and contains no error or missing nodes.
- Every accepted node kind belongs to the reviewed grammar subset.
- Command names and matcher-relevant arguments decode to literal words.
- Source size, node count, nesting depth, and stage count stay below fixed limits.
- Redirections belong to the reviewed stream forms.

Start with a 256 KiB shell-source cap, 16,384 AST nodes, 64 levels of nesting, and 256 simple-command stages. A corpus report must verify that these limits cover ordinary commands. Exceeding any limit returns a pass-through result.

The parser preserves source spans and never reconstructs a command for execution. Replace `parse_words` and `split_compound` along with the redirect scanner. Remove those helpers after parser parity tests pass.

## Shell model

The accepted syntax tree needs only the information used for classification:

```rust
struct ShellProgram {
    lists: Vec<CommandList>,
}

struct CommandList {
    pipelines: Vec<Pipeline>,
    connectors: Vec<Connector>,
}

struct Pipeline {
    stages: Vec<SimpleCommand>,
    merge_stderr: bool,
}

struct SimpleCommand {
    words: Vec<LiteralWord>,
    redirects: Vec<Redirection>,
}

enum Connector {
    Sequence,
    And,
    Or,
}
```

These types model only the information needed for planning. Unsupported tree nodes stop planning and preserve the result.

## Output contracts

Every accepted command stage produces one output state:

```rust
enum OutputState {
    Empty,
    Typed(TypedOutput),
    Exact,
    Unknown,
    Conflict,
}
```

`TypedOutput` contains the reducer kind plus merged success and failure policies. Setup commands may return `Empty`, but their possible diagnostics still affect status confidence. Pass-through rules return `Exact`. Unknown programs return `Unknown`.

The planner combines states conservatively:

- `Empty` combined with a typed state keeps the typed state.
- Two typed states with the same reducer merge their policies by taking the larger limits and stricter savings gates.
- A reviewed line-preserving transform carries the incoming typed state forward.
- Different reducer kinds produce `Conflict`.
- `Exact`, `Unknown`, or `Conflict` anywhere in a visible path prevents reduction.
- A pass-through guard in any stage overrides every producer or transform rule.

This model applies to newlines and pipelines as well as `;`, `&&`, and `||`. Conditional connectors join every path that could have produced visible output. The planner does not guess which branch ran from the final text.

## Transform rules

Extend rule schema version 1 in place with a third action:

```json
{
  "id": "pipeline/head",
  "match": {
    "program": ["head"]
  },
  "action": "transform",
  "transform": {
    "kind": "line_preserving"
  }
}
```

A transform rule requires `transform` and forbids `reducer`, `success`, and `failure`. The first supported transform kind is `line_preserving`. It means that every emitted content line is copied from an input line, although order and duplication may change.

Add reviewed transform rules for these initial commands:

- `cat` with options that leave line content unchanged.
- `tee` when standard output remains a copy of standard input.
- `head` and `tail` in line mode.
- `sort` when it emits newline-delimited input lines.
- `uniq` when it does not add counts or group markers.

Each transform needs explicit guards for options that alter bytes, add prefixes, use NUL separators, write only to a file, execute children, or produce a structured format. Every transform and guard must have one synthetic fixture.

External packs may declare transform rules under the same strict validation and explicit compilation rules as reducer rules. Ambiguous transform matches preserve the result.

## Streams and status

The planner must model visible stdout and stderr separately even when Pi later renders them as one text result.

A normal pipeline sends one stage's stdout to the next stage while each stage's stderr remains visible. `2>&1`, `1>&2`, and `|&` change that graph and need explicit handling. File redirects, descriptor manipulation, process substitution, and unknown stream forms pass through initially.

Return a plan that records status confidence:

```rust
enum StatusConfidence {
    Complete,
    FinalStageOnly,
    Conditional,
}

struct ResultPlan {
    rule: Rule,
    status_confidence: StatusConfidence,
}
```

A plain `&&` chain has complete success information when the final status is zero. Semicolon chains and ordinary pipelines expose only the last relevant status. An `||` chain may contain an expected earlier failure. Plans with `FinalStageOnly` or `Conditional` status use the larger failure policy. Explicit `set -o pipefail` may restore complete pipeline status when the accepted syntax tree proves that it applies.

Transport outcomes such as process exit messages, timeout notices, and session identifiers remain mandatory evidence.

## Evidence selection

Keep the current evidence classes and replace fixed per-class head and tail shares with bounded ranked selection.

The selector must retain evidence in this order:

1. Command outcomes and transport outcomes.
2. Typed diagnostics plus their bounded context.
3. One sample for every registered source term present in the original result.
4. Structural summaries and omission counts.
5. Diverse examples selected by stable family-specific keys.
6. Additional beginning and ending examples when space remains.

Exact duplicate records should appear once with a repetition count. Search summaries should prefer examples from different files. Test and build summaries should prefer distinct failing targets or diagnostic signatures. List summaries should prefer distinct groups before repeated rows.

Evaluate 384-byte and 512-byte success budgets against the current 704-byte budget. Failures keep the current cap unless evidence proves that a larger bounded cap materially improves diagnosis. The selected budget must fit all mandatory evidence and the recovery marker. If it cannot, YARP keeps the original result.

The selector may retain only bounded records and bounded keys. It must not collect the full output, build an input-sized map, or make memory depend on the number of lines.

## Runtime flow

The Pi extension continues its current sequence:

1. Capture the original command and result metadata.
2. Archive the exact result before changing visible content.
3. Send the original command and visible text to `yarp result-reduce`.
4. Parse the shell source and build a `ResultPlan`.
5. Apply the selected typed reducer under the plan's status policy.
6. Return a changed result only when both savings gates pass.

Simple commands may still use the existing pre-execution wrapper when one rule selects the parsed argument vector. Pipelines and command chains remain unmodified and use post-result reduction only.

Rust and Pi package versions must continue to agree exactly. A mismatch disables integration and leaves the original behavior intact.

## Offline analysis

Extend `toolcall-extractor analyze-ceiling` so the checked-in analyzer can reproduce the shell syntax census. The report must use fixed public syntax IDs and aggregate counts only. It must not store commands, arguments, paths, output text, session identifiers, timestamps, or hashes.

Add separate measurements for each implementation slice:

- Parser coverage with no reduction behavior change.
- Existing compound behavior under the new planner.
- New pipeline and chain eligibility.
- Evidence-ranking savings with the command set held fixed.
- Combined production behavior.

This separation prevents a reducer improvement from hiding a parser regression.

Private review should inspect the largest changed successes and failures for every newly active shell form. It should also cover mixed operators, upstream failures, stream merges, truncated host results, non-ASCII text, and long lines. Private samples and reports stay outside Git.

## Implementation sequence

### Parser foundation

- Audit and pin the parser dependencies.
- Add the `src/shell/` module and resource limits.
- Port simple-command parsing with byte-for-byte behavior parity.
- Port setup commands and assignments together with reviewed wrappers and safe stream merges.
- Replace the old parser helpers and remove them.

No new shell form becomes reducible in this slice.

### Planner parity

- Add `OutputState`, status confidence, and policy merging.
- Reproduce every current simple and compound selection.
- Add diagnostics that explain parser and planner vetoes without exposing command content.
- Confirm the corpus baseline does not change.

### Transform contracts

- Extend the source and compiled schemas.
- Extend validation for transform rules and update their fixtures and documentation.
- Add the first line-preserving transform rules and their guards.
- Test external-pack compilation and ambiguity handling.

### Pipeline support

- Enable top-level pipelines whose stages all produce one compatible output type.
- Add newline-separated lists and compatible mixed connectors through the same planner.
- Keep redirections and dynamic shell forms closed unless a reviewed contract covers them.
- Run the paired corpus benchmark after each newly enabled syntax family.

### Adaptive evidence

- Add bounded duplicate counting and family-specific diversity keys.
- Evaluate the three success budgets independently from parser changes.
- Keep the simplest budget policy that passes diagnostic and retrieval checks.

### Release verification

- Review private success and failure samples.
- Run held-out retrieval tasks against changed summaries.
- Run live Pi canaries before and after `/reload`.
- Run every repository quality and security check.
- Run coverage and performance checks together with the privacy gate.

## Test matrix

At minimum, tests must cover these decisions:

| Command shape | Expected result |
| --- | --- |
| `rg TODO . | head -50` | `search_summary` |
| `find . -type f | sort | uniq` | `list_summary` |
| `cargo test | tee test.log` | `test_summary` with conservative status |
| `cargo test | head -100` without `pipefail` | `test_summary` with failure policy |
| `set -o pipefail && cargo test | head -100` | Status follows the real pipeline result |
| `cargo test && cargo test` | `test_summary` |
| `cargo test; cargo test` | `test_summary` with conservative status |
| `cargo test && cargo build` | Pass through because reducer kinds differ |
| `rg --json TODO . | jq .` | Pass through because structured output is guarded |
| `find . -print0 | xargs -0 echo` | Pass through because NUL and child execution are guarded |
| `cat source.rs | sed 's/x/y/'` | Pass through because content changes |
| `echo "$VALUE" | rg x` | Pass through because expansion is unresolved |
| `producer | unknown-filter` | Pass through because one stage is unknown |
| `producer |& head` | Pass through until merged-stderr handling is reviewed |
| A parse tree with `ERROR` or missing nodes | Pass through |
| A command above any parser limit | Pass through |

Parser tests must cover quoting, escaped operators, comments, nested substitutions, here documents, descriptor syntax, malformed input, deep nesting, and large commands. Property tests and fuzz targets must prove that parser failure always returns pass-through behavior and never changes the original command.

## Performance and memory

Keep the direct reducer throughput gate above 100 MB/s. Built-in rule matching must remain below 100 microseconds at p95. The complete result-reducer process must remain below 20 milliseconds at p95 on the existing benchmark.

The indexed-output benchmark measures representative commands and the maximum accepted source size. Ordinary parser and planner work must remain below one millisecond at p95. It reports median, p95, maximum time, and input size. The `shell_parser_fuzz` example is a bounded stdin harness for malformed and generated inputs:

```sh
printf '%s' 'cargo test |' | target/release/examples/shell_parser_fuzz
```

The configured stream memory bound remains below 4 MiB per stream. Parser limits must provide a separate fixed upper bound that does not depend on output size.

## Release decision

The retrospective minimum worthwhile effect is one additional percentage point of shell-output reduction, equal to 14,455,264 characters in the frozen corpus. The candidate removes 68,823,905 more characters than the branch baseline, a gain of 4.7612 percentage points. The measured effect clears the threshold; the maintainer still controls release approval.

A candidate may ship only when all of these conditions hold:

- The primary reduction effect clears the approved threshold.
- Registered diagnostic vetoes remain zero.
- Unsupported, guarded, ambiguous, and exact-inspection forms remain byte-exact.
- Held-out retrieval succeeds at the existing release standard.
- Direct throughput, process latency, and memory gates pass.
- Rust coverage remains at least 85% and Pi coverage does not regress materially.
- Rust tests, Pi tests, audits, Slophammer, formatting, Clippy, and diff checks pass.

If the safe subset misses the approved threshold, keep the parser and planner only when they materially simplify existing code or enable an approved coverage goal. Otherwise stop and return the measured blocked forms without broadening the safety contract.

## Pi contract impact

This plan uses Pi's documented `tool_call`, `tool_result`, execution lifecycle, and session lifecycle hooks. It changes no Pi internals and writes no session entries. It adds no persistent schema or runtime state. The existing private archive remains the only persistent YARP data.
