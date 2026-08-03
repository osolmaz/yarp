# Global output cap implementation plan

## Goal

YARP will limit archived Pi tool-result text to 5 KiB by default after applying any command-aware summary. Larger text will keep bounded evidence from the beginning and end plus a local recovery reference. The exact source behind the pre-cap text must be committed to the existing private archive before YARP returns shortened content.

The cap applies through Pi's documented `tool_result` hook. It does not change Pi session state, add another persistent store, or use Pi internals.

## Rationale

The aggregate corpus contains 565,682 textual outputs with 1,941,717,513 Unicode characters. A 4,678-character ceiling would remove 55% of those characters and affect 18.51% of outputs. A 5,673-character ceiling would remove 50% and affect 16.09%. The 5 KiB default sits between those measured candidates for mostly ASCII output.

These figures locate the operating range; they do not prove task quality. The runtime cap counts UTF-8 bytes, so non-ASCII results can reach it with fewer characters. Onur Solmaz approved the 5 KiB global default on 2026-08-03 as a reversible, configurable policy.

## Configuration

`YARP_OUTPUT_CAP_BYTES` controls the visible text budget:

- unset: 5,120 bytes;
- `0`: disable the generic cap while retaining command-aware summaries;
- 1,024 through 16,777,216: use that exact UTF-8 byte budget;
- any other value: report a warning and disable the generic cap for the session.

`YARP_DISABLED=1` continues to disable all pruning. `YARP_ARCHIVE_DISABLED=1` disables the generic cap because YARP cannot shorten text without a committed recovery source.

## Result policy

YARP will process each result in this order:

1. Commit the complete structured result exposed by Pi to `result/before`.
2. Apply an existing typed shell summary when one is selected safely.
3. Measure the UTF-8 bytes in the remaining text content, including a typed summary.
4. Leave text at or below the configured budget unchanged.
5. For a large typed summary, reuse its already committed `source_output` or `result_text` recovery source.
6. For other large text, commit the exact concatenation of its original text blocks to `result_text/before`.
7. Keep UTF-8-safe prefixes and suffixes within the budget and place a recovery marker between them.
8. Stage the shortened result and complete the existing archive lifecycle.

The budget includes retained text and the recovery marker. Image blocks remain unchanged and do not count toward the text budget. Their relative order is preserved. If recovery capture, result staging, or configuration validation fails, YARP keeps the uncapped result and reports the failure through the existing fail-open path.

The recovery marker names the archive reference and provides a valid bounded search command. `yarp search` then prints copyable `yarp read` commands for exact ranges.

## Archive contract

The existing schema remains at version 1. `result_text/before` expands from a single host text item to the exact ordered concatenation of all text blocks exposed immediately before the generic cap. This source remains valid UTF-8 and records whether Pi described the source as complete, incomplete, or unknown.

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
- typed-summary precedence and capping of an oversized typed summary;
- exact `result_text` capture before a generic cap;
- pass-through when recovery capture fails;
- interaction with `YARP_DISABLED` and `YARP_ARCHIVE_DISABLED`.

Race and mutation tests are outside this change's completion path, as requested.

## Documentation

Update the README, Pi extension guide, archive specification, bundled skill, and repository guidance. The documentation must state that the output-size study measured Unicode characters while this runtime policy uses UTF-8 bytes.
