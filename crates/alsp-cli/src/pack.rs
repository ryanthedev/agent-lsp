//! `alsp pack`: argument handling and exit codes around `alsp_evidence::build_pack`.

use alsp_coverage::{observe, CoverageTool, Observed};
use alsp_decide::{default_model_or_unavailable, DEFAULT_GAP_THRESHOLD};
use alsp_evidence::{build_pack_with_source, EvidencePack, PackRequest, PackSource};
use alsp_gaps::{predict, GapsConfig, MutantsTool};
use alsp_index::{BuildPolicy, Tool};
use clap::{Args as ClapArgs, ValueEnum};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Text,
    Json,
}

/// Evidence beyond the symbols, callers and entry points.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum With {
    /// Observed line and branch coverage of each changed symbol (`cargo llvm-cov`, cached).
    Coverage,
    /// Predicted test gaps (`cargo mutants --list`, nothing executed; scored by Gemini through sdd). Measures
    /// coverage too, to find the mutants no test runs.
    Gaps,
}

#[derive(ClapArgs)]
pub struct Args {
    /// Repository (any directory inside its work tree).
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Base revision of the diff.
    #[arg(long)]
    base: String,
    /// Head revision of the diff. When it is not the checked-out commit, its tree is exported to
    /// the cache and indexed there.
    #[arg(long, default_value = "HEAD")]
    head: String,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// Never run rust-analyzer to index (so none of the repository's build scripts or proc
    /// macros): serve a fresh cached index or nothing (exit 3). Without it, indexing runs that
    /// code. It does not stop `--with` from running the repository's tests.
    #[arg(long)]
    no_build: bool,
    /// Extra evidence, comma separated: coverage, gaps. Both run the repository's tests
    /// (indexing already runs its build scripts and proc macros), with or without `--no-build`:
    /// point alsp only at code you trust. `gaps` sends function bodies to Google's Gemini API,
    /// through sdd's `decide` command (alsp holds no key).
    #[arg(long, value_enum, value_delimiter = ',')]
    with: Vec<With>,
    /// Most mutants sent to the model per review (mutants on uncovered lines are free).
    #[arg(long, default_value_t = 100)]
    gap_budget: usize,
    /// Report a predicted gap when P(tests catch) is below this.
    #[arg(long, default_value_t = DEFAULT_GAP_THRESHOLD, value_parser = unit_interval)]
    gap_threshold: f64,
    /// Cache directory root (default: $ALSP_CACHE_DIR, else $XDG_CACHE_HOME/alsp, else ~/.cache/alsp).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
}

pub(crate) fn unit_interval(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if v.is_finite() && (0.0..=1.0).contains(&v) { Ok(v) } else { Err(format!("{s} is not between 0 and 1")) }
}

/// Progress for a person: what ran, how long, what it cost. On stderr so stdout stays the pack.
fn report_coverage(obs: &Observed) {
    for r in obs.runs() {
        match &r.error {
            None => eprintln!("alsp: coverage {}: {} in {:.1}s", r.package, if r.cached { "cached" } else { "measured" }, r.millis as f64 / 1000.0),
            Some(_) => eprintln!("alsp: coverage {}: unavailable after {:.1}s (see the pack's notes)", r.package, r.millis as f64 / 1000.0),
        }
    }
}

fn enrich(pack: &mut EvidencePack, src: &PackSource, a: &Args) {
    let coverage = a.with.contains(&With::Coverage);
    let gaps = a.with.contains(&With::Gaps);
    if !(coverage || gaps) || pack.symbols.is_empty() {
        return;
    }
    let obs = observe(pack, src, &CoverageTool { notify: |m| eprintln!("alsp: {m}"), ..Default::default() });
    report_coverage(&obs);
    if coverage {
        obs.attach(pack, src);
    } else {
        pack.notes.extend(obs.notes().iter().cloned());
    }
    if gaps {
        let m = default_model_or_unavailable();
        let cfg = GapsConfig { budget: a.gap_budget, threshold: a.gap_threshold };
        let r = predict(pack, src, &obs, m.as_ref(), &MutantsTool::default(), &cfg);
        eprintln!(
            "alsp: gaps: {} mutants listed in {:.1}s, {} uncovered (free), {} sent to the model, {} dropped over budget; {} input tokens, ${:.4}",
            r.listed,
            r.list_millis as f64 / 1000.0,
            r.uncovered,
            r.scored + r.unscored,
            r.dropped,
            r.input_tokens,
            r.cost_usd
        );
    }
}

