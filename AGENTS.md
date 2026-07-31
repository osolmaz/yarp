# YARP

- Keep the command rewrite allowlist narrow. Rewrite failures fail open to the original command; archive failures follow `docs/tool-call-archive-spec.md`.
- Preserve child exit codes and keep stdout and stderr on their original streams.
- Do not add telemetry, analytics, or network access.
- Keep persistent runtime state limited to the local tool-call archive defined in `docs/tool-call-archive-spec.md` unless the user explicitly approves another store.
- Keep `toolcall-extractor` offline and independent from the live archive and pruning runtime. It may read only explicitly supplied local session roots, must never alter agent state, and writes only its own DuckDB database. Follow `docs/toolcall-extractor-implementation-plan.md`.
- Use strict TypeScript without explicit `any`, unsafe casts, or unchecked external input.
- Do not use unsafe Rust.
- Add tests for every behavior change.
- Avoid runtime dependencies when the standard library is enough.
- Follow https://github.com/osolmaz/slophammer/blob/main/docs/AGENT_ENTRYPOINT.md.
- Before finishing, run `cargo fmt --check`, `cargo check --workspace --locked`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --all-targets --locked`, `cargo llvm-cov --workspace --all-targets --fail-under-lines 85`, `cargo audit --deny warnings`, `npm audit`, `npm run typecheck:pi`, `npm run test:pi`, `slophammer-rs check .`, and `git diff --check`.
- Mutation tests are declared in CI. Run them only when explicitly requested or when investigating test quality.
