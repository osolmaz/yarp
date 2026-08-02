use serde::{Deserialize, Serialize};

pub const SOURCE_SCHEMA_VERSION: u32 = 1;
pub const ENGINE_ABI_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackManifest {
    pub schema_version: u32,
    pub id: String,
    pub rules: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(rename = "match")]
    pub matcher: CommandMatcher,
    pub action: Action,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reducer: Option<Reducer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<OutputPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<OutputPolicy>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandMatcher {
    pub program: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv_prefix: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv_contains_all: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Reduce,
    Transform,
    Passthrough,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transform {
    LinePreserving,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reducer {
    SearchSummary,
    DiffSummary,
    TestSummary,
    BuildSummary,
    LogSummary,
    StatusSummary,
    ListSummary,
    LineFilter {
        #[serde(default, skip_serializing_if = "is_false")]
        strip_ansi: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        drop: Vec<LinePattern>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keep: Vec<LinePattern>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinePattern {
    pub kind: PatternKind,
    pub value: String,
    #[serde(default, skip_serializing_if = "is_sensitive")]
    pub case: PatternCase,
    #[serde(default, skip_serializing_if = "is_no_trim")]
    pub trim: PatternTrim,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    Exact,
    Prefix,
    Suffix,
    Contains,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternCase {
    #[default]
    Sensitive,
    AsciiInsensitive,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternTrim {
    #[default]
    None,
    Start,
    Both,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPolicy {
    pub max_line_bytes: usize,
    pub max_output_bytes: usize,
    pub min_savings_bytes: usize,
    pub min_savings_basis_points: u16,
}

impl CommandMatcher {
    #[must_use]
    pub fn matches(&self, arguments: &[String]) -> bool {
        let Some(program) = arguments.first() else {
            return false;
        };
        if !self.program.iter().any(|candidate| candidate == program) {
            return false;
        }
        let tail = normalized_arguments(program, &arguments[1..]);
        if !tail.starts_with(&self.argv_prefix) {
            return false;
        }
        self.argv_contains_all.iter().all(|required| {
            tail.iter()
                .any(|argument| contains_normalized_argument(argument, required))
        })
    }
}

fn contains_normalized_argument(argument: &str, required: &str) -> bool {
    argument == required
        || (required.starts_with("--")
            && argument
                .strip_prefix(required)
                .is_some_and(|suffix| suffix.starts_with('=')))
        || contains_short_option(argument, required)
}

fn contains_short_option(argument: &str, required: &str) -> bool {
    let required = required.as_bytes();
    let argument = argument.as_bytes();
    required.len() == 2
        && required[0] == b'-'
        && required[1] != b'-'
        && argument.len() > 2
        && argument[0] == b'-'
        && argument[1] != b'-'
        && argument[1..].contains(&required[1])
}

fn normalized_arguments<'a>(program: &str, arguments: &'a [String]) -> &'a [String] {
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        let takes_value = match program {
            "git" => matches!(argument, "-C" | "-c" | "--git-dir" | "--work-tree"),
            "npm" => matches!(argument, "--prefix" | "-w" | "--workspace"),
            "pnpm" | "yarn" | "bun" => {
                matches!(argument, "-C" | "--dir" | "--filter")
            }
            "uv" => matches!(argument, "--project"),
            "gh" => matches!(argument, "-R" | "--repo"),
            _ => false,
        };
        if takes_value {
            if index + 1 >= arguments.len() {
                return arguments;
            }
            index += 2;
            continue;
        }
        let standalone = match program {
            "git" => {
                argument.starts_with("--git-dir=")
                    || argument.starts_with("--work-tree=")
                    || matches!(
                        argument,
                        "--no-pager"
                            | "--paginate"
                            | "--literal-pathspecs"
                            | "--glob-pathspecs"
                            | "--noglob-pathspecs"
                    )
            }
            "npm" => {
                argument.starts_with("--prefix=")
                    || argument.starts_with("--workspace=")
                    || matches!(argument, "--workspaces" | "-ws" | "-s" | "--silent")
            }
            "pnpm" | "yarn" | "bun" => {
                argument.starts_with("--dir=")
                    || argument.starts_with("--filter=")
                    || matches!(
                        argument,
                        "-r" | "--recursive"
                            | "-w"
                            | "--workspace-root"
                            | "--parallel"
                            | "--stream"
                            | "--aggregate-output"
                            | "-s"
                            | "--silent"
                    )
            }
            "uv" => argument.starts_with("--project="),
            "gh" => argument.starts_with("--repo="),
            "cargo" => argument.starts_with('+') && argument.len() > 1,
            _ => false,
        };
        if standalone {
            index += 1;
            continue;
        }
        break;
    }
    &arguments[index..]
}

// Serde skip predicates receive references.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "Serde skip predicates receive references"
)]
const fn is_false(value: &bool) -> bool {
    !*value
}

// Serde skip predicates receive references.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "Serde skip predicates receive references"
)]
const fn is_sensitive(value: &PatternCase) -> bool {
    matches!(value, PatternCase::Sensitive)
}

// Serde skip predicates receive references.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "Serde skip predicates receive references"
)]
const fn is_no_trim(value: &PatternTrim) -> bool {
    matches!(value, PatternTrim::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(program: &str, prefix: &[&str]) -> CommandMatcher {
        CommandMatcher {
            program: vec![program.to_owned()],
            argv_prefix: prefix.iter().map(ToString::to_string).collect(),
            argv_contains_all: Vec::new(),
        }
    }

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn skips_reviewed_program_level_options() {
        assert!(matcher("git", &["diff"]).matches(&arguments(&[
            "git",
            "-C",
            "/repo",
            "--no-pager",
            "diff",
            "--check"
        ])));
        assert!(
            matcher("pnpm", &["test"]).matches(&arguments(&["pnpm", "-C", "frontend", "test"]))
        );
        assert!(matcher("pnpm", &["lint"]).matches(&arguments(&[
            "pnpm", "--filter", "web", "-r", "--silent", "lint"
        ])));
        assert!(matcher("npm", &["run", "test"]).matches(&arguments(&[
            "npm",
            "--workspaces",
            "run",
            "test"
        ])));
        assert!(matcher("cargo", &["test"]).matches(&arguments(&["cargo", "+nightly", "test"])));
    }

    #[test]
    fn recognizes_reviewed_option_assignment_forms() {
        let long = CommandMatcher {
            program: vec!["gh".to_owned()],
            argv_prefix: vec!["pr".to_owned(), "view".to_owned()],
            argv_contains_all: vec!["--json".to_owned()],
        };
        assert!(long.matches(&arguments(&["gh", "pr", "view", "--json=number,title"])));

        let short = CommandMatcher {
            program: vec!["kubectl".to_owned()],
            argv_prefix: Vec::new(),
            argv_contains_all: vec!["-o".to_owned()],
        };
        assert!(short.matches(&arguments(&["kubectl", "get", "pods", "-ojson"])));

        let bundled = CommandMatcher {
            program: vec!["git".to_owned()],
            argv_prefix: vec!["status".to_owned()],
            argv_contains_all: vec!["-z".to_owned()],
        };
        assert!(bundled.matches(&arguments(&["git", "status", "-sbz"])));
    }

    #[test]
    fn does_not_skip_unknown_options_or_missing_values() {
        assert!(!matcher("git", &["diff"]).matches(&arguments(&["git", "--unknown", "diff"])));
        assert!(!matcher("git", &["diff"]).matches(&arguments(&["git", "-C"])));
    }
}
