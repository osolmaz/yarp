use std::collections::{BTreeMap, BTreeSet};

use super::model::{
    Action, CommandMatcher, LinePattern, OutputPolicy, PackManifest, Reducer, Rule,
    SOURCE_SCHEMA_VERSION,
};

pub const MAX_RULES: usize = 10_000;
pub const MAX_SOURCE_FILE_BYTES: usize = 64 * 1024;
pub const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_COMPILED_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_STREAM_MEMORY_BYTES: usize = 4 * 1024 * 1024;

/// Validate source manifest fields and explicit rule paths.
///
/// # Errors
///
/// Returns an error when a version, ID, path, count, or uniqueness constraint fails.
pub fn validate_manifest(manifest: &PackManifest) -> Result<(), String> {
    if manifest.schema_version != SOURCE_SCHEMA_VERSION {
        return Err(format!(
            "schema_version must be {SOURCE_SCHEMA_VERSION}, got {}",
            manifest.schema_version
        ));
    }
    validate_pack_id(&manifest.id)?;
    if manifest.rules.is_empty() || manifest.rules.len() > MAX_RULES {
        return Err(format!(
            "rules must contain between 1 and {MAX_RULES} paths"
        ));
    }
    let mut paths = BTreeSet::new();
    for path in &manifest.rules {
        validate_rule_path(path)?;
        if !paths.insert(path) {
            return Err(format!("duplicate rule path: {path}"));
        }
    }
    Ok(())
}

/// Validate all rules and reject duplicate IDs and known reduction overlaps.
///
/// # Errors
///
/// Returns an error when a rule is invalid or conflicts with another rule.
pub fn validate_rules(rules: &[Rule]) -> Result<(), String> {
    if rules.is_empty() || rules.len() > MAX_RULES {
        return Err(format!("pack must contain between 1 and {MAX_RULES} rules"));
    }
    let mut ids = BTreeSet::new();
    let mut exact_matchers = BTreeMap::<String, &str>::new();
    for rule in rules {
        validate_rule(rule)?;
        if !ids.insert(rule.id.as_str()) {
            return Err(format!("duplicate rule id: {}", rule.id));
        }
        let key = matcher_key(&rule.matcher);
        if let Some(existing) = exact_matchers.insert(key, &rule.id) {
            return Err(format!(
                "rules {existing} and {} have the same command matcher",
                rule.id
            ));
        }
    }
    reject_known_reduction_overlaps(rules)
}

/// Validate one declarative command rule.
///
/// # Errors
///
/// Returns an error when matching, action, reducer, pattern, or budget constraints fail.
pub fn validate_rule(rule: &Rule) -> Result<(), String> {
    validate_rule_id(&rule.id)?;
    validate_matcher(&rule.matcher)?;
    match rule.action {
        Action::Passthrough => {
            if rule.transform.is_some()
                || rule.reducer.is_some()
                || rule.success.is_some()
                || rule.failure.is_some()
            {
                return Err(format!(
                    "passthrough rule {} must not define transform, reducer, or output policies",
                    rule.id
                ));
            }
        }
        Action::Transform => {
            if rule.transform.is_none() {
                return Err(format!("transform rule {} is missing transform", rule.id));
            }
            if rule.reducer.is_some() || rule.success.is_some() || rule.failure.is_some() {
                return Err(format!(
                    "transform rule {} must not define reducer or output policies",
                    rule.id
                ));
            }
        }
        Action::Reduce => {
            if rule.transform.is_some() {
                return Err(format!("reduce rule {} must not define transform", rule.id));
            }
            let reducer = rule
                .reducer
                .as_ref()
                .ok_or_else(|| format!("reduce rule {} is missing reducer", rule.id))?;
            let success = rule
                .success
                .as_ref()
                .ok_or_else(|| format!("reduce rule {} is missing success policy", rule.id))?;
            let failure = rule
                .failure
                .as_ref()
                .ok_or_else(|| format!("reduce rule {} is missing failure policy", rule.id))?;
            validate_reducer(reducer)?;
            validate_policy("success", success)?;
            validate_policy("failure", failure)?;
            let memory_bound = stream_memory_bound_parts(reducer, success, failure)?;
            if memory_bound > MAX_STREAM_MEMORY_BYTES {
                return Err(format!(
                    "reduce rule {} requires {memory_bound} bytes per stream, above the {MAX_STREAM_MEMORY_BYTES}-byte limit",
                    rule.id
                ));
            }
        }
    }
    Ok(())
}

