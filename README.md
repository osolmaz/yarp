# YARP

YARP (Yet Another Result Pruner) is a command-output pruner and local tool-call archive for Pi. It applies a bounded typed summary chosen for each supported command before output enters Pi's context, stores every Pi tool call before and after processing, and keeps exact omitted output available through short local references. Remaining tool-result text is capped at 5 KiB by default after the exact text is archived.

YARP leaves unknown or ambiguous commands unchanged during execution. Structured-output and exact-inspection commands also bypass typed summaries, but their remaining text is normally subject to the global cap. Direct `yarp search` and `yarp read` recovery commands use their own configured bounds and never receive another outer cap marker. YARP never rewrites pipelines, redirects, substitutions, or compound shell source. The Pi extension may summarize the result of a conservatively classified compound command after execution, without replaying or changing it. Wrapped commands keep their exit codes, and stdout never mixes with stderr.

## Install

Install the Rust binary:

```sh
cargo install yarp-cli --locked
```

Then install the Pi package:

```sh
pi install git:github.com/osolmaz/yarp
```

Cargo installs the Rust binary from crates.io. Pi installs the package from the public GitHub repository and loads YARP's bundled usage and recovery skill automatically. `yarp-cli` is the only published Rust crate; rule-pack parsing and compilation remain internal.

The Rust binary also exposes the skill through the Skillflag convention for inspection or installation in another agent:

```sh
yarp --skill list
yarp --skill list --json
yarp --skill show yarp
yarp --skill export yarp | skill-install --agent portable --scope user
```

## Use

The Pi extension handles supported `bash` and `exec_command` calls automatically. Built-in rules cover common repository inspection and search plus tests and builds. They also cover linting and package scripts. Search coverage includes `rg`, `grep`, `egrep`, `fgrep`, and `git grep`. File and system coverage includes `ls`, `find`, `fd`, `tree`, `du`, `df`, `free`, `lsof`, and common `ps` listings. YARP also handles common Git and GitHub lists, container inventories, Kubernetes inspection, Hugging Face Job and Space logs, and Herdr lists. Exact-output forms such as structured JSON, NUL-delimited output, custom process columns, child-executing file searches, streaming output, and Git object inspection stay untouched.

Run a supported command directly when needed:

```sh
yarp run -- cargo test --workspace
```

Ask YARP whether a shell command can be wrapped:

```sh
yarp rewrite "git status --short"
```

An unsupported command exits with status 3 and prints nothing. The Pi extension treats that result as a request to run the original command.

The global text cap is 5,120 UTF-8 bytes, including its recovery marker. Direct `yarp search` and `yarp read` commands instead default to 32,768 bytes and 1,900 lines, staying below Pi's native limits.

Manage every user-facing policy setting through one versioned TOML file:

```sh
yarp config path
yarp config init
yarp config show
yarp config set output.cap_bytes 8192
yarp config set output.recovery_cap_bytes 32768
yarp config check
```

The file lives at `$XDG_CONFIG_HOME/yarp/config.toml`, falling back to `$HOME/.config/yarp/config.toml`. A missing file uses defaults. Invalid files disable the Pi extension for that session instead of applying a partial configuration. See the [configuration specification](docs/configuration-spec.md) for all fields and validation rules.

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

A `transform` action is result-only. It carries an existing typed output through a reviewed line-preserving pipeline stage and has no reducer or output policy of its own. YARP currently recognizes guarded forms of `cat`, `tee`, line-based `head` and `tail`, `sort`, and `uniq`. A transform cannot make unknown input reducible or run as a standalone YARP command.

Validate and compile a source pack explicitly:

```sh
yarp rules check ./example-rules
yarp rules compile ./example-rules --output ./example-rules.yrp
yarp rules verify ./example-rules.yrp
yarp rewrite --rule-pack ./example-rules.yrp "example-check"
```

Compiled packs use engine ABI 2. Recompile any `.yrp` pack made by an earlier YARP version; incompatible packs are rejected before rule selection.

