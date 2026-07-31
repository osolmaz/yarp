# YARP

YARP is a small command-output pruner for Pi. It wraps a strict allowlist of developer commands and removes the middle of very long output before that output enters Pi's context.

YARP leaves unsupported commands, shell pipelines, redirects, substitutions, and compound commands unchanged. It preserves the wrapped command's exit code and keeps stdout and stderr separate.

## Install

Install the Rust binary:

```sh
cargo install --git https://github.com/osolmaz/yarp.git --locked
```

Then install the Pi package:

```sh
pi install git:github.com/osolmaz/yarp
```

The repository is private, so both commands use your existing GitHub access.

## Use

The Pi extension handles supported `bash` and `exec_command` calls automatically. Supported command families include:

- `git status`, `git diff`, `git log`, and `git show`
- `cargo build`, `cargo check`, `cargo clippy`, and `cargo test`
- `go test`, `pytest`, and `dotnet build` or `dotnet test`
- test, build, lint, check, and type-check scripts run through npm, pnpm, or Yarn

Run a supported command directly when needed:

```sh
yarp run -- cargo test --workspace
```

Ask YARP whether a shell command can be wrapped:

```sh
yarp rewrite "git status --short"
```

An unsupported command exits with status 3 and prints nothing. The Pi extension treats that result as a request to run the original command.

Set `YARP_DISABLED=1` to turn off automatic rewriting.

## Limits

YARP keeps the first 160 and last 40 lines of each output stream. It marks omitted lines in the middle. A single line is limited to 16 KiB.

YARP does not store output, collect usage data, access the network, or keep command history.
