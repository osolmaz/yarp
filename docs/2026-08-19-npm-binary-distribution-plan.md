---
title: One-command npm distribution for YARP
author: Onur Solmaz <2453968+osolmaz@users.noreply.github.com>
date: 2026-08-19
---

# One-command npm distribution for YARP

## Status

Deferred. This document records a possible future distribution design. It does not authorize package creation or publication.

## Goal

Let a Pi user install YARP with one command and without a local Rust toolchain:

```sh
pi install npm:pi-yarp
```

The installation must provide the Pi extension, the YARP skill, and a compatible native YARP binary. It must not download executable files from an install script or compile Rust during installation.

## Current contract

YARP currently has two distribution paths:

```sh
cargo install yarp-cli --locked
pi install git:github.com/osolmaz/yarp
```

The `yarp-cli` crate supplies the Rust binary. The Git repository supplies the Pi extension and skill. The npm package manifest remains private, and the repository does not publish a Pi package to npm.

## Proposed package layout

Use separate packages for the portable Pi integration and each supported native binary:

- `pi-yarp`: Pi extension, skill, JavaScript launcher, and package selection logic.
- One native package for each supported operating-system and CPU pair, such as `@osolmaz/yarp-linux-x64`.
- `yarp-cli` on crates.io: the existing direct Rust installation path.

`pi-yarp` declares the native packages as exact-version `optionalDependencies`. Each native package uses npm `os` and `cpu` fields so npm selects only the compatible package. The Pi extension resolves the selected binary by absolute package path. It does not depend on the user's `PATH`.

Package names and the first supported platform set require explicit approval before publication.

## Release design

1. Build release binaries on GitHub-hosted runners for each supported platform.
2. Run the native test suite on the same platform before packaging its binary.
3. Publish native packages and `pi-yarp` from one immutable release tag.
4. Use npm trusted publishing with provenance. Do not store an npm token in the repository.
5. Pin every native optional dependency to the exact `pi-yarp` version.
6. Publish the crates.io package from the same source tag when its version changes.
7. Produce checksums for release inspection, while relying on npm package integrity for installation.

A platform is supported only after its build and end-to-end tests pass in CI. The first release must not claim platforms that the repository does not test.

## Runtime behavior

The extension resolves the binary during startup and verifies that its version matches the Pi package version. A missing, unsupported, or mismatched native package disables YARP and reports one precise installation error.

The release replaces the Pi runtime's dependency on a separately installed Cargo binary. It does not keep a `PATH` fallback. The crates.io package remains available for users who want the standalone CLI directly.

## Non-goals

- Downloading a binary from GitHub or another host in `postinstall`.
- Compiling Rust during npm installation.
- Shipping several platform binaries in one npm tarball.
- Selecting an unpinned or `latest` binary at installation time.
- Claiming support for an operating system that CI does not test.
- Changing YARP's pruning, archive, recovery, or privacy behavior.

## Implementation steps

1. Audit the Rust binary and archive behavior on each proposed platform.
2. Add a release build matrix with pinned actions and platform-specific tests.
3. Add the native package manifests with narrow file allowlists, `os`, `cpu`, license, repository, and provenance metadata.
4. Add the `pi-yarp` manifest with the `pi-package` keyword, extension and skill resources, exact optional dependencies, and a small binary resolver.
5. Change the extension to invoke only the package-local binary.
6. Add fresh-install tests that use packed npm artifacts rather than repository paths.
7. Bootstrap the npm package names, configure trusted publishers, and verify public visibility before the first real release.
8. Update the README and OnurPi wrapper only after the npm installation passes end-to-end checks.

## Acceptance criteria

- A fresh supported host without Rust can run `pi install npm:pi-yarp`.
- Installation runs no package lifecycle script that downloads or compiles executable code.
- Pi loads the extension and skill from the installed npm package.
- A real supported shell command is rewritten, reduced, archived, and recoverable through the packaged binary.
- The extension fails clearly on an unsupported platform or a version mismatch.
- The npm tarballs contain only declared runtime files and the correct platform binary.
- npm provenance identifies the YARP repository and immutable release tag.
- The existing archive integrity, permission, privacy, and recovery tests still pass.

## Verification

Run these checks against packed release artifacts in a clean temporary environment:

```sh
npm pack --dry-run --json
npm audit
cargo fmt --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
npm run typecheck:pi
npm run coverage:pi
slophammer-rs check .
```

For each supported platform, install the packed `pi-yarp` and native package artifacts, start Pi with the package, run a command that produces reducible output, and recover omitted output through the returned YARP reference. Verify the package page only after npm shows the published version and `pi-package` keyword.
