# YARP

YARP is a command-output pruner and local tool-call archive for Pi. It applies a bounded reducer chosen for each supported command before output enters Pi's context, and it stores every Pi tool call before and after YARP processing.

YARP leaves unknown or ambiguous commands unchanged. Structured-output and exact-inspection commands also stay unchanged. Shell pipelines and commands with redirects, substitutions, or compound syntax pass through. Wrapped commands keep their exit codes, and stdout never mixes with stderr.

## Install

Install the Rust binary:

```sh
cargo install --git https://github.com/osolmaz/yarp.git --locked
```

Then install the Pi package:

```sh
pi install git:github.com/osolmaz/yarp
```

Both commands install from the public GitHub repository.

## Use

The Pi extension handles supported `bash` and `exec_command` calls automatically. Built-in rules cover common repository inspection, search, test, build, lint, and package-script commands. They also cover CI inspection plus container and cluster commands. Exact-output forms such as structured JSON, NUL-delimited output, and Git object inspection stay untouched.

Run a supported command directly when needed:

```sh
yarp run -- cargo test --workspace
```

Ask YARP whether a shell command can be wrapped:

```sh
yarp rewrite "git status --short"
```

An unsupported command exits with status 3 and prints nothing. The Pi extension treats that result as a request to run the original command.

Set `YARP_DISABLED=1` to turn off automatic rewriting while keeping the archive active.

## Rules

Built-in rules are validated during the Rust build and embedded in the binary. Inspect the selected action without running a command:

```sh
yarp rules explain -- cargo test --workspace
yarp rules list
```

Create an external source pack when a project has another repetitive command. A minimal pack contains `pack.json` and one listed rule:

```json
{
  "schema_version": 1,
  "id": "example-rules",
  "rules": ["rules/check.json"]
}
```

```json
{
  "id": "example/check",
  "match": { "program": ["example-check"] },
  "action": "reduce",
  "reducer": { "kind": "head_tail" },
  "success": {
    "head_lines": 20,
    "tail_lines": 10,
    "max_line_bytes": 16384,
    "max_output_bytes": 32768,
    "min_savings_bytes": 256
  },
  "failure": {
    "head_lines": 80,
    "tail_lines": 60,
    "max_line_bytes": 32768,
    "max_output_bytes": 131072,
    "min_savings_bytes": 256
  }
}
```

Validate and compile it explicitly:

```sh
yarp rules check ./example-rules
yarp rules compile ./example-rules --output ./example-rules.yrp
yarp rules verify ./example-rules.yrp
yarp rewrite --rule-pack ./example-rules.yrp "example-check"
```

Set `YARP_RULE_PACKS` to an operating-system path list for global packs. A trusted Pi project may instead keep a compiled pack at `.yarp/rules.yrp`; the extension ignores that path until Pi reports the project as trusted. YARP does not scan for source packs, compile them automatically, download rules, or execute code from a rule.

See the [command-aware pruning plan](docs/command-aware-pruning-implementation-plan.md) for the complete source schema, limits, matching rules, and fail-open behavior.

## Archive

YARP stores tool calls in one local SQLite database:

```text
~/.local/share/yarp/tool-calls.sqlite3
```

Inputs and results are stored before and after YARP processing. Wrapped shell commands also store exact stdout and stderr before and after pruning. Identical snapshots share one compressed payload.

Inspect the archive without printing tool content:

```sh
yarp archive stats
yarp archive verify
```

Delete finished calls older than a UTC timestamp only when you choose to:

```sh
yarp archive prune --before 2026-01-01T00:00:00Z
```

Set `YARP_ARCHIVE_DISABLED=1` to opt out of capture. The archive may contain commands, source code, file contents, and secrets printed by tools. It stays local and uses private filesystem permissions. See the [archive specification](docs/tool-call-archive-spec.md) for the full format and failure rules.

## Analyze existing tool calls offline

The workspace also includes `toolcall-extractor`, a separate offline program for normalizing existing Pi, Codex, Claude Code, and Cursor tool calls into a private DuckDB database. It does not run from YARP or the Pi extension.

Build it from a checkout:

```sh
cargo build --release -p toolcall-extractor
```

Imports require an explicit Unix-user label and source path. For example:

```sh
target/release/toolcall-extractor extract --unix-user "$USER" codex \
  --sessions "$HOME/.codex/sessions" \
  --state-db "$HOME/.codex/state_5.sqlite"
```

Inspect counts and integrity without printing tool content:

```sh
target/release/toolcall-extractor stats
target/release/toolcall-extractor issues
target/release/toolcall-extractor verify
target/release/toolcall-extractor benchmark-yarp
```

For a different Unix user, run only the reader under that identity and keep the DuckDB writer under your own account:

```sh
sudo -u other-user target/release/toolcall-extractor stream \
  --unix-user other-user claude --projects /home/other-user/.claude/projects |
  target/release/toolcall-extractor ingest --unix-user other-user --agent claude
```

This sends framed normalized records through the pipe and creates no intermediate export. An unchanged replay keeps the same calls and results. A changed source replaces the records owned by that source item.

The default database is `~/.local/share/toolcall-extractor/toolcalls.duckdb`. Tool inputs and outputs can contain secrets, so its directory and files are private. The extractor reads agent state without modifying it, has no network code, and stops before its files reach 10,000,000,000 bytes. See [the implementation plan](docs/toolcall-extractor-implementation-plan.md) for supported formats and privacy boundaries.

A complete local validation across Pi, Codex, Claude Code, and Cursor imported 718,008 calls with no orphan records. The command-aware rules removed 67,524,245 characters from stored shell outputs. This is 24.7632% of eligible output and 3.47755% of all rendered output. The eligible set is broader than the earlier generic policy because more commands now have explicit rules. The generated database and transcripts are not included in the repository.

## Limits

Each rule has separate success and failure limits for head lines, tail lines, one line, total output, and minimum useful savings. Processing is streaming and bounded. Short output remains byte-for-byte exact when compacting it would not save enough space.

YARP does not collect usage data or access the network. Archive capture is local and enabled by default. The offline extractor writes only when invoked explicitly.
