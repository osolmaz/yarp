# YARP configuration specification

This document defines the YARP configuration format and `yarp config` commands.

## File

YARP reads one user configuration file:

```text
$XDG_CONFIG_HOME/yarp/config.toml
```

When `XDG_CONFIG_HOME` is unset, YARP uses:

```text
$HOME/.config/yarp/config.toml
```

A missing file is valid and selects all defaults. YARP does not search parent directories, load project configuration, merge files, fetch remote content, or expand environment variables inside values.

Relative paths in the file resolve from the directory that contains `config.toml`. YARP does not expand `~` in path values.

## Minimal file

```toml
version = 1
```

`version` is required when the file exists. Version 1 accepts only the fields listed below.

## Full example

```toml
version = 1

[pruning]
enabled = true

[output]
cap_bytes = 5120
recovery_cap_bytes = 32768
recovery_cap_lines = 1900

[archive]
enabled = true
path = "../data/tool-calls.sqlite3"

[rules]
packs = [
  "rules/team.yrp",
  "/opt/yarp/rules/company.yrp",
]
```

## Fields

| Field | Type | Default | Rule |
| --- | --- | ---: | --- |
| `version` | integer | none | Required when the file exists. Must equal `1`. |
| `pruning.enabled` | boolean | `true` | Enables command rewriting, typed result reduction, and generic output capping. Archive capture is controlled separately. |
| `output.cap_bytes` | integer | `5120` | Visible UTF-8 byte budget for ordinary tool results after typed reduction. `0` disables only the generic cap. Other values must be from `1024` through `16777216`. |
| `output.recovery_cap_bytes` | integer | `32768` | Maximum stdout bytes produced by `yarp search` or `yarp read`. Must be from `1024` through `49152`. |
| `output.recovery_cap_lines` | integer | `1900` | Maximum stdout lines produced by `yarp search` or `yarp read`. Must be from `1` through `1900`. |
| `archive.enabled` | boolean | `true` | Enables live Pi tool-call archive capture. Generic capping cannot run without successful capture. |
| `archive.path` | string | XDG data path | Archive database path. An omitted value uses `$XDG_DATA_HOME/yarp/tool-calls.sqlite3`, falling back to `$HOME/.local/share/yarp/tool-calls.sqlite3`. The value must not be empty. |
| `rules.packs` | array of strings | `[]` | Additional compiled rule packs. Empty entries are invalid. |

The recovery byte ceiling stays below Pi's 50 KiB tool-output limit. The recovery line ceiling leaves room below Pi's 2,000-line limit. These bounds prevent Pi from shortening recovery output before YARP can report it accurately.

The trusted project pack at `.yarp/rules.yrp` remains governed by Pi project trust and YARP's existing path checks. It is not a user setting.

## Commands

The configuration interface is:

```sh
yarp config path
yarp config init
yarp config show
yarp config show --json
yarp config get output.recovery_cap_bytes
yarp config set output.recovery_cap_bytes 32768
yarp config unset output.recovery_cap_bytes
yarp config check
```

`path` prints the resolved file path. `init` creates a commented file with all defaults and refuses to overwrite an existing file. `show` prints the complete resolved configuration. `show --json` emits the same resolved fields as a stable JSON object for the Pi extension. `get` prints one resolved value. `set` and `unset` edit one known field, preserve unrelated comments and formatting, validate the complete result, and write it atomically. `check` validates without changing the file.

`set` parses values according to the known field type. Boolean and integer fields take one value. `archive.path` takes one path. `rules.packs` takes zero or more path arguments:

```sh
yarp config set pruning.enabled false
yarp config set archive.path /srv/yarp/tool-calls.sqlite3
yarp config set rules.packs /opt/yarp/base.yrp /opt/yarp/team.yrp
yarp config set rules.packs
```

The final command stores an empty rule-pack list.

## Validation

YARP rejects:

- Invalid TOML.
- A missing or unsupported `version`.
- Unknown sections or fields.
- Duplicate sections or fields.
- Wrong value types.
- Integers outside their stated ranges.
- Empty path strings.
- Rule-pack entries that are empty, unreadable, or not valid compiled packs.

A missing file uses defaults. An invalid file does not fall back to a partial configuration. The Pi extension reports the error and leaves tool execution unchanged for that session. Standalone commands report a concise configuration error and exit nonzero.

`yarp config set` and `yarp config unset` must validate the complete candidate file before replacing the old one. They write a temporary file in the same directory, flush it, set private permissions, rename it atomically, and flush the directory. On POSIX systems, YARP creates the directory with mode `0700` and the file with mode `0600`.

## Loading

The Rust binary is the only TOML parser. The Pi extension runs `yarp config show --json` once when its session starts and validates the returned object as a strict, versioned TypeScript union. It does not read or parse TOML. The JSON object contains every resolved section and value, including defaults and absolute archive and rule-pack paths. It contains no file contents, provenance records, or unknown fields.

A direct CLI invocation reads the file when that process starts. A running Pi session keeps one resolved snapshot. Changes made through `yarp config set` take effect in Pi after `/reload` or restart. YARP does not watch the file.

Explicit command arguments still control one invocation. For example, `yarp search --max-results` can request fewer matches than the configured recovery ceiling. Command arguments cannot raise the configured byte or line ceiling.

## Removed environment settings

The configuration file replaces these former YARP-specific environment settings:

| Current setting | Configuration field |
| --- | --- |
| `YARP_DISABLED` | `pruning.enabled` |
| `YARP_ARCHIVE_DISABLED` | `archive.enabled` |
| `YARP_OUTPUT_CAP_BYTES` | `output.cap_bytes` |
| `YARP_ARCHIVE_PATH` | `archive.path` |
| `YARP_RULE_PACKS` | `rules.packs` |

YARP does not keep aliases or merge these old environment values with the file. Standard `HOME`, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME` remain part of file and data path resolution.

## Fixed limits

Configuration covers user policy. Parser depth, protocol frame size, memory bounds, database integrity checks, compression settings, archive locking, and corruption handling remain fixed implementation limits. Per-command search patterns, context, result counts, read ranges, and prune timestamps remain command arguments.

## Boundaries

The file contains no credentials and has no include mechanism. It does not change Pi session entries, the tool-call archive schema, or Pi internals. Configuration resolution performs no network access and starts no background process. If neither `XDG_CONFIG_HOME` nor `HOME` can locate the file, YARP reports a configuration-path error instead of creating a file elsewhere.
