#![cfg_attr(test, allow(non_snake_case))]
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

mod doctor;
mod outline;
mod pack;
mod query;
mod review;
mod settings;

#[derive(Parser)]
#[command(name = "alsp", version, about = "Evidence layer for code review agents; `alsp` alone prints the agent cheat sheet", disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Evidence pack for a git diff: changed symbols, type-resolved callers, entry points.
    /// Unmarked facts are resolved by the index; `~` marks heuristic ones. Exit 3: no index could
    /// be served (stale under --no-build, or every Cargo root failed), so an empty list means unknown.
    Pack(pack::Args),
    /// Where a symbol is defined, its kind and reference count; several matches are all listed.
    Sym(query::Common),
    /// Type-resolved references to a symbol or line, grouped by enclosing symbol, with entry points.
    Refs(query::Common),
    /// Tests that reach a symbol or line; `--run` adds whether the package's tests execute it.
    Tests(query::TestsArgs),
    /// Observed line and branch coverage of a symbol's span (runs the repository's tests).
    Cover(query::Common),
    /// Mutants in a symbol's span and the predicted chance the tests catch each (runs tests; sends code to Gemini).
    Gaps(query::GapsArgs),
    /// Batched edit/impact brief for one or more targets: declaration, collisions, callers, links, tests.
    Prep(query::PrepArgs),
    /// Instant structural skeleton of Rust files, directories, or a dependency crate (`--dep`).
    Outline(outline::Args),
    /// Help: `alsp help agents` prints the agent cheat sheet, `alsp help <command>` a command's help.
    Help { topic: Vec<String> },
    /// Review a diff with mechanical evidence and optional read-only agents.
    Review(review::Args),
    /// Check that everything alsp needs is installed and print the fix for what is not (changes nothing, spends nothing). Exit 1: a required tool is missing.
    Doctor(doctor::Args),
    /// Append a verdict or inspect the review ledger.
    Ledger { #[command(subcommand)] command: LedgerCommand },
}

#[derive(Subcommand)]
enum LedgerCommand {
    Mark { id: String, verdict: String, #[arg(long, default_value = ".")] repo: PathBuf },
    Stats { #[arg(long, default_value = ".")] repo: PathBuf },
    List { #[arg(long, default_value = ".")] repo: PathBuf, #[arg(long)] source: Option<String>, #[arg(long)] unmarked: bool },
    ImportGaps { file: PathBuf, #[arg(long, default_value = ".")] repo: PathBuf },
}

fn help(topic: &[String]) -> ExitCode {
    let mut cmd = Cli::command();
    match topic.first().map(String::as_str) {
        None => { let _ = cmd.print_help(); println!(); ExitCode::SUCCESS }
        Some("agents") => { print!("{}", query::AGENT_HELP); ExitCode::SUCCESS }
        Some(name) => match cmd.find_subcommand_mut(name) {
            Some(sub) => { let _ = sub.print_help(); println!(); ExitCode::SUCCESS }
            None => fail(&format!("no help topic `{name}`: try `alsp help agents` or `alsp --help`")),
        },
    }
}

fn main() -> ExitCode {
    let Some(command) = Cli::parse().command else {
        print!("{}", query::AGENT_HELP);
        return ExitCode::SUCCESS;
    };
    match command {
        Command::Sym(args) => query::sym(args),
        Command::Refs(args) => query::refs(args),
        Command::Tests(args) => query::tests(args),
        Command::Cover(args) => query::cover(args),
        Command::Gaps(args) => query::gaps(args),
        Command::Prep(args) => query::prep(args),
        Command::Outline(args) => outline::run(args),
        Command::Help { topic } => help(&topic),
        Command::Pack(args) => pack::run(args),
        Command::Review(args) => review::run(args),
        Command::Doctor(args) => doctor::run(args),
        Command::Ledger { command } => match command {
            LedgerCommand::Mark { id, verdict, repo } if verdict == "held" || verdict == "not-held" => match alsp_review::mark(&repo, &id, &verdict) {
                Ok(0) => fail("finding id not found"), Ok(n) => { println!("marked {n} ledger row(s) {verdict}"); ExitCode::SUCCESS }, Err(e) => fail(&e.to_string()),
            },
            LedgerCommand::Mark { .. } => fail("verdict must be held or not-held"),
            LedgerCommand::Stats { repo } => match alsp_review::stats(&repo) {
                Ok(s) => { println!("source\theld\tnot-held\tunmarked\ttotal\theld-rate"); for (source,(held,not_held,unmarked)) in s { let marked=held+not_held; let rate=if marked==0{"—".into()}else{format!("{held}/{marked}")}; println!("{}\t{held}\t{not_held}\t{unmarked}\t{}\t{rate}",alsp_evidence::escape(&source),held+not_held+unmarked); } ExitCode::SUCCESS }, Err(e) => fail(&e.to_string()),
            },
            LedgerCommand::List { repo, source, unmarked } => match alsp_review::list_findings(&repo,source.as_deref(),unmarked) {
                Ok(rows) => { for r in rows { println!("{} {} {}:{} [{}] {}",alsp_evidence::escape(&r.id),alsp_evidence::escape(&r.source),alsp_evidence::escape(&r.file),r.line,alsp_evidence::escape(&alsp_review::normalize_severity(&r.severity)),alsp_review::cap_text(&r.text,180));} ExitCode::SUCCESS }, Err(e) => fail(&e.to_string()),
            },
            LedgerCommand::ImportGaps { file, repo } => match review::import(file,repo) { Ok(n) => { println!("imported {n} gap verdicts"); ExitCode::SUCCESS }, Err(e) => fail(&e) },
        },
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("alsp: {msg}");
    ExitCode::from(2)
}
