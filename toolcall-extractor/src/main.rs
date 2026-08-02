use std::io::{self, BufReader};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use toolcall_extractor::adapters;
use toolcall_extractor::benchmark;
use toolcall_extractor::ceiling::{self, AnalysisOptions};
use toolcall_extractor::database::Database;
use toolcall_extractor::error::{Error, Result};
use toolcall_extractor::private_fs;
use toolcall_extractor::sink::Sink;
use toolcall_extractor::stream::{self, JsonlSink};

#[derive(Parser)]
#[command(name = "toolcall-extractor", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Extract(ExtractArgs),
    Stream(StreamArgs),
    Ingest(IngestArgs),
    Stats(DatabaseArgs),
    Issues(DatabaseArgs),
    Verify(DatabaseArgs),
    BenchmarkYarp(DatabaseArgs),
    AnalyzeCeiling(CeilingArgs),
}

#[derive(Args)]
struct ExtractArgs {
    #[arg(long)]
    unix_user: String,
    #[arg(long)]
    database: Option<PathBuf>,
    #[command(subcommand)]
    source: Source,
}

#[derive(Args)]
struct StreamArgs {
    #[arg(long)]
    unix_user: String,
    #[command(subcommand)]
    source: Source,
}

#[derive(Args)]
struct IngestArgs {
    #[arg(long)]
    unix_user: String,
    #[arg(long, default_value = "stream")]
    agent: String,
    #[arg(long)]
    database: Option<PathBuf>,
}

#[derive(Args)]
struct DatabaseArgs {
    #[arg(long)]
    database: Option<PathBuf>,
}

#[derive(Args)]
struct CeilingArgs {
    #[arg(long)]
    database: Option<PathBuf>,
    #[arg(long, default_value_t = 704)]
    summary_character_budget: u64,
    #[arg(long, default_value_t = 256)]
    minimum_removed_characters: u64,
    #[arg(long, default_value_t = 1_500)]
    minimum_savings_basis_points: u64,
}

#[derive(Subcommand)]
enum Source {
    Pi {
        #[arg(long)]
        sessions: PathBuf,
    },
    Codex {
        #[arg(long)]
        sessions: PathBuf,
        #[arg(long)]
        state_db: Option<PathBuf>,
    },
    Claude {
        #[arg(long)]
        projects: PathBuf,
    },
    Cursor {
        #[arg(long)]
        chats: PathBuf,
        #[arg(long)]
        acp_sessions: PathBuf,
        #[arg(long)]
        projects: PathBuf,
    },
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("toolcall-extractor: {error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Extract(arguments) => extract(arguments),
        Command::Stream(arguments) => stream_source(&arguments),
        Command::Ingest(arguments) => ingest(arguments),
        Command::Stats(arguments) => {
            let path = database_path(arguments.database)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&Database::stats(&path)?)?
            );
            Ok(())
        }
        Command::Issues(arguments) => Database::print_issues(&database_path(arguments.database)?),
        Command::Verify(arguments) => {
            let path = database_path(arguments.database)?;
            let verification = Database::verify(&path)?;
            println!("{}", serde_json::to_string_pretty(&verification)?);
            if verification.is_valid() {
                Ok(())
            } else {
                Err(Error::InvalidSource(
                    "database verification failed".to_owned(),
                ))
            }
        }
        Command::BenchmarkYarp(arguments) => {
            let report = benchmark::run(&database_path(arguments.database)?)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::AnalyzeCeiling(arguments) => analyze_ceiling(arguments),
    }
}

fn analyze_ceiling(arguments: CeilingArgs) -> Result<()> {
    if arguments.summary_character_budget == 0 {
        return Err(Error::InvalidArguments(
            "summary character budget must be positive".to_owned(),
        ));
    }
    if arguments.minimum_savings_basis_points > 10_000 {
        return Err(Error::InvalidArguments(
            "minimum savings basis points must not exceed 10000".to_owned(),
        ));
    }
    let report = ceiling::run(
        &database_path(arguments.database)?,
        AnalysisOptions {
            summary_character_budget: arguments.summary_character_budget,
            minimum_removed_characters: arguments.minimum_removed_characters,
            minimum_savings_basis_points: arguments.minimum_savings_basis_points,
        },
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn extract(arguments: ExtractArgs) -> Result<()> {
    let path = database_path(arguments.database)?;
    let agent = arguments.source.agent();
    let mut database = Database::open(&path, &arguments.unix_user, agent)?;
    let result = run_source(&arguments.unix_user, &arguments.source, &mut database);
    let finish_result = database.finish(result.is_ok());
    result?;
    finish_result?;
    Ok(())
}

fn stream_source(arguments: &StreamArgs) -> Result<()> {
    let stdout = io::stdout();
    let mut sink = JsonlSink::new(stdout.lock());
    sink.start_stream()?;
    run_source(&arguments.unix_user, &arguments.source, &mut sink)?;
    sink.finish_stream()
}

fn ingest(arguments: IngestArgs) -> Result<()> {
    let path = database_path(arguments.database)?;
    let mut database = Database::open(&path, &arguments.unix_user, &arguments.agent)?;
    let result = stream::ingest(BufReader::new(io::stdin().lock()), &mut database);
    let finish_result = database.finish(result.is_ok());
    result?;
    finish_result?;
    Ok(())
}

fn run_source(unix_user: &str, source: &Source, sink: &mut impl Sink) -> Result<u64> {
    match source {
        Source::Pi { sessions } => adapters::pi::extract(unix_user, sessions, sink),
        Source::Codex { sessions, state_db } => {
            adapters::codex::extract(unix_user, sessions, state_db.as_deref(), sink)
        }
        Source::Claude { projects } => adapters::claude::extract(unix_user, projects, sink),
        Source::Cursor {
            chats,
            acp_sessions,
            projects,
        } => adapters::cursor::extract(unix_user, chats, acp_sessions, projects, sink),
    }
}

impl Source {
    const fn agent(&self) -> &'static str {
        match self {
            Self::Pi { .. } => "pi",
            Self::Codex { .. } => "codex",
            Self::Claude { .. } => "claude",
            Self::Cursor { .. } => "cursor",
        }
    }
}

fn database_path(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(path) if path.as_os_str().is_empty() => Err(Error::InvalidArguments(
            "database path cannot be empty".to_owned(),
        )),
        Some(path) => Ok(path),
        None => private_fs::default_database_path(),
    }
}
