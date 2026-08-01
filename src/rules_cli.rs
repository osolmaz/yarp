use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::NamedTempFile;
use yarp_rule_pack::{Action, CompiledPack, Reducer, SourcePack, compile};

use crate::rules::{
    PackRequest, Registry, Selection, digest_hex, requests_from_environment, requests_from_paths,
};

pub fn run(arguments: &[String]) -> i32 {
    match arguments {
        [command, source] if command == "check" => check(Path::new(source)),
        [command, source, output_flag, output]
            if command == "compile" && output_flag == "--output" =>
        {
            compile_pack(Path::new(source), Path::new(output))
        }
        [command, pack] if command == "verify" => verify(Path::new(pack)),
        [command, rest @ ..] if command == "list" => list(rest),
        [command, rest @ ..] if command == "explain" => explain(rest),
        _ => usage_error("invalid rules arguments"),
    }
}

fn check(path: &Path) -> i32 {
    match SourcePack::load(path) {
        Ok(pack) => {
            println!("pack: {}", pack.manifest.id);
            println!("rules: {}", pack.rules.len());
            println!("source_sha256: {}", digest_hex(&pack.source_digest));
            0
        }
        Err(error) => data_error(&error),
    }
}

fn compile_pack(source_path: &Path, output_path: &Path) -> i32 {
    let result = SourcePack::load(source_path)
        .and_then(|source| compile(&source))
        .and_then(|body| write_atomic(output_path, &body));
    match result {
        Ok(()) => 0,
        Err(error) => data_error(&error),
    }
}

fn verify(path: &Path) -> i32 {
    match CompiledPack::open(path, None).and_then(|mut pack| {
        pack.verify_all()?;
        println!("pack: {}", pack.id);
        println!("rules: {}", pack.rules.len());
        println!("source_sha256: {}", digest_hex(&pack.source_digest));
        println!("rule_pack: ok");
        Ok(())
    }) {
        Ok(()) => 0,
        Err(error) => data_error(&error),
    }
}

fn list(arguments: &[String]) -> i32 {
    let (paths, json_output, rest) = match parse_pack_options(arguments, false) {
        Ok(value) => value,
        Err(error) => return usage_error(&error),
    };
    if !rest.is_empty() {
        return usage_error("rules list has unexpected arguments");
    }
    let requests = match combined_requests(&paths) {
        Ok(value) => value,
        Err(error) => return data_error(&error),
    };
    let mut registry = match Registry::load(&requests) {
        Ok(value) => value,
        Err(error) => return data_error(&error),
    };
    for diagnostic in registry.diagnostics() {
        eprintln!("yarp: {diagnostic}");
    }
    let summaries = match registry.summaries() {
        Ok(value) => value,
        Err(error) => return data_error(&error),
    };
    if json_output {
        let value = summaries
            .iter()
            .map(|summary| {
                json!({
                    "pack_id": summary.pack_id,
                    "rule_id": summary.rule.id,
                    "action": summary.rule.action,
                    "match": summary.rule.matcher,
                    "reducer": summary.rule.reducer,
                    "success": summary.rule.success,
                    "failure": summary.rule.failure,
                })
            })
            .collect::<Vec<_>>();
        match serde_json::to_string_pretty(&value) {
            Ok(value) => println!("{value}"),
            Err(error) => return data_error(&format!("could not encode rule list: {error}")),
        }
    } else {
        for summary in summaries {
            let action = match summary.rule.action {
                Action::Reduce => "reduce",
                Action::Passthrough => "passthrough",
            };
            let reducer = summary.rule.reducer.as_ref().map_or("-", reducer_name);
            println!(
                "{}\t{}\t{}\t{}\t{}",
                summary.pack_id,
                summary.rule.id,
                action,
                reducer,
                summary.rule.matcher.program.join(",")
            );
        }
    }
    0
}

