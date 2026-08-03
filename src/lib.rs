#![forbid(unsafe_code)]

pub mod agent_skill;
pub mod archive;
pub mod archive_query;
pub mod config;
mod config_cli;
pub mod reducers;
mod result_reducer;
pub mod rewrite;
pub mod rules;
mod rules_cli;
pub mod runner;
pub mod shell;

use rewrite::ArchiveCommandRef;
use std::io;
use std::io::Write as _;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const HELP: &str = "YARP prunes developer command output and archives Pi tool calls.\n\nUsage:\n  yarp plan --json [--rule-pack <path>]... <shell-command>\n  yarp rewrite [--rule-pack <path>]... <shell-command>\n  yarp run [--rule-pack <path>]... -- <command> [arguments...]\n  yarp config <path|init|show|get|set|unset|check>\n  yarp rules check <source-pack>\n  yarp rules compile <source-pack> --output <compiled-pack>\n  yarp rules verify <compiled-pack>\n  yarp rules list [--rule-pack <path>]... [--json]\n  yarp rules explain [--rule-pack <path>]... [--json] -- <command> [arguments...]\n  yarp search REF PATTERN [options]\n  yarp read REF [SOURCE] START:END\n  yarp read REF SOURCE --bytes START:END\n  yarp archive stats\n  yarp archive verify\n  yarp archive prune --before <UTC timestamp>\n  yarp archive ingest\n  yarp --skill list\n  yarp --skill show yarp\n  yarp --skill export yarp\n  yarp --help\n  yarp --version\n";

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
        [command, format, rest @ ..] if command == "plan" && format == "--json" => {
            match parse_rewrite(rest) {
                Ok((packs, reference, shell_command)) => plan(shell_command, reference, &packs),
                Err(error) => usage_error(&error),
            }
        }
        [command, rest @ ..] if command == "rewrite" => match parse_rewrite(rest) {
            Ok((packs, reference, shell_command)) => rewrite(shell_command, reference, &packs),
            Err(error) => usage_error(&error),
        },
        [command, rest @ ..] if command == "run" => match parse_run(rest) {
            Ok((key, child, packs, expected)) => {
                match runner::run_with_rules(child, key.as_ref(), &packs, expected.as_ref()) {
                    Ok(code) => code,
                    Err(error) => usage_error(&error),
                }
            }
            Err(error) => usage_error(&error),
        },
        [command, rest @ ..] if command == "config" => config_cli::run(rest),
        [command, rest @ ..] if command == "rules" => rules_cli::run(rest),
        [command, rest @ ..] if command == "search" => archive_search(rest),
        [command, rest @ ..] if command == "read" => archive_read(rest),
        [command] if command == "result-reduce" => {
            match result_reducer::run(io::stdin().lock(), io::stdout().lock()) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("yarp: {error}");
                    65
                }
            }
        }
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
        [command, subcommand, rest @ ..] if command == "archive" && subcommand == "restore" => {
            archive_restore(rest)
        }
        [command, subcommand, option, timestamp]
            if command == "archive" && subcommand == "prune" && option == "--before" =>
        {
            archive_prune(timestamp)
        }
        _ => usage_error("invalid arguments"),
    }
}

fn plan(
    shell_command: &str,
    reference: Option<ArchiveCommandRef<'_>>,
    packs: &[rules::PackRequest],
) -> i32 {
    match rewrite::plan_with_options(shell_command, reference, packs)
        .and_then(|plan| serde_json::to_string(&plan).map_err(|error| error.to_string()))
    {
        Ok(plan) => {
            println!("{plan}");
            0
        }
        Err(error) => {
            eprintln!("yarp: {error}");
            3
        }
    }
}

fn rewrite(
    shell_command: &str,
    reference: Option<ArchiveCommandRef<'_>>,
    packs: &[rules::PackRequest],
) -> i32 {
    match rewrite::rewrite_with_options(shell_command, reference, packs) {
        Ok(Some(rewritten)) => {
            print!("{rewritten}");
            0
        }
        Ok(None) => 3,
        Err(error) => {
            eprintln!("yarp: {error}");
            3
        }
    }
}

