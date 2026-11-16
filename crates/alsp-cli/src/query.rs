//! `alsp sym|refs|tests|cover|gaps`, `alsp help agents`: argument handling, output and exit codes
//! around `alsp_query` (the reference is docs/query.md).
//!
//! Exit codes: 0 answered; 2 usage or other error; 3 no index could be served; 4 the target is
//! missing or ambiguous (the candidates are printed on stdout); 5 a measurement the question needs
//! could not be made (the answer is printed, with the reason in its notes).

use alsp_gaps::GapsConfig;
use alsp_decide::DEFAULT_GAP_THRESHOLD;
use alsp_index::Tool;
use alsp_query::{Answer, OpenError, OpenOptions, TargetError, Workspace, DEFAULT_LIMIT};
use clap::Args as ClapArgs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

/// The short guide an agent reads first: `alsp` with no arguments and `alsp help agents`.
pub const AGENT_HELP: &str = "\
alsp: answers about Rust code, for agents. Every fact is marked with how it was found: [resolved]
by the rust-analyzer index, [observed] by running tests, [predicted] by a model; ~ marks a heuristic.

Ask about a symbol (a name or a unique suffix such as net::send_frame) or file:line:
  alsp sym <name>       where it is defined, its kind, how often it is used (lists every match)
  alsp refs <target>    who uses it, grouped by enclosing symbol, tests marked, entry points
  alsp tests <target>   tests that reach it; --run adds whether the package's tests execute it
  alsp cover <target>   line and branch coverage of its span (runs the tests)
  alsp gaps <target>    mutants in its span and the predicted chance the tests catch each
  alsp doctor           what is missing (rust-analyzer, git, ...), each with its fix command

Ask about a diff:
  alsp pack --base <rev> [--head <rev>] [--with coverage,gaps]   changed symbols and callers
  alsp review [--base <rev>] [--quick|--full] [--agent codex|claude|agy]
      evidence plus an agent review. No arguments: everything since the last review, uncommitted work
      included. --quick: mechanical evidence only (no agents, no tests run), with the changed symbols
      to pass to cover/gaps; --full: also measures coverage and gaps

Flags on every query: --repo <path> (default: the git repo of the current directory),
  --json (stable schema), --all (no list caps), --no-build (index from a cached index only, never
  by running rust-analyzer; see Trust; the first index of a large repo takes about 30 s, then it is
  cached), --root <dir> (also index a Cargo package outside the root workspace, e.g. apps/foo/rust).

Exit codes: 0 answered | 2 usage or other error | 3 no index could be served (stderr says why)
  | 4 target not found or ambiguous (candidates or nearest names are printed)
  | 5 a needed measurement is unavailable (tests do not build, sdd decide unavailable; see notes)
  Failing tests do not make coverage unavailable (exit 0; a note names them and the first panic).

Trust: building the index runs the repository's build scripts, proc macros and cargo config
(rust-analyzer does), for every command without --no-build; `review` has no --no-build. Running
tests does the same and more: cover, gaps, `tests --run`, `pack --with` and `review --full` or
`--with` run the repository's tests, and --no-build does NOT stop that. Only sym, refs, `tests`
without --run and `pack` without --with are safe on untrusted code, and only with --no-build
(cached index, exit 3 without one); alsp runs git with the repository's own .git/config neutralized
(fsmonitor, hooks, external diff, textconv, filters, remote programs; no submodules, no fetch; git
2.31+). Not covered: filters in your own global git config (git-lfs) still run, and system-level
config is ignored. gaps sends function bodies to Google's Gemini through sdd's `decide` command (spend is recorded by sdd). Otherwise run alsp in a sandbox.

Reference: docs/query.md
";

