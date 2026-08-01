use crate::rules::{PackRequest, Registry, Selection, digest_hex};

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
            "go test -json ./...",
            "cargo test --message-format=json",
            "gh pr view 1 --json=number,title",
            "git diff --stat",
            "kubectl get pods -ojson",
            "npm test -- --reporter=json",
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
