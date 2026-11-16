mod range;

use crate::pack::{self, With};
use alsp_evidence::Git;
use alsp_review::{allowed_locations, ledger_review, record_review, Ledgered, ReviewRecord, compose_prompt, import_gaps, mechanical_findings, merge_agent_findings, merge_findings, prove_agent_findings, render_report, run_agents, Agent, Finding};
use clap::{Args as ClapArgs, ValueEnum};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Agents can take several minutes on a multi-file diff.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

#[derive(Clone, Copy, ValueEnum)]
pub enum Format { Text, Json }

#[derive(ClapArgs)]
pub struct Args {
    #[arg(long, default_value = ".")] repo: PathBuf,
    /// Revision the diff starts from. Default: the end of this branch's last review, else the merge-base
    /// with the default branch (origin/HEAD, main, master).
    #[arg(long)] base: Option<String>,
    /// Revision the diff ends at. Default: HEAD plus uncommitted work. Giving it reviews that commit only.
    #[arg(long)] head: Option<String>,
    /// Review commits only; ignore uncommitted changes (and untracked files).
    #[arg(long)] committed: bool,
    /// Also review untracked, non-ignored files (never ones named like secrets, nor over 1 MB). Off by default:
    /// everything reviewed is sent to the agents.
    #[arg(long)] untracked: bool,
    /// Mechanical evidence only: no agents, no coverage or gaps (seconds, free).
    #[arg(long, conflicts_with = "full")] quick: bool,
    /// Also measure coverage and predict test gaps (runs tests; Gemini cost through sdd).
    #[arg(long)] full: bool,
    #[arg(long, value_enum)] agent: Vec<AgentName>,
    #[arg(long)] print: bool,
    #[arg(long, value_delimiter = ',') ] with: Vec<With>,
    #[arg(long, value_enum, default_value_t = Format::Text)] format: Format,
    #[arg(long)] prompt: Option<PathBuf>,
    /// Seconds each agent may run before it is killed and reported as timed out.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)] timeout: u64,
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentName { Codex, Claude, Agy }
impl From<AgentName> for Agent { fn from(a: AgentName)->Self { match a {AgentName::Codex=>Agent::Codex,AgentName::Claude=>Agent::Claude,AgentName::Agy=>Agent::Agy} } }

pub fn git_diff(repo: &std::path::Path, base: &str, head: &str) -> Result<String,String> {
    let out=alsp_evidence::git_command(repo)?.arg("diff").arg("--no-ext-diff").arg("--no-textconv").arg("--no-renames").arg(base).arg(head).arg("--").output().map_err(|e|e.to_string())?;
    if !out.status.success() { return Err(String::from_utf8_lossy(&out.stderr).into_owned()); }
    String::from_utf8(out.stdout).map_err(|e|e.to_string())
}

fn base_file(repo: &std::path::Path, base: &str, file: &str) -> Option<String> {
    let out=alsp_evidence::git_command(repo).ok()?.arg("show").arg("--no-ext-diff").arg("--no-textconv").arg(format!("{base}:{file}")).output().ok()?;
    out.status.success().then(||String::from_utf8(out.stdout).ok()).flatten()
}

/// Agents named in `.alsp/enabled` (hand-written; no command writes it); none when the file is absent or unusable.
fn configured_agents(repo: &std::path::Path) -> Vec<Agent> {
    let root = Git::open(repo).map(|g| g.root).unwrap_or_else(|_| repo.to_path_buf());
    match crate::settings::read_enabled(&root) {
        Ok(s) => s.agent.unwrap_or_default().iter().filter_map(|n| match n.as_str() { "claude" => Some(Agent::Claude), "codex" => Some(Agent::Codex), "agy" => Some(Agent::Agy), _ => None }).collect(),
        Err(e) => { eprintln!("alsp: ignoring review settings: {e}"); vec![] }
    }
}

pub fn run(a: Args) -> ExitCode {
    let fail = |msg: &str| { eprintln!("alsp: {msg}"); ExitCode::from(2) };
    if let Some(p)=&a.prompt { if let Err(e)=std::fs::read_to_string(p) {return fail(&format!("cannot read --prompt {}: {e}",p.display()))} }
    let git = match Git::open(&a.repo) { Ok(g)=>g, Err(e)=>return fail(&e) };
    let range = match range::resolve(&git, a.base.as_deref(), a.head.as_deref(), a.committed, a.untracked) { Ok(r)=>r, Err(e)=>return fail(&e) };
    // Tiers set the defaults; explicit --agent / --with override them.
    let tier = if a.quick {"quick"} else if a.full {"full"} else {"default"};
    let agents: Vec<Agent> = if !a.agent.is_empty() { a.agent.iter().copied().map(Into::into).collect() } else if a.quick { vec![] } else { configured_agents(&a.repo) };
    let with = if !a.with.is_empty() { a.with.clone() } else if a.full { vec![With::Coverage, With::Gaps] } else { vec![] };
    let agent_names: Vec<String> = agents.iter().map(|x| x.name().to_string()).collect();
    // On stderr, so stdout stays exactly the report (or the prompt, or JSON); on a terminal it is still the first line.
    eprintln!("{}", range::banner(&range, tier, &agent_names, with.contains(&With::Coverage), with.contains(&With::Gaps)));
    for skipped in &range.untracked_skipped { eprintln!("alsp: skipped untracked {skipped}"); }
    let record_of = |head: &str| (!a.print && range.record).then(|| ReviewRecord { branch: range.branch.clone(), head: range.checked_out.clone(), reviewed: head.to_string(), uncommitted: range.uncommitted, time: String::new() });
    let warn_unrecorded = |e: std::io::Error| eprintln!("alsp: cannot record this review ({e}); the next ad-hoc review will start from an earlier point");
    let record = |head: &str| if let Some(rec) = record_of(head) { if let Err(e) = record_review(&git.root, rec) { warn_unrecorded(e); } };
    // An ad-hoc range (no --base) that is empty ends here. An explicit --base keeps the full, empty report.
    match range.is_empty(&git) {
        Err(e) => return fail(&e),
        Ok(true) if a.base.is_none() => {
            let line = match a.format { Format::Json if !a.print => serde_json::json!({"base":range.base,"head":range.head,"findings":[],"nothing_to_review":true}).to_string(), _ => "nothing to review".into() };
            if a.print { eprintln!("{line}"); } else { let _ = writeln!(std::io::stdout(), "{line}"); }
            record(&range.head);
            return ExitCode::SUCCESS;
        }
        Ok(_) => {}
    }
    let (pack,src)=match pack::build_for_review(a.repo.clone(),range.base.clone(),range.head.clone(),with) {Ok(x)=>x,Err(e)=>return fail(&e)};
    let diff=match git_diff(&a.repo,&pack.base,&pack.head) {Ok(x)=>x,Err(e)=>return fail(&e)};
    let pack_text=pack.to_text();
    if a.print {
        let prompt=compose_prompt(&pack_text,&diff,"all changed crates",a.prompt.as_deref());
        let _=std::io::stdout().write_all(prompt.as_bytes()); return ExitCode::SUCCESS;
    }
    let base_fn=|file:&str|base_file(&a.repo,&pack.base,file);
    let mut findings=mechanical_findings(&pack,&src,base_fn);
    let allowed=allowed_locations(&pack,&src.changed_lines);
    let mut skipped=Vec::new(); let mut unparsed=Vec::new(); let mut runs=Vec::new(); let mut dropped=0; let mut split=0;
    let served = pack.facts_served();
    // Without an index the pack is empty, and an agent's findings could not be proved against it.
    if !served { eprintln!("alsp: no index could be served; the changed symbols are unknown (see the Index line), no agents were run, and nothing was ledgered"); }
    if served && !agents.is_empty() {
        let r=run_agents(&agents,&a.repo,&pack.head,a.prompt.as_deref(),&pack_text,&diff,Duration::from_secs(a.timeout));
        let proved=prove_agent_findings(r.findings,&allowed); dropped=proved.dropped;
        let mut symbols=std::collections::BTreeMap::new();for s in &pack.symbols{symbols.entry(s.file.clone()).or_insert_with(Vec::new).push((s.span.start,s.span.end,s.name.clone()));}
        findings.extend(merge_agent_findings(proved.kept,&symbols)); skipped=r.skipped; unparsed=r.unparsed; runs=r.runs; split=r.split_crates;
        for run in runs.iter().filter(|run|run.unparsed) { let text=unparsed.iter().find(|text|text.starts_with(&run.agent)).cloned().unwrap_or_else(||format!("{} produced no parseable result",run.agent)); findings.push(Finding::new(format!("unparsed:{}",run.agent),"(agent output)",1,"medium",text)); }
    }
    findings=merge_findings(findings);
    match ledger_review(&a.repo,&pack,&findings,record_of(&pack.head)) {
        Err(e)=>return fail(&format!("cannot append review ledger: {e}")),
        Ok(Ledgered::Written{record_error:Some(e)})=>warn_unrecorded(e),
        Ok(_)=>{}
    }
    let (changed_symbols,changed_symbols_more)=alsp_review::changed_symbols_json(&pack);
    let report=match a.format {
        Format::Text=>render_report(&pack,&findings,dropped,&skipped,&runs,split),
        Format::Json=>serde_json::json!({"base":pack.base,"head":pack.head,"base_reason":range.base_reason,"uncommitted":range.uncommitted,"changed_symbols":changed_symbols,"changed_symbols_more":changed_symbols_more,"findings":findings,"agents_skipped":skipped,"agent_runs":runs,"dropped_by_proof":dropped,"unparsed":unparsed,"split_crates":split,"index":pack.index,"index_served":served,"notes":pack.notes}).to_string()+"\n",
    };
    let _=std::io::stdout().write_all(report.as_bytes());
    // 3, as for `pack`: the symbol list is unknown, not empty.
    if served { ExitCode::SUCCESS } else { ExitCode::from(3) }
}

pub fn import(file: PathBuf, repo: PathBuf)->Result<usize,String>{import_gaps(&repo,&file)}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrap { #[command(flatten)] args: Args }

    #[test]
    fn test_default_agent_timeout_is_600_seconds_and_overridable() {
        let d=Wrap::parse_from(["x","--base","b"]); assert_eq!(d.args.timeout,600);
        let o=Wrap::parse_from(["x","--base","b","--timeout","7"]); assert_eq!(o.args.timeout,7);
    }
}