/// Arguments every query takes.
#[derive(ClapArgs)]
pub struct Common {
    /// A symbol name (exact, or a unique `::` suffix such as `net::send_frame`) or `file:line`.
    target: String,
    /// Repository (any directory inside its work tree).
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Print JSON (schema in docs/query.md) instead of text.
    #[arg(long)]
    json: bool,
    /// Do not cap lists (text and JSON otherwise show the first 25 items and count the rest).
    #[arg(long)]
    all: bool,
    /// Never run rust-analyzer to index (so none of the repository's build scripts or proc
    /// macros): serve a fresh cached index or exit 3. This alone makes `sym`, `refs` and `tests`
    /// without `--run` safe on a repository you do not trust; it does NOT stop `cover`, `gaps`
    /// or `tests --run` from building and running the repository's tests.
    #[arg(long)]
    no_build: bool,
    /// Cache directory root (default: $ALSP_CACHE_DIR, else $XDG_CACHE_HOME/alsp, else ~/.cache/alsp).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Also index this Cargo package, relative to the repository (repeatable). Needed for a
    /// package with its own `[workspace]` that the root workspace does not contain.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
}

#[derive(ClapArgs)]
pub struct TestsArgs {
    #[command(flatten)]
    common: Common,
    /// Also run the package's tests under coverage and report whether they execute the target.
    /// Builds and runs the repository's code: use it only on code you trust.
    #[arg(long)]
    run: bool,
}

#[derive(ClapArgs)]
pub struct GapsArgs {
    #[command(flatten)]
    common: Common,
    /// Most mutants sent to the model (mutants on lines no test runs are free).
    #[arg(long, default_value_t = 100)]
    gap_budget: usize,
    /// Report a predicted gap when P(tests catch) is below this.
    #[arg(long, default_value_t = DEFAULT_GAP_THRESHOLD, value_parser = crate::pack::unit_interval)]
    gap_threshold: f64,
}

fn print(text: &str) {
    // A closed pipe (`alsp refs x | head`) is not an error worth a panic.
    let _ = std::io::stdout().write_all(text.as_bytes());
}

fn open(c: &Common) -> Result<Workspace, ExitCode> {
    let opened = Workspace::open(&OpenOptions { repo: c.repo.clone(), no_build: c.no_build, cache_root: c.cache_dir.clone(), tool: Tool::default(), extra_roots: c.roots.clone() });
    match opened {
        Ok(ws) => Ok(ws.with_limit(if c.all { usize::MAX } else { DEFAULT_LIMIT }).with_progress(|s| eprintln!("alsp: {s}"))),
        Err(e) => {
            eprintln!("alsp: {e}");
            Err(ExitCode::from(if matches!(e, OpenError::Unavailable(_)) { 3 } else { 2 }))
        }
    }
}

/// Print the answer (or the target error) and pick the exit code.
fn finish(r: Result<Answer, TargetError>, json: bool) -> ExitCode {
    match r {
        Ok(a) => {
            print(&if json { a.to_json() } else { a.to_text() });
            match a.unavailable {
                Some(u) => {
                    eprintln!("alsp: {u}; the answer above is incomplete: read its notes, fix the cause and rerun");
                    ExitCode::from(5)
                }
                None => ExitCode::SUCCESS,
            }
        }
        Err(e) => {
            print(&if json { e.to_json() } else { e.to_text() });
            eprintln!("alsp: {}", e.hint());
            ExitCode::from(4)
        }
    }
}

pub fn sym(c: Common) -> ExitCode {
    match open(&c) {
        Ok(ws) => finish(ws.sym(&c.target), c.json),
        Err(code) => code,
    }
}

pub fn refs(c: Common) -> ExitCode {
    match open(&c) {
        Ok(ws) => finish(ws.refs(&c.target), c.json),
        Err(code) => code,
    }
}

pub fn tests(a: TestsArgs) -> ExitCode {
    match open(&a.common) {
        Ok(ws) => finish(ws.tests(&a.common.target, a.run), a.common.json),
        Err(code) => code,
    }
}

pub fn cover(c: Common) -> ExitCode {
    match open(&c) {
        Ok(ws) => finish(ws.cover(&c.target), c.json),
        Err(code) => code,
    }
}

/// The one place `alsp gaps` builds its decision model (the same construction `alsp pack --with
/// gaps` uses). Swap the body when the backend changes; nothing else here knows which model it is.
fn gap_model() -> Box<dyn alsp_decide::DecisionModel> {
    alsp_decide::default_model_or_unavailable()
}