fn validate_pack_id(id: &str) -> Result<(), String> {
    validate_identifier(id, 128, false, "pack id")
}

fn validate_rule_id(id: &str) -> Result<(), String> {
    validate_identifier(id, 128, true, "rule id")
}

fn validate_identifier(id: &str, max: usize, slash: bool, label: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > max || !id.is_ascii() {
        return Err(format!("{label} must contain 1 through {max} ASCII bytes"));
    }
    let valid = |byte: u8| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'_' | b'-')
            || (slash && byte == b'/')
    };
    if !id.bytes().all(valid) || !id.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(format!("invalid {label}: {id}"));
    }
    if !id.as_bytes()[id.len() - 1].is_ascii_alphanumeric() {
        return Err(format!("invalid {label}: {id}"));
    }
    if id
        .as_bytes()
        .windows(2)
        .any(|pair| is_separator(pair[0], slash) && is_separator(pair[1], slash))
    {
        return Err(format!("{label} has consecutive separators: {id}"));
    }
    Ok(())
}

const fn is_separator(byte: u8, slash: bool) -> bool {
    matches!(byte, b'.' | b'_' | b'-') || (slash && byte == b'/')
}

// Source rule paths require the exact lowercase .json suffix.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "source rule paths require the exact lowercase .json suffix"
)]
fn validate_rule_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || !path.ends_with(".json")
    {
        return Err(format!("invalid rule path: {path}"));
    }
    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(format!("invalid rule path: {path}"));
        }
    }
    Ok(())
}

fn validate_matcher(matcher: &CommandMatcher) -> Result<(), String> {
    if matcher.program.is_empty() || matcher.program.len() > 32 {
        return Err("program must contain between 1 and 32 names".to_owned());
    }
    let mut programs = BTreeSet::new();
    for program in &matcher.program {
        if program.is_empty()
            || program.len() > 128
            || !program.is_ascii()
            || program.bytes().any(|byte| {
                byte.is_ascii_whitespace()
                    || matches!(
                        byte,
                        b'/' | b'\\'
                            | 0
                            | b'|'
                            | b'&'
                            | b';'
                            | b'<'
                            | b'>'
                            | b'('
                            | b')'
                            | b'`'
                            | b'$'
                            | b'#'
                    )
            })
        {
            return Err(format!("invalid program name: {program}"));
        }
        if !programs.insert(program) {
            return Err(format!("duplicate program name: {program}"));
        }
    }
    validate_tokens("argv_prefix", &matcher.argv_prefix)?;
    validate_tokens("argv_contains_all", &matcher.argv_contains_all)
}

fn validate_tokens(label: &str, tokens: &[String]) -> Result<(), String> {
    if tokens.len() > 64 {
        return Err(format!("{label} must not contain more than 64 tokens"));
    }
    let mut unique = BTreeSet::new();
    for token in tokens {
        if token.chars().count() > 1_024 || token.contains('\0') {
            return Err(format!("invalid {label} token"));
        }
        if !unique.insert(token) {
            return Err(format!("duplicate {label} token: {token}"));
        }
    }
    Ok(())
}

fn validate_reducer(reducer: &Reducer) -> Result<(), String> {
    if let Reducer::LineFilter {
        strip_ansi,
        drop,
        keep,
    } = reducer
    {
        if drop.len().saturating_add(keep.len()) > 256 {
            return Err("line_filter must not contain more than 256 patterns".to_owned());
        }
        if !strip_ansi && drop.is_empty() && keep.is_empty() {
            return Err("line_filter must strip ANSI or define a line pattern".to_owned());
        }
        let mut patterns = BTreeSet::new();
        for pattern in drop.iter().chain(keep) {
            validate_pattern(pattern)?;
            let key = serde_jcs::to_string(pattern)
                .map_err(|error| format!("could not canonicalize line pattern: {error}"))?;
            if !patterns.insert(key) {
                return Err("duplicate line pattern".to_owned());
            }
        }
    }
    Ok(())
}

