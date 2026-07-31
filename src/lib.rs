#![forbid(unsafe_code)]

pub mod rewrite;
pub mod runner;

const HELP: &str = "YARP prunes output from a small allowlist of developer commands.\n\nUsage:\n  yarp rewrite <shell-command>\n  yarp run -- <command> [arguments...]\n  yarp --help\n  yarp --version\n";

/// Run the command-line interface and return the process exit code.
#[must_use]
pub fn run_cli(arguments: &[String]) -> i32 {
    match arguments {
        [] => {
            print!("{HELP}");
            0
        }
        [argument] if argument == "--help" || argument == "-h" => {
            print!("{HELP}");
            0
        }
        [argument] if argument == "--version" || argument == "-V" => {
            println!("yarp {}", env!("CARGO_PKG_VERSION"));
            0
        }
        [command, shell_command] if command == "rewrite" => {
            if let Some(rewritten) = rewrite::rewrite(shell_command) {
                print!("{rewritten}");
                0
            } else {
                3
            }
        }
        [command, separator, child @ ..] if command == "run" && separator == "--" => {
            match runner::run(child) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("yarp: {error}");
                    64
                }
            }
        }
        _ => {
            eprintln!("yarp: invalid arguments\n\n{HELP}");
            64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_rewrite_fails_open_with_distinct_status() {
        assert_eq!(run_cli(&["rewrite".into(), "cat .env".into()]), 3);
    }

    #[test]
    fn invalid_arguments_return_usage_error() {
        assert_eq!(run_cli(&["rewrite".into()]), 64);
        assert_eq!(run_cli(&["unknown".into()]), 64);
    }
}
