use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use yarp_rule_pack::{
    Action, CommandMatcher, LinePattern, OutputPolicy, PatternCase, PatternKind, PatternTrim,
    Reducer, Rule, SourcePack, decode_json,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureFile {
    schema_version: u32,
    cases: Vec<MatchFixture>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchFixture {
    id: String,
    rule_id: String,
    argv: Vec<String>,
    expected_action: String,
}

fn main() {
    if let Err(error) = generate() {
        panic!("could not compile built-in rules: {error}");
    }
}

fn generate() -> Result<(), String> {
    let root = Path::new("rules");
    let source = SourcePack::load(root)?;
    let fixture_path = root.join("fixtures/builtin.json");
    validate_fixtures(&fixture_path, &source.rules)?;

    println!(
        "cargo:rerun-if-changed={}",
        root.join("pack.json").display()
    );
    println!("cargo:rerun-if-changed={}", fixture_path.display());
    println!("cargo:rerun-if-changed={}", root.join("schema").display());
    for path in &source.manifest.rules {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }

    let generated = render_registry(&source)?;
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    fs::write(out.join("builtin_rules.rs"), generated)
        .map_err(|error| format!("could not write generated registry: {error}"))
}

fn validate_fixtures(path: &Path, rules: &[Rule]) -> Result<(), String> {
    let body = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let fixtures: FixtureFile =
        decode_json(&body).map_err(|error| format!("{}: {error}", path.display()))?;
    if fixtures.schema_version != 1 {
        return Err(format!("{}: schema_version must be 1", path.display()));
    }
    let rules_by_id: BTreeMap<&str, &Rule> =
        rules.iter().map(|rule| (rule.id.as_str(), rule)).collect();
    let mut fixture_ids = BTreeSet::new();
    let mut covered = BTreeSet::new();
    for fixture in fixtures.cases {
        if fixture.id.is_empty() || !fixture_ids.insert(fixture.id.clone()) {
            return Err(format!("{}: duplicate or empty fixture id", path.display()));
        }
        let rule = rules_by_id.get(fixture.rule_id.as_str()).ok_or_else(|| {
            format!(
                "{}: fixture {} names an unknown rule",
                path.display(),
                fixture.id
            )
        })?;
        if !rule.matcher.matches(&fixture.argv) {
            return Err(format!(
                "{}: fixture {} does not match rule {}",
                path.display(),
                fixture.id,
                fixture.rule_id
            ));
        }
        let expected = match rule.action {
            Action::Reduce => "reduce",
            Action::Passthrough => "passthrough",
        };
        if fixture.expected_action != expected {
            return Err(format!(
                "{}: fixture {} expects the wrong action",
                path.display(),
                fixture.id
            ));
        }
        covered.insert(fixture.rule_id);
    }
    for rule in rules {
        if !covered.contains(rule.id.as_str()) {
            return Err(format!("built-in rule {} has no fixture", rule.id));
        }
    }
    Ok(())
}

fn render_registry(source: &SourcePack) -> Result<String, String> {
    let mut rules: Vec<&Rule> = source.rules.iter().collect();
    rules.sort_by(|left, right| left.id.cmp(&right.id));
    let mut output = String::new();
    writeln!(
        output,
        "pub const BUILTIN_PACK_ID: &str = {:?};",
        source.manifest.id
    )
    .map_err(format_error)?;
    writeln!(
        output,
        "pub const BUILTIN_SOURCE_DIGEST: [u8; 32] = {:?};",
        source.source_digest
    )
    .map_err(format_error)?;
    writeln!(
        output,
        "pub static BUILTIN_RULES: std::sync::LazyLock<Vec<yarp_rule_pack::Rule>> = std::sync::LazyLock::new(|| vec!["
    )
    .map_err(format_error)?;
    for rule in &rules {
        writeln!(output, "{},", rule_expression(rule)).map_err(format_error)?;
    }
    writeln!(output, "]);\n").map_err(format_error)?;

    let indices: BTreeMap<&str, usize> = rules
        .iter()
        .enumerate()
        .map(|(index, rule)| (rule.id.as_str(), index))
        .collect();
    let mut programs = BTreeMap::<&str, Vec<usize>>::new();
    for rule in rules {
        for program in &rule.matcher.program {
            programs
                .entry(program)
                .or_default()
                .push(indices[rule.id.as_str()]);
        }
    }
    writeln!(
        output,
        "pub static BUILTIN_PROGRAM_INDEX: &[(&str, &[usize])] = &["
    )
    .map_err(format_error)?;
    for (program, candidates) in programs {
        writeln!(output, "({program:?}, &{candidates:?}),").map_err(format_error)?;
    }
    writeln!(output, "];\n").map_err(format_error)?;
    Ok(output)
}

fn rule_expression(rule: &Rule) -> String {
    format!(
        "yarp_rule_pack::Rule {{ id: {:?}.to_owned(), matcher: {}, action: {}, reducer: {}, success: {}, failure: {} }}",
        rule.id,
        matcher_expression(&rule.matcher),
        action_expression(rule.action),
        rule.reducer
            .as_ref()
            .map(reducer_expression)
            .map_or_else(|| "None".to_owned(), |value| format!("Some({value})")),
        option_policy(rule.success.as_ref()),
        option_policy(rule.failure.as_ref()),
    )
}

fn matcher_expression(matcher: &CommandMatcher) -> String {
    format!(
        "yarp_rule_pack::CommandMatcher {{ program: {}, argv_prefix: {}, argv_contains_all: {} }}",
        strings_expression(&matcher.program),
        strings_expression(&matcher.argv_prefix),
        strings_expression(&matcher.argv_contains_all),
    )
}

const fn action_expression(action: Action) -> &'static str {
    match action {
        Action::Reduce => "yarp_rule_pack::Action::Reduce",
        Action::Passthrough => "yarp_rule_pack::Action::Passthrough",
    }
}

fn reducer_expression(reducer: &Reducer) -> String {
    match reducer {
        Reducer::SearchSummary => "yarp_rule_pack::Reducer::SearchSummary".to_owned(),
        Reducer::DiffSummary => "yarp_rule_pack::Reducer::DiffSummary".to_owned(),
        Reducer::TestSummary => "yarp_rule_pack::Reducer::TestSummary".to_owned(),
        Reducer::BuildSummary => "yarp_rule_pack::Reducer::BuildSummary".to_owned(),
        Reducer::LogSummary => "yarp_rule_pack::Reducer::LogSummary".to_owned(),
        Reducer::StatusSummary => "yarp_rule_pack::Reducer::StatusSummary".to_owned(),
        Reducer::ListSummary => "yarp_rule_pack::Reducer::ListSummary".to_owned(),
        Reducer::LineFilter {
            strip_ansi,
            drop,
            keep,
        } => format!(
            "yarp_rule_pack::Reducer::LineFilter {{ strip_ansi: {strip_ansi}, drop: {}, keep: {} }}",
            patterns_expression(drop),
            patterns_expression(keep),
        ),
    }
}

fn patterns_expression(patterns: &[LinePattern]) -> String {
    let values = patterns
        .iter()
        .map(|pattern| {
            format!(
                "yarp_rule_pack::LinePattern {{ kind: {}, value: {:?}.to_owned(), case: {}, trim: {} }}",
                pattern_kind_expression(pattern.kind),
                pattern.value,
                pattern_case_expression(pattern.case),
                pattern_trim_expression(pattern.trim),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("vec![{values}]")
}

const fn pattern_kind_expression(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::Exact => "yarp_rule_pack::PatternKind::Exact",
        PatternKind::Prefix => "yarp_rule_pack::PatternKind::Prefix",
        PatternKind::Suffix => "yarp_rule_pack::PatternKind::Suffix",
        PatternKind::Contains => "yarp_rule_pack::PatternKind::Contains",
    }
}

const fn pattern_case_expression(case: PatternCase) -> &'static str {
    match case {
        PatternCase::Sensitive => "yarp_rule_pack::PatternCase::Sensitive",
        PatternCase::AsciiInsensitive => "yarp_rule_pack::PatternCase::AsciiInsensitive",
    }
}

const fn pattern_trim_expression(trim: PatternTrim) -> &'static str {
    match trim {
        PatternTrim::None => "yarp_rule_pack::PatternTrim::None",
        PatternTrim::Start => "yarp_rule_pack::PatternTrim::Start",
        PatternTrim::Both => "yarp_rule_pack::PatternTrim::Both",
    }
}

fn option_policy(policy: Option<&OutputPolicy>) -> String {
    policy.map_or_else(
        || "None".to_owned(),
        |policy| {
            format!(
                "Some(yarp_rule_pack::OutputPolicy {{ max_line_bytes: {}, max_output_bytes: {}, min_savings_bytes: {}, min_savings_basis_points: {} }})",
                rust_number(policy.max_line_bytes),
                rust_number(policy.max_output_bytes),
                rust_number(policy.min_savings_bytes),
                rust_number(usize::from(policy.min_savings_basis_points)),
            )
        },
    )
}

fn rust_number(value: usize) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push('_');
        }
        output.push(character);
    }
    output
}

fn strings_expression(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| format!("{value:?}.to_owned()"))
        .collect::<Vec<_>>()
        .join(",");
    format!("vec![{values}]")
}

fn format_error(error: std::fmt::Error) -> String {
    format!("could not render generated registry: {error}")
}
