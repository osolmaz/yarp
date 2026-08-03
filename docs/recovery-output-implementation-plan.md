# Recovery output implementation plan

## Status

Implemented. Rust owns configuration, shell classification, and recovery bounds. The Pi extension validates both machine-readable contracts and applies the selected result policy through public extension hooks.

## Goal

A direct `yarp search` or `yarp read` command returns bounded recovery output or a bounded diagnostic. The Pi extension does not shorten that result again or create another recovery reference.

Ordinary tool results keep the configured 5,120-byte default. Recovery commands use the byte and line ceilings from [the YARP configuration specification](configuration-spec.md), with defaults of 32,768 bytes and 1,900 lines.

## Resolved problem

The old Pi extension applied its generic output cap after every tool result, including shell commands that recover omitted YARP output. A large `yarp search` or `yarp read` result could therefore receive another cap marker and another archive reference. The model then had to recover the recovery output instead of reading the requested evidence.

The archive prevented data loss, but chained retrieval added tool calls and could hide the line ranges printed by the first query.

## Result policies

The extension uses three result policies.

### Ordinary

An ordinary result follows the current path:

1. Archive the complete result exposed by Pi.
2. Apply a typed summary when a reviewed rule selects one.
3. Apply `output.cap_bytes` to the remaining text.
4. Commit a recovery source before returning shortened text.

### Recovery

A recovery result comes only from a statically proven direct `yarp search` or `yarp read` command:

1. Archive the call and result normally.
2. Do not apply typed summarization.
3. Do not apply the ordinary output cap.
4. Rely on the recovery command's configured byte and line ceilings.
5. Return its output without an outer YARP cap marker.

### Pass-through

A pass-through result preserves the result exposed by Pi. The extension still archives it when the archive is available, but it does not summarize or cap it.

Pass-through applies when the shell policy process fails, times out, returns malformed data, or cannot prove that its plan matches the command that Pi executed. Archive and staging failures keep their existing fail-open behavior and return the pre-cap result.

## Shell policy

YARP's Rust shell parser remains the authority for command meaning. The Pi extension must not classify recovery commands with string prefixes, regular expressions, output markers, tool names, or shell parsing written in TypeScript.

The binary exposes `yarp plan --json <shell-command>`, one versioned machine command that plans both execution and result handling. The existing `yarp rewrite` command calls the same Rust planner and keeps its human-facing output. The JSON response has one stable shape:

```json
{
  "version": 1,
  "execution": {
    "kind": "original"
  },
  "result": {
    "kind": "recovery"
  }
}
```

A rewritten command includes its replacement:

```json
{
  "version": 1,
  "execution": {
    "kind": "rewrite",
    "command": "yarp run ..."
  },
  "result": {
    "kind": "ordinary"
  }
}
```

The TypeScript extension validates this response without `any`, unchecked casts, or permissive fallback fields. Unknown versions, fields, and enum values are invalid.

The parser may return `recovery` only when all of these are true:

- The shell source is one simple command.
- The executable word is exactly `yarp`.
- The subcommand is exactly `search` or `read`.
- The source contains no pipeline, chain, redirect, substitution, wrapper, setup command, or background execution.

Quoted arguments and supported search or read options do not change the policy. Commands such as these remain ordinary:

```sh
yarp search "$ref" error | head
command yarp read "$ref" stdout 1:20
env DEBUG=1 yarp search "$ref" warning
yarp search "$ref" error > matches.txt
yarp search "$ref" error; print-extra-output
```

The generated recovery marker already uses the direct accepted form.

## Pi extension flow

At `tool_call`, the extension requests the Rust shell policy before changing the command. It stores the validated result policy, the original command, and the tool-call ID in the existing in-memory active-call record.

If a later extension changes the shell input, YARP must not reuse a stale recovery policy. At `tool_result`, it compares the final command in the event with the command covered by the policy. A mismatch selects pass-through unless a fresh bounded policy check proves the final command.

The extension then uses the documented `tool_result` return value to apply the chosen result policy. No Pi-owned object is mutated after execution.

## Recovery bounds

`yarp search` and `yarp read` load the resolved configuration directly in Rust.

`yarp search` must fit its complete rendered stdout within both `output.recovery_cap_bytes` and `output.recovery_cap_lines`. It may reduce displayed match groups and context to fit. It reports the number of omitted selected lines and prints exact reads against the original archive reference.

`yarp read` is exact. It must reject a requested line or byte range that would exceed either configured ceiling before writing stdout. The diagnostic suggests a smaller range against the same original archive reference. It must not return a shortened exact range.

Recovery diagnostics must also fit both ceilings. The extension never applies the ordinary cap to repair an oversized recovery result. A recovery command that violates its own bound is a defect and exits nonzero with a bounded diagnostic.

The one-hop invariant is:

> Recovery output never contains a new outer YARP cap reference.

## Configuration

The durable settings are:

```toml
[output]
cap_bytes = 5120
recovery_cap_bytes = 32768
recovery_cap_lines = 1900
```

The recovery settings are configurable through `yarp config`. They are not environment overrides. Each `yarp search` or `yarp read` process loads the latest valid file. Extension-owned settings such as the ordinary cap use the snapshot loaded at Pi session start and require `/reload` or restart after a configuration change.

A smaller recovery ceiling produces smaller search pages and requires smaller exact reads. A larger ceiling is allowed only up to 49,152 bytes and 1,900 lines, leaving room below Pi's native 50 KiB and 2,000-line limits.

## Failure behavior

- A missing config file uses defaults.
- An invalid config disables YARP processing in the Pi extension for that session and leaves tool execution unchanged.
- A shell policy timeout, process error, or invalid JSON selects pass-through for that call.
- An archive failure returns the result that Pi exposed before YARP shortening.
- A recovery command that cannot load valid configuration exits with a concise error instead of emitting unbounded output.
- No failure path creates a second archive, a temporary recovery file, or an outer recovery marker.

## Verification

Rust tests cover:

- Direct `yarp search` and `yarp read` classification.
- Quoted patterns and valid options.
- Rejection of wrappers, redirects, pipelines, chains, substitutions, and background commands.
- The versioned policy JSON.
- Configurable recovery byte and line defaults, minima, maxima, and invalid values.
- Bounded search rendering under both limits.
- Exact-read rejection before stdout when either limit would be exceeded.
- Bounded diagnostics.
- Preservation of the original archive reference in every suggestion.

Pi extension tests cover:

- Ordinary output still capped at the configured budget.
- Recovery output from above 5,120 bytes through the configured recovery ceiling remains unchanged.
- Recovery output bypasses typed reduction.
- Forged marker text does not select recovery policy.
- Compound shell source does not select recovery policy.
- Stale policy after a later input mutation selects pass-through.
- Policy timeout, invalid JSON, and unknown versions select pass-through.
- Archive and staging failures preserve pre-cap output.
- No recovery result contains a new outer cap reference.

A non-interactive smoke check loads the source extension with `pi -e` against the built binary. The Pi test harness exercises config loading, reload, planning, archiving, result policies, and recovery bypass through the same public hook registrations.

## Contract impact

- **Session state:** Normal Pi behavior saves the final bounded tool result. YARP adds no session entry.
- **Other persistent data:** YARP adds `config.toml`. The existing archive remains the only runtime data store, with no schema change.
- **Pi internals:** None.
- **Public Pi API:** Documented `tool_call`, `tool_result`, session lifecycle hooks, and `pi.exec` only.
- **Network and services:** None.