fn validate_pattern(pattern: &LinePattern) -> Result<(), String> {
    let bytes = pattern.value.as_bytes();
    if bytes.is_empty() || bytes.len() > 4_096 || bytes.contains(&0) {
        return Err("line pattern value must contain 1 through 4096 non-NUL bytes".to_owned());
    }
    Ok(())
}

/// Calculate the conservative retained-memory bound for one rule stream.
///
/// # Errors
///
/// Returns an error when the rule is incomplete or the calculation overflows `usize`.
pub fn stream_memory_bound(rule: &Rule) -> Result<usize, String> {
    let reducer = rule
        .reducer
        .as_ref()
        .ok_or_else(|| "reduction rule is missing a reducer".to_owned())?;
    let success = rule
        .success
        .as_ref()
        .ok_or_else(|| "reduction rule is missing a success policy".to_owned())?;
    let failure = rule
        .failure
        .as_ref()
        .ok_or_else(|| "reduction rule is missing a failure policy".to_owned())?;
    stream_memory_bound_parts(reducer, success, failure)
}

fn stream_memory_bound_parts(
    reducer: &Reducer,
    success: &OutputPolicy,
    failure: &OutputPolicy,
) -> Result<usize, String> {
    let raw = success.max_output_bytes.max(failure.max_output_bytes);
    let summaries = success
        .max_output_bytes
        .checked_add(failure.max_output_bytes)
        .and_then(|value| value.checked_mul(5))
        .ok_or_else(|| "summary memory bound overflowed".to_owned())?;
    let max_line = success.max_line_bytes.max(failure.max_line_bytes);
    let line_state = max_line
        .checked_mul(2)
        .ok_or_else(|| "line memory bound overflowed".to_owned())?;
    let pattern_state = match reducer {
        Reducer::LineFilter { drop, keep, .. } => {
            drop.iter()
                .chain(keep)
                .try_fold(0_usize, |total, pattern| {
                    total
                        .checked_add(pattern.value.len())
                        .ok_or_else(|| "line pattern memory bound overflowed".to_owned())
                })?
        }
        _ => 0,
    };
    // Two collectors retain at most 128 records in each of five evidence classes, plus one
    // source-line representative for each registered diagnostic category.
    let record_overhead = 2_usize
        .checked_mul(5)
        .and_then(|value| value.checked_mul(128))
        .and_then(|value| value.checked_mul(64))
        .ok_or_else(|| "record memory bound overflowed".to_owned())?;
    let diagnostic_representatives = 2_usize
        .checked_mul(5)
        .and_then(|value| value.checked_mul(max_line.saturating_add(64)))
        .ok_or_else(|| "diagnostic representative memory bound overflowed".to_owned())?;
    let fingerprint_overhead = 2_usize
        .checked_mul(5)
        .and_then(|value| value.checked_mul(128))
        .and_then(|value| value.checked_mul(96))
        .ok_or_else(|| "fingerprint memory bound overflowed".to_owned())?;
    let diversity_key_overhead = 2_usize
        .checked_mul(128)
        .and_then(|value| value.checked_mul(224))
        .ok_or_else(|| "diversity key memory bound overflowed".to_owned())?;
    [
        raw,
        summaries,
        line_state,
        pattern_state,
        record_overhead,
        diagnostic_representatives,
        fingerprint_overhead,
        diversity_key_overhead,
        8 * 1024,
        64 * 1024,
    ]
    .into_iter()
    .try_fold(0_usize, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| "stream memory bound overflowed".to_owned())
    })
}

fn validate_policy(label: &str, policy: &OutputPolicy) -> Result<(), String> {
    if !(1..=1_048_576).contains(&policy.max_line_bytes) {
        return Err(format!(
            "{label}.max_line_bytes must be between 1 and 1048576"
        ));
    }
    if !(704..=4_194_304).contains(&policy.max_output_bytes) {
        return Err(format!(
            "{label}.max_output_bytes must be between 704 and 4194304"
        ));
    }
    if policy.min_savings_bytes > 1_048_576 {
        return Err(format!("{label}.min_savings_bytes must not exceed 1048576"));
    }
    if policy.min_savings_basis_points > 10_000 {
        return Err(format!(
            "{label}.min_savings_basis_points must not exceed 10000"
        ));
    }
    Ok(())
}