fn archive_search(arguments: &[String]) -> i32 {
    match archive_query::search(arguments) {
        Ok(archive_query::SearchOutcome::Matches(output)) => {
            if let Err(error) = io::stdout().write_all(&output) {
                eprintln!("yarp: could not write search output: {error}");
                return 74;
            }
            0
        }
        Ok(archive_query::SearchOutcome::NoMatches(output)) => {
            if let Err(error) = io::stdout().write_all(&output) {
                eprintln!("yarp: could not write search output: {error}");
                return 74;
            }
            1
        }
        Err(error) => {
            eprintln!("yarp: {error}");
            65
        }
    }
}

fn archive_read(arguments: &[String]) -> i32 {
    match archive_query::read(arguments) {
        Ok(output) => match io::stdout().write_all(&output) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("yarp: could not write exact archive range: {error}");
                74
            }
        },
        Err(error) => {
            eprintln!("yarp: {error}");
            65
        }
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
    match archive::Archive::open_read_only().and_then(|archive| archive.verify()) {
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
            65
        }
    }
}

fn archive_restore(arguments: &[String]) -> i32 {
    let key = match parse_archive_key(arguments) {
        Ok(value) => value,
        Err(error) => return usage_error(&error),
    };
    match archive::Archive::open_read_only()
        .and_then(|archive| archive.restore_streams(&key, io::stdout().lock(), io::stderr().lock()))
    {
        Ok(()) => 0,
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

fn parse_rewrite(
    arguments: &[String],
) -> Result<(Vec<rules::PackRequest>, Option<ArchiveCommandRef<'_>>, &str), String> {
    let mut packs = rules::requests_from_config()?;
    let mut project_root = None;
    let mut project_pack_seen = false;
    let mut agent = None;
    let mut account = None;
    let mut session = None;
    let mut call = None;
    let mut index = 0;
    while index + 1 < arguments.len() {
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("{} requires a value", arguments[index]))?;
        match arguments[index].as_str() {
            "--project-root" if project_root.is_none() => {
                project_root = Some(std::path::PathBuf::from(value));
            }
            "--rule-pack" => {
                let path = if let Some(root) = &project_root {
                    project_pack_seen = true;
                    rules::canonical_project_pack(root, std::path::Path::new(value))?
                } else {
                    value.into()
                };
                packs.push(rules::PackRequest {
                    path,
                    expected_digest: None,
                    expected_compiled_digest: None,
                });
            }
            "--archive-agent" if agent.is_none() => agent = Some(value.as_str()),
            "--archive-account" if account.is_none() => account = Some(value.as_str()),
            "--archive-session" if session.is_none() => session = Some(value.as_str()),
            "--archive-call" if call.is_none() => call = Some(value.as_str()),
            value if value.starts_with("--") => {
                return Err(format!("unknown or duplicate rewrite option: {value}"));
            }
            _ => break,
        }
        index += 2;
    }
    let [shell_command] = &arguments[index..] else {
        return Err("rewrite requires one shell command".to_owned());
    };
    if project_root.is_some() && !project_pack_seen {
        return Err("--project-root requires a following --rule-pack".to_owned());
    }
    let reference = match (agent, account, session, call) {
        (None, None, None, None) => None,
        (Some(agent), Some(account), Some(session_id), Some(call_id)) => Some(ArchiveCommandRef {
            agent,
            account,
            session_id,
            call_id,
        }),
        _ => return Err("archived rewrite requires every archive identifier".to_owned()),
    };
    Ok((packs, reference, shell_command))
}

fn parse_archive_key(arguments: &[String]) -> Result<archive::ArchiveKey, String> {
    let [
        agent_flag,
        agent,
        account_flag,
        account,
        session_flag,
        session,
        call_flag,
        call,
    ] = arguments
    else {
        return Err("invalid archive key arguments".to_owned());
    };
    if agent_flag != "--archive-agent"
        || account_flag != "--archive-account"
        || session_flag != "--archive-session"
        || call_flag != "--archive-call"
    {
        return Err("invalid archive key options".to_owned());
    }
    Ok(archive::ArchiveKey {
        session: archive::SessionIdentity {
            agent: agent.clone(),
            account: account.clone(),
            source_session_id: session.clone(),
            started_at_ms: None,
        },
        source_call_id: call.clone(),
    })
}

type ParsedRun<'a> = (
    Option<archive::ArchiveKey>,
    &'a [String],
    Vec<rules::PackRequest>,
    Option<runner::ExpectedSelection>,
);

