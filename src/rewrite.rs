#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Return a wrapper command only for simple shell commands on the allowlist.
#[must_use]
pub fn rewrite(command: &str) -> Option<String> {
    let command = command.trim();
    let words = parse_words(command)?;
    is_allowed_words(&words).then(|| format!("yarp run -- {command}"))
}

/// Check an already parsed argument list before running a child process.
pub fn is_allowed_argv(arguments: &[String]) -> bool {
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    is_allowed_words(&words)
}

fn is_allowed_words<T: AsRef<str>>(words: &[T]) -> bool {
    let word = |index: usize| words.get(index).map(AsRef::as_ref);

    matches!(
        (word(0), word(1), word(2)),
        (Some("git"), Some("status" | "diff" | "log" | "show"), _)
            | (
                Some("cargo"),
                Some("build" | "check" | "clippy" | "test"),
                _
            )
            | (Some("go" | "npm" | "pnpm" | "yarn"), Some("test"), _)
            | (Some("dotnet"), Some("build" | "test"), _)
            | (Some("pytest"), _, _)
            | (
                Some("npm" | "pnpm" | "yarn"),
                Some("run"),
                Some("build" | "check" | "lint" | "test" | "typecheck"),
            )
    )
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
    Some(words)
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
    fn rewrites_allowlisted_commands_without_changing_the_original_text() {
        assert_eq!(
            rewrite("  git diff -- 'file name'  ").as_deref(),
            Some("yarp run -- git diff -- 'file name'")
        );
        assert_eq!(
            rewrite("npm run lint -- --fix").as_deref(),
            Some("yarp run -- npm run lint -- --fix")
        );
        assert_eq!(
            rewrite("pytest -q").as_deref(),
            Some("yarp run -- pytest -q")
        );
    }

    #[test]
    fn rejects_commands_outside_the_allowlist() {
        for command in [
            "",
            "cat .env",
            "curl https://example.com",
            "git push",
            "npm install",
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
        assert!(is_allowed_argv(&strings(&["cargo", "test", "--workspace"])));
        assert!(is_allowed_argv(&strings(&["go", "test", "./..."])));
        assert!(is_allowed_argv(&strings(&["dotnet", "build"])));
        assert!(is_allowed_argv(&strings(&["pnpm", "run", "typecheck"])));
        assert!(!is_allowed_argv(&strings(&["git", "push"])));
        assert!(!is_allowed_argv(&[]));
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }
}
