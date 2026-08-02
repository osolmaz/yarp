use std::collections::BTreeSet;

use tree_sitter::{Node, Parser};

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_AST_NODES: usize = 16_384;
const MAX_NESTING_DEPTH: usize = 64;
const MAX_STAGES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Connector {
    Sequence,
    And,
    Or,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShellProgram {
    pub(crate) items: Vec<ShellItem>,
    pub(crate) connectors: Vec<Connector>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShellItem {
    Simple(SimpleCommand),
    Pipeline(Vec<SimpleCommand>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SimpleCommand {
    pub(crate) words: Vec<String>,
    pub(crate) stream_merges: Vec<StreamMerge>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StreamMerge {
    StderrToStdout,
    StdoutToStderr,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SyntaxFeature {
    Pipeline,
    AndChain,
    OtherRedirection,
    StreamMerge,
    Semicolon,
    Multiline,
    ParameterExpansion,
    OrChain,
    CommandSubstitution,
    PathInvocation,
    ControlStructure,
    UnsupportedWrapper,
    Comment,
    Background,
    NestedShell,
    SubshellGroup,
    ShellFunction,
    ParserFailure,
}

impl SyntaxFeature {
    const fn label(self) -> &'static str {
        match self {
            Self::Pipeline => "pipeline",
            Self::AndChain => "and_chain",
            Self::OtherRedirection => "other_redirection",
            Self::StreamMerge => "stream_merge",
            Self::Semicolon => "semicolon",
            Self::Multiline => "multiline",
            Self::ParameterExpansion => "parameter_expansion",
            Self::OrChain => "or_chain",
            Self::CommandSubstitution => "command_substitution",
            Self::PathInvocation => "path_invocation",
            Self::ControlStructure => "control_structure",
            Self::UnsupportedWrapper => "unsupported_wrapper",
            Self::Comment => "comment",
            Self::Background => "background",
            Self::NestedShell => "nested_shell",
            Self::SubshellGroup => "subshell_group",
            Self::ShellFunction => "shell_function",
            Self::ParserFailure => "parser_failure",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyntaxFeatures {
    values: BTreeSet<SyntaxFeature>,
}

impl SyntaxFeatures {
    #[must_use]
    pub fn contains(&self, feature: SyntaxFeature) -> bool {
        self.values.contains(&feature)
    }

    pub fn labels(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.values.iter().map(|feature| feature.label())
    }

    fn insert(&mut self, feature: SyntaxFeature) {
        self.values.insert(feature);
    }
}

/// Inspect fixed public shell syntax features without retaining source values.
#[must_use]
pub fn inspect_syntax(source: &str) -> SyntaxFeatures {
    let mut features = inspect_lexical_syntax(source);
    if source.len() > MAX_SOURCE_BYTES {
        features.insert(SyntaxFeature::ParserFailure);
        return features;
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        features.insert(SyntaxFeature::ParserFailure);
        return features;
    }
    let Some(tree) = parser.parse(source, None) else {
        features.insert(SyntaxFeature::ParserFailure);
        return features;
    };
    let root = tree.root_node();
    if root.has_error() {
        features.insert(SyntaxFeature::ParserFailure);
    }
    let mut node_count = 0_usize;
    inspect_syntax_tree(root, source, &mut features, 0, &mut node_count);
    features
}

/// Parse one conservatively supported Bash command without executing or expanding it.
///
/// # Errors
///
/// Returns an error for unsupported syntax, dynamic words, unsafe redirects, or resource limits.
pub(crate) fn parse(source: &str) -> Result<ShellProgram, String> {
    if source.trim().is_empty() {
        return Err("empty shell source".to_owned());
    }
    if source.len() > MAX_SOURCE_BYTES {
        return Err("shell source exceeds the parser byte limit".to_owned());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|error| format!("could not load Bash grammar: {error}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Bash parser returned no syntax tree".to_owned())?;
    let root = tree.root_node();
    if root.start_byte() != 0 || root.end_byte() != source.len() {
        return Err("Bash syntax tree does not cover the full source".to_owned());
    }
    validate_tree_limits(root)?;
    if root.has_error() {
        return Err("Bash syntax tree contains an error or missing node".to_owned());
    }
    if root.kind() != "program" {
        return Err("Bash syntax tree has an unsupported root".to_owned());
    }

    let mut program = ShellProgram {
        items: Vec::new(),
        connectors: Vec::new(),
    };
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "comment" {
            return Err("shell comments are unsupported".to_owned());
        }
        let connector = (!program.items.is_empty()).then_some(Connector::Sequence);
        append_statement(child, source, connector, &mut program)?;
    }
    if program.items.is_empty() || program.items.len() > MAX_STAGES {
        return Err("shell command count is outside the supported range".to_owned());
    }
    if program.connectors.len().saturating_add(1) != program.items.len() {
        return Err("shell connector count is inconsistent".to_owned());
    }
    Ok(program)
}

/// Parse exactly one simple command for pre-execution rewriting.
///
/// # Errors
///
/// Returns an error when the source is compound or contains stream redirects.
pub(crate) fn parse_simple_words(source: &str) -> Result<Vec<String>, String> {
    let program = parse(source)?;
    let [ShellItem::Simple(command)] = program.items.as_slice() else {
        return Err("shell source is not one simple command".to_owned());
    };
    if !program.connectors.is_empty() || !command.stream_merges.is_empty() {
        return Err("simple command contains unsupported stream syntax".to_owned());
    }
    Ok(command.words.clone())
}

fn validate_tree_limits(root: Node<'_>) -> Result<(), String> {
    let mut count = 0_usize;
    let mut stack = vec![(root, 1_usize)];
    while let Some((node, depth)) = stack.pop() {
        count = count.saturating_add(1);
        if count > MAX_AST_NODES {
            return Err("Bash syntax tree exceeds the node limit".to_owned());
        }
        if depth > MAX_NESTING_DEPTH {
            return Err("Bash syntax tree exceeds the nesting limit".to_owned());
        }
        if node.is_error() || node.is_missing() {
            return Err("Bash syntax tree contains an error or missing node".to_owned());
        }
        if !node.is_named() && node.kind() == "&" {
            return Err("background shell jobs are unsupported".to_owned());
        }
        let mut cursor = node.walk();
        stack.extend(
            node.children(&mut cursor)
                .map(|child| (child, depth.saturating_add(1))),
        );
    }
    Ok(())
}

fn append_statement(
    node: Node<'_>,
    source: &str,
    preceding: Option<Connector>,
    output: &mut ShellProgram,
) -> Result<(), String> {
    if node.kind() == "list" {
        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        if children.len() != 2 {
            return Err("shell list has an unsupported shape".to_owned());
        }
        let connector = list_connector(node)?;
        append_statement(children[0], source, preceding, output)?;
        append_statement(children[1], source, Some(connector), output)?;
        return Ok(());
    }

    let item = parse_item(node, source)?;
    if let Some(connector) = preceding {
        if output.items.is_empty() {
            return Err("shell connector has no left command".to_owned());
        }
        output.connectors.push(connector);
    } else if !output.items.is_empty() {
        return Err("shell command is missing a connector".to_owned());
    }
    output.items.push(item);
    Ok(())
}

fn list_connector(node: Node<'_>) -> Result<Connector, String> {
    let mut cursor = node.walk();
    let operators = node
        .children(&mut cursor)
        .filter(|child| !child.is_named())
        .filter_map(|child| match child.kind() {
            "&&" => Some(Connector::And),
            "||" => Some(Connector::Or),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [connector] = operators.as_slice() else {
        return Err("shell list has an unsupported connector".to_owned());
    };
    Ok(*connector)
}

fn parse_item(node: Node<'_>, source: &str) -> Result<ShellItem, String> {
    match node.kind() {
        "command" | "declaration_command" | "variable_assignment" | "variable_assignments" => {
            Ok(ShellItem::Simple(parse_simple(node, source)?))
        }
        "redirected_statement" => parse_redirected(node, source),
        "pipeline" => parse_pipeline(node, source),
        kind => Err(format!("unsupported shell statement: {kind}")),
    }
}

fn parse_pipeline(node: Node<'_>, source: &str) -> Result<ShellItem, String> {
    let mut stage_cursor = node.walk();
    let stage_nodes = node.named_children(&mut stage_cursor).collect::<Vec<_>>();
    if stage_nodes.len() < 2 || stage_nodes.len() > MAX_STAGES {
        return Err("pipeline stage count is outside the supported range".to_owned());
    }
    let mut token_cursor = node.walk();
    if node
        .children(&mut token_cursor)
        .any(|child| !child.is_named() && child.kind() == "|&")
    {
        return Err("pipelines that merge stderr are unsupported".to_owned());
    }
    let mut stages = Vec::with_capacity(stage_nodes.len());
    for stage in stage_nodes {
        let simple = match parse_item(stage, source)? {
            ShellItem::Simple(simple) => simple,
            ShellItem::Pipeline(_) => return Err("nested pipelines are unsupported".to_owned()),
        };
        stages.push(simple);
    }
    Ok(ShellItem::Pipeline(stages))
}

fn parse_redirected(node: Node<'_>, source: &str) -> Result<ShellItem, String> {
    let body = node
        .child_by_field_name("body")
        .ok_or_else(|| "redirected shell statement has no body".to_owned())?;
    let ShellItem::Simple(mut command) = parse_item(body, source)? else {
        return Err("redirects around compound statements are unsupported".to_owned());
    };
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.id() == body.id() {
            continue;
        }
        command
            .stream_merges
            .push(parse_stream_merge(child, source)?);
    }
    Ok(ShellItem::Simple(command))
}

fn parse_simple(node: Node<'_>, source: &str) -> Result<SimpleCommand, String> {
    if matches!(
        node.kind(),
        "declaration_command" | "variable_assignment" | "variable_assignments"
    ) {
        return Ok(SimpleCommand {
            words: decode_fragment_words(node_text(node, source)?)?,
            stream_merges: Vec::new(),
        });
    }
    if node.kind() != "command" {
        return Err("unsupported simple command node".to_owned());
    }
    let mut words = Vec::new();
    let mut stream_merges = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "file_redirect" => stream_merges.push(parse_stream_merge(child, source)?),
            "herestring_redirect" | "subshell" => {
                return Err("dynamic command input is unsupported".to_owned());
            }
            _ => words.push(decode_one_word(node_text(child, source)?)?),
        }
    }
    if words.is_empty() {
        return Err("simple command has no literal words".to_owned());
    }
    Ok(SimpleCommand {
        words,
        stream_merges,
    })
}

fn parse_stream_merge(node: Node<'_>, source: &str) -> Result<StreamMerge, String> {
    if node.kind() != "file_redirect" {
        return Err("unsupported shell redirection".to_owned());
    }
    let compact = node_text(node, source)?
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    match compact.as_slice() {
        b"2>&1" => Ok(StreamMerge::StderrToStdout),
        b"1>&2" => Ok(StreamMerge::StdoutToStderr),
        _ => Err("unsupported shell redirection".to_owned()),
    }
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> Result<&'a str, String> {
    source
        .get(node.byte_range())
        .ok_or_else(|| "Bash syntax node is outside the source".to_owned())
}

fn decode_one_word(source: &str) -> Result<String, String> {
    let words = decode_fragment_words(source)?;
    let [word] = words.as_slice() else {
        return Err("shell word did not decode to one literal argument".to_owned());
    };
    Ok(word.clone())
}

fn decode_fragment_words(source: &str) -> Result<Vec<String>, String> {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = Quote::None;
    let mut characters = source.chars();
    while let Some(character) = characters.next() {
        match quote {
            Quote::None => match character {
                '\'' => {
                    quote = Quote::Single;
                    started = true;
                }
                '"' => {
                    quote = Quote::Double;
                    started = true;
                }
                '\\' => {
                    let escaped = characters
                        .next()
                        .ok_or_else(|| "shell word has a trailing escape".to_owned())?;
                    if matches!(escaped, '\n' | '\r') {
                        return Err("escaped newlines are unsupported".to_owned());
                    }
                    word.push(escaped);
                    started = true;
                }
                '*' | '?' | '[' if word.is_empty() || word.starts_with('-') => {
                    return Err("shell word contains unsafe pathname expansion".to_owned());
                }
                '{' | '}' => return Err("shell brace expansion is unsupported".to_owned()),
                '\n' | '\r' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`' | '$' | '#' => {
                    return Err("shell word contains dynamic or control syntax".to_owned());
                }
                value if value.is_whitespace() => finish_word(&mut words, &mut word, &mut started),
                _ => {
                    word.push(character);
                    started = true;
                }
            },
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(character);
                }
            }
            Quote::Double => match character {
                '"' => quote = Quote::None,
                '\\' => {
                    let escaped = characters
                        .next()
                        .ok_or_else(|| "shell word has a trailing escape".to_owned())?;
                    if matches!(escaped, '\n' | '\r') {
                        return Err("escaped newlines are unsupported".to_owned());
                    }
                    word.push(escaped);
                }
                '`' | '$' => return Err("shell expansion is unsupported".to_owned()),
                _ => word.push(character),
            },
        }
    }
    if quote != Quote::None {
        return Err("shell word has an unterminated quote".to_owned());
    }
    finish_word(&mut words, &mut word, &mut started);
    if words.is_empty() {
        return Err("shell fragment has no literal words".to_owned());
    }
    Ok(words)
}

fn finish_word(words: &mut Vec<String>, word: &mut String, started: &mut bool) {
    if *started {
        words.push(std::mem::take(word));
        *started = false;
    }
}

fn inspect_syntax_tree(
    node: Node<'_>,
    source: &str,
    features: &mut SyntaxFeatures,
    depth: usize,
    node_count: &mut usize,
) {
    *node_count = node_count.saturating_add(1);
    if depth > MAX_NESTING_DEPTH || *node_count > MAX_AST_NODES {
        features.insert(SyntaxFeature::ParserFailure);
        return;
    }
    match node.kind() {
        "for_statement"
        | "c_style_for_statement"
        | "while_statement"
        | "if_statement"
        | "case_statement" => features.insert(SyntaxFeature::ControlStructure),
        "function_definition" => features.insert(SyntaxFeature::ShellFunction),
        "subshell" => features.insert(SyntaxFeature::SubshellGroup),
        "comment" => features.insert(SyntaxFeature::Comment),
        "file_redirect" => {
            if parse_stream_merge(node, source).is_ok() {
                features.insert(SyntaxFeature::StreamMerge);
            } else {
                features.insert(SyntaxFeature::OtherRedirection);
            }
        }
        "herestring_redirect" | "heredoc_redirect" | "process_substitution" => {
            features.insert(SyntaxFeature::OtherRedirection);
        }
        "simple_expansion" | "expansion" => features.insert(SyntaxFeature::ParameterExpansion),
        "command_substitution" | "arithmetic_expansion" => {
            features.insert(SyntaxFeature::CommandSubstitution);
        }
        "command" => inspect_command_name(node, source, features),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        inspect_syntax_tree(child, source, features, depth + 1, node_count);
    }
}

fn inspect_command_name(node: Node<'_>, source: &str, features: &mut SyntaxFeatures) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(name) = node_text(name, source).and_then(decode_one_word) else {
        return;
    };
    if name.contains('/') {
        features.insert(SyntaxFeature::PathInvocation);
    }
    let base = name.rsplit('/').next().unwrap_or(&name);
    if matches!(
        base,
        "sudo" | "doas" | "nice" | "nohup" | "stdbuf" | "unbuffer" | "xargs" | "parallel" | "watch"
    ) {
        features.insert(SyntaxFeature::UnsupportedWrapper);
    }
    if matches!(base, "bash" | "sh" | "zsh" | "fish") {
        features.insert(SyntaxFeature::NestedShell);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LexicalQuote {
    None,
    Single,
    Double,
}

struct LexicalScanner<'a> {
    bytes: &'a [u8],
    features: SyntaxFeatures,
    index: usize,
    quote: LexicalQuote,
    escaped: bool,
    comment: bool,
    boundary: bool,
}

impl<'a> LexicalScanner<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            bytes: source.as_bytes(),
            features: SyntaxFeatures::default(),
            index: 0,
            quote: LexicalQuote::None,
            escaped: false,
            comment: false,
            boundary: true,
        }
    }

    fn finish(mut self) -> SyntaxFeatures {
        while self.index < self.bytes.len() {
            self.advance();
        }
        self.features
    }

    fn advance(&mut self) {
        let byte = self.bytes[self.index];
        if self.comment {
            if matches!(byte, b'\n' | b'\r') {
                self.comment = false;
                self.features.insert(SyntaxFeature::Multiline);
                self.boundary = true;
            }
            self.index += 1;
            return;
        }
        if self.escaped {
            self.escaped = false;
            self.boundary = false;
            self.index += 1;
            return;
        }
        match self.quote {
            LexicalQuote::Single => {
                if byte == b'\'' {
                    self.quote = LexicalQuote::None;
                }
            }
            LexicalQuote::Double => self.observe_double_quoted(byte),
            LexicalQuote::None => self.observe_unquoted(byte),
        }
        self.index += 1;
    }

    fn observe_double_quoted(&mut self, byte: u8) {
        match byte {
            b'\\' => self.escaped = true,
            b'"' => self.quote = LexicalQuote::None,
            b'`' => self.features.insert(SyntaxFeature::CommandSubstitution),
            b'$' if self.bytes.get(self.index + 1) == Some(&b'(') => {
                self.features.insert(SyntaxFeature::CommandSubstitution);
            }
            b'$' => self.features.insert(SyntaxFeature::ParameterExpansion),
            b'\n' | b'\r' => self.features.insert(SyntaxFeature::Multiline),
            _ => {}
        }
    }

    fn observe_unquoted(&mut self, byte: u8) {
        match byte {
            b'\\' => self.escaped = true,
            b'\'' => self.quote = LexicalQuote::Single,
            b'"' => self.quote = LexicalQuote::Double,
            b'#' if self.boundary => {
                self.features.insert(SyntaxFeature::Comment);
                self.comment = true;
            }
            b'\n' | b'\r' => self.features.insert(SyntaxFeature::Multiline),
            b'`' => self.features.insert(SyntaxFeature::CommandSubstitution),
            b'$' if self.bytes.get(self.index + 1) == Some(&b'(') => {
                self.features.insert(SyntaxFeature::CommandSubstitution);
            }
            b'$' => self.features.insert(SyntaxFeature::ParameterExpansion),
            b'|' if self.bytes.get(self.index + 1) == Some(&b'|') => {
                self.features.insert(SyntaxFeature::OrChain);
                self.index += 1;
            }
            b'|' => {
                self.features.insert(SyntaxFeature::Pipeline);
                if self.bytes.get(self.index + 1) == Some(&b'&') {
                    self.index += 1;
                }
            }
            b'&' if self.is_redirect_ampersand() => {}
            b'&' if self.bytes.get(self.index + 1) == Some(&b'&') => {
                self.features.insert(SyntaxFeature::AndChain);
                self.index += 1;
            }
            b'&' => self.features.insert(SyntaxFeature::Background),
            b';' => self.features.insert(SyntaxFeature::Semicolon),
            b'<' | b'>' if !self.is_safe_stream_merge() => {
                self.features.insert(SyntaxFeature::OtherRedirection);
            }
            _ => {}
        }
        self.boundary = byte.is_ascii_whitespace()
            || matches!(byte, b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>');
    }

    fn is_redirect_ampersand(&self) -> bool {
        self.bytes.get(self.index.saturating_sub(1)) == Some(&b'>')
            || self.bytes.get(self.index + 1) == Some(&b'>')
    }

    fn is_safe_stream_merge(&self) -> bool {
        let start = self.index.saturating_sub(1);
        let end = self.index.saturating_add(3).min(self.bytes.len());
        matches!(&self.bytes[start..end], b"2>&1" | b"1>&2")
    }
}

fn inspect_lexical_syntax(source: &str) -> SyntaxFeatures {
    LexicalScanner::new(source).finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_commands_and_literal_quotes() {
        assert_eq!(
            parse_simple_words("git diff -- 'file name'").expect("simple"),
            ["git", "diff", "--", "file name"]
        );
        assert_eq!(
            parse_simple_words("GOWORK=off go test ./...").expect("assignment"),
            ["GOWORK=off", "go", "test", "./..."]
        );
        assert_eq!(
            parse_simple_words(r"printf escaped\|operator").expect("escaped operator"),
            ["printf", "escaped|operator"]
        );
        assert_eq!(
            parse_simple_words("echo src/*.rs ~/private").expect("safe path expansion"),
            ["echo", "src/*.rs", "~/private"]
        );
    }

    #[test]
    fn parses_lists_and_pipelines_without_expanding_them() {
        let program = parse("cd repo && rg TODO . | sort | head -50; true").expect("program");
        assert_eq!(program.items.len(), 3);
        assert_eq!(program.connectors, [Connector::And, Connector::Sequence]);
        let ShellItem::Pipeline(stages) = &program.items[1] else {
            panic!("pipeline");
        };
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].words, ["rg", "TODO", "."]);
        assert_eq!(stages[2].words, ["head", "-50"]);
    }

    #[test]
    fn accepts_only_reviewed_stream_merges() {
        let program = parse("cargo test 2>&1 || true").expect("stream merge");
        let ShellItem::Simple(command) = &program.items[0] else {
            panic!("simple");
        };
        assert_eq!(command.stream_merges, [StreamMerge::StderrToStdout]);
        let features = inspect_syntax("cargo test 2>&1");
        assert!(!features.contains(SyntaxFeature::Background));
        assert!(!features.contains(SyntaxFeature::OtherRedirection));
        assert!(features.contains(SyntaxFeature::StreamMerge));
        assert!(parse("cargo test > result.log").is_err());
        assert!(parse("cargo test |& head").is_err());
    }

    #[test]
    fn reports_fixed_syntax_features_without_source_values() {
        let features = inspect_syntax(
            "cd /repo && /usr/bin/rg $TERM . | head -50; printf '%s' \"$(pwd)\" > result &",
        );
        assert!(features.contains(SyntaxFeature::Pipeline));
        assert!(features.contains(SyntaxFeature::AndChain));
        assert!(features.contains(SyntaxFeature::Semicolon));
        assert!(features.contains(SyntaxFeature::ParameterExpansion));
        assert!(features.contains(SyntaxFeature::CommandSubstitution));
        assert!(features.contains(SyntaxFeature::OtherRedirection));
        assert!(features.contains(SyntaxFeature::Background));
        assert!(features.contains(SyntaxFeature::PathInvocation));
        assert!(!features.labels().any(|label| label.contains("repo")));
    }

    #[test]
    fn rejects_dynamic_or_unbounded_shell_forms() {
        for source in [
            "echo $HOME",
            "echo $(pwd)",
            "echo `pwd`",
            "echo *.rs",
            "echo --j*",
            "echo {one,two}",
            "echo value # comment",
            "cargo test &",
            "cargo test | head &",
            "(cargo test)",
            "for x in a; do echo $x; done",
            "cat <<EOF\nvalue\nEOF",
            "cargo test |",
            "cargo test &&",
            "((((((((((((((((((((cargo test))))))))))))))))))))",
        ] {
            assert!(parse(source).is_err(), "accepted {source:?}");
        }
        let oversized = "x".repeat(MAX_SOURCE_BYTES + 1);
        assert!(parse(&oversized).is_err());
        assert!(inspect_syntax(&oversized).contains(SyntaxFeature::ParserFailure));
        let too_many_stages = std::iter::repeat_n("cat", MAX_STAGES + 1)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(parse(&too_many_stages).is_err());
    }

    #[test]
    fn arbitrary_parse_failures_are_pure_and_fail_open_without_mutation() {
        let alphabet = b"abc $'\"\\|&;()<>#\n`";
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        for length in 0..128 {
            let mut source = String::with_capacity(length);
            for _ in 0..length {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let modulus = u64::try_from(alphabet.len()).expect("alphabet length");
                let index = usize::try_from(state % modulus).expect("alphabet index");
                source.push(char::from(alphabet[index]));
            }
            let before = source.clone();
            let _ = parse(&source);
            assert_eq!(source, before);
        }
    }
}
