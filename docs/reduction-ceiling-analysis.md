# Reduction ceiling analysis

The reduction ceiling analysis is an offline report over the private `toolcall-extractor` database. It measures current YARP behavior, accounts for every unchanged shell result, and ranks public command families by the output they could remove under a fixed summary budget.

The report helps decide which typed reducers deserve review. It does not approve a rule or predict savings from a reducer that has not been written.

## Run the analysis

Build the offline extractor, then give the command an explicit output path:

```sh
cargo build --release -p toolcall-extractor
target/release/toolcall-extractor analyze-ceiling \
  --output "$HOME/.local/share/toolcall-extractor/analysis/reduction-ceiling.json"
```

The default database remains `$HOME/.local/share/toolcall-extractor/toolcalls.duckdb`. Use `--database` to select another extracted corpus.

The output directory must have mode `0700`. The command creates a missing directory with that mode, writes the report atomically with mode `0600`, and rejects symlink destinations. It prints only a completion message to standard output.

## Scope

The analysis covers stored text results from recognized shell tools. It follows `benchmark-yarp` for command selection and uses the same reducers and character accounting. Child-status handling and recovery markers also match the benchmark.

Results that YARP already changes contribute to the observed baseline. Each unchanged result goes into one of four classes:

- `existing_rule` means a typed rule selected the result, but the savings gates kept the original bytes.
- `review_candidate` means a fixed public command category has a plausible typed reducer. A maintainer still has to review its flags and output shapes.
- `required_pass_through` covers guards, ambiguous matches, unsupported shell syntax, exact inspection, structured output, and other forms that the current contract preserves.
- `unclear` covers commands that cannot be typed from privacy-safe command features alone.

These classes describe the current policy. They do not change runtime behavior.

## Character-budget scenarios

The default scenario assumes a future summary occupies at most 704 Unicode scalar values. A result counts only when that replacement would remove at least 256 characters and 15% of the original result.

The totals contain production behavior measured by the current matcher and reducers. Three scenarios show what additional rules could remove:

- `review_candidates_at_budget` adds the hypothetical savings from every review candidate.
- `review_candidates_and_unclear_at_budget` also assumes every unclear family becomes safely reducible.
- `all_unchanged_at_budget` applies the budget to every unchanged result. This diagnostic upper bound violates pass-through policy and cannot justify a release.

The scenarios are budget calculations. They do not show that a useful summary exists. A family becomes a real candidate only after representative success and failure outputs pass semantic review and a production reducer reproduces the projected savings.

## Family metrics

Families are sorted by hypothetical removable characters. Each family includes result and character counts, status coverage, line volume, and the largest stored result. It also counts results with diagnostic words, non-ASCII text, NUL bytes, and lines over the report's stated byte threshold.

Family IDs come from public built-in rule IDs or a fixed category in the analyzer. The report omits arbitrary executable names, script names, arguments, paths, working directories, agent identities, session IDs, call IDs, timestamps and hashes. It also omits all source text.

## Shell syntax coverage

A separate read-only census examined the same 371,241 shell results and 1,445,526,406 output characters. It used a Bash syntax tree when parsing succeeded and a quote-aware scanner for the 24,934 commands that the parser could not read. The census retained no command text or output. Counts in the table overlap because one command can use several forms.

