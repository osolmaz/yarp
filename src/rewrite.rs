use crate::rules::{PackRequest, Registry, Selection, digest_hex};
use yarp_rule_pack::{OutputPolicy, Rule};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

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
    let command = command.trim();
    let words = parse_words(command).ok_or_else(|| "unsupported shell syntax".to_owned())?;
    let mut registry = Registry::load(packs)?;
    let Selection::Reduce(selected) = registry.select(&words)? else {
        return Ok(None);
    };

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
    Ok(Some(wrapper))
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
    let words = parse_words(command).ok_or_else(|| "unsupported shell syntax".to_owned())?;
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

/// Select one compatible typed rule for conservative post-result reduction.
///
/// # Errors
///
/// Returns an error when shell syntax is ambiguous, guarded, unsupported, or mixes reducer
/// families. Callers fail open on every error.
pub fn select_result_rule(command: &str) -> Result<Rule, String> {
    let normalized = strip_safe_stream_redirects(command)
        .ok_or_else(|| "unsupported compound shell syntax".to_owned())?;
    let segments = split_compound(&normalized)
        .ok_or_else(|| "unsupported compound shell syntax".to_owned())?;
    let mut selected: Option<Rule> = None;
    for segment in segments {
        let words =
            parse_words(segment).ok_or_else(|| "unsupported compound command".to_owned())?;
        let Some(words) = normalize_result_words(words)? else {
            continue;
        };
        let mut registry = Registry::builtins_only();
        let Selection::Reduce(candidate) = registry
            .select(&words)
            .map_err(|error| format!("could not classify compound command: {error}"))?
        else {
            return Err("compound command contains an unsupported or guarded command".to_owned());
        };
        if let Some(existing) = &mut selected {
            if existing.reducer != candidate.rule.reducer {
                return Err("compound command mixes reducer families".to_owned());
            }
            let Some(existing_success) = existing.success else {
                return Err("selected rule has no success policy".to_owned());
            };
            let Some(candidate_success) = candidate.rule.success else {
                return Err("candidate rule has no success policy".to_owned());
            };
            let Some(existing_failure) = existing.failure else {
                return Err("selected rule has no failure policy".to_owned());
            };
            let Some(candidate_failure) = candidate.rule.failure else {
                return Err("candidate rule has no failure policy".to_owned());
            };
            existing.success = Some(merge_policy(existing_success, candidate_success));
            existing.failure = Some(merge_policy(existing_failure, candidate_failure));
        } else {
            selected = Some((*candidate.rule).clone());
        }
    }
    selected.ok_or_else(|| "compound command has no supported output command".to_owned())
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

fn strip_safe_stream_redirects(command: &str) -> Option<String> {
    let mut output = Vec::with_capacity(command.len());
    let bytes = command.as_bytes();
    let mut quote = Quote::None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Quote::Single if byte == b'\'' => quote = Quote::None,
            Quote::Double if byte == b'"' => quote = Quote::None,
            Quote::Double | Quote::None if byte == b'\\' => {
                output.push(byte);
                index = index.checked_add(1)?;
                output.push(*bytes.get(index)?);
                index += 1;
                continue;
            }
            Quote::None if byte == b'\'' => quote = Quote::Single,
            Quote::None if byte == b'"' => quote = Quote::Double,
            Quote::None => {
                let token = bytes.get(index..index.saturating_add(4));
                let boundary_before =
                    index == 0 || bytes.get(index - 1).is_some_and(u8::is_ascii_whitespace);
                let boundary_after = bytes
                    .get(index + 4)
                    .is_none_or(|next| next.is_ascii_whitespace() || matches!(next, b';' | b'&'));
                if boundary_before
                    && boundary_after
                    && (token == Some(&b"2>&1"[..]) || token == Some(&b"1>&2"[..]))
                {
                    index += 4;
                    continue;
                }
            }
            Quote::Single | Quote::Double => {}
        }
        output.push(byte);
        index += 1;
    }
    if quote != Quote::None {
        return None;
    }
    String::from_utf8(output).ok()
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
        [program, flag] if program == "set" && flag.starts_with('-') => true,
        [program, flag, value]
            if program == "set" && flag.starts_with('-') && value == "pipefail" =>
        {
            true
        }
        [program, ..] if matches!(program.as_str(), "export" | "umask") && words.len() > 1 => true,
        _ => false,
    }
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

fn split_compound(command: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = Quote::None;
    let bytes = command.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Quote::Single if byte == b'\'' => quote = Quote::None,
            Quote::Double if byte == b'"' => quote = Quote::None,
            Quote::Double if byte == b'\\' => {
                index = index.checked_add(1)?;
                if index >= bytes.len() {
                    return None;
                }
            }
            Quote::Double if matches!(byte, b'$' | b'`') => return None,
            Quote::None if byte == b'\'' => quote = Quote::Single,
            Quote::None if byte == b'"' => quote = Quote::Double,
            Quote::None if byte == b'\\' => {
                index = index.checked_add(1)?;
                if index >= bytes.len() {
                    return None;
                }
            }
            Quote::None if byte == b';' => {
                push_segment(command, start, index, &mut segments)?;
                start = index + 1;
            }
            Quote::None if matches!(byte, b'&' | b'|') && bytes.get(index + 1) == Some(&byte) => {
                push_segment(command, start, index, &mut segments)?;
                index += 1;
                start = index + 1;
            }
            Quote::None
                if matches!(
                    byte,
                    b'\n' | b'\r' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' | b'`' | b'$' | b'#'
                ) =>
            {
                return None;
            }
            Quote::Single | Quote::Double | Quote::None => {}
        }
        index += 1;
    }
    if quote != Quote::None {
        return None;
    }
    push_segment(command, start, bytes.len(), &mut segments)?;
    Some(segments)
}

fn push_segment<'a>(
    command: &'a str,
    start: usize,
    end: usize,
    segments: &mut Vec<&'a str>,
) -> Option<()> {
    let segment = command.get(start..end)?.trim();
    if segment.is_empty() {
        return None;
    }
    segments.push(segment);
    Some(())
}

fn parse_words(command: &str) -> Option<Vec<String>> {
    if command.is_empty() {
        return None;
    }

    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = Quote::None;
    let mut characters = command.chars();

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
                    word.push(characters.next()?);
                    started = true;
                }
                '\n' | '\r' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`' | '$' | '#' => {
                    return None;
                }
                character if character.is_whitespace() => {
                    finish_word(&mut words, &mut word, &mut started);
                }
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
                    word.push(characters.next()?);
                }
                '`' | '$' => return None,
                _ => word.push(character),
            },
        }
    }

    if quote != Quote::None {
        return None;
    }
    finish_word(&mut words, &mut word, &mut started);
    (!words.is_empty()).then_some(words)
}

fn finish_word(words: &mut Vec<String>, word: &mut String, started: &mut bool) {
    if *started {
        words.push(std::mem::take(word));
        *started = false;
    }
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
        assert!(rewrite("git -C /repo diff --check").is_some());
        assert!(rewrite("pnpm -C frontend test").is_some());
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
            "git status -sbz",
            "git grep -l needle",
            "git grep -lz needle",
            "git log --oneline -z",
            "ls -la",
            "du -sh .",
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
        assert!(select_result_rule("cargo test | cat").is_err());
        assert!(select_result_rule("cargo test > result.log").is_err());
        assert!(select_result_rule("cargo test && echo done").is_err());
        assert!(select_result_rule("set && cargo test").is_err());
        assert!(select_result_rule("yarp search ref error").is_err());
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

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }
}