fn parse_run(arguments: &[String]) -> Result<ParsedRun<'_>, String> {
    let mut packs = Vec::new();
    let mut selected_pack = None;
    let mut selected_rule = None;
    let mut selected_digest = None;
    let mut agent = None;
    let mut account = None;
    let mut session = None;
    let mut call = None;
    let mut index = 0;
    while index < arguments.len() && arguments[index] != "--" {
        let option = arguments[index].as_str();
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("{option} requires a value"))?;
        match option {
            "--selected-pack" if selected_pack.is_none() => selected_pack = Some(value.clone()),
            "--selected-rule" if selected_rule.is_none() => selected_rule = Some(value.clone()),
            "--selected-digest" if selected_digest.is_none() => {
                selected_digest = Some(rules::parse_digest(value)?);
            }
            "--archive-agent" if agent.is_none() => agent = Some(value.clone()),
            "--archive-account" if account.is_none() => account = Some(value.clone()),
            "--archive-session" if session.is_none() => session = Some(value.clone()),
            "--archive-call" if call.is_none() => call = Some(value.clone()),
            "--rule-pack" => {
                let (request, next_index) = parse_run_pack(arguments, index, value)?;
                packs.push(request);
                index = next_index;
                continue;
            }
            value if value.starts_with("--") => {
                return Err(format!("unknown or duplicate run option: {value}"));
            }
            _ => return Err(format!("invalid run option: {option}")),
        }
        index += 2;
    }
    let Some(separator) = arguments.get(index) else {
        return Err("run requires -- and a child command".to_owned());
    };
    if separator != "--" || index + 1 >= arguments.len() {
        return Err("run requires -- and a child command".to_owned());
    }
    let child = &arguments[index + 1..];
    let expected = match (selected_pack, selected_rule, selected_digest) {
        (None, None, None) => None,
        (Some(pack_id), Some(rule_id), Some(source_digest)) => Some(runner::ExpectedSelection {
            pack_id,
            rule_id,
            source_digest,
        }),
        _ => {
            return Err(
                "run requires selected pack, selected rule, and selected digest together"
                    .to_owned(),
            );
        }
    };
    let archive_key = match (agent, account, session, call) {
        (None, None, None, None) => None,
        (Some(agent), Some(account), Some(session), Some(call)) => Some(archive::ArchiveKey {
            session: archive::SessionIdentity {
                agent,
                account,
                source_session_id: session,
                started_at_ms: None,
            },
            source_call_id: call,
        }),
        _ => return Err("archived run requires every archive identifier".to_owned()),
    };
    if expected.is_none() {
        let mut configured = rules::requests_from_config()?;
        configured.extend(packs);
        packs = configured;
    }
    Ok((archive_key, child, packs, expected))
}

fn parse_run_pack(
    arguments: &[String],
    mut index: usize,
    path: &str,
) -> Result<(rules::PackRequest, usize), String> {
    let mut expected_digest = None;
    let mut expected_compiled_digest = None;
    index += 2;
    if arguments
        .get(index)
        .is_some_and(|value| value == "--rule-pack-digest")
    {
        let digest = arguments
            .get(index + 1)
            .ok_or_else(|| "--rule-pack-digest requires a value".to_owned())?;
        expected_digest = Some(rules::parse_digest(digest)?);
        index += 2;
    }
    if arguments
        .get(index)
        .is_some_and(|value| value == "--rule-pack-compiled-digest")
    {
        let digest = arguments
            .get(index + 1)
            .ok_or_else(|| "--rule-pack-compiled-digest requires a value".to_owned())?;
        expected_compiled_digest = Some(rules::parse_digest(digest)?);
        index += 2;
    }
    if expected_digest.is_some() != expected_compiled_digest.is_some() {
        return Err("rule pack source and compiled digests must be supplied together".to_owned());
    }
    Ok((
        rules::PackRequest {
            path: path.into(),
            expected_digest,
            expected_compiled_digest,
        },
        index,
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
        let (key, child, packs, expected) = parse_run(&arguments).expect("parse");
        let key = key.expect("archive key");
        assert_eq!(key.session.agent, "pi");
        assert_eq!(key.source_call_id, "call");
        assert_eq!(child, &["git".to_owned(), "status".to_owned()]);
        assert!(packs.is_empty());
        assert!(expected.is_none());
    }
}