/// Build the same enriched evidence pack used by `alsp pack`, for review orchestration.
pub fn build_for_review(repo: PathBuf, base: String, head: String, with: Vec<With>) -> Result<(EvidencePack, PackSource), String> {
    let req = PackRequest { repo, base, head, policy: BuildPolicy::Build, cache_root: None, tool: Tool::default() };
    let (mut pack, src) = build_pack_with_source(&req).map_err(|e| e.to_string())?;
    let args = Args { repo: PathBuf::new(), base: pack.base.clone(), head: pack.head.clone(), format: Format::Text,
        no_build: false, with, gap_budget: 100, gap_threshold: DEFAULT_GAP_THRESHOLD, cache_dir: None };
    if pack.facts_served() { enrich(&mut pack, &src, &args); }
    Ok((pack, src))
}

pub fn run(a: Args) -> ExitCode {
    let req = PackRequest {
        repo: a.repo.clone(),
        base: a.base.clone(),
        head: a.head.clone(),
        policy: if a.no_build { BuildPolicy::NoBuild } else { BuildPolicy::Build },
        cache_root: a.cache_dir.clone(),
        tool: Tool::default(),
    };
    let (mut pack, src) = match build_pack_with_source(&req) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("alsp: {e}");
            return ExitCode::from(2);
        }
    };
    if pack.facts_served() {
        enrich(&mut pack, &src, &a);
    }
    let out = match a.format {
        Format::Text => pack.to_text(),
        Format::Json => pack.to_json() + "\n",
    };
    // A closed pipe (`alsp pack | head`) is not an error worth a panic.
    let _ = std::io::stdout().write_all(out.as_bytes());
    if pack.facts_served() {
        ExitCode::SUCCESS
    } else {
        eprintln!("alsp: no index could be served; the empty symbol list means unknown, not unchanged");
        ExitCode::from(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alsp_evidence::PackSource;
    use alsp_index::{IndexState, IndexStatus, UnitReport};
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn args(extra: &[&str]) -> Args {
        Cli::try_parse_from(["alsp", "--base", "HEAD~1"].iter().chain(extra)).unwrap().args
    }

    fn empty_pack() -> EvidencePack {
        EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: IndexState { status: IndexStatus::Fresh, rebuilt: false, units: vec![UnitReport { path: ".".into(), ok: true, detail: None }], millis: 0, notes: vec![] },
            symbols: vec![],
            notes: vec![],
        }
    }

    fn source() -> PackSource {
        PackSource { tree: PathBuf::new(), cache_dir: PathBuf::new(), build_env: vec![], changed_lines: Default::default() }
    }

    #[test]
    fn unit_interval_accepts_the_closed_unit_range_only() {
        assert_eq!(unit_interval("0"), Ok(0.0));
        assert_eq!(unit_interval("1"), Ok(1.0));
        assert_eq!(unit_interval("0.35"), Ok(0.35));
        assert!(unit_interval("-0.1").unwrap_err().contains("between 0 and 1"));
        assert!(unit_interval("1.5").is_err());
        assert!(unit_interval("abc").unwrap_err().contains("not a number"));
        assert!(unit_interval("nan").is_err());
        assert!(unit_interval("inf").is_err());
    }

    #[test]
    fn report_coverage_handles_an_observation_with_no_runs() {
        report_coverage(&Observed::default());
    }

    #[test]
    fn enrich_does_nothing_without_with_or_without_symbols() {
        let mut p = empty_pack();
        enrich(&mut p, &source(), &args(&[]));
        assert!(p.notes.is_empty() && p.symbols.is_empty());
        // evidence requested but no symbols: returns before measuring anything
        enrich(&mut p, &source(), &args(&["--with", "coverage,gaps"]));
        assert!(p.notes.is_empty() && p.symbols.is_empty());
    }

    #[test]
    fn args_parse_defaults_and_reject_bad_values() {
        let a = args(&[]);
        assert_eq!((a.head.as_str(), a.gap_budget, a.no_build), ("HEAD", 100, false));
        assert_eq!(a.gap_threshold, DEFAULT_GAP_THRESHOLD);
        assert!(a.with.is_empty());
        assert!(Cli::try_parse_from(["alsp", "--base", "x", "--gap-threshold", "2"]).is_err());
        assert!(Cli::try_parse_from(["alsp", "--base", "x", "--with", "bogus"]).is_err());
    }

    #[test]
    fn run_exits_2_when_the_repository_cannot_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let a = args(&["--repo", missing.to_str().unwrap(), "--cache-dir", dir.path().to_str().unwrap()]);
        assert_eq!(format!("{:?}", run(a)), format!("{:?}", ExitCode::from(2)));
    }
}