fn explain(arguments: &[String]) -> i32 {
    let (paths, json_output, rest) = match parse_pack_options(arguments, true) {
        Ok(value) => value,
        Err(error) => return usage_error(&error),
    };
    let Some((separator, command)) = rest.split_first() else {
        return usage_error("rules explain requires -- and a command");
    };
    if separator != "--" || command.is_empty() {
        return usage_error("rules explain requires -- and a command");
    }
    let requests = match combined_requests(&paths) {
        Ok(value) => value,
        Err(error) => return data_error(&error),
    };
    let mut registry = match Registry::load(&requests) {
        Ok(value) => value,
        Err(error) => return data_error(&error),
    };
    let diagnostics = registry.diagnostics().to_vec();
    let selection = match registry.select(command) {
        Ok(value) => value,
        Err(error) => return data_error(&error),
    };
    if json_output {
        let mut value = match &selection {
            Selection::Reduce(selected) => json!({
                "outcome": "reduce",
                "pack_id": selected.pack_id,
                "rule_id": selected.rule.id,
                "reducer": selected.rule.reducer,
                "success": selected.rule.success,
                "failure": selected.rule.failure,
            }),
            Selection::Passthrough(ids) => json!({
                "outcome": "passthrough",
                "matching_rules": ids,
            }),
            Selection::Ambiguous(ids) => json!({
                "outcome": "ambiguous",
                "matching_rules": ids,
            }),
            Selection::Unsupported => json!({ "outcome": "unsupported" }),
        };
        if !diagnostics.is_empty()
            && let Some(object) = value.as_object_mut()
        {
            object.insert("diagnostics".to_owned(), json!(diagnostics));
        }
        match serde_json::to_string_pretty(&value) {
            Ok(value) => println!("{value}"),
            Err(error) => return data_error(&format!("could not encode explanation: {error}")),
        }
    } else {
        for diagnostic in diagnostics {
            println!("diagnostic: {diagnostic}");
        }
        match selection {
            Selection::Reduce(selected) => {
                println!("outcome: reduce");
                println!("pack: {}", selected.pack_id);
                println!("rule: {}", selected.rule.id);
                println!(
                    "reducer: {}",
                    selected.rule.reducer.as_ref().map_or("-", reducer_name)
                );
            }
            Selection::Passthrough(ids) => {
                println!("outcome: passthrough");
                println!("rules: {}", ids.join(","));
            }
            Selection::Ambiguous(ids) => {
                println!("outcome: ambiguous");
                println!("rules: {}", ids.join(","));
            }
            Selection::Unsupported => println!("outcome: unsupported"),
        }
    }
    0
}

fn parse_pack_options(
    arguments: &[String],
    stop_at_separator: bool,
) -> Result<(Vec<PathBuf>, bool, &[String]), String> {
    let mut paths = Vec::new();
    let mut json_output = false;
    let mut index = 0;
    while index < arguments.len() {
        if stop_at_separator && arguments[index] == "--" {
            break;
        }
        match arguments[index].as_str() {
            "--rule-pack" => {
                let path = arguments
                    .get(index + 1)
                    .ok_or_else(|| "--rule-pack requires a path".to_owned())?;
                paths.push(PathBuf::from(path));
                index += 2;
            }
            "--json" => {
                json_output = true;
                index += 1;
            }
            _ => break,
        }
    }
    Ok((paths, json_output, &arguments[index..]))
}

fn combined_requests(paths: &[PathBuf]) -> Result<Vec<PackRequest>, String> {
    let mut requests = requests_from_environment()?;
    requests.extend(requests_from_paths(paths));
    Ok(requests)
}

fn reducer_name(reducer: &Reducer) -> &'static str {
    match reducer {
        Reducer::HeadTail => "head_tail",
        Reducer::LineFilter { .. } => "line_filter",
        Reducer::CargoTest => "cargo_test",
        Reducer::GitDiff => "git_diff",
        Reducer::GitStatus => "git_status",
        Reducer::Search => "search",
    }
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|error| format!("could not create temporary rule pack: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("could not protect temporary rule pack: {error}"))?;
    }
    temporary
        .write_all(body)
        .map_err(|error| format!("could not write rule pack: {error}"))?;
    temporary
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("could not flush rule pack: {error}"))?;
    temporary
        .persist(path)
        .map_err(|error| format!("could not install {}: {}", path.display(), error.error))?;
    sync_parent(parent)?;
    Ok(())
}

fn sync_parent(parent: &Path) -> Result<(), String> {
    let directory = fs::File::open(parent)
        .map_err(|error| format!("could not open {} for sync: {error}", parent.display()))?;
    directory
        .sync_all()
        .map_err(|error| format!("could not sync {}: {error}", parent.display()))
}

fn usage_error(error: &str) -> i32 {
    eprintln!("yarp: {error}");
    64
}

fn data_error(error: &str) -> i32 {
    eprintln!("yarp: {error}");
    65
}
