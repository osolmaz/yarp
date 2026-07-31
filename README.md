# YARP

YARP is a command-output pruner and local tool-call archive for Pi. It wraps a strict allowlist of developer commands, removes the middle of very long output before that output enters Pi's context, and stores every Pi tool call before and after YARP processing.

YARP leaves unsupported commands, shell pipelines, redirects, substitutions, or compound commands unchanged. It preserves the wrapped command's exit code and keeps stdout and stderr separate.

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

The Pi extension handles supported `bash` and `exec_command` calls automatically. Supported command families include:

- Git status, diff, log, or show commands
- Cargo build, check, clippy, or test commands
- Go tests and pytest, plus .NET build or test commands
- test, build, lint, check, or type-check scripts run through npm, pnpm, or Yarn

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

The default database is `~/.local/share/toolcall-extractor/toolcalls.duckdb`. Tool inputs and outputs can contain secrets, so its directory and files are private. The extractor reads agent state without modifying it, has no network code, and stops before its files reach 10,000,000,000 bytes. See [the implementation plan](docs/toolcall-extractor-implementation-plan.md) for supported formats and privacy boundaries.

## Limits

YARP keeps the first 160 and last 40 lines of each output stream. It marks omitted lines in the middle. A single line is limited to 16 KiB.

YARP does not collect usage data or access the network. Archive capture is local and enabled by default. The offline extractor writes only when invoked explicitly.