| Shell form | Results | Share of shell results | Output characters | Share of shell output |
| --- | ---: | ---: | ---: | ---: |
| Pipeline `\|` | 85,499 | 23.03% | 378,071,055 | 26.15% |
| AND chain `&&` | 72,465 | 19.52% | 305,685,936 | 21.15% |
| Redirection other than `2>&1` or `1>&2` | 47,612 | 12.83% | 114,227,424 | 7.90% |
| Semicolon chain `;` | 44,234 | 11.92% | 116,415,972 | 8.05% |
| Multiline shell input | 36,139 | 9.73% | 78,919,466 | 5.46% |
| Parameter expansion | 25,272 | 6.81% | 69,522,342 | 4.81% |
| OR chain `\|\|` | 21,484 | 5.79% | 40,996,357 | 2.84% |
| Command substitution | 19,993 | 5.39% | 43,851,862 | 3.03% |
| Executable path | 8,142 | 2.19% | 16,586,798 | 1.15% |
| Loop or conditional | 7,032 | 1.89% | 18,910,812 | 1.31% |
| Unsupported wrapper | 3,874 | 1.04% | 12,900,853 | 0.89% |
| Comment | 2,455 | 0.66% | 3,150,079 | 0.22% |
| Background command | 2,402 | 0.65% | 2,073,429 | 0.14% |
| Nested shell | 1,069 | 0.29% | 1,744,076 | 0.12% |
| Subshell group | 491 | 0.13% | 831,575 | 0.06% |
| Shell function | 166 | 0.04% | 394,181 | 0.03% |

The current result classifier accepts simple `;`, `&&`, and `||` chains only when each output command selects the same reducer family. It also accepts reviewed setup commands and assignments along with known wrappers and the `2>&1` and `1>&2` stream merges. This selected 3,958 compound results in the measured corpus. Of those, 1,486 changed and removed 11,782,428 characters. A pipeline always passes through.

The broad `shell/unsupported-syntax` family contains 179,696 unchanged results and 702,810,072 output characters. The operator and syntax rows above explain much of that family, but they must not be added together because of overlap. Exact-text inspection, structured output, count-only output, and machine-readable forms remain separate intentional pass-through categories.

## Shell result planning

Pipeline support should use one shell result planner across command combinations. A non-executing parser should produce a syntax tree and reject incomplete or recovered parses while preserving the original command text. The planner should identify typed output and carry it only through reviewed line-preserving stages. Every other stage vetoes reduction. Current output types cover search and list output plus test and build output.

The first useful transforms are `cat`, `tee`, line-based `head` and `tail`, `sort`, and `uniq`. Their options need guards for byte output, NUL separators, numbering, counts, custom formatting, and other forms that change line content. A pass-through guard or unknown stage anywhere in the pipeline blocks reduction.

YARP should execute the original pipeline unchanged. Rewriting individual stages could change buffering, signals, stream routing, and exit status. The Pi extension can continue archiving the exact result before the existing `tool_result` hook asks YARP to reduce it. Without explicit `pipefail`, the planner should use the larger failure policy because an earlier stage may have failed even when the pipeline exit code is zero.

Pipelines and compatible linear chains have the best measured opportunity. Simple multiline input and reviewed wrappers can use the same planner. Parameter expansion, command substitution, arbitrary redirection, loops, background jobs, nested shells, executable paths, and aliases should continue to pass through until their meaning can be established without running or expanding shell source.

A proposed retrospective release threshold is one additional percentage point of shell-output reduction, equal to 14,455,264 characters in this corpus. The maintainer must approve that threshold before it becomes a decision rule. Every release still requires zero registered diagnostic vetoes, exact archive recovery, model retrieval checks, bounded memory, and the existing throughput and latency gates.

This design changes no Pi internals or session entries. It adds no persistent schema or runtime state. It continues using Pi's documented shell-tool hooks.

## Review process

Review families in descending order of `removable_characters_at_budget`. For each family:

1. Check whether its command and flag forms have one stable human-readable output contract.
2. Review successful and failed forms in the private database. Inspect truncated and structured forms as well as exact inspections and streaming output.
3. Mark unsafe forms with a build-time guard before adding a reducer rule.
4. Implement the typed reducer and public synthetic fixtures.
5. Run `benchmark-yarp` again and compare observed removal with the budget scenario.
6. Keep the rule only when diagnostic retention and exact recovery pass. Model retrieval must work, and the memory and throughput gates must pass too.

Private samples and review notes stay outside Git. The same rule applies to databases and generated reports. Only the analyzer, its synthetic tests, and this method belong in the repository.
