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

The output directory must have mode `0700`. The command walks and creates parent directories with descriptor-relative no-follow operations, writes the report atomically with mode `0600`, and rejects symlinks in every path component. It prints only a completion message to standard output.

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

The read-only analyzer examined the same 371,241 shell results and 1,445,526,406 output characters with YARP's bounded Bash parser and lexical feature scanner. It found 211 parser failures. The report retains no command text or output. Counts in the table overlap because one command can use several forms.

| Shell form | Results | Share of shell results | Output characters | Share of shell output |
| --- | ---: | ---: | ---: | ---: |
| Pipeline `\|` | 85,405 | 23.01% | 377,915,307 | 26.14% |
| AND chain `&&` | 72,448 | 19.52% | 305,684,143 | 21.15% |
| Redirection other than `2>&1` or `1>&2` | 48,388 | 13.03% | 114,679,650 | 7.93% |
| Semicolon chain `;` | 44,579 | 12.01% | 117,295,522 | 8.11% |
| Multiline shell input | 36,139 | 9.73% | 78,919,466 | 5.46% |
| Parameter expansion | 25,079 | 6.76% | 68,821,658 | 4.76% |
| OR chain `\|\|` | 21,489 | 5.79% | 41,001,742 | 2.84% |
| Command substitution | 19,828 | 5.34% | 43,830,953 | 3.03% |
| Executable path | 9,004 | 2.43% | 17,844,279 | 1.23% |
| Loop or conditional | 8,416 | 2.27% | 20,884,933 | 1.44% |
| Unsupported wrapper | 4,366 | 1.18% | 14,151,654 | 0.98% |
| `2>&1` or `1>&2` stream merge | 7,800 | 2.10% | 7,305,229 | 0.51% |
| Comment | 2,460 | 0.66% | 3,152,780 | 0.22% |
| Nested shell | 1,166 | 0.31% | 1,803,679 | 0.12% |
| Background command | 1,334 | 0.36% | 1,266,407 | 0.09% |
| Subshell group | 660 | 0.18% | 980,491 | 0.07% |
| Shell function | 297 | 0.08% | 470,289 | 0.03% |
| Parser failure | 211 | 0.06% | 162,887 | 0.01% |

The result planner accepts a chain or pipeline only when every possible visible output has one reducer family. It also accepts reviewed setup commands and assignments along with known wrappers and the `2>&1` and `1>&2` stream merges. The planner selected 16,945 compound results. Of those, 10,093 changed and removed 82,731,268 characters. Pipelines accounted for 12,990 selected results, 8,660 changed results, and 71,113,736 removed characters.

The broad `shell/unsupported-syntax` family now contains 167,134 unchanged results and 628,070,213 output characters. The operator and syntax rows above explain much of that family, but they must not be added together because of overlap. Exact-text inspection, structured output, count-only output, and machine-readable forms remain separate intentional pass-through categories.

## Shell result planning

The [shell result planning implementation plan](shell-result-planning-implementation-plan.md) records the parser, rule contract, safety checks, and release gates. One non-executing parser now serves simple commands, chains, and pipelines. It rejects incomplete or recovered parses while preserving the original command text. The planner carries typed output only through reviewed line-preserving stages. Every other stage vetoes reduction. Current output types cover search, list, test, build, diff, log, and status output.

The first transforms are guarded forms of `cat`, `tee`, line-based `head` and `tail`, `sort`, and `uniq`. Their guards cover byte output, NUL separators, numbering, counts, custom formatting, file operands that would introduce unrelated input, and other forms that change the output contract. A pass-through guard or unknown stage anywhere in the pipeline blocks reduction.

YARP executes the original pipeline unchanged. Rewriting individual stages could change buffering, signals, stream routing, and exit status. The Pi extension archives the exact result before the existing `tool_result` hook asks YARP to reduce it. Without explicit `pipefail`, the planner uses the larger failure policy because an earlier stage may have failed even when the pipeline exit code is zero.

Parameter expansion, command substitution, arbitrary redirection, loops, background jobs, nested shells, process substitution, here documents, executable paths, and aliases still pass through when their meaning cannot be established without running or expanding shell source.

Before this work, typed summaries changed 29,439 results and removed 306,302,222 characters. The combined parser, planner, transforms, and ranked evidence selector change 37,466 results and remove 375,125,749 characters. The absolute gain is 8,027 changed results and 68,823,527 removed characters, or 4.7611 percentage points of shell output. This clears the proposed one-point minimum effect by 54,368,263 characters. Registered diagnostic vetoes and missing recovery markers remain zero. Release still requires retrieval checks, bounded memory, and the throughput and latency gates.

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