Set `rules.packs` with `yarp config` for user-wide packs. A trusted Pi project may instead keep a compiled pack at `.yarp/rules.yrp`; the extension ignores that path until Pi reports the project as trusted. YARP does not scan for source packs, compile them automatically, download rules, or execute code from a rule.

See the [indexed output summary plan](docs/indexed-output-summaries-implementation-plan.md) and the checked-in JSON schemas under `rules/schema/` for the complete reducer contract. They define matching and limits plus fail-open behavior. The [global output cap plan](docs/global-output-cap-implementation-plan.md) defines the size-first fallback and recovery contract. The [shell result planning implementation plan](docs/shell-result-planning-implementation-plan.md) records the shell parser and result-selection design.

## Archive

YARP stores tool calls in one local SQLite database by default:

```text
~/.local/share/yarp/tool-calls.sqlite3
```

Inputs and results are stored before and after YARP processing. Wrapped shell commands also store exact stdout and stderr before and after pruning. Identical snapshots share one compressed payload.

Typed summaries and globally capped results include an opaque reference when output is omitted. Search one archived call, then copy an exact range printed by the search result:

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

Run `yarp config set archive.enabled false` to opt out of capture. The archive may contain commands, source code, file contents, and secrets printed by tools. It stays local and uses private filesystem permissions. See the [archive specification](docs/tool-call-archive-spec.md) for the full format and failure rules.

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
target/release/toolcall-extractor analyze-ceiling
```

The command prints aggregate JSON with fixed public family labels to standard output. It does not create another persistent store; callers may redirect the report under their own private `umask` when they need a local snapshot. See the [reduction ceiling analysis](docs/reduction-ceiling-analysis.md) for the report fields and review process.

For a different Unix user, run only the reader under that identity and keep the DuckDB writer under your own account:

```sh
sudo -u other-user target/release/toolcall-extractor stream \
  --unix-user other-user claude --projects /home/other-user/.claude/projects |
  target/release/toolcall-extractor ingest --unix-user other-user --agent claude
```

This sends framed normalized records through the pipe and creates no intermediate export. An unchanged replay keeps the same calls and results. A changed source replaces the records owned by that source item.

The default database is `~/.local/share/toolcall-extractor/toolcalls.duckdb`. Tool inputs and outputs can contain secrets, so its directory and files are private. The extractor reads agent state without modifying it, has no network code, and stops before its files reach 10,000,000,000 bytes. See [the implementation plan](docs/toolcall-extractor-implementation-plan.md) for supported formats and privacy boundaries.

A complete local validation across Pi, Codex, Claude Code, and Cursor imported 718,008 calls with no orphan records. The production matcher evaluated all 371,241 stored shell results. Typed summaries changed 38,064 results and removed 377,272,234 characters: 91.5737% of eligible output, 26.0993% of shell output, and 19.4298% of all rendered output. No ambiguous reduction or registered diagnostic veto occurred. The generated database and transcripts remain outside the repository, as does the report.

## Limits

Each rule selects a typed search, diff, test, build, log, status, list, or literal-filter summary. Success and failure policies independently bound one line and total output and require both an absolute and proportional saving. Generic words such as `error` are shown as source-term samples unless the command-specific parser has enough evidence to label a diagnostic. Typed processing is streaming and bounded below 4 MiB per stream or result.

After typed processing, YARP measures the UTF-8 bytes across all text blocks in an archived Pi result. Text within the configured cap stays byte-for-byte exact. Larger text keeps UTF-8-safe content from the beginning and end, with the recovery marker counted inside the same budget. Image blocks remain unchanged and do not count toward the text cap. The output-size study used Unicode characters, so its thresholds are not byte-identical for non-ASCII output.

YARP does not collect usage data or access the network. Archive capture is local and enabled by default. The offline extractor writes only when invoked explicitly.

## License

[MIT](LICENSE)
