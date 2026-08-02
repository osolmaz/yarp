use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use yarp_cli::reducers::RecoveryMarker;
use yarp_cli::rules::Reducer;

use crate::benchmark::{
    BenchmarkSelection, benchmark_selection, result_succeeded, shell_command, use_success_policy,
};
use crate::database::Database;
use crate::error::Result;

const SCOPE: &str = "stored_shell_result_text";
const RECOVERY_REFERENCE: &str = "yr_0123456789abcdef0123456789abcdef";
const RECOVERY_SOURCE: &str = "result_text";
const RECOVERY_COMPLETENESS: &str = "unknown";
const DIAGNOSTIC_TERMS: [&[u8]; 5] = [b"failure", b"panic", b"error", b"warning", b"test result"];
const OVERSIZED_LINE_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug)]
pub struct AnalysisOptions {
    pub summary_character_budget: u64,
    pub minimum_removed_characters: u64,
    pub minimum_savings_basis_points: u64,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self {
            summary_character_budget: 704,
            minimum_removed_characters: 256,
            minimum_savings_basis_points: 1_500,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CeilingReport {
    pub scope: &'static str,
    pub assumptions: Assumptions,
    pub totals: Totals,
    pub scenarios: Scenarios,
    pub shell_syntax: Vec<SyntaxReport>,
    pub families: Vec<FamilyReport>,
}

#[derive(Debug, Serialize)]
pub struct Assumptions {
    pub summary_character_budget: u64,
    pub minimum_removed_characters: u64,
    pub minimum_savings_basis_points: u64,
    pub oversized_line_threshold_bytes: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub evaluated_results: u64,
    pub evaluated_output_characters: u64,
    pub shell_results: u64,
    pub shell_output_characters: u64,
    pub observed_changed_results: u64,
    pub observed_removed_characters: u64,
    pub unchanged_shell_results: u64,
    pub unchanged_shell_output_characters: u64,
}

#[derive(Debug, Serialize)]
pub struct Scenarios {
    pub review_candidates_at_budget: Scenario,
    pub review_candidates_and_unclear_at_budget: Scenario,
    pub all_unchanged_at_budget: Scenario,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Scenario {
    pub additional_changed_results: u64,
    pub additional_removed_characters: u64,
    pub total_changed_results: u64,
    pub total_removed_characters: u64,
    pub removed_percent_of_shell_output: f64,
    pub removed_percent_of_evaluated_output: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FamilyMetrics {
    pub results: u64,
    pub output_characters: u64,
    pub output_lines: u64,
    pub maximum_result_characters: u64,
    pub success_results: u64,
    pub failure_results: u64,
    pub unknown_status_results: u64,
    pub diagnostic_results: u64,
    pub non_ascii_results: u64,
    pub nul_results: u64,
    pub oversized_line_results: u64,
    pub eligible_results_at_budget: u64,
    pub removable_characters_at_budget: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SyntaxMetrics {
    pub results: u64,
    pub output_characters: u64,
    pub eligible_results_at_budget: u64,
    pub removable_characters_at_budget: u64,
}

#[derive(Debug, Serialize)]
pub struct SyntaxReport {
    pub id: &'static str,
    pub metrics: SyntaxMetrics,
}

#[derive(Debug, Serialize)]
pub struct FamilyReport {
    pub id: String,
    pub classification: FamilyClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_reducer: Option<SuggestedReducer>,
    pub metrics: FamilyMetrics,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilyClass {
    ExistingRule,
    ReviewCandidate,
    RequiredPassThrough,
    Unclear,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestedReducer {
    SearchSummary,
    DiffSummary,
    TestSummary,
    BuildSummary,
    LogSummary,
    StatusSummary,
    ListSummary,
    LineFilter,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FamilyKey {
    id: String,
    class: FamilyClass,
    suggested_reducer: Option<SuggestedReducer>,
}

#[derive(Default)]
struct Accumulator {
    totals: Totals,
    shell_syntax: BTreeMap<&'static str, SyntaxMetrics>,
    families: BTreeMap<FamilyKey, FamilyMetrics>,
}

/// Analyze the remaining shell-result output without emitting source content.
///
/// # Errors
///
/// Returns an error when the private corpus cannot be opened or queried.
pub fn run(path: &Path, options: AnalysisOptions) -> Result<CeilingReport> {
    let connection = Database::open_read_only(path)?;
    let mut statement = connection.prepare(
        "SELECT c.tool_name, c.input_format, c.input_text, r.output_text, r.is_error,
                r.output_json
         FROM tool_calls c
         JOIN tool_results r ON r.call_key = c.call_key
         WHERE r.output_text IS NOT NULL",
    )?;
    let mut rows = statement.query([])?;
    let mut accumulator = Accumulator::default();
    while let Some(row) = rows.next()? {
        let tool: String = row.get(0)?;
        let input_format: String = row.get(1)?;
        let input: String = row.get(2)?;
        let output: String = row.get(3)?;
        let is_error: Option<bool> = row.get(4)?;
        let output_json: Option<String> = row.get(5)?;
        accumulator.observe_result(
            &tool,
            &input_format,
            &input,
            &output,
            is_error,
            output_json.as_deref(),
            options,
        );
    }
    Ok(accumulator.finish(options))
}

impl Accumulator {
    fn observe_result(
        &mut self,
        tool: &str,
        input_format: &str,
        input: &str,
        output: &str,
        is_error: Option<bool>,
        output_json: Option<&str>,
        options: AnalysisOptions,
    ) {
        let output_characters = character_count(output);
        self.totals.evaluated_results = self.totals.evaluated_results.saturating_add(1);
        self.totals.evaluated_output_characters = self
            .totals
            .evaluated_output_characters
            .saturating_add(output_characters);
        let Some(command) = shell_command(tool, input_format, input) else {
            return;
        };
        self.totals.shell_results = self.totals.shell_results.saturating_add(1);
        self.totals.shell_output_characters = self
            .totals
            .shell_output_characters
            .saturating_add(output_characters);
        for id in yarp_cli::shell::inspect_syntax(&command).labels() {
            self.shell_syntax
                .entry(id)
                .or_default()
                .observe(output_characters, options);
        }
        let selection = benchmark_selection(&command);
        let succeeded = result_succeeded(is_error, output_json);
        let current = current_output(&selection, output, succeeded);
        if current.as_slice() != output.as_bytes() {
            self.totals.observed_changed_results =
                self.totals.observed_changed_results.saturating_add(1);
            self.totals.observed_removed_characters = self
                .totals
                .observed_removed_characters
                .saturating_add(output_characters.saturating_sub(character_count_bytes(&current)));
            return;
        }
        self.totals.unchanged_shell_results = self.totals.unchanged_shell_results.saturating_add(1);
        self.totals.unchanged_shell_output_characters = self
            .totals
            .unchanged_shell_output_characters
            .saturating_add(output_characters);
        let key = family_for_unchanged(&selection, &command);
        self.families.entry(key).or_default().observe(
            output,
            output_characters,
            succeeded,
            options,
        );
    }

    fn finish(self, options: AnalysisOptions) -> CeilingReport {
        let mut shell_syntax = self
            .shell_syntax
            .into_iter()
            .map(|(id, metrics)| SyntaxReport { id, metrics })
            .collect::<Vec<_>>();
        shell_syntax.sort_by(|left, right| {
            right
                .metrics
                .output_characters
                .cmp(&left.metrics.output_characters)
                .then_with(|| left.id.cmp(right.id))
        });
        let mut classes = BTreeMap::<FamilyClass, FamilyMetrics>::new();
        let mut families = self
            .families
            .into_iter()
            .map(|(key, metrics)| {
                classes.entry(key.class).or_default().add(&metrics);
                FamilyReport {
                    id: key.id,
                    classification: key.class,
                    suggested_reducer: key.suggested_reducer,
                    metrics,
                }
            })
            .collect::<Vec<_>>();
        families.sort_by(|left, right| {
            right
                .metrics
                .removable_characters_at_budget
                .cmp(&left.metrics.removable_characters_at_budget)
                .then_with(|| left.id.cmp(&right.id))
        });
        let candidates = scenario(
            &self.totals,
            classes_for(&classes, &[FamilyClass::ReviewCandidate]),
        );
        let candidates_and_unclear = scenario(
            &self.totals,
            classes_for(
                &classes,
                &[FamilyClass::ReviewCandidate, FamilyClass::Unclear],
            ),
        );
        let all_unchanged = scenario(&self.totals, classes.values());
        CeilingReport {
            scope: SCOPE,
            assumptions: Assumptions {
                summary_character_budget: options.summary_character_budget,
                minimum_removed_characters: options.minimum_removed_characters,
                minimum_savings_basis_points: options.minimum_savings_basis_points,
                oversized_line_threshold_bytes: OVERSIZED_LINE_BYTES,
            },
            totals: self.totals,
            scenarios: Scenarios {
                review_candidates_at_budget: candidates,
                review_candidates_and_unclear_at_budget: candidates_and_unclear,
                all_unchanged_at_budget: all_unchanged,
            },
            shell_syntax,
            families,
        }
    }
}

impl SyntaxMetrics {
    fn observe(&mut self, output_characters: u64, options: AnalysisOptions) {
        self.results = self.results.saturating_add(1);
        self.output_characters = self.output_characters.saturating_add(output_characters);
        let removable = budget_removal(output_characters, options);
        if removable > 0 {
            self.eligible_results_at_budget = self.eligible_results_at_budget.saturating_add(1);
            self.removable_characters_at_budget = self
                .removable_characters_at_budget
                .saturating_add(removable);
        }
    }
}

impl FamilyMetrics {
    fn observe(
        &mut self,
        output: &str,
        output_characters: u64,
        succeeded: Option<bool>,
        options: AnalysisOptions,
    ) {
        self.results = self.results.saturating_add(1);
        self.output_characters = self.output_characters.saturating_add(output_characters);
        self.output_lines = self.output_lines.saturating_add(line_count(output));
        self.maximum_result_characters = self.maximum_result_characters.max(output_characters);
        match succeeded {
            Some(true) => self.success_results = self.success_results.saturating_add(1),
            Some(false) => self.failure_results = self.failure_results.saturating_add(1),
            None => {
                self.unknown_status_results = self.unknown_status_results.saturating_add(1);
            }
        }
        self.diagnostic_results = self
            .diagnostic_results
            .saturating_add(u64::from(contains_diagnostic(output.as_bytes())));
        self.non_ascii_results = self
            .non_ascii_results
            .saturating_add(u64::from(!output.is_ascii()));
        self.nul_results = self
            .nul_results
            .saturating_add(u64::from(output.as_bytes().contains(&0)));
        self.oversized_line_results = self
            .oversized_line_results
            .saturating_add(u64::from(has_oversized_line(output.as_bytes())));
        let removable = budget_removal(output_characters, options);
        if removable > 0 {
            self.eligible_results_at_budget = self.eligible_results_at_budget.saturating_add(1);
            self.removable_characters_at_budget = self
                .removable_characters_at_budget
                .saturating_add(removable);
        }
    }

    fn add(&mut self, other: &Self) {
        self.results = self.results.saturating_add(other.results);
        self.output_characters = self
            .output_characters
            .saturating_add(other.output_characters);
        self.output_lines = self.output_lines.saturating_add(other.output_lines);
        self.maximum_result_characters = self
            .maximum_result_characters
            .max(other.maximum_result_characters);
        self.success_results = self.success_results.saturating_add(other.success_results);
        self.failure_results = self.failure_results.saturating_add(other.failure_results);
        self.unknown_status_results = self
            .unknown_status_results
            .saturating_add(other.unknown_status_results);
        self.diagnostic_results = self
            .diagnostic_results
            .saturating_add(other.diagnostic_results);
        self.non_ascii_results = self
            .non_ascii_results
            .saturating_add(other.non_ascii_results);
        self.nul_results = self.nul_results.saturating_add(other.nul_results);
        self.oversized_line_results = self
            .oversized_line_results
            .saturating_add(other.oversized_line_results);
        self.eligible_results_at_budget = self
            .eligible_results_at_budget
            .saturating_add(other.eligible_results_at_budget);
        self.removable_characters_at_budget = self
            .removable_characters_at_budget
            .saturating_add(other.removable_characters_at_budget);
    }
}

fn current_output(
    selection: &BenchmarkSelection,
    output: &str,
    succeeded: Option<bool>,
) -> Vec<u8> {
    let BenchmarkSelection::Reduce {
        rule,
        status_confidence,
        transform_diagnostics,
        ..
    } = selection
    else {
        return output.as_bytes().to_vec();
    };
    yarp_cli::reducers::reduce_bytes_with_recovery_and_transform_diagnostics(
        rule,
        output.as_bytes(),
        use_success_policy(succeeded, *status_confidence),
        Some(RecoveryMarker {
            archive_ref: RECOVERY_REFERENCE,
            source: RECOVERY_SOURCE,
            completeness: RECOVERY_COMPLETENESS,
        }),
        *transform_diagnostics,
    )
    .unwrap_or_else(|_| output.as_bytes().to_vec())
}

fn family_for_unchanged(selection: &BenchmarkSelection, command: &str) -> FamilyKey {
    match selection {
        BenchmarkSelection::Reduce { rule, label, .. } => FamilyKey {
            id: format!("existing/{label}"),
            class: FamilyClass::ExistingRule,
            suggested_reducer: rule.reducer.as_ref().map(suggested_reducer),
        },
        BenchmarkSelection::Passthrough { labels } => FamilyKey {
            id: public_rule_group("guarded", labels),
            class: FamilyClass::RequiredPassThrough,
            suggested_reducer: None,
        },
        BenchmarkSelection::Ambiguous { labels } => FamilyKey {
            id: public_rule_group("ambiguous", labels),
            class: FamilyClass::RequiredPassThrough,
            suggested_reducer: None,
        },
        BenchmarkSelection::Unsupported => unsupported_family(command),
    }
}

fn public_rule_group(prefix: &str, labels: &[String]) -> String {
    let mut labels = labels.to_vec();
    labels.sort();
    labels.dedup();
    format!("{prefix}/{}", labels.join("+"))
}

fn unsupported_family(command: &str) -> FamilyKey {
    let Ok((words, _)) = yarp_cli::rewrite::select_builtin_command(command) else {
        return required(
            "shell/unsupported-syntax",
            "compound, redirected, expanded, or otherwise unsupported shell source stays exact",
        );
    };
    assess_words(&words)
}

fn assess_words(words: &[String]) -> FamilyKey {
    let Some((program, arguments)) = normalized_program(words) else {
        return required(
            "shell/setup-or-wrapper",
            "setup commands and unsupported wrappers stay exact",
        );
    };
    match program {
        "ls" => list_candidate("filesystem/ls"),
        "find" => list_candidate("filesystem/find"),
        "fd" => list_candidate("filesystem/fd"),
        "tree" => list_candidate("filesystem/tree"),
        "du" => list_candidate("filesystem/du"),
        "df" => list_candidate("filesystem/df"),
        "ps" => list_candidate("system/ps"),
        "lsof" => list_candidate("system/lsof"),
        "free" => list_candidate("system/free"),
        "make" => build_candidate("build/make"),
        "ninja" => build_candidate("build/ninja"),
        "cmake" => build_candidate("build/cmake"),
        "meson" => build_candidate("build/meson"),
        "msbuild" => build_candidate("build/msbuild"),
        "bazel" | "buck" | "buck2" => assess_build_driver(program, arguments),
        "docker" | "podman" => assess_container(program, arguments),
        "kubectl" => assess_cluster(arguments),
        "git" => assess_git(arguments),
        "gh" => assess_gh(arguments),
        "npm" | "pnpm" | "yarn" | "bun" => assess_package(program, arguments),
        "cargo" => developer_unclear("developer/cargo-other"),
        "go" => developer_unclear("developer/go-other"),
        "dotnet" => developer_unclear("developer/dotnet-other"),
        "pytest" => developer_unclear("developer/pytest-other"),
        "ruff" => developer_unclear("developer/ruff-other"),
        "mypy" => developer_unclear("developer/mypy-other"),
        "pyright" => developer_unclear("developer/pyright-other"),
        "cat" | "head" | "tail" | "sed" | "awk" | "less" | "more" | "bat" => required(
            "inspection/exact-text",
            "file and stream inspection output is exact source evidence",
        ),
        "wc" | "cksum" | "md5sum" | "sha1sum" | "sha256sum" | "sha512sum" => required(
            "inspection/count-or-digest",
            "counts and digests are exact inspection results",
        ),
        "rg" | "grep" | "egrep" | "fgrep" | "ag" | "ack" => required(
            "search/unsupported-form",
            "unsupported search forms may be filename-only, count-only, structured, or exact",
        ),
        "curl" | "wget" | "http" | "xh" => required(
            "network/response",
            "network response bodies and protocol output stay exact",
        ),
        "jq" | "yq" | "xmlstarlet" | "sqlite3" | "psql" | "mysql" => required(
            "structured-or-query-output",
            "structured transformations and query results stay exact",
        ),
        "echo" | "printf" | "pwd" | "which" | "type" | "command" => required(
            "shell/exact-small-output",
            "shell-produced values are exact and usually small",
        ),
        "rm" | "mv" | "cp" | "install" | "mkdir" | "rmdir" | "touch" | "chmod" | "chown" | "ln"
        | "kill" | "pkill" => required(
            "mutation/command",
            "mutation command output is not a summary target",
        ),
        "python" | "python3" | "node" | "deno" | "ruby" | "perl" | "bash" | "sh" | "zsh"
        | "fish" => unclear(
            "script/other",
            "interpreter output cannot be typed from the interpreter name alone",
        ),
        "yarp" => required(
            "yarp/recovery-or-control",
            "YARP control and recovery output must not recurse",
        ),
        _ if program.starts_with('.') => unclear(
            "local-program/other",
            "local program output needs private semantic review",
        ),
        _ => unclear(
            "command/other",
            "the program is not in the fixed public command taxonomy",
        ),
    }
}

fn normalized_program(words: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0;
    while words.get(index).is_some_and(|word| is_assignment(word)) {
        index += 1;
    }
    match words.get(index).map(String::as_str) {
        Some("env") => {
            index += 1;
            if words.get(index).is_some_and(|word| word == "--") {
                index += 1;
            }
            while words.get(index).is_some_and(|word| is_assignment(word)) {
                index += 1;
            }
        }
        Some("command" | "exec") => {
            index += 1;
            if words.get(index).is_some_and(|word| word == "--") {
                index += 1;
            }
        }
        Some("time") => index += 1,
        Some("timeout") if words.get(index + 1).is_some() => index += 2,
        _ => {}
    }
    let raw_program = words.get(index)?;
    let program = raw_program.rsplit('/').next().unwrap_or(raw_program);
    Some((program, words.get(index + 1..).unwrap_or_default()))
}

fn list_candidate(id: &str) -> FamilyKey {
    candidate(
        id,
        "list_summary",
        "human-readable inventory output may support a typed list summary",
    )
}

fn build_candidate(id: &str) -> FamilyKey {
    candidate(
        id,
        "build_summary",
        "build-tool output may support the existing typed build summary",
    )
}

fn developer_unclear(id: &str) -> FamilyKey {
    unclear(
        id,
        "the unsupported developer-tool form needs semantic review",
    )
}

fn assess_build_driver(program: &str, arguments: &[String]) -> FamilyKey {
    match first_subcommand(program, arguments) {
        Some("test") => candidate(
            "build-driver/test",
            "test_summary",
            "test-runner output may support the existing typed test summary",
        ),
        Some("build" | "check") => candidate(
            "build-driver/build",
            "build_summary",
            "build output may support the existing typed build summary",
        ),
        _ => unclear(
            "build-driver/other",
            "build-driver output depends on an unsupported or missing subcommand",
        ),
    }
}

fn assess_container(program: &str, arguments: &[String]) -> FamilyKey {
    match first_subcommand(program, arguments) {
        Some("ps" | "images" | "stats") => candidate(
            "container/inventory",
            "list_summary",
            "human-readable container inventories may support a typed list summary",
        ),
        Some("logs") => candidate(
            "container/logs",
            "log_summary",
            "non-following container logs may support the typed log summary after flag review",
        ),
        Some("build" | "buildx") => candidate(
            "container/build",
            "build_summary",
            "container build output may support the typed build summary after progress-mode review",
        ),
        Some("inspect" | "exec" | "cp" | "save" | "export") => required(
            "container/exact-or-structured",
            "container inspection, execution, and transfer output stays exact",
        ),
        Some("run" | "start" | "stop" | "restart" | "rm" | "rmi" | "pull" | "push") => required(
            "container/mutation",
            "container lifecycle command output is not a summary target",
        ),
        _ => unclear(
            "container/other",
            "the container subcommand needs semantic review",
        ),
    }
}

fn assess_cluster(arguments: &[String]) -> FamilyKey {
    match first_subcommand("kubectl", arguments) {
        Some("events") => candidate(
            "cluster/events",
            "log_summary",
            "human-readable cluster events may support a typed log summary",
        ),
        Some("top") => candidate(
            "cluster/inventory",
            "list_summary",
            "human-readable cluster resource inventories may support a typed list summary",
        ),
        Some("logs") => candidate(
            "cluster/logs",
            "log_summary",
            "non-following cluster logs may support the typed log summary after flag review",
        ),
        Some("get" | "describe") => candidate(
            "cluster/inspection",
            "list_summary",
            "human-readable cluster inspection may support a typed list summary after output-mode review",
        ),
        Some("exec" | "cp" | "port-forward" | "proxy") => required(
            "cluster/exact-or-streaming",
            "cluster execution, transfer, and streaming output stays exact",
        ),
        Some("apply" | "create" | "delete" | "edit" | "patch" | "replace" | "rollout") => required(
            "cluster/mutation",
            "cluster mutation output is not a summary target",
        ),
        _ => unclear(
            "cluster/other",
            "the cluster subcommand needs semantic review",
        ),
    }
}

fn assess_git(arguments: &[String]) -> FamilyKey {
    let subcommand = git_subcommand(arguments);
    match subcommand {
        Some("tag" | "remote" | "worktree" | "stash") => candidate(
            "git/other-list",
            "list_summary",
            "human-readable Git inventories may support a typed list summary after form review",
        ),
        Some("reflog") => candidate(
            "git/reflog",
            "log_summary",
            "human-readable reflog output may support a typed log summary",
        ),
        Some(
            "show" | "blame" | "cat-file" | "rev-parse" | "ls-files" | "ls-tree" | "for-each-ref",
        ) => required(
            "git/exact-inspection",
            "Git object, filename, and exact-format inspection stays exact",
        ),
        Some(
            "add" | "commit" | "push" | "pull" | "fetch" | "merge" | "rebase" | "reset" | "restore"
            | "checkout" | "switch" | "cherry-pick" | "revert" | "clean" | "gc",
        ) => required(
            "git/mutation",
            "Git mutation and transport output is not a summary target",
        ),
        Some(_) => unclear(
            "git/other",
            "the unsupported Git form needs semantic review",
        ),
        None => unclear(
            "git/no-subcommand",
            "Git output without a clear subcommand needs review",
        ),
    }
}

fn assess_gh(arguments: &[String]) -> FamilyKey {
    match first_subcommand("gh", arguments) {
        Some("api") => required(
            "gh/api",
            "API output may be structured or exact and stays unchanged",
        ),
        Some("auth" | "config" | "alias" | "extension" | "secret" | "variable") => required(
            "gh/control-or-sensitive",
            "authentication, configuration, and potentially sensitive output stays exact",
        ),
        Some("pr" | "issue" | "run" | "workflow" | "release" | "repo" | "search") => candidate(
            "gh/other-inspection",
            "list_summary",
            "human-readable hosting inspection may support an existing typed summary after flag review",
        ),
        _ => unclear("gh/other", "the hosting subcommand needs semantic review"),
    }
}

fn assess_package(program: &str, arguments: &[String]) -> FamilyKey {
    let subcommand = first_subcommand(program, arguments);
    if matches!(
        subcommand,
        Some("install" | "add" | "remove" | "uninstall" | "update" | "upgrade" | "publish")
    ) {
        return required(
            "package/mutation",
            "package mutation output is not a summary target",
        );
    }
    if let Some(task @ ("test" | "check" | "build" | "lint" | "typecheck" | "coverage")) =
        subcommand
    {
        let (id, reducer) = package_task(program, task);
        return candidate(
            id,
            reducer,
            "a standard package task may support an existing typed summary after argument review",
        );
    }
    unclear(
        "package/other-task",
        "package task output cannot be typed from an unrecognized script name",
    )
}

fn package_task(program: &str, task: &str) -> (&'static str, &'static str) {
    match (program, task) {
        ("npm", "test" | "coverage") => ("package/npm-test-task", "test_summary"),
        ("pnpm", "test" | "coverage") => ("package/pnpm-test-task", "test_summary"),
        ("yarn", "test" | "coverage") => ("package/yarn-test-task", "test_summary"),
        ("bun", "test" | "coverage") => ("package/bun-test-task", "test_summary"),
        ("npm", "lint") => ("package/npm-lint-task", "line_filter"),
        ("pnpm", "lint") => ("package/pnpm-lint-task", "line_filter"),
        ("yarn", "lint") => ("package/yarn-lint-task", "line_filter"),
        ("bun", "lint") => ("package/bun-lint-task", "line_filter"),
        ("npm", _) => ("package/npm-build-task", "build_summary"),
        ("pnpm", _) => ("package/pnpm-build-task", "build_summary"),
        ("yarn", _) => ("package/yarn-build-task", "build_summary"),
        ("bun", _) => ("package/bun-build-task", "build_summary"),
        _ => unreachable!("fixed package taxonomy contains an unknown package manager"),
    }
}

fn first_subcommand<'a>(program: &str, arguments: &'a [String]) -> Option<&'a str> {
    let value_options: &[&str] = match program {
        "docker" | "podman" => &[
            "-c",
            "--context",
            "--config",
            "-H",
            "--host",
            "-l",
            "--log-level",
            "--connection",
            "--url",
            "--identity",
            "--root",
            "--runroot",
            "--tmpdir",
            "--storage-driver",
            "--events-backend",
            "--runtime",
        ],
        "kubectl" => &[
            "-n",
            "--namespace",
            "--context",
            "--cluster",
            "--user",
            "--kubeconfig",
            "-s",
            "--server",
            "--token",
            "--as",
            "--as-group",
            "--cache-dir",
            "--certificate-authority",
            "--client-certificate",
            "--client-key",
            "--request-timeout",
            "--tls-server-name",
            "--profile",
            "--profile-output",
        ],
        "gh" => &["-R", "--repo", "--hostname"],
        "npm" => &[
            "--prefix",
            "-w",
            "--workspace",
            "--userconfig",
            "--registry",
        ],
        "pnpm" | "yarn" | "bun" => &["-C", "--dir", "--filter", "--cwd", "--config", "--registry"],
        "bazel" | "buck" | "buck2" => &[
            "--output_base",
            "--output_user_root",
            "--server_javabase",
            "--bazelrc",
            "--host_jvm_args",
            "--isolation-dir",
        ],
        _ => &[],
    };
    let mut index = 0;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            return arguments.get(index + 1).map(String::as_str);
        }
        if value_options.contains(&argument) {
            index = index.saturating_add(2);
            continue;
        }
        if value_options.iter().any(|option| {
            let Some(suffix) = argument.strip_prefix(option) else {
                return false;
            };
            (!suffix.is_empty() && option.starts_with('-') && !option.starts_with("--"))
                || suffix.starts_with('=')
        }) {
            index = index.saturating_add(1);
            continue;
        }
        if argument.starts_with('-') {
            index = index.saturating_add(1);
            continue;
        }
        return Some(argument);
    }
    None
}

fn git_subcommand(arguments: &[String]) -> Option<&str> {
    let mut skip_value = false;
    for argument in arguments {
        if skip_value {
            skip_value = false;
            continue;
        }
        if matches!(
            argument.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            skip_value = true;
            continue;
        }
        if argument.starts_with('-') {
            continue;
        }
        return Some(argument);
    }
    None
}

fn candidate(id: &str, reducer: &str, _reason: &str) -> FamilyKey {
    FamilyKey {
        id: id.to_owned(),
        class: FamilyClass::ReviewCandidate,
        suggested_reducer: Some(suggested_reducer_label(reducer)),
    }
}

fn required(id: &str, _reason: &str) -> FamilyKey {
    FamilyKey {
        id: id.to_owned(),
        class: FamilyClass::RequiredPassThrough,
        suggested_reducer: None,
    }
}

fn unclear(id: &str, _reason: &str) -> FamilyKey {
    FamilyKey {
        id: id.to_owned(),
        class: FamilyClass::Unclear,
        suggested_reducer: None,
    }
}

fn suggested_reducer(reducer: &Reducer) -> SuggestedReducer {
    match reducer {
        Reducer::SearchSummary => SuggestedReducer::SearchSummary,
        Reducer::DiffSummary => SuggestedReducer::DiffSummary,
        Reducer::TestSummary => SuggestedReducer::TestSummary,
        Reducer::BuildSummary => SuggestedReducer::BuildSummary,
        Reducer::LogSummary => SuggestedReducer::LogSummary,
        Reducer::StatusSummary => SuggestedReducer::StatusSummary,
        Reducer::ListSummary => SuggestedReducer::ListSummary,
        Reducer::LineFilter { .. } => SuggestedReducer::LineFilter,
    }
}

fn suggested_reducer_label(reducer: &str) -> SuggestedReducer {
    match reducer {
        "search_summary" => SuggestedReducer::SearchSummary,
        "diff_summary" => SuggestedReducer::DiffSummary,
        "test_summary" => SuggestedReducer::TestSummary,
        "build_summary" => SuggestedReducer::BuildSummary,
        "log_summary" => SuggestedReducer::LogSummary,
        "status_summary" => SuggestedReducer::StatusSummary,
        "list_summary" => SuggestedReducer::ListSummary,
        "line_filter" => SuggestedReducer::LineFilter,
        _ => unreachable!("fixed reducer taxonomy contains an unknown reducer"),
    }
}

fn scenario<'a>(totals: &Totals, metrics: impl Iterator<Item = &'a FamilyMetrics>) -> Scenario {
    let (additional_changed_results, additional_removed_characters) =
        metrics.fold((0_u64, 0_u64), |(results, characters), metric| {
            (
                results.saturating_add(metric.eligible_results_at_budget),
                characters.saturating_add(metric.removable_characters_at_budget),
            )
        });
    let total_removed_characters = totals
        .observed_removed_characters
        .saturating_add(additional_removed_characters);
    Scenario {
        additional_changed_results,
        additional_removed_characters,
        total_changed_results: totals
            .observed_changed_results
            .saturating_add(additional_changed_results),
        total_removed_characters,
        removed_percent_of_shell_output: percent(
            total_removed_characters,
            totals.shell_output_characters,
        ),
        removed_percent_of_evaluated_output: percent(
            total_removed_characters,
            totals.evaluated_output_characters,
        ),
    }
}

fn classes_for<'a>(
    classes: &'a BTreeMap<FamilyClass, FamilyMetrics>,
    selected: &'a [FamilyClass],
) -> impl Iterator<Item = &'a FamilyMetrics> {
    selected.iter().filter_map(|class| classes.get(class))
}

fn budget_removal(original_characters: u64, options: AnalysisOptions) -> u64 {
    let removed = original_characters.saturating_sub(options.summary_character_budget);
    if removed < options.minimum_removed_characters {
        return 0;
    }
    let basis_points = u128::from(removed)
        .saturating_mul(10_000)
        .checked_div(u128::from(original_characters))
        .unwrap_or(0);
    if basis_points < u128::from(options.minimum_savings_basis_points) {
        0
    } else {
        removed
    }
}

fn character_count(value: &str) -> u64 {
    u64::try_from(value.chars().count()).unwrap_or(u64::MAX)
}

fn character_count_bytes(value: &[u8]) -> u64 {
    character_count(&String::from_utf8_lossy(value))
}

fn line_count(value: &str) -> u64 {
    let newlines = value
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    let unterminated = usize::from(!value.is_empty() && !value.ends_with('\n'));
    u64::try_from(newlines.saturating_add(unterminated)).unwrap_or(u64::MAX)
}

fn contains_diagnostic(value: &[u8]) -> bool {
    DIAGNOSTIC_TERMS
        .iter()
        .any(|term| contains_ascii_case_insensitive(value, term))
}

fn contains_ascii_case_insensitive(value: &[u8], needle: &[u8]) -> bool {
    value.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn has_oversized_line(value: &[u8]) -> bool {
    value
        .split(|byte| *byte == b'\n')
        .any(|line| line.len() > OVERSIZED_LINE_BYTES)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{InputFormat, SessionRecord, ToolCallRecord, ToolResultRecord};
    use crate::sink::Sink;

    use super::*;

    #[test]
    fn applies_budget_and_savings_gates() {
        let options = AnalysisOptions::default();
        assert_eq!(budget_removal(704, options), 0);
        assert_eq!(budget_removal(900, options), 0);
        assert_eq!(budget_removal(1_000, options), 296);
    }

    #[test]
    fn fixed_taxonomy_never_copies_command_values() {
        assert_eq!(
            unsupported_family("find /private/customer-name -type f").id,
            "filesystem/find"
        );
        assert_eq!(
            unsupported_family("./secret-project-check --customer acme").id,
            "command/other"
        );
        assert_eq!(
            unsupported_family("printf 'secret value'").id,
            "shell/exact-small-output"
        );
        assert_eq!(
            unsupported_family("cat private.txt | grep password").id,
            "shell/unsupported-syntax"
        );
    }

    #[test]
    fn classifies_public_command_families_conservatively() {
        assert_eq!(
            unsupported_family("ls -la").class,
            FamilyClass::ReviewCandidate
        );
        assert_eq!(
            unsupported_family("cat file").class,
            FamilyClass::RequiredPassThrough
        );
        assert_eq!(
            unsupported_family("python private.py").class,
            FamilyClass::Unclear
        );
        assert_eq!(
            unsupported_family("git show HEAD:file").class,
            FamilyClass::RequiredPassThrough
        );
        assert_eq!(
            unsupported_family("docker ps").class,
            FamilyClass::ReviewCandidate
        );
        assert_eq!(
            unsupported_family("npm install").class,
            FamilyClass::RequiredPassThrough
        );
    }

    #[test]
    fn covers_the_public_command_taxonomy() {
        let cases = [
            ("ls", "filesystem/ls", FamilyClass::ReviewCandidate),
            ("find .", "filesystem/find", FamilyClass::ReviewCandidate),
            ("fd", "filesystem/fd", FamilyClass::ReviewCandidate),
            ("tree", "filesystem/tree", FamilyClass::ReviewCandidate),
            ("du", "filesystem/du", FamilyClass::ReviewCandidate),
            ("df", "filesystem/df", FamilyClass::ReviewCandidate),
            ("ps aux", "system/ps", FamilyClass::ReviewCandidate),
            ("lsof", "system/lsof", FamilyClass::ReviewCandidate),
            ("free", "system/free", FamilyClass::ReviewCandidate),
            ("make check", "build/make", FamilyClass::ReviewCandidate),
            ("ninja target", "build/ninja", FamilyClass::ReviewCandidate),
            (
                "cmake --build .",
                "build/cmake",
                FamilyClass::ReviewCandidate,
            ),
            ("meson compile", "build/meson", FamilyClass::ReviewCandidate),
            (
                "msbuild project",
                "build/msbuild",
                FamilyClass::ReviewCandidate,
            ),
            (
                "bazel test //...",
                "build-driver/test",
                FamilyClass::ReviewCandidate,
            ),
            (
                "buck2 build //...",
                "build-driver/build",
                FamilyClass::ReviewCandidate,
            ),
            (
                "bazel query //...",
                "build-driver/other",
                FamilyClass::Unclear,
            ),
            (
                "docker images",
                "container/inventory",
                FamilyClass::ReviewCandidate,
            ),
            (
                "docker --context prod ps",
                "container/inventory",
                FamilyClass::ReviewCandidate,
            ),
            (
                "docker logs app",
                "container/logs",
                FamilyClass::ReviewCandidate,
            ),
            (
                "docker build .",
                "container/build",
                FamilyClass::ReviewCandidate,
            ),
            (
                "docker inspect app",
                "container/exact-or-structured",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "docker pull image",
                "container/mutation",
                FamilyClass::RequiredPassThrough,
            ),
            ("docker compose ps", "container/other", FamilyClass::Unclear),
            (
                "kubectl events",
                "cluster/events",
                FamilyClass::ReviewCandidate,
            ),
            (
                "kubectl top pods",
                "cluster/inventory",
                FamilyClass::ReviewCandidate,
            ),
            (
                "kubectl logs pod",
                "cluster/logs",
                FamilyClass::ReviewCandidate,
            ),
            (
                "kubectl get pods",
                "cluster/inspection",
                FamilyClass::ReviewCandidate,
            ),
            (
                "kubectl -n system get pods",
                "cluster/inspection",
                FamilyClass::ReviewCandidate,
            ),
            (
                "kubectl exec pod -- sh",
                "cluster/exact-or-streaming",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "kubectl apply -f file",
                "cluster/mutation",
                FamilyClass::RequiredPassThrough,
            ),
            ("kubectl version", "cluster/other", FamilyClass::Unclear),
            ("git tag", "git/other-list", FamilyClass::ReviewCandidate),
            ("git reflog", "git/reflog", FamilyClass::ReviewCandidate),
            (
                "git -C repo show HEAD",
                "git/exact-inspection",
                FamilyClass::RequiredPassThrough,
            ),
            ("git push", "git/mutation", FamilyClass::RequiredPassThrough),
            ("git help", "git/other", FamilyClass::Unclear),
            ("git", "git/no-subcommand", FamilyClass::Unclear),
            (
                "gh api repos/x/y",
                "gh/api",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "gh auth status",
                "gh/control-or-sensitive",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "gh workflow list",
                "gh/other-inspection",
                FamilyClass::ReviewCandidate,
            ),
            (
                "gh --repo owner/project workflow list",
                "gh/other-inspection",
                FamilyClass::ReviewCandidate,
            ),
            ("gh version", "gh/other", FamilyClass::Unclear),
            (
                "npm test",
                "package/npm-test-task",
                FamilyClass::ReviewCandidate,
            ),
            (
                "npm --prefix web test",
                "package/npm-test-task",
                FamilyClass::ReviewCandidate,
            ),
            (
                "pnpm build",
                "package/pnpm-build-task",
                FamilyClass::ReviewCandidate,
            ),
            (
                "yarn lint",
                "package/yarn-lint-task",
                FamilyClass::ReviewCandidate,
            ),
            (
                "bun coverage",
                "package/bun-test-task",
                FamilyClass::ReviewCandidate,
            ),
            (
                "npm install",
                "package/mutation",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "pnpm private-task",
                "package/other-task",
                FamilyClass::Unclear,
            ),
            (
                "cargo metadata",
                "developer/cargo-other",
                FamilyClass::Unclear,
            ),
            ("go env", "developer/go-other", FamilyClass::Unclear),
            (
                "dotnet info",
                "developer/dotnet-other",
                FamilyClass::Unclear,
            ),
            (
                "pytest --help",
                "developer/pytest-other",
                FamilyClass::Unclear,
            ),
            ("ruff version", "developer/ruff-other", FamilyClass::Unclear),
            (
                "mypy --version",
                "developer/mypy-other",
                FamilyClass::Unclear,
            ),
            (
                "pyright --version",
                "developer/pyright-other",
                FamilyClass::Unclear,
            ),
            (
                "curl example.test",
                "network/response",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "wc -l file",
                "inspection/count-or-digest",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "jq . file",
                "structured-or-query-output",
                FamilyClass::RequiredPassThrough,
            ),
            (
                "rm file",
                "mutation/command",
                FamilyClass::RequiredPassThrough,
            ),
            ("python script.py", "script/other", FamilyClass::Unclear),
            (
                "yarp search ref term",
                "yarp/recovery-or-control",
                FamilyClass::RequiredPassThrough,
            ),
            ("unknown-program", "command/other", FamilyClass::Unclear),
        ];
        for (command, id, class) in cases {
            let family = unsupported_family(command);
            assert_eq!(family.id, id, "{command}");
            assert_eq!(family.class, class, "{command}");
        }
    }

    #[test]
    fn profiles_output_without_retaining_content() {
        let mut metrics = FamilyMetrics::default();
        metrics.observe(
            "{\"warning\":\"é\"}\n",
            16,
            Some(false),
            AnalysisOptions::default(),
        );
        assert_eq!(metrics.results, 1);
        assert_eq!(metrics.failure_results, 1);
        assert_eq!(metrics.diagnostic_results, 1);
        assert_eq!(metrics.non_ascii_results, 1);
        assert_eq!(metrics.output_lines, 1);
    }

    #[test]
    fn current_rule_and_guard_labels_are_public_only() {
        let (_, reduced) = yarp_cli::rewrite::select_builtin_command("cargo test").expect("rule");
        let selection = match reduced {
            yarp_cli::rules::Selection::Reduce(selected) => BenchmarkSelection::Reduce {
                label: format!("{}/{}", selected.pack_id, selected.rule.id),
                rule: Box::new((*selected.rule).clone()),
                status_confidence: yarp_cli::rewrite::StatusConfidence::Complete,
                transform_diagnostics: yarp_cli::rewrite::TransformDiagnostics::default(),
            },
            _ => panic!("expected reducer"),
        };
        let family = family_for_unchanged(&selection, "cargo test");
        assert!(family.id.ends_with("rust/cargo-test"));
        let guarded = BenchmarkSelection::Passthrough {
            labels: vec!["guards/example".to_owned()],
        };
        assert_eq!(
            family_for_unchanged(&guarded, "private value").id,
            "guarded/guards/example"
        );
    }

    #[test]
    fn analyzes_a_synthetic_database_without_source_content() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("data/toolcalls.duckdb");
        let mut database = Database::open(&path, "test", "test").expect("database");
        database.begin_source().expect("begin");
        database
            .session(&SessionRecord {
                session_key: "session".to_owned(),
                unix_user: "test".to_owned(),
                agent: "test".to_owned(),
                native_session_id: "native".to_owned(),
                started_at_ms: None,
            })
            .expect("session");
        for (index, (tool, command, output)) in [
            ("read", "private path", "non-shell".to_owned()),
            (
                "exec_command",
                "cargo test",
                "test item ... ok\n".repeat(1_000),
            ),
            ("exec_command", "make custom", "file\n".repeat(1_000)),
            ("exec_command", "cat private", "exact\n".repeat(1_000)),
            ("exec_command", "python private.py", "short\n".to_owned()),
            (
                "exec_command",
                "rg secret . | head -50",
                "src/file.rs:1: secret\n".repeat(1_000),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let call_key = format!("call-{index}");
            database
                .tool_call(&ToolCallRecord {
                    call_key: call_key.clone(),
                    session_key: "session".to_owned(),
                    native_call_id: None,
                    native_worker_id: None,
                    called_at_ms: None,
                    provider: None,
                    model: None,
                    working_directory: None,
                    tool_name: tool.to_owned(),
                    input_format: InputFormat::Text,
                    input_text: command.to_owned(),
                    input_sha256: vec![u8::try_from(index).expect("index"); 32],
                })
                .expect("call");
            database
                .tool_result(&ToolResultRecord {
                    result_key: format!("result-{index}"),
                    call_key,
                    returned_at_ms: None,
                    is_error: Some(false),
                    output_text: Some(output),
                    output_json: None,
                    result_sha256: vec![u8::try_from(index + 1).expect("index"); 32],
                })
                .expect("result");
        }
        database.commit_source().expect("commit");
        database.finish(true).expect("finish");
        drop(database);

        let report = run(&path, AnalysisOptions::default()).expect("analysis");
        assert_eq!(report.totals.evaluated_results, 6);
        assert_eq!(report.totals.shell_results, 5);
        assert_eq!(report.totals.observed_changed_results, 2);
        assert_eq!(report.totals.unchanged_shell_results, 3);
        assert!(report.totals.observed_removed_characters > 0);
        assert!(
            report
                .families
                .iter()
                .any(|family| family.id == "build/make")
        );
        assert!(
            report
                .shell_syntax
                .iter()
                .any(|syntax| syntax.id == "pipeline" && syntax.metrics.results == 1)
        );
        let encoded = serde_json::to_string(&report).expect("serialize");
        assert!(!encoded.contains("private path"));
        assert!(!encoded.contains("private.py"));
        assert!(!encoded.contains("file\\n"));
        assert!(!encoded.contains("secret"));
    }

    #[test]
    fn covers_helpers_and_metric_aggregation() {
        assert_eq!(
            normalized_program(&[
                "A=1".to_owned(),
                "env".to_owned(),
                "--".to_owned(),
                "B=2".to_owned(),
                "/usr/bin/ls".to_owned()
            ]),
            Some(("ls", &[][..]))
        );
        assert_eq!(
            git_subcommand(&["-C".to_owned(), "repo".to_owned(), "status".to_owned()]),
            Some("status")
        );
        assert!(contains_ascii_case_insensitive(
            b"An ErRoR happened",
            b"error"
        ));
        assert!(!contains_ascii_case_insensitive(b"all good", b"error"));
        assert!(has_oversized_line(&vec![b'x'; 1_048_577]));
        assert_eq!(
            public_rule_group("guarded", &["b".to_owned(), "a".to_owned(), "a".to_owned()]),
            "guarded/a+b"
        );

        let mut left = FamilyMetrics {
            results: 1,
            output_characters: 10,
            maximum_result_characters: 10,
            ..FamilyMetrics::default()
        };
        left.add(&FamilyMetrics {
            results: 2,
            output_characters: 20,
            maximum_result_characters: 15,
            eligible_results_at_budget: 1,
            removable_characters_at_budget: 5,
            ..FamilyMetrics::default()
        });
        assert_eq!(left.results, 3);
        assert_eq!(left.output_characters, 30);
        assert_eq!(left.maximum_result_characters, 15);
        assert_eq!(left.removable_characters_at_budget, 5);
    }

    #[test]
    fn scenarios_add_only_selected_class_metrics() {
        let totals = Totals {
            evaluated_output_characters: 2_000,
            shell_output_characters: 1_000,
            observed_changed_results: 2,
            observed_removed_characters: 200,
            ..Totals::default()
        };
        let metrics = FamilyMetrics {
            eligible_results_at_budget: 3,
            removable_characters_at_budget: 300,
            ..FamilyMetrics::default()
        };
        let scenario = scenario(&totals, std::iter::once(&metrics));
        assert_eq!(scenario.total_changed_results, 5);
        assert_eq!(scenario.total_removed_characters, 500);
        assert!((scenario.removed_percent_of_shell_output - 50.0).abs() < f64::EPSILON);
        assert!((scenario.removed_percent_of_evaluated_output - 25.0).abs() < f64::EPSILON);
    }
}
