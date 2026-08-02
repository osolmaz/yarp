# YARP

YARP is a command-output pruner and local tool-call archive for Pi. It applies a bounded typed summary chosen for each supported command before output enters Pi's context, stores every Pi tool call before and after processing, and keeps exact omitted output available through short local references.

YARP leaves unknown or ambiguous commands unchanged. Structured-output and exact-inspection commands also stay unchanged. It never rewrites pipelines, redirects, substitutions, or compound shell source. The Pi extension may summarize the result of a conservatively classified compound command after execution, without replaying or changing it. Wrapped commands keep their exit codes, and stdout never mixes with stderr.

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

The Pi extension handles supported `bash` and `exec_command` calls automatically. Built-in rules cover common repository inspection and search plus tests and builds. They also cover linting and package scripts. Search coverage includes `rg`, `grep`, `egrep`, `fgrep`, and `git grep`. File and system coverage includes `ls`, `find`, `fd`, `tree`, `du`, `df`, `free`, `lsof`, and common `ps` listings. YARP also handles common Git and GitHub lists, container inventories, and Kubernetes inspection. Exact-output forms such as structured JSON, NUL-delimited output, custom process columns, child-executing file searches, streaming output, and Git object inspection stay untouched.

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
  "reducer": { "kind": "test_summary" },
  "success": {
    "max_line_bytes": 16384,
    "max_output_bytes": 32768,
    "min_savings_bytes": 256,
    "min_savings_basis_points": 1500
  },
  "failure": {
    "max_line_bytes": 32768,
    "max_output_bytes": 131072,
    "min_savings_bytes": 256,
    "min_savings_basis_points": 500
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

See the [indexed output summary plan](docs/indexed-output-summaries-implementation-plan.md) and the checked-in JSON schemas under `rules/schema/` for the complete reducer contract. They define matching and limits plus fail-open behavior. The [shell result planning implementation plan](docs/shell-result-planning-implementation-plan.md) covers adaptive evidence selection for pipelines and command chains.

## Archive

YARP stores tool calls in one local SQLite database:

```text
~/.local/share/yarp/tool-calls.sqlite3
```

Inputs and results are stored before and after YARP processing. Wrapped shell commands also store exact stdout and stderr before and after pruning. Identical snapshots share one compressed payload.

Typed summaries include an opaque reference when omitted output is recoverable. Search one archived call, then copy an exact range printed by the search result:

```sh
yarp search yr_0123456789abcdef0123456789abcdef 'error|FAILED'
yarp read yr_0123456789abcdef0123456789abcdef stderr 118:130
```

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

Measure how much unchanged shell output belongs to reviewable command families without printing commands or tool content:

```sh
target/release/toolcall-extractor analyze-ceiling \
  --output "$HOME/.local/share/toolcall-extractor/analysis/reduction-ceiling.json"
```

The report is written atomically under a mode-`0700` directory as a mode-`0600` file. It contains aggregate counts and fixed public family labels only. See the [reduction ceiling analysis](docs/reduction-ceiling-analysis.md) for the report fields and review process.

For a different Unix user, run only the reader under that identity and keep the DuckDB writer under your own account:

```sh
sudo -u other-user target/release/toolcall-extractor stream \
  --unix-user other-user claude --projects /home/other-user/.claude/projects |
  target/release/toolcall-extractor ingest --unix-user other-user --agent claude
```

This sends framed normalized records through the pipe and creates no intermediate export. An unchanged replay keeps the same calls and results. A changed source replaces the records owned by that source item.

The default database is `~/.local/share/toolcall-extractor/toolcalls.duckdb`. Tool inputs and outputs can contain secrets, so its directory and files are private. The extractor reads agent state without modifying it, has no network code, and stops before its files reach 10,000,000,000 bytes. See [the implementation plan](docs/toolcall-extractor-implementation-plan.md) for supported formats and privacy boundaries.

A complete local validation across Pi, Codex, Claude Code, and Cursor imported 718,008 calls with no orphan records. The production matcher evaluated all 371,241 stored shell results. Typed summaries changed 29,439 results and removed 306,302,222 characters: 92.2887% of eligible output, 21.1897% of shell output, and 15.7748% of all rendered output. No ambiguous reduction or registered diagnostic veto occurred. The generated database and transcripts remain outside the repository, as does the report.

## Limits

Each rule selects a typed search, diff, test, build, log, status, list, or literal-filter summary. Success and failure policies independently bound one line and total output and require both an absolute and proportional saving. Generic words such as `error` are shown as source-term samples unless the command-specific parser has enough evidence to label a diagnostic. Processing is streaming and bounded below 4 MiB per stream or result. Short output remains byte-for-byte exact when compacting it would not save enough space.

YARP does not collect usage data or access the network. Archive capture is local and enabled by default. The offline extractor writes only when invoked explicitly.
