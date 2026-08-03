---
name: yarp
description: Use when a YARP summary or omission marker appears, when output must be recovered through a yr_ reference, or when inspecting and troubleshooting YARP command pruning, rules, or its local archive.
compatibility: Requires the yarp CLI and access to the local YARP archive that created the reference.
---

# YARP

YARP gives the model a bounded typed summary of supported command output and keeps exact omitted output in a private local archive. After typed summarization, the Pi extension caps remaining result text at 5,120 UTF-8 bytes by default. It handles rewriting, result reduction, generic capping, and archival automatically.

## Use the inline summary first

Treat the visible typed summary as the primary result. Do not retrieve omitted output merely because a recovery marker exists. Retrieve only when the omitted evidence is needed to answer the user, verify a claim, diagnose a failure, or choose the next action.

Do not rerun the original command when the archived result can answer the question. A rerun may be slower, have side effects, or observe different state.

A global-cap marker keeps content from both the beginning and end of the result. Read both visible sections before deciding that recovery is needed.

## Search omitted output

Copy the opaque `yr_...` reference exactly from the YARP marker. Search for the smallest useful set of terms:

```sh
yarp search REF 'error|FAILED'
yarp search REF 'literal text' -F -i
yarp search REF 'warning' -v -C 3 -m 20
yarp search REF -e 'panic' -e 'fatal' -C 2
```

Search patterns are bounded regular expressions by default. Use `-F` for literal text, `-i` for case-insensitive matching, `-w` for ASCII word boundaries, and `-e` for multiple patterns. Use `-A`, `-B`, or `-C` for bounded context and `-m` to limit displayed matches.

An exit status of 1 with `No matches` means the search completed successfully but found nothing. Refine the terms before considering a rerun of the original command.

Search output includes copyable exact-read commands. Prefer those commands instead of inventing line ranges.

## Read an exact range

Read an inclusive line range when the source and lines are known:

```sh
yarp read REF stdout 118:130
yarp read REF stderr 42:57
yarp read REF source_output 24:36
yarp read REF result_text 8:20
```

Available source names are reported when a reference has more than one source. They may include `stdout` and `stderr`, plus `source_output` or `result_text`. Omit `SOURCE` only when the reference has one readable source:

```sh
yarp read REF 118:130
```

For binary data or a line too large for bounded text search, read a half-open byte range:

```sh
yarp read REF stdout --bytes 0:4096
```

Retrieve the smallest range that answers the question. Archive output can contain source code and file contents plus commands or secrets. Do not print broad ranges or expose archive paths, database contents, internal call identifiers, or unrelated output.

## Inspect YARP without executing a command

Explain whether a command selects a reviewed rule:

```sh
yarp rules explain -- cargo test --workspace
yarp rules list
```

Check archive integrity and aggregate storage without printing tool content:

```sh
yarp archive verify
yarp archive stats
```

Unknown or ambiguous commands execute unchanged and bypass typed summaries. The same applies to structured-output forms and streaming or exact-inspection commands. Their remaining result text may still receive the archive-backed global cap.

`YARP_OUTPUT_CAP_BYTES` configures that cap for a Pi process. Unset means 5,120 bytes, `0` disables only the generic cap, and values from 1,024 through 16,777,216 select an exact byte budget. Do not change or persist this setting unless the user asks.

## Safety boundaries

- Do not invoke `yarp archive prune` unless the user explicitly requests deletion and provides the intended UTC boundary.
- Do not set `YARP_DISABLED`, `YARP_ARCHIVE_DISABLED`, or `YARP_OUTPUT_CAP_BYTES` persistently unless the user explicitly asks to change that behavior.
- Do not inspect the SQLite archive directly. Use the bounded `search`, `read`, `stats`, and `verify` interfaces.
- Do not treat the presence of a recovery marker as evidence that recovery is required.
- Preserve command status and diagnostics when troubleshooting. YARP must fail open when it cannot reduce safely.
