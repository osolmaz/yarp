# Global output cap implementation plan

## Goal

YARP will limit archived Pi tool-result text to 5 KiB by default after applying any command-aware summary. Every non-recovery result is in scope, including pass-through results from unknown or ambiguous commands. Larger text will keep bounded evidence from the beginning and end plus a local recovery reference. The exact source behind the pre-cap text must be committed to the existing private archive before YARP returns shortened content.

The cap applies through Pi's documented `tool_result` hook. It does not change Pi session state, add another persistent store, or use Pi internals.

## Rationale

The aggregate corpus contains 565,682 textual outputs with 1,941,717,513 Unicode characters. A 4,678-character ceiling would remove 55% of those characters and affect 18.51% of outputs. A 5,673-character ceiling would remove 50% and affect 16.09%. The 5 KiB default sits between those measured candidates for mostly ASCII output.

These figures locate the operating range; they do not prove task quality. The runtime cap counts UTF-8 bytes, so non-ASCII results can reach it with fewer characters. Onur Solmaz approved the 5 KiB global default on 2026-08-03 as a reversible, configurable policy.

## Configuration

`output.cap_bytes` in the versioned TOML file controls the visible text budget:

- omitted: 5,120 bytes;
- `0`: disable the generic cap while retaining command-aware summaries;
- 1,024 through 16,777,216: use that exact UTF-8 byte budget;
- any other value: reject the configuration.

`pruning.enabled = false` disables command rewriting and typed result reduction while retaining archive capture. The generic cap still runs when `output.cap_bytes` allows it. `archive.enabled = false` disables live capture and the generic cap because YARP cannot shorten text without a committed recovery source.

[The YARP configuration specification](configuration-spec.md) defines the full file and `yarp config` commands. Direct `yarp search` and `yarp read` commands use separate configurable byte and line ceilings and do not pass through the ordinary cap. See [the recovery output implementation plan](recovery-output-implementation-plan.md).

## Result policy

YARP will process each result in this order:

1. Commit the complete structured result exposed by Pi to `result/before`.
2. Apply an existing typed shell summary when one is selected safely.
3. Measure the UTF-8 bytes in the remaining text content, including a typed summary.
4. Leave text at or below the configured budget unchanged.
5. For a large typed summary, reuse its already committed raw streams, `source_output`, or `result_text` recovery source. A wrapped summary also commits its exact visible text as a fallback without displacing the raw streams.
6. For any other large ordinary or pass-through text, commit the exact concatenation of its original text blocks to `result_text/before`.
7. Keep UTF-8-safe prefixes and suffixes within the budget and place a recovery marker between them.
8. Stage the shortened result and complete the existing archive lifecycle.

Only statically proven direct `yarp search` and `yarp read` recovery output skips this generic cap. Recovery commands have separate byte and line limits.

The budget includes retained text and the recovery marker. Image blocks remain unchanged and do not count toward the text budget. Their relative order is preserved. If recovery capture, result staging, or configuration validation fails, YARP keeps the uncapped result and reports the failure through the existing fail-open path.

The recovery marker names the archive reference and provides a valid bounded search command. `yarp search` then prints copyable `yarp read` commands for exact ranges.

## Archive contract

The existing schema remains at version 1. `result_text/before` expands from a single host text item to the exact ordered concatenation of all text blocks exposed immediately before the generic cap. This source remains valid UTF-8 and records whether Pi described the host text as complete, incomplete, or unknown, independently of a separate full-output path. Recovery selects wrapped streams first, then `result_text`, then `source_output`. This keeps capped host text searchable when another source is binary without hiding raw wrapped output.

No telemetry, network access, service, watcher, socket, or second database is added. Existing archive permissions, integrity checks, retention behavior, and local recovery commands remain unchanged.

## Tests

Focused tests will cover:

- the 5,120-byte default and explicit override;
- disabling the generic cap with `0`;
- rejecting malformed, too-small, and too-large values;
- exact pass-through at the budget;
- UTF-8-safe head and tail selection;
- a hard output bound including the marker;
- preservation of image order around shortened text;
- typed-summary precedence and capping of oversized post-result and wrapped summaries;
- exact `result_text` capture before a generic cap;
- generic capping after pass-through command handling;
- pass-through when recovery capture fails;
- interaction with `pruning.enabled` and `archive.enabled`.

Race and mutation tests are outside this change's completion path, as requested.

## Documentation

Update the README, Pi extension guide, archive specification, bundled skill, and repository guidance. The documentation must state that the output-size study measured Unicode characters while this runtime policy uses UTF-8 bytes.
