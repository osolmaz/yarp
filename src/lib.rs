#![forbid(unsafe_code)]

pub mod archive;
pub mod rewrite;
pub mod runner;

use rewrite::ArchiveCommandRef;
use std::io;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const HELP: &str = "YARP prunes developer command output and archives Pi tool calls.\n\nUsage:\n  yarp rewrite <shell-command>\n  yarp run -- <command> [arguments...]\n  yarp archive stats\n  yarp archive verify\n  yarp archive prune --before <UTC timestamp>\n  yarp archive ingest\n  yarp --help\n  yarp --version\n";

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
        [command, shell_command] if command == "rewrite" => rewrite(shell_command, None),
        [command, rest @ ..] if command == "rewrite" => match parse_archive_rewrite(rest) {
            Ok((reference, shell_command)) => rewrite(shell_command, Some(reference)),
            Err(_) => usage_error("invalid arguments"),
        },
        [command, rest @ ..] if command == "run" => match parse_run(rest) {
            Ok((key, child)) => match runner::run(child, key.as_ref()) {
                Ok(code) => code,
                Err(error) => usage_error(&error),
            },
            Err(error) => usage_error(&error),
        },
        [command, subcommand] if command == "archive" && subcommand == "stats" => archive_stats(),
        [command, subcommand] if command == "archive" && subcommand == "verify" => archive_verify(),
        [command, subcommand] if command == "archive" && subcommand == "ingest" => {
            match archive::run_ingest(io::stdin().lock(), io::stdout().lock()) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("yarp: {error}");
                    74
                }
            }
        }
        [command, subcommand, option, timestamp]
            if command == "archive" && subcommand == "prune" && option == "--before" =>
        {
            archive_prune(timestamp)
        }
        _ => usage_error("invalid arguments"),
    }
}

fn rewrite(shell_command: &str, reference: Option<ArchiveCommandRef<'_>>) -> i32 {
    if let Some(rewritten) = rewrite::rewrite_with_archive(shell_command, reference) {
        print!("{rewritten}");
        0
    } else {
        3
    }
}

fn archive_stats() -> i32 {
    match archive::Archive::open().and_then(|archive| archive.stats()) {
        Ok(stats) => {
            println!("sessions: {}", stats.sessions);
            println!("calls: {}", stats.calls);
            println!("incomplete_calls: {}", stats.incomplete_calls);
            println!("logical_payload_bytes: {}", stats.logical_payload_bytes);
            println!("stored_payload_bytes: {}", stats.stored_payload_bytes);
            println!("database_bytes: {}", stats.database_bytes);
            if let Some(oldest) = stats.oldest_call_ms {
                println!("oldest_call_ms: {oldest}");
            }
            if let Some(newest) = stats.newest_call_ms {
                println!("newest_call_ms: {newest}");
            }
            0
        }
        Err(error) => {
            eprintln!("yarp: {error}");
            74
        }
    }
}

fn archive_verify() -> i32 {
    match archive::Archive::open().and_then(|archive| archive.verify()) {
        Ok(report) => {
            println!("incomplete_calls: {}", report.incomplete_calls);
            if report.errors.is_empty() {
                println!("archive: ok");
                0
            } else {
                for error in report.errors {
                    eprintln!("yarp: archive verification failed: {error}");
                }
                65
            }
        }
        Err(error) => {
            eprintln!("yarp: {error}");
            74
        }
    }
}

fn archive_prune(timestamp: &str) -> i32 {
    let parsed = match OffsetDateTime::parse(timestamp, &Rfc3339) {
        Ok(value) => value,
        Err(error) => return usage_error(&format!("invalid UTC timestamp: {error}")),
    };
    if parsed.offset() != time::UtcOffset::UTC {
        return usage_error("prune timestamp must use UTC with a Z suffix");
    }
    let Ok(milliseconds) = i64::try_from(parsed.unix_timestamp_nanos() / 1_000_000) else {
        return usage_error("prune timestamp is outside the supported range");
    };
    match archive::Archive::open().and_then(|mut archive| archive.prune_before(milliseconds)) {
        Ok(deleted) => {
            println!("pruned_calls: {deleted}");
            0
        }
        Err(error) => {
            eprintln!("yarp: {error}");
            74
        }
    }
}

fn parse_archive_rewrite(arguments: &[String]) -> Result<(ArchiveCommandRef<'_>, &str), String> {
    let [
        agent_flag,
        agent,
        account_flag,
        account,
        session_flag,
        session,
        call_flag,
        call,
        shell,
    ] = arguments
    else {
        return Err("invalid archived rewrite arguments".to_owned());
    };
    if agent_flag != "--archive-agent"
        || account_flag != "--archive-account"
        || session_flag != "--archive-session"
        || call_flag != "--archive-call"
    {
        return Err("invalid archived rewrite options".to_owned());
    }
    Ok((
        ArchiveCommandRef {
            agent,
            account,
            session_id: session,
            call_id: call,
        },
        shell,
    ))
}

fn parse_run(arguments: &[String]) -> Result<(Option<archive::ArchiveKey>, &[String]), String> {
    if let [separator, child @ ..] = arguments
        && separator == "--"
        && !child.is_empty()
    {
        return Ok((None, child));
    }
    let [
        agent_flag,
        agent,
        account_flag,
        account,
        session_flag,
        session,
        call_flag,
        call,
        separator,
        child @ ..,
    ] = arguments
    else {
        return Err("invalid run arguments".to_owned());
    };
    if agent_flag != "--archive-agent"
        || account_flag != "--archive-account"
        || session_flag != "--archive-session"
        || call_flag != "--archive-call"
        || separator != "--"
        || child.is_empty()
    {
        return Err("invalid archived run options".to_owned());
    }
    Ok((
        Some(archive::ArchiveKey {
            session: archive::SessionIdentity {
                agent: agent.clone(),
                account: account.clone(),
                source_session_id: session.clone(),
                started_at_ms: None,
            },
            source_call_id: call.clone(),
        }),
        child,
    ))
}

fn usage_error(error: &str) -> i32 {
    eprintln!("yarp: {error}\n\n{HELP}");
    64
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
        assert_eq!(run_cli(&["run".into(), "--".into()]), 64);
    }

    #[test]
    fn parses_archived_run_metadata() {
        let arguments = vec![
            "--archive-agent".into(),
            "pi".into(),
            "--archive-account".into(),
            "onur".into(),
            "--archive-session".into(),
            "session".into(),
            "--archive-call".into(),
            "call".into(),
            "--".into(),
            "git".into(),
            "status".into(),
        ];
        let (key, child) = parse_run(&arguments).expect("parse");
        let key = key.expect("archive key");
        assert_eq!(key.session.agent, "pi");
        assert_eq!(key.source_call_id, "call");
        assert_eq!(child, &["git".to_owned(), "status".to_owned()]);
    }
}
