#[cfg(test)]
use crate::rule_pack as yarp_rule_pack;
use crate::rule_pack::{OutputPolicy, Rule, Transform};
use crate::rules::{PackRequest, Registry, Selection, digest_hex};
use crate::shell::{self, Connector, ShellItem, SimpleCommand};
use serde::Serialize;

/// Metadata passed to an archived shell wrapper.
#[derive(Clone, Copy, Debug)]
pub struct ArchiveCommandRef<'a> {
    pub agent: &'a str,
    pub account: &'a str,
    pub session_id: &'a str,
    pub call_id: &'a str,
}

/// Return a wrapper command only for a simple shell command selected by a built-in rule.
#[must_use]
pub fn rewrite(command: &str) -> Option<String> {
    rewrite_with_archive(command, None)
}

/// Return an archived wrapper command for a command selected by a built-in rule.
#[must_use]
pub fn rewrite_with_archive(
    command: &str,
    archive: Option<ArchiveCommandRef<'_>>,
) -> Option<String> {
    rewrite_with_options(command, archive, &[]).ok().flatten()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShellPlan {
    pub version: u32,
    pub execution: ExecutionPlan,
    pub result: ResultPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionPlan {
    Original,
    Rewrite { command: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResultPolicy {
    Ordinary,
    Recovery,
}

/// Plan shell execution and result handling using one conservative parse.
///
/// # Errors
///
/// Returns an error when configured rule packs cannot be loaded or selected safely.
pub fn plan_with_options(
    command: &str,
    archive: Option<ArchiveCommandRef<'_>>,
    packs: &[PackRequest],
) -> Result<ShellPlan, String> {
    let command = command.trim();
    let Some(words) = shell::parse_simple_words(command).ok() else {
        return Ok(original_plan(ResultPolicy::Ordinary));
    };
    if is_recovery_command(&words) {
        return Ok(original_plan(ResultPolicy::Recovery));
    }
    let mut registry = Registry::load(packs)?;
    let Selection::Reduce(selected) = registry.select(&words)? else {
        return Ok(original_plan(ResultPolicy::Ordinary));
    };
    let wrapper = selected_wrapper(command, archive, &registry, &selected)?;
    Ok(ShellPlan {
        version: 1,
        execution: ExecutionPlan::Rewrite { command: wrapper },
        result: ResultPolicy::Ordinary,
    })
}

/// Return a wrapper command using built-in and explicitly supplied compiled rule packs.
///
/// # Errors
///
/// Returns an error when a configured pack cannot be loaded or a selected pack path cannot be
/// represented in a shell wrapper.
pub fn rewrite_with_options(
    command: &str,
    archive: Option<ArchiveCommandRef<'_>>,
    packs: &[PackRequest],
) -> Result<Option<String>, String> {
    plan_with_options(command, archive, packs).map(|plan| match plan.execution {
        ExecutionPlan::Original => None,
        ExecutionPlan::Rewrite { command } => Some(command),
    })
}

const fn original_plan(result: ResultPolicy) -> ShellPlan {
    ShellPlan {
        version: 1,
        execution: ExecutionPlan::Original,
        result,
    }
}

fn is_recovery_command(words: &[String]) -> bool {
    matches!(
        words,
        [program, subcommand, arguments @ ..]
            if program == "yarp"
                && matches!(subcommand.as_str(), "search" | "read")
                && !arguments.iter().any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    )
}

fn selected_wrapper(
    command: &str,
    archive: Option<ArchiveCommandRef<'_>>,
    registry: &Registry,
    selected: &crate::rules::SelectedRule,
) -> Result<String, String> {
    let mut wrapper = format!(
        "yarp run --selected-pack {} --selected-rule {} --selected-digest {}",
        shell_quote(&selected.pack_id),
        shell_quote(&selected.rule.id),
        shell_quote(&digest_hex(&selected.source_digest)),
    );
    for reference in registry.references() {
        let path = reference
            .path
            .to_str()
            .ok_or_else(|| "rule pack path is not valid UTF-8".to_owned())?;
        wrapper.push_str(" --rule-pack ");
        wrapper.push_str(&shell_quote(path));
        wrapper.push_str(" --rule-pack-digest ");
        wrapper.push_str(&shell_quote(&digest_hex(&reference.source_digest)));
        wrapper.push_str(" --rule-pack-compiled-digest ");
        wrapper.push_str(&shell_quote(&digest_hex(&reference.compiled_digest)));
    }
    if let Some(reference) = archive {
        wrapper.push_str(" --archive-agent ");
        wrapper.push_str(&shell_quote(reference.agent));
        wrapper.push_str(" --archive-account ");
        wrapper.push_str(&shell_quote(reference.account));
        wrapper.push_str(" --archive-session ");
        wrapper.push_str(&shell_quote(reference.session_id));
        wrapper.push_str(" --archive-call ");
        wrapper.push_str(&shell_quote(reference.call_id));
    }
    wrapper.push_str(" -- ");
    wrapper.push_str(command);
    Ok(wrapper)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Parse one conservative shell command and select it using built-in rules.
///
/// # Errors
///
/// Returns an error when the shell source is unsupported or rule selection fails.
pub fn select_builtin_command(command: &str) -> Result<(Vec<String>, Selection), String> {
    let words = shell::parse_simple_words(command)?;
    let selection = Registry::builtins_only().select(&words)?;
    Ok((words, selection))
}

/// Select a command from an already parsed argument list using built-in rules.
///
/// # Errors
///
/// Returns an error only when the embedded registry cannot read a selected record.
pub fn select_builtin_argv(arguments: &[String]) -> Result<Selection, String> {
    Registry::builtins_only().select(arguments)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusConfidence {
    Complete,
    FinalStageOnly,
    Conditional,
}

const TRANSFORM_DIAGNOSTIC_PREFIXES: [(&str, &[u8]); 6] = [
    ("cat", b"cat:"),
    ("tee", b"tee:"),
    ("head", b"head:"),
    ("tail", b"tail:"),
    ("sort", b"sort:"),
    ("uniq", b"uniq:"),
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransformDiagnostics(u8);

impl TransformDiagnostics {
    fn insert(&mut self, program: &str) {
        if let Some(index) = TRANSFORM_DIAGNOSTIC_PREFIXES
            .iter()
            .position(|(candidate, _)| *candidate == program)
        {
            self.0 |= 1_u8 << index;
        }
    }

    fn merge(&mut self, other: Self) {
        self.0 |= other.0;
    }

    #[must_use]
    pub fn matches_line(self, line: &[u8]) -> bool {
        TRANSFORM_DIAGNOSTIC_PREFIXES
            .iter()
            .enumerate()
            .any(|(index, (_, prefix))| self.0 & (1_u8 << index) != 0 && line.starts_with(prefix))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultPlan {
    pub rule: Rule,
    pub status_confidence: StatusConfidence,
    pub transform_diagnostics: TransformDiagnostics,
    pub fail_open_setup_diagnostics: bool,
}

/// Select one compatible typed plan for conservative post-result reduction.
///
/// # Errors
///
/// Returns an error when shell syntax is ambiguous, guarded, unsupported, or mixes reducer
/// families. Callers fail open on every error.
pub fn select_result_plan(command: &str) -> Result<ResultPlan, String> {
    let program = shell::parse(command)?;
    let mut selected: Option<Rule> = None;
    let mut confidence = StatusConfidence::Complete;
    let mut transform_diagnostics = TransformDiagnostics::default();
    let mut fail_open_setup_diagnostics = false;
    let mut pipefail = Some(false);
    let mut previous_had_output = false;

    for (index, item) in program.items.iter().enumerate() {
        let connector = index
            .checked_sub(1)
            .and_then(|connector| program.connectors.get(connector))
            .copied();
        if let ShellItem::Simple(command) = item
            && let Some(value) = pipefail_setting(&command.words)
        {
            pipefail = if matches!(connector, None | Some(Connector::Sequence)) {
                Some(value)
            } else {
                None
            };
        }
        let (candidate, setup) = match item {
            ShellItem::Simple(command) => select_simple(command)?,
            ShellItem::Pipeline(stages) => {
                if pipefail != Some(true) {
                    confidence = merge_confidence(confidence, StatusConfidence::FinalStageOnly);
                }
                let (rule, diagnostics) = select_pipeline(stages)?;
                transform_diagnostics.merge(diagnostics);
                (Some(rule), false)
            }
        };
        let has_output = candidate.is_some();
        let may_have_setup_diagnostics =
            setup && matches!(item, ShellItem::Simple(command) if setup_may_emit(&command.words));
        fail_open_setup_diagnostics |= may_have_setup_diagnostics;
        let may_have_visible_output = has_output || may_have_setup_diagnostics;
        match connector {
            Some(Connector::Sequence) if previous_had_output => {
                confidence = merge_confidence(confidence, StatusConfidence::FinalStageOnly);
            }
            Some(Connector::Or) if previous_had_output || may_have_visible_output => {
                confidence = merge_confidence(confidence, StatusConfidence::Conditional);
            }
            None if index > 0 => {
                return Err("compound command is missing a connector".to_owned());
            }
            Some(Connector::And | Connector::Sequence | Connector::Or) | None => {}
        }
        if let Some(candidate) = candidate {
            merge_rule(&mut selected, candidate)?;
        }
        previous_had_output = previous_had_output || may_have_visible_output;
    }

    let rule =
        selected.ok_or_else(|| "compound command has no supported output command".to_owned())?;
    Ok(ResultPlan {
        rule,
        status_confidence: confidence,
        transform_diagnostics,
        fail_open_setup_diagnostics,
    })
}

/// Select one compatible typed rule for callers that do not need status confidence.
///
/// # Errors
///
/// Returns the same errors as [`select_result_plan`].
pub fn select_result_rule(command: &str) -> Result<Rule, String> {
    select_result_plan(command).map(|plan| plan.rule)
}

fn select_simple(command: &SimpleCommand) -> Result<(Option<Rule>, bool), String> {
    if is_setup_command(&command.words) || command.words.iter().all(|word| is_assignment(word)) {
        return Ok((None, true));
    }
    let Some(words) = normalize_result_words(command.words.clone())? else {
        return Ok((None, true));
    };
    let mut registry = Registry::builtins_only();
    match registry
        .select(&words)
        .map_err(|error| format!("could not classify compound command: {error}"))?
    {
        Selection::Reduce(candidate) => Ok((Some((*candidate.rule).clone()), false)),
        Selection::Transform(_) => Err("line transform has no pipeline input".to_owned()),
        Selection::Passthrough(_) | Selection::Ambiguous(_) | Selection::Unsupported => {
            Err("compound command contains an unsupported or guarded command".to_owned())
        }
    }
}

fn select_pipeline(stages: &[SimpleCommand]) -> Result<(Rule, TransformDiagnostics), String> {
    let mut selected = None;
    let mut diagnostics = TransformDiagnostics::default();
    for stage in stages {
        if is_setup_command(&stage.words) || stage.words.iter().all(|word| is_assignment(word)) {
            return Err("pipeline contains a setup command".to_owned());
        }
        let words = normalize_result_words(stage.words.clone())?
            .ok_or_else(|| "pipeline stage has no output command".to_owned())?;
        let mut registry = Registry::builtins_only();
        match registry
            .select(&words)
            .map_err(|error| format!("could not classify pipeline stage: {error}"))?
        {
            Selection::Reduce(candidate) => merge_rule(&mut selected, (*candidate.rule).clone())?,
            Selection::Transform(candidate) => {
                if candidate.rule.transform != Some(Transform::LinePreserving) {
                    return Err("pipeline transform is unsupported".to_owned());
                }
                validate_line_preserving(&words)?;
                if selected.is_none() {
                    return Err("pipeline transform has no typed input".to_owned());
                }
                diagnostics.insert(&words[0]);
            }
            Selection::Passthrough(_) | Selection::Ambiguous(_) | Selection::Unsupported => {
                return Err("pipeline contains an unsupported or guarded command".to_owned());
            }
        }
    }
    selected
        .map(|rule| (rule, diagnostics))
        .ok_or_else(|| "pipeline has no supported output command".to_owned())
}

fn validate_line_preserving(words: &[String]) -> Result<(), String> {
    let Some((program, arguments)) = words.split_first() else {
        return Err("pipeline transform has no program".to_owned());
    };
    match program.as_str() {
        "cat" => validate_stdin_only(arguments),
        "tee" => validate_tee(arguments),
        "head" | "tail" => validate_line_selector(program, arguments),
        "sort" => validate_sort(arguments),
        "uniq" => validate_uniq(arguments),
        _ => Err("pipeline transform program is unsupported".to_owned()),
    }
}

fn validate_stdin_only(arguments: &[String]) -> Result<(), String> {
    let mut operands = false;
    for argument in arguments {
        if argument == "--" && !operands {
            operands = true;
        } else if argument != "-" {
            return Err("pipeline transform must read only standard input".to_owned());
        }
    }
    Ok(())
}

fn validate_tee(arguments: &[String]) -> Result<(), String> {
    let mut operands = false;
    for argument in arguments {
        if operands {
            continue;
        }
        match argument.as_str() {
            "--" => operands = true,
            "-a" | "-i" | "-p" | "--append" | "--ignore-interrupts" | "--output-error" => {}
            value if value.starts_with("--output-error=") => {}
            value if !value.starts_with('-') || value == "-" => operands = true,
            _ => return Err("tee transform uses an unsupported option".to_owned()),
        }
    }
    Ok(())
}

fn validate_line_selector(program: &str, arguments: &[String]) -> Result<(), String> {
    let mut index = 0;
    let mut operands = false;
    while index < arguments.len() {
        let argument = &arguments[index];
        if operands {
            if argument != "-" {
                return Err(format!("{program} transform must read only standard input"));
            }
            index += 1;
            continue;
        }
        if argument == "--" {
            operands = true;
            index += 1;
            continue;
        }
        if argument == "-" {
            operands = true;
            index += 1;
            continue;
        }
        if matches!(argument.as_str(), "-q" | "--quiet" | "--silent") {
            index += 1;
            continue;
        }
        if argument == "-n" || argument == "--lines" {
            let count = arguments
                .get(index + 1)
                .ok_or_else(|| format!("{program} line option is missing its value"))?;
            validate_line_count(count, program)?;
            index += 2;
            continue;
        }
        if let Some(count) = argument.strip_prefix("--lines=") {
            validate_line_count(count, program)?;
            index += 1;
            continue;
        }
        if let Some(count) = argument.strip_prefix("-n")
            && !count.is_empty()
        {
            validate_line_count(count, program)?;
            index += 1;
            continue;
        }
        if argument.strip_prefix('-').is_some_and(|count| {
            !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            index += 1;
            continue;
        }
        return Err(format!(
            "{program} transform uses an unsupported option or operand"
        ));
    }
    Ok(())
}

fn validate_line_count(value: &str, program: &str) -> Result<(), String> {
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value).as_bytes();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(format!("{program} line count is not a literal integer"));
    }
    Ok(())
}

fn validate_sort(arguments: &[String]) -> Result<(), String> {
    let mut index = 0;
    let mut operands = false;
    while index < arguments.len() {
        let argument = &arguments[index];
        if operands || argument == "-" || !argument.starts_with('-') {
            if argument != "-" {
                return Err("sort transform must read only standard input".to_owned());
            }
            operands = true;
            index += 1;
            continue;
        }
        if argument == "--" {
            operands = true;
            index += 1;
            continue;
        }
        if sort_option_takes_value(argument) {
            if sort_option_has_attached_value(argument) {
                index += 1;
            } else {
                index = index
                    .checked_add(2)
                    .filter(|next| *next <= arguments.len())
                    .ok_or_else(|| "sort option is missing its value".to_owned())?;
            }
            continue;
        }
        if is_safe_sort_flag(argument) {
            index += 1;
            continue;
        }
        return Err("sort transform uses an unsupported option".to_owned());
    }
    Ok(())
}

fn sort_option_takes_value(value: &str) -> bool {
    [
        "-k",
        "-t",
        "-S",
        "-T",
        "--key",
        "--field-separator",
        "--buffer-size",
        "--temporary-directory",
        "--batch-size",
        "--random-source",
    ]
    .iter()
    .any(|option| value == *option || value.starts_with(&format!("{option}=")))
        || ["-k", "-t", "-S", "-T"]
            .iter()
            .any(|option| value.starts_with(option) && value.len() > option.len())
}

fn sort_option_has_attached_value(value: &str) -> bool {
    value.contains('=')
        || ["-k", "-t", "-S", "-T"]
            .iter()
            .any(|option| value.starts_with(option) && value.len() > option.len())
}

fn is_safe_sort_flag(value: &str) -> bool {
    if value.starts_with("--") {
        return matches!(
            value,
            "--stable"
                | "--unique"
                | "--reverse"
                | "--numeric-sort"
                | "--general-numeric-sort"
                | "--human-numeric-sort"
                | "--version-sort"
                | "--month-sort"
                | "--random-sort"
                | "--ignore-case"
                | "--ignore-leading-blanks"
                | "--dictionary-order"
                | "--ignore-nonprinting"
        );
    }
    value.len() > 1
        && value[1..].bytes().all(|byte| {
            matches!(
                byte,
                b'b' | b'd'
                    | b'f'
                    | b'g'
                    | b'h'
                    | b'i'
                    | b'M'
                    | b'n'
                    | b'R'
                    | b'r'
                    | b's'
                    | b'u'
                    | b'V'
            )
        })
}

fn validate_uniq(arguments: &[String]) -> Result<(), String> {
    let mut index = 0;
    let mut operands = false;
    let mut operand_count = 0_usize;
    while index < arguments.len() {
        let argument = &arguments[index];
        if operands || argument == "-" || !argument.starts_with('-') {
            if argument != "-" || operand_count > 0 {
                return Err("uniq transform must read only standard input".to_owned());
            }
            operands = true;
            operand_count += 1;
            index += 1;
            continue;
        }
        if argument == "--" {
            operands = true;
            index += 1;
            continue;
        }
        if uniq_option_takes_value(argument) {
            if uniq_option_has_attached_value(argument) {
                index += 1;
            } else {
                index = index
                    .checked_add(2)
                    .filter(|next| *next <= arguments.len())
                    .ok_or_else(|| "uniq option is missing its value".to_owned())?;
            }
            continue;
        }
        if is_safe_uniq_flag(argument) {
            index += 1;
            continue;
        }
        return Err("uniq transform uses an unsupported option".to_owned());
    }
    Ok(())
}

fn uniq_option_takes_value(value: &str) -> bool {
    [
        "-f",
        "-s",
        "-w",
        "--skip-fields",
        "--skip-chars",
        "--check-chars",
    ]
    .iter()
    .any(|option| value == *option || value.starts_with(&format!("{option}=")))
        || ["-f", "-s", "-w"]
            .iter()
            .any(|option| value.starts_with(option) && value.len() > option.len())
}

fn uniq_option_has_attached_value(value: &str) -> bool {
    value.contains('=')
        || ["-f", "-s", "-w"]
            .iter()
            .any(|option| value.starts_with(option) && value.len() > option.len())
}

fn is_safe_uniq_flag(value: &str) -> bool {
    if value.starts_with("--") {
        return matches!(
            value,
            "--all-repeated" | "--repeated" | "--unique" | "--ignore-case"
        );
    }
    value.len() > 1
        && value[1..]
            .bytes()
            .all(|byte| matches!(byte, b'D' | b'd' | b'u' | b'i'))
}

fn merge_rule(selected: &mut Option<Rule>, candidate: Rule) -> Result<(), String> {
    let Some(existing) = selected else {
        *selected = Some(candidate);
        return Ok(());
    };
    if existing.reducer != candidate.reducer {
        return Err("compound command mixes reducer families".to_owned());
    }
    let existing_success = existing
        .success
        .ok_or_else(|| "selected rule has no success policy".to_owned())?;
    let candidate_success = candidate
        .success
        .ok_or_else(|| "candidate rule has no success policy".to_owned())?;
    let existing_failure = existing
        .failure
        .ok_or_else(|| "selected rule has no failure policy".to_owned())?;
    let candidate_failure = candidate
        .failure
        .ok_or_else(|| "candidate rule has no failure policy".to_owned())?;
    existing.success = Some(merge_policy(existing_success, candidate_success));
    existing.failure = Some(merge_policy(existing_failure, candidate_failure));
    Ok(())
}

const fn merge_confidence(left: StatusConfidence, right: StatusConfidence) -> StatusConfidence {
    match (left, right) {
        (StatusConfidence::Conditional, _) | (_, StatusConfidence::Conditional) => {
            StatusConfidence::Conditional
        }
        (StatusConfidence::FinalStageOnly, _) | (_, StatusConfidence::FinalStageOnly) => {
            StatusConfidence::FinalStageOnly
        }
        (StatusConfidence::Complete, StatusConfidence::Complete) => StatusConfidence::Complete,
    }
}

fn pipefail_setting(words: &[String]) -> Option<bool> {
    match words {
        [program, option, value] if program == "set" && value == "pipefail" && option == "-o" => {
            Some(true)
        }
        [program, option, value] if program == "set" && value == "pipefail" && option == "+o" => {
            Some(false)
        }
        _ => None,
    }
}

fn merge_policy(left: OutputPolicy, right: OutputPolicy) -> OutputPolicy {
    OutputPolicy {
        max_line_bytes: left.max_line_bytes.max(right.max_line_bytes),
        max_output_bytes: left.max_output_bytes.max(right.max_output_bytes),
        min_savings_bytes: left.min_savings_bytes.max(right.min_savings_bytes),
        min_savings_basis_points: left
            .min_savings_basis_points
            .max(right.min_savings_basis_points),
    }
}

fn normalize_result_words(mut words: Vec<String>) -> Result<Option<Vec<String>>, String> {
    if is_setup_command(&words) {
        return Ok(None);
    }
    while words.first().is_some_and(|word| is_assignment(word)) {
        words.remove(0);
    }
    let Some(program) = words.first().map(String::as_str) else {
        return Ok(None);
    };
    match program {
        "env" => {
            words.remove(0);
            if words.first().is_some_and(|word| word == "--") {
                words.remove(0);
            }
            while words.first().is_some_and(|word| is_assignment(word)) {
                words.remove(0);
            }
        }
        "command" | "exec" => {
            words.remove(0);
            if words.first().is_some_and(|word| word == "--") {
                words.remove(0);
            }
        }
        "time" if words.get(1).is_some_and(|word| !word.starts_with('-')) => {
            words.remove(0);
        }
        "timeout"
            if words.len() >= 3 && words.get(1).is_some_and(|word| !word.starts_with('-')) =>
        {
            words.drain(0..2);
        }
        _ => {}
    }
    if words.is_empty() {
        return Ok(None);
    }
    if words.first().is_some_and(|word| {
        matches!(
            word.as_str(),
            "env" | "command" | "exec" | "time" | "timeout"
        )
    }) {
        return Err("compound command uses an unsupported command wrapper".to_owned());
    }
    Ok(Some(words))
}

fn is_setup_command(words: &[String]) -> bool {
    match words {
        [program] if matches!(program.as_str(), ":" | "true" | "false") => true,
        [program, path] if program == "cd" && !path.starts_with('-') => true,
        [program, flag] if program == "set" && is_quiet_set_flag(flag) => true,
        [program, flag, value]
            if program == "set" && matches!(flag.as_str(), "-o" | "+o") && value == "pipefail" =>
        {
            true
        }
        [program, values @ ..] if program == "export" && !values.is_empty() => values
            .iter()
            .all(|value| is_assignment(value) || is_identifier(value)),
        [program, value] if program == "umask" && !value.starts_with('-') => true,
        _ => false,
    }
}

/// Return whether output contains a diagnostic from an accepted fallible setup command.
#[must_use]
pub fn contains_setup_diagnostic(input: &[u8]) -> bool {
    [
        b"cd:".as_slice(),
        b"umask:".as_slice(),
        b"export:".as_slice(),
        b"readonly variable".as_slice(),
        b"not a valid identifier".as_slice(),
    ]
    .iter()
    .any(|pattern| {
        input.windows(pattern.len()).any(|window| {
            window
                .iter()
                .zip(*pattern)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
    })
}

fn setup_may_emit(words: &[String]) -> bool {
    words.iter().all(|word| is_assignment(word))
        || words
            .first()
            .is_some_and(|program| matches!(program.as_str(), "cd" | "export" | "umask"))
}

fn is_quiet_set_flag(value: &str) -> bool {
    matches!(
        value,
        "-e" | "+e"
            | "-u"
            | "+u"
            | "-f"
            | "+f"
            | "-C"
            | "+C"
            | "-m"
            | "+m"
            | "-b"
            | "+b"
            | "-n"
            | "+n"
            | "-E"
            | "+E"
            | "-T"
            | "+T"
            | "-P"
            | "+P"
            | "-h"
            | "+h"
            | "-k"
            | "+k"
            | "-p"
            | "+p"
    )
}

fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(name, _)| is_identifier(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_selected_commands_without_changing_the_original_text() {
        let expected = format!(
            "yarp run --selected-pack 'yarp-builtins' --selected-rule 'git/diff' --selected-digest '{}' -- git diff -- 'file name'",
            digest_hex(&crate::rules::BUILTIN_SOURCE_DIGEST)
        );
        assert_eq!(
            rewrite("  git diff -- 'file name'  ").as_deref(),
            Some(expected.as_str())
        );
        assert!(
            rewrite("npm run lint -- --fix")
                .is_some_and(|value| value.ends_with("-- npm run lint -- --fix"))
        );
        assert!(rewrite("pytest -q").is_some());
        assert!(rewrite("rg -n needle .").is_some());
        assert!(rewrite("egrep -n needle file").is_some());
        assert!(rewrite("fgrep needle file").is_some());
        assert!(rewrite("podman ps").is_some());
        assert!(rewrite("git reflog --date=iso").is_some());
        assert!(rewrite("git remote show origin").is_some());
        assert!(rewrite("gh repo view osolmaz/yarp").is_some());
        assert!(rewrite("pnpm -s build").is_some());
        assert!(rewrite("git -C /repo diff --check").is_some());
        assert!(rewrite("pnpm -C frontend test").is_some());
        assert!(rewrite("pnpm --filter web -r lint").is_some());
        assert!(rewrite("ls -la").is_some());
        assert!(rewrite("find src -name '*.rs'").is_some());
        assert!(rewrite("ps aux").is_some());
    }

    #[test]
    fn adds_archive_identifiers_with_shell_quoting() {
        let rewritten = rewrite_with_archive(
            "git status --short",
            Some(ArchiveCommandRef {
                agent: "pi",
                account: "o'nur",
                session_id: "session-1",
                call_id: "call-1",
            }),
        )
        .expect("rewrite");
        assert_eq!(
            rewritten,
            format!(
                "yarp run --selected-pack 'yarp-builtins' --selected-rule 'git/status' --selected-digest '{}' --archive-agent 'pi' --archive-account 'o'\\''nur' --archive-session 'session-1' --archive-call 'call-1' -- git status --short",
                digest_hex(&crate::rules::BUILTIN_SOURCE_DIGEST)
            )
        );
    }

    #[test]
    fn classifies_only_direct_archive_queries_as_recovery() {
        for command in [
            "yarp search yr_0123456789abcdef0123456789abcdef error",
            "yarp read yr_0123456789abcdef0123456789abcdef stdout 1:20",
            "yarp search yr_0123456789abcdef0123456789abcdef 'error|warning' -C 2",
        ] {
            let plan = plan_with_options(command, None, &[]).expect("recovery plan");
            assert_eq!(plan.execution, ExecutionPlan::Original);
            assert_eq!(plan.result, ResultPolicy::Recovery);
        }
        for command in [
            "command yarp search ref error",
            "env DEBUG=1 yarp search ref error",
            "/usr/bin/yarp read ref 1:20",
            "yarp search ref error | head",
            "yarp read ref 1:20 > output",
            "yarp search ref error; printf extra",
            "yarp search $(printf ref) error",
            "yarp search ref error &",
            "yarp search --help",
        ] {
            let plan = plan_with_options(command, None, &[]).expect("ordinary plan");
            assert_eq!(
                plan.result,
                ResultPolicy::Ordinary,
                "classified {command:?}"
            );
        }
    }

    #[test]
    fn leaves_unknown_and_guarded_commands_unchanged() {
        for command in [
            "",
            "cat .env",
            "curl https://example.com",
            "git push",
            "npm install",
            "git show HEAD:file",
            "rg --json needle .",
            "rg --files",
            "rg -l needle .",
            "rg -0l needle .",
            "grep -l needle file",
            "grep -lZ needle file",
            "egrep -l needle file",
            "fgrep --count needle file",
            "git status -sbz",
            "git grep -l needle",
            "git grep -lz needle",
            "git log --oneline -z",
            "ls --zero",
            "find . -print0",
            "find . -exec echo {} +",
            "fd -0 needle .",
            "fd -x echo {}",
            "tree -J .",
            "du --null .",
            "df --output=source,target",
            "lsof -F pcfn",
            "free --seconds 1",
            "ps aux -o pid=",
            "cmake -S . -B build --trace",
            "git worktree list --porcelain",
            "git tag --list -z",
            "git reflog delete main@{0}",
            "git reflog --format=%H",
            "gh workflow view ci.yml --yaml",
            "docker ps --format json",
            "docker images --quiet",
            "podman ps --format json",
            "podman logs --follow container",
            "journalctl --output=json",
            "systemctl status -o json sshd",
            "ninja -t compdb",
            "tsc --showConfig",
            "eslint --format json .",
            "npx eslint --format=json .",
            "pyright --outputjson",
            "ruff check --output-format json .",
            "uv run ruff --show-settings",
            "mypy --output=json .",
            "pytest --json-report",
            "python3 -m pytest --collect-only",
            "go vet -json ./...",
            "go test -json ./...",
            "cargo test --message-format=json",
            "cargo audit --json",
            "pnpm exec oxlint --format json",
            "gh pr view 1 --json=number,title",
            "git diff --stat",
            "kubectl get pods -ojson",
            "npm test -- --reporter=json",
            "codex review --json --base main",
            "yarp run -- git status",
        ] {
            assert_eq!(rewrite(command), None, "accepted {command:?}");
        }
    }

    #[test]
    fn rejects_shell_control_syntax_and_expansion() {
        for command in [
            "git status | cat",
            "git status > result",
            "git status && echo done",
            "git status; echo done",
            "git status $(touch bad)",
            "git status `touch bad`",
            "git status # comment",
            "git status\nwhoami",
            "git status $HOME",
        ] {
            assert_eq!(rewrite(command), None, "accepted {command:?}");
        }
    }

    #[test]
    fn classifies_only_compatible_compound_results() {
        let rule = select_result_rule("cd repo && cargo check; cargo build")
            .expect("compatible build result");
        assert_eq!(rule.action, yarp_rule_pack::Action::Reduce);
        assert!(matches!(
            rule.reducer,
            Some(yarp_rule_pack::Reducer::BuildSummary)
        ));
        assert!(select_result_rule("cargo test && cargo build").is_err());
        assert!(matches!(
            select_result_rule("GOWORK=off go test ./... 2>&1 || true")
                .expect("assignment and stream merge"),
            Rule {
                reducer: Some(yarp_rule_pack::Reducer::TestSummary),
                ..
            }
        ));
        assert!(matches!(
            select_result_rule("env CI=1 pnpm build && :").expect("environment wrapper"),
            Rule {
                reducer: Some(yarp_rule_pack::Reducer::BuildSummary),
                ..
            }
        ));
        assert!(matches!(
            select_result_rule("cargo test | cat").expect("line-preserving pipeline"),
            Rule {
                reducer: Some(yarp_rule_pack::Reducer::TestSummary),
                ..
            }
        ));
        assert!(select_result_rule("cargo test > result.log").is_err());
        assert!(select_result_rule("cargo test && echo done").is_err());
        assert!(select_result_rule("set && cargo test").is_err());
        assert!(select_result_rule("yarp search ref error").is_err());
    }

    #[test]
    fn plans_safe_composite_results_and_rejects_uncertain_stages() {
        let search = select_result_plan("rg TODO . | sort | head -50").expect("search pipeline");
        assert!(matches!(
            search.rule.reducer,
            Some(yarp_rule_pack::Reducer::SearchSummary)
        ));
        assert_eq!(search.status_confidence, StatusConfidence::FinalStageOnly);

        let list = select_result_plan("find . -type f | sort | uniq").expect("list pipeline");
        assert!(matches!(
            list.rule.reducer,
            Some(yarp_rule_pack::Reducer::ListSummary)
        ));
        assert_eq!(list.status_confidence, StatusConfidence::FinalStageOnly);

        let test = select_result_plan("cargo test | tee test.log").expect("test pipeline");
        assert!(matches!(
            test.rule.reducer,
            Some(yarp_rule_pack::Reducer::TestSummary)
        ));
        assert_eq!(test.status_confidence, StatusConfidence::FinalStageOnly);

        let pipefail = select_result_plan("set -o pipefail && cargo test | head -100")
            .expect("pipefail pipeline");
        assert_eq!(pipefail.status_confidence, StatusConfidence::Complete);
        let skipped_pipefail = select_result_plan("false && set -o pipefail; cargo test | head -1")
            .expect("conditionally skipped pipefail");
        assert_eq!(
            skipped_pipefail.status_confidence,
            StatusConfidence::FinalStageOnly
        );
        let conditional_disable =
            select_result_plan("set -o pipefail; false && set +o pipefail; cargo test | head -1")
                .expect("conditional pipefail disable");
        assert_eq!(
            conditional_disable.status_confidence,
            StatusConfidence::FinalStageOnly
        );

        let sequence = select_result_plan("cargo test; cargo test").expect("test sequence");
        assert_eq!(sequence.status_confidence, StatusConfidence::FinalStageOnly);
        for command in [
            "cd missing; cargo test",
            "export CI=1; cargo test",
            "VALUE=1; cargo test",
            "umask invalid; cargo test",
        ] {
            let plan = select_result_plan(command).expect("fallible setup sequence");
            assert_eq!(
                plan.status_confidence,
                StatusConfidence::FinalStageOnly,
                "{command:?}"
            );
        }
        let conjunction = select_result_plan("cargo test && cargo test").expect("test conjunction");
        assert_eq!(conjunction.status_confidence, StatusConfidence::Complete);
        let multiline = select_result_plan("cargo test\ncargo test").expect("multiline tests");
        assert_eq!(
            multiline.status_confidence,
            StatusConfidence::FinalStageOnly
        );
        let masked = select_result_plan("cargo test; true").expect("masked test status");
        assert_eq!(masked.status_confidence, StatusConfidence::FinalStageOnly);
        assert!(select_result_plan("export CI=1; umask 077; cargo test").is_ok());

        for command in [
            "cargo test | head source.txt",
            "find . -type f | sort existing.txt",
            "find . -type f | sort --compress-program=cat",
            "find . -type f | sort --compress-program cat",
            "ls -D",
            "git tag --list --format=%(refname)",
            "git stash list --pretty=format:%H",
            "rg --json TODO . | jq .",
            "find . -print0 | xargs -0 echo",
            "cat source.rs | sed 's/x/y/'",
            "set -o; cargo test",
            "set -x; cargo test",
            "set -v; cargo test",
            "export -p; cargo test",
            "umask -S; cargo test",
            "echo \"$VALUE\" | rg x",
            "cargo test | unknown-filter",
            "cargo test |& head",
            "cat source.rs | head",
        ] {
            assert!(select_result_plan(command).is_err(), "planned {command:?}");
        }
    }

    #[test]
    fn handles_quotes_and_escapes_conservatively() {
        assert!(rewrite("git diff file\\ name").is_some());
        assert!(rewrite("'git' \"status\"").is_some());
        assert!(rewrite("git diff '$HOME'").is_some());
        assert_eq!(rewrite("git diff \"$HOME\""), None);
        assert_eq!(rewrite("git diff 'unterminated"), None);
        assert_eq!(rewrite("git diff trailing\\"), None);
    }

    #[test]
    fn validates_parsed_child_arguments() {
        assert!(matches!(
            select_builtin_argv(&strings(&["cargo", "test", "--workspace"])),
            Ok(Selection::Reduce(_))
        ));
        assert!(matches!(
            select_builtin_argv(&strings(&["go", "test", "./..."])),
            Ok(Selection::Reduce(_))
        ));
        assert!(matches!(
            select_builtin_argv(&strings(&["git", "show", "HEAD:file"])),
            Ok(Selection::Passthrough(_))
        ));
        assert_eq!(
            select_builtin_argv(&strings(&["git", "push"])),
            Ok(Selection::Unsupported)
        );
    }

    #[test]
    fn classifies_reviewed_commands_and_guards_streaming_or_structured_output() {
        for arguments in [
            &["hf", "jobs", "logs", "job-id"][..],
            &["hf", "spaces", "logs", "owner/space", "--build"][..],
        ] {
            let Ok(Selection::Reduce(selected)) = select_builtin_argv(&strings(arguments)) else {
                panic!("did not reduce {arguments:?}");
            };
            assert!(matches!(
                selected.rule.reducer,
                Some(yarp_rule_pack::Reducer::LogSummary)
            ));
        }
        for arguments in [
            &["herdr", "pane", "list"][..],
            &["herdr", "tab", "list", "--workspace", "w1"][..],
            &["herdr", "workspace", "list"][..],
        ] {
            let Ok(Selection::Reduce(selected)) = select_builtin_argv(&strings(arguments)) else {
                panic!("did not reduce {arguments:?}");
            };
            assert!(matches!(
                selected.rule.reducer,
                Some(yarp_rule_pack::Reducer::ListSummary)
            ));
        }
        let arguments = ["pnpm", "vitest", "run", "src/example.test.ts"];
        let Ok(Selection::Reduce(selected)) = select_builtin_argv(&strings(&arguments)) else {
            panic!("did not reduce {arguments:?}");
        };
        assert!(matches!(
            selected.rule.reducer,
            Some(yarp_rule_pack::Reducer::TestSummary)
        ));
        for arguments in [
            &["hf", "jobs", "logs", "job-id", "--follow"][..],
            &["hf", "jobs", "logs", "job-id", "-f"][..],
            &["hf", "jobs", "logs", "job-id", "--json"][..],
            &["hf", "jobs", "logs", "job-id", "--format=json"][..],
            &["hf", "jobs", "logs", "job-id", "--format", "quiet"][..],
            &["pnpm", "vitest", "run", "--watch"][..],
        ] {
            assert!(matches!(
                select_builtin_argv(&strings(arguments)),
                Ok(Selection::Passthrough(_))
            ));
        }
        for arguments in [&["pnpm", "vitest"][..], &["pnpm", "vitest", "watch"][..]] {
            assert!(!matches!(
                select_builtin_argv(&strings(arguments)),
                Ok(Selection::Reduce(_))
            ));
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }
}