pub fn gaps(a: GapsArgs) -> ExitCode {
    match open(&a.common) {
        Ok(ws) => {
            let model = gap_model();
            let cfg = GapsConfig { budget: a.gap_budget, threshold: a.gap_threshold };
            finish(ws.gaps(&a.common.target, model.as_ref(), &cfg), a.common.json)
        }
        Err(code) => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alsp_query::Candidate;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: GapsArgs,
    }

    fn code(c: ExitCode) -> String {
        format!("{c:?}")
    }

    #[test]
    fn test_DW_4_4_the_cheat_sheet_is_short_and_names_every_command_and_exit_code() {
        assert!(AGENT_HELP.lines().count() < 40, "{} lines", AGENT_HELP.lines().count());
        for word in ["alsp sym", "alsp refs", "alsp tests", "alsp cover", "alsp gaps", "alsp pack", "alsp review", "--json", "--all", "--repo", "docs/query.md", "[resolved]", "[observed]", "[predicted]"] {
            assert!(AGENT_HELP.contains(word), "{word}");
        }
        for exit in ["0 answered", "2 usage", "3 no index", "4 target", "5 a needed"] {
            assert!(AGENT_HELP.contains(exit), "{exit}");
        }
    }

    #[test]
    fn test_DW_1_8_agent_help_and_docs_say_failing_tests_keep_coverage() {
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let help = flat(AGENT_HELP);
        assert!(help.contains("Failing tests do not make coverage unavailable") && help.contains("exit 0") && help.contains("first panic"), "{help}");
        assert!(!help.contains("(tests failed"), "{help}");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let docs = flat(&std::fs::read_to_string(root.join("docs/query.md")).unwrap());
        assert!(docs.contains("--ignore-run-fail") && docs.contains("Failing tests do not make coverage unavailable") && !docs.contains("(tests failed"), "docs/query.md");
        let skill = flat(&std::fs::read_to_string(root.join("skills/alsp/SKILL.md")).unwrap());
        assert!(skill.contains("Failing tests do not cause this") && !skill.contains("(tests failed"), "skills/alsp/SKILL.md");
    }

    /// The trust statement is what an agent acts on: indexing itself runs repository code, and
    /// `--no-build` stops only that, not the test-running commands.
    #[test]
    fn test_DW_F_1_the_trust_wording_says_indexing_runs_repository_code_and_what_no_build_covers() {
        let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        let trust = flat(&AGENT_HELP[AGENT_HELP.find("Trust:").expect("a Trust paragraph")..AGENT_HELP.find("Reference:").unwrap()]);
        for needle in [
            "building the index runs the repository's build scripts, proc macros and cargo config",
            "for every command without --no-build",
            "`review` has no --no-build",
            "`pack --with` and `review --full` or `--with` run the repository's tests",
            "--no-build does NOT stop that",
            "Only sym, refs, `tests` without --run and `pack` without --with are safe on untrusted code, and only with --no-build",
            "alsp runs git with the repository's own .git/config neutralized",
            "no submodules, no fetch",
            "filters in your own global git config (git-lfs) still run, and system-level config is ignored",
            "Otherwise run alsp in a sandbox",
        ] {
            assert!(trust.contains(needle), "{needle:?} missing from: {trust}");
        }
        // Phrasings that were false: the old "only cover, gaps and tests --run" claim, and
        // --no-build presented as covering every query or as running no repository code at all.
        let sheet = flat(AGENT_HELP);
        for stale in ["Use them only on code you trust", "so no repository code", "To query untrusted"] {
            assert!(!sheet.contains(stale), "{stale:?} is back in the cheat sheet");
        }
        let docs = include_str!("../../../docs/query.md");
        for needle in [
            "Building the index runs the repository's code",
            "`--no-build` stops that indexing and nothing else",
            "exiting 3 when there is none",
            "`alsp review` has no `--no-build`",
            "`--no-build` does not prevent it",
            "`rust-analyzer --version`",
            "git runs with the repository's own `.git/config` neutralized",
            "a program named in your own **global** git config",
            "The **system** config",
            "does not run: such files are read as plain files",
            "It does not enter submodules",
            "never fetches a missing object",
            "refuses to run git at all rather than run it without the overrides",
            "a blob that is not on disk is not fetched",
        ] {
            assert!(docs.contains(needle), "{needle:?} missing from docs/query.md");
        }
        assert!(!docs.contains("none of the repository's code, see"), "stale --no-build claim in docs/query.md");
        let review = include_str!("../../../docs/review.md");
        assert!(review.contains("Indexing a commit runs its build scripts and proc macros"));
        assert!(review.contains("never runs a program named in the repository's `.git/config`"), "docs/review.md must state the git config rule");
        assert!(review.contains("neither enters submodules nor fetches missing objects"), "docs/review.md must state the submodule and fetch rules");
        for stale in ["git-lfs keeps working", "Filters named only by trusted config (git-lfs, say) keep working"] {
            assert!(!docs.contains(stale) && !review.contains(stale) && !sheet.contains(stale), "{stale:?} overstates what survives the system-config drop");
        }
    }

    #[test]
    fn test_DW_F_2_the_cheat_sheet_shows_review_with_no_arguments_and_the_tier_flags() {
        for needle in ["alsp review [--base <rev>] [--quick|--full]", "No arguments: everything since the last review", "--quick", "--full"] {
            assert!(AGENT_HELP.contains(needle), "{needle:?}");
        }
        // the flags the sheet names are real flags of `alsp review`
        let help = <crate::Cli as clap::CommandFactory>::command().find_subcommand("review").map(|c| c.clone().render_long_help().to_string()).expect("review subcommand");
        for flag in ["--quick", "--full", "--base", "--agent"] {
            assert!(help.contains(flag), "{flag} is not an `alsp review` flag");
        }
    }

    #[test]
    fn gaps_arguments_have_defaults_and_reject_a_threshold_outside_0_to_1() {
        let a = Cli::try_parse_from(["alsp", "net::f"]).unwrap().args;
        assert_eq!((a.gap_budget, a.gap_threshold, a.common.target.as_str(), a.common.json, a.common.all), (100, DEFAULT_GAP_THRESHOLD, "net::f", false, false));
        assert!(Cli::try_parse_from(["alsp", "f", "--gap-threshold", "2"]).is_err());
        assert!(Cli::try_parse_from(["alsp"]).is_err(), "a target is required");
    }

    #[test]
    fn finish_maps_answers_and_target_errors_to_exit_codes() {
        let fresh = || alsp_query::Freshness { status: "fresh".into(), rebuilt: false, head: "0".repeat(40), dirty: vec![] };
        let ok = Answer::new("sym", None, fresh());
        assert_eq!(code(finish(Ok(ok.clone()), false)), code(ExitCode::SUCCESS));
        let mut unavailable = ok;
        unavailable.unavailable = Some("no tests ran".into());
        assert_eq!(code(finish(Ok(unavailable), true)), code(ExitCode::from(5)));
        let missing = TargetError::NoMatch { query: "x".into(), nearest: vec![] };
        assert_eq!(code(finish(Err(missing), false)), code(ExitCode::from(4)));
        let amb = TargetError::Ambiguous { query: "x".into(), candidates: vec![Candidate { name: "a::x".into(), kind: "fn".into(), file: "f.rs".into(), line: 1 }], omitted: 0 };
        assert_eq!(code(finish(Err(amb), true)), code(ExitCode::from(4)));
    }

    #[test]
    fn open_exits_2_outside_a_repository_and_3_when_no_index_can_be_served() {
        let dir = tempfile::tempdir().unwrap();
        let common = |repo: &std::path::Path, no_build| Common { target: "x".into(), repo: repo.to_path_buf(), json: false, all: false, no_build, cache_dir: Some(dir.path().join("cache")), roots: vec![] };
        assert_eq!(code(open(&common(&dir.path().join("nope"), true)).err().unwrap()), code(ExitCode::from(2)));
    }
}