fn matcher_key(matcher: &CommandMatcher) -> String {
    serde_jcs::to_string(matcher).unwrap_or_default()
}

fn reject_known_reduction_overlaps(rules: &[Rule]) -> Result<(), String> {
    for (index, left) in rules.iter().enumerate() {
        if left.action == Action::Passthrough {
            continue;
        }
        for right in &rules[index + 1..] {
            if right.action == Action::Passthrough {
                continue;
            }
            if matchers_may_overlap(&left.matcher, &right.matcher) {
                return Err(format!(
                    "reduction rules {} and {} can match the same command",
                    left.id, right.id
                ));
            }
        }
    }
    Ok(())
}

fn matchers_may_overlap(left: &CommandMatcher, right: &CommandMatcher) -> bool {
    let program_overlap = left
        .program
        .iter()
        .any(|program| right.program.contains(program));
    program_overlap
        && (left.argv_prefix.starts_with(&right.argv_prefix)
            || right.argv_prefix.starts_with(&left.argv_prefix))
}

#[cfg(test)]
mod tests {
    use super::super::model::{PatternCase, PatternKind, PatternTrim, Transform};
    use super::*;

    fn policy() -> OutputPolicy {
        OutputPolicy {
            max_line_bytes: 16_384,
            max_output_bytes: 32_768,
            min_savings_bytes: 120,
            min_savings_basis_points: 1_000,
        }
    }

    fn rule(id: &str, prefix: &[&str]) -> Rule {
        Rule {
            id: id.to_owned(),
            matcher: CommandMatcher {
                program: vec!["tool".to_owned()],
                argv_prefix: prefix.iter().map(ToString::to_string).collect(),
                argv_contains_all: Vec::new(),
            },
            action: Action::Reduce,
            transform: None,
            reducer: Some(Reducer::ListSummary),
            success: Some(policy()),
            failure: Some(policy()),
        }
    }

    #[test]
    fn rejects_overlapping_reduction_rules() {
        let error = validate_rules(&[rule("one", &["test"]), rule("two", &["test", "unit"])])
            .expect_err("overlap");
        assert!(error.contains("same command"));
    }

    #[test]
    fn accepts_disjoint_prefixes_and_passthrough_guards() {
        let mut guard = rule("guard", &["test"]);
        guard.action = Action::Passthrough;
        guard.matcher.argv_contains_all = vec!["--json".to_owned()];
        guard.reducer = None;
        guard.transform = None;
        guard.success = None;
        guard.failure = None;
        validate_rules(&[guard, rule("build", &["build"]), rule("test", &["test"])])
            .expect("valid rules");
    }

    #[test]
    fn validates_transform_rule_contracts() {
        let mut transform = rule("transform", &[]);
        transform.action = Action::Transform;
        transform.transform = Some(Transform::LinePreserving);
        transform.reducer = None;
        transform.success = None;
        transform.failure = None;
        validate_rule(&transform).expect("valid transform");

        transform.reducer = Some(Reducer::ListSummary);
        let error = validate_rule(&transform).expect_err("transform reducer conflict");
        assert!(error.contains("must not define reducer"));
    }

    #[test]
    fn rejects_policies_that_cannot_fit_mandatory_evidence() {
        let mut undersized = rule("undersized", &["run"]);
        undersized
            .success
            .as_mut()
            .expect("success")
            .max_output_bytes = 703;
        let error = validate_rule(&undersized).expect_err("mandatory evidence budget");
        assert!(error.contains("between 704"));
    }

    #[test]
    fn rejects_rules_above_the_aggregate_memory_limit() {
        let mut oversized = rule("oversized", &["run"]);
        let policy = oversized.success.as_mut().expect("success policy");
        policy.max_output_bytes = 4_194_304;
        policy.max_line_bytes = 1_048_576;
        oversized.failure = oversized.success;
        let error = validate_rule(&oversized).expect_err("memory limit");
        assert!(error.contains("per stream"));
    }

    #[test]
    fn validates_line_patterns() {
        let pattern = LinePattern {
            kind: PatternKind::Prefix,
            value: "Compiling ".to_owned(),
            case: PatternCase::Sensitive,
            trim: PatternTrim::Start,
        };
        validate_pattern(&pattern).expect("valid pattern");
    }
}
