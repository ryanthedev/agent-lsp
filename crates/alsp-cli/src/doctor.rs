//! `alsp doctor`: reports whether everything alsp needs is present, and the command that fixes
//! each gap. It changes nothing and spends nothing.
//!
//! Trust rules (doctor runs next to a repository it does not trust):
//! - Programs are found only in absolute `PATH` entries, and a file that resolves inside the
//!   repository is never run. A tool is asked `--version` (or an agent CLI `plugin list`) and
//!   nothing else.
//! - Probes start from a neutral directory, never from the repository, so a repository cannot
//!   steer rustup or cargo through its own toolchain file.
//! - The repository root is found by looking for `.git` up the directory tree; git is never run to
//!   find it, so no program is started before the repository is known and excluded.
//! - doctor never builds an index and never runs a build script: rust-analyzer is only asked for
//!   its version (through `alsp_index::locate_rust_analyzer`), sdd and bun are only looked for.
//! - Every probe has a time limit, and the whole process group is killed when it passes.

use alsp_evidence::escape;
use clap::Args as ClapArgs;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DEFAULT_PROBE_SECS: u64 = 10;
/// Agent CLIs are slow to list plugins (codex takes ~30s), so those probes get longer by default.
const DEFAULT_PLUGIN_PROBE_SECS: u64 = 45;
const MAX_PROBE_SECS: u64 = 120;
/// Probe output kept: enough to find `alsp` in a plugin list, bounded against a chatty tool.
const OUTPUT_CAP: usize = 64 * 1024;
/// How long to wait for the output threads once the probe has ended; a grandchild that kept a
/// pipe open must not hold doctor up.
const DRAIN_WAIT: Duration = Duration::from_secs(2);
const DETAIL_CAP: usize = 300;

const MUTANTS_FIX: &str = "cargo install cargo-mutants --locked";
const LLVM_COV_FIX: &str = "cargo install cargo-llvm-cov --locked";
const SDD_FIX: &str = "clone https://github.com/ryanthedev/sdd to ~/repos/sdd (or set ALSP_SDD_ROOT to a checkout) and install bun (https://bun.sh, or set ALSP_BUN)";

/// An agent CLI, how to get it, and how to install the alsp plugin into it (docs/plugin.md).
struct Agent {
    cli: &'static str,
    install: &'static str,
    plugin_fix: &'static str,
}

const AGENTS: [Agent; 3] = [
    Agent { cli: "claude", install: "npm install -g @anthropic-ai/claude-code", plugin_fix: "claude plugin marketplace add ryanthedev/agent-lsp && claude plugin install alsp@agent-lsp" },
    Agent { cli: "codex", install: "npm install -g @openai/codex", plugin_fix: "codex plugin marketplace add ryanthedev/agent-lsp && codex plugin add alsp@agent-lsp" },
    Agent { cli: "agy", install: "install the agy CLI (optional: only `alsp review` with agy needs it)", plugin_fix: "git clone https://github.com/ryanthedev/agent-lsp && agy plugin import <path to the clone>" },
];

#[derive(ClapArgs)]
pub struct Args {
    /// Repository to check (any directory inside its work tree). Outside one, repository checks
    /// are skipped.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Print one JSON object per check (name, status, required, fix, detail) instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ready,
    Missing,
    /// The probe hit its time limit.
    TimedOut,
    /// Not applicable here (no repository, or the agent CLI it needs is absent).
    Skipped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ready => "ready",
            Status::Missing => "missing",
            Status::TimedOut => "timed-out",
            Status::Skipped => "skipped",
        }
    }
}

#[derive(Debug)]
struct Check {
    name: String,
    status: Status,
    required: bool,
    detail: String,
    fix: Option<String>,
}

impl Check {
    fn new(name: &str, required: bool, status: Status, detail: impl Into<String>, fix: Option<&str>) -> Check {
        Check { name: name.into(), status, required, detail: detail.into(), fix: fix.map(str::to_string) }
    }
    fn ready(name: &str, required: bool, detail: impl Into<String>) -> Check {
        Check::new(name, required, Status::Ready, detail, None)
    }
    fn missing(name: &str, required: bool, detail: impl Into<String>, fix: &str) -> Check {
        Check::new(name, required, Status::Missing, detail, Some(fix))
    }
    fn skipped(name: &str, detail: impl Into<String>) -> Check {
        Check::new(name, false, Status::Skipped, detail, None)
    }
    /// A required check that is not ready.
    fn is_problem(&self) -> bool {
        self.required && matches!(self.status, Status::Missing | Status::TimedOut)
    }
}

/// What the checks read from the environment; explicit so unit tests need not touch the process's.
struct Ctx {
    path: OsString,
    /// Canonical repository root; nothing under it is run. `None` outside a work tree.
    repo: Option<PathBuf>,
    /// Neutral working directory for every probe.
    cwd: PathBuf,
    limit: Duration,
    /// Limit for `plugin list` probes.
    plugin_limit: Duration,
}

impl Ctx {
    fn from_process(repo_arg: &Path) -> Ctx {
        let path = std::env::var_os("PATH").unwrap_or_default();
        // When set, the variable limits every probe, plugin probes included.
        let set = std::env::var("ALSP_DOCTOR_TIMEOUT_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()).map(|s| s.clamp(1, MAX_PROBE_SECS));
        let secs = |default| Duration::from_secs(set.unwrap_or(default));
        let mut ctx = Ctx { path, repo: None, cwd: "/".into(), limit: secs(DEFAULT_PROBE_SECS), plugin_limit: secs(DEFAULT_PLUGIN_PROBE_SECS) };
        ctx.repo = find_work_tree(repo_arg);
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| "/".into());
        ctx.cwd = if ctx.inside_repo(&tmp) { "/".into() } else { tmp };
        ctx
    }

    fn inside_repo(&self, real: &Path) -> bool {
        self.repo.as_ref().is_some_and(|r| real.starts_with(r))
    }

    /// The first executable `name` in an absolute `PATH` entry that does not resolve inside the
    /// repository.
    fn find(&self, name: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path).filter(|d| d.is_absolute()).map(|d| d.join(name)).find(|cand| {
            cand.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) && std::fs::canonicalize(cand).is_ok_and(|real| !self.inside_repo(&real))
        })
    }
}

/// The work tree containing `start`: the nearest ancestor holding a `.git` entry (a directory, or
/// the file a linked worktree or submodule has). Read from the file system rather than asked of
/// git, because running git here would mean running a program found through `PATH` before the
/// repository, which it must be kept out of, is even known.
fn find_work_tree(start: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(start).ok()?;
    real.ancestors().find(|dir| dir.join(".git").symlink_metadata().is_ok()).map(Path::to_path_buf)
}

enum Probe {
    /// Exit 0; stdout then stderr, capped.
    Output(String),
    Failed(String),
    TimedOut,
}

/// Run `program args` from the neutral directory, stopping it after `limit`. The child leads its own
/// process group, which is killed when the limit passes, so a probe's helpers die with it.
fn probe(program: &Path, args: &[&str], ctx: &Ctx, limit: Duration) -> Probe {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(&ctx.cwd).env("PATH", &ctx.path).env("RUSTUP_AUTO_INSTALL", "0");
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Probe::Failed(format!("cannot run {}: {e}", program.display())),
    };
    let (out, err) = (drain(child.stdout.take().expect("piped")), drain(child.stderr.take().expect("piped")));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > limit => {
                // SAFETY: signalling the group this probe created; the pid is our own child's.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                return Probe::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Probe::Failed(format!("waiting for {} failed: {e}", program.display())),
        }
    };
    let (stdout, stderr) = (out.recv_timeout(DRAIN_WAIT).unwrap_or_default(), err.recv_timeout(DRAIN_WAIT).unwrap_or_default());
    if status.success() {
        return Probe::Output(format!("{stdout}{stderr}"));
    }
    let why = if stderr.trim().is_empty() { &stdout } else { &stderr };
    Probe::Failed(format!("exit {}: {}", status.code().map_or("signal".into(), |c| c.to_string()), shorten(why)))
}

/// Reads a pipe to the end on a thread, keeping at most `OUTPUT_CAP` bytes (the rest is read and
/// dropped so the child never blocks on a full pipe).
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut kept, mut buf) = (Vec::new(), [0u8; 8192]);
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buf[..n.min(OUTPUT_CAP.saturating_sub(kept.len()))]);
        }
        let _ = tx.send(String::from_utf8_lossy(&kept).into_owned());
    });
    rx
}

/// The last `DETAIL_CAP` characters, trimmed.
fn shorten(s: &str) -> String {
    let s = s.trim();
    let skip = s.chars().count().saturating_sub(DETAIL_CAP);
    if skip == 0 { s.to_string() } else { format!("...{}", s.chars().skip(skip).collect::<String>()) }
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// Whether `git --version` output names git 2.31 or newer (`GIT_CONFIG_COUNT` exists), the floor
/// `alsp_evidence::git_command` enforces.
fn git_is_new_enough(version_output: &str) -> bool {
    let mut nums = version_output.trim().strip_prefix("git version ").unwrap_or("").split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse::<u32>().ok());
    matches!((nums.next(), nums.next()), (Some(major), Some(minor)) if (major, minor) >= (2, 31))
}

/// A tool found on `PATH` and asked for its version.
fn tool_check(ctx: &Ctx, name: &str, required: bool, binary: &str, args: &[&str], fix: &str) -> Check {
    let Some(path) = ctx.find(binary) else {
        return Check::missing(name, required, format!("`{binary}` is not on PATH"), fix);
    };
    match probe(&path, args, ctx, ctx.limit) {
        Probe::Output(out) if !first_line(&out).is_empty() => Check::ready(name, required, format!("{} ({})", first_line(&out), path.display())),
        Probe::Output(_) => Check::missing(name, required, format!("{} printed no version", path.display()), fix),
        Probe::Failed(e) => Check::missing(name, required, e, fix),
        Probe::TimedOut => timed_out(name, required, &path, ctx.limit, fix),
    }
}

fn timed_out(name: &str, required: bool, path: &Path, limit: Duration, fix: &str) -> Check {
    Check::new(name, required, Status::TimedOut, format!("{} did not answer within {}s and was stopped (set ALSP_DOCTOR_TIMEOUT_SECS higher if it is only slow)", path.display(), limit.as_secs()), Some(fix))
}

fn git_check(ctx: &Ctx) -> Check {
    const FIX: &str = "install git 2.31 or newer (https://git-scm.com/downloads)";
    let Some(path) = ctx.find("git") else { return Check::missing("git", true, "`git` is not on PATH", FIX) };
    match probe(&path, &["--version"], ctx, ctx.limit) {
        Probe::Output(out) if git_is_new_enough(&out) => Check::ready("git", true, format!("{} ({})", first_line(&out), path.display())),
        Probe::Output(out) => Check::missing("git", true, format!("{:?} is older than the 2.31 alsp needs", first_line(&out)), FIX),
        Probe::Failed(e) => Check::missing("git", true, e, FIX),
        Probe::TimedOut => timed_out("git", true, &path, ctx.limit, FIX),
    }
}

fn rust_analyzer_check(ctx: &Ctx) -> Check {
    // Outside a work tree there is no repository to avoid or pin: look from a path that does not
    // exist, so only the environment decides.
    let base = ctx.repo.clone().unwrap_or_else(|| ctx.cwd.join("alsp-doctor-no-repository"));
    match alsp_index::locate_rust_analyzer(&base) {
        Ok(l) => Check::ready("rust-analyzer", true, format!("{} ({}; {})", l.version, l.path.display(), l.source)),
        Err(e) => {
            let tried = e.tried.iter().map(|t| format!("{}: {}", t.candidate, t.error)).collect::<Vec<_>>().join("; ");
            Check::missing("rust-analyzer", true, if tried.is_empty() { "none found".into() } else { format!("tried {tried}") }, &e.fix)
        }
    }
}

fn sdd_check() -> Check {
    match alsp_decide::SddDecider::from_environment() {
        Ok(_) => Check::ready("sdd decide + bun", false, "found (not run)"),
        Err(e) => Check::missing("sdd decide + bun", false, e.to_string(), SDD_FIX),
    }
}

fn agent_checks(ctx: &Ctx, out: &mut Vec<Check>) {
    for a in AGENTS {
        let (cli_name, plugin_name) = (format!("{} CLI", a.cli), format!("{} plugin", a.cli));
        let Some(path) = ctx.find(a.cli) else {
            out.push(Check::missing(&cli_name, false, format!("`{}` is not on PATH", a.cli), a.install));
            out.push(Check::skipped(&plugin_name, format!("{} CLI not found", a.cli)));
            continue;
        };
        out.push(Check::ready(&cli_name, false, path.display().to_string()));
        out.push(match probe(&path, &["plugin", "list"], ctx, ctx.plugin_limit) {
            Probe::Output(list) if list.to_ascii_lowercase().contains("alsp") => Check::ready(&plugin_name, false, "alsp is installed"),
            Probe::Output(_) => Check::missing(&plugin_name, false, "alsp is not in `plugin list`", a.plugin_fix),
            Probe::Failed(e) => Check::missing(&plugin_name, false, format!("`{} plugin list` failed: {e}", a.cli), a.plugin_fix),
            Probe::TimedOut => timed_out(&plugin_name, false, &path, ctx.plugin_limit, a.plugin_fix),
        });
    }
}

fn run_checks(ctx: &Ctx) -> Vec<Check> {
    let mut checks = vec![Check::ready("alsp", true, env!("CARGO_PKG_VERSION")), git_check(ctx)];
    checks.push(match &ctx.repo {
        Some(root) => Check::ready("repository", false, root.display().to_string()),
        None => Check::skipped("repository", "not inside a git work tree; repository checks are skipped (rust-analyzer is looked for without a pinned toolchain)"),
    });
    checks.push(rust_analyzer_check(ctx));
    let mut cov = tool_check(ctx, "cargo-llvm-cov", false, "cargo-llvm-cov", &["llvm-cov", "--version"], LLVM_COV_FIX);
    cov.detail.push_str("; needed by `cover` and `tests --run`");
    checks.push(cov);
    checks.push(tool_check(ctx, "cargo-mutants", false, "cargo-mutants", &["mutants", "--version"], MUTANTS_FIX));
    checks.push(sdd_check());
    agent_checks(ctx, &mut checks);
    checks
}

fn to_json(checks: &[Check]) -> Value {
    Value::Array(checks.iter().map(|c| json!({ "name": c.name, "status": c.status.as_str(), "required": c.required, "fix": c.fix, "detail": c.detail })).collect())
}

fn to_text(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let need = if c.required { "required" } else { "optional" };
        out.push_str(&format!("{:<9} {} ({need}): {}\n", c.status.as_str(), escape(&c.name), escape(&c.detail)));
        if let (Some(fix), Status::Missing | Status::TimedOut) = (&c.fix, c.status) {
            out.push_str(&format!("          fix: {}\n", escape(fix)));
        }
    }
    let problems = checks.iter().filter(|c| c.is_problem()).count();
    out.push_str(&match problems {
        0 => "alsp doctor: ready\n".to_string(),
        1 => "alsp doctor: 1 problem\n".to_string(),
        n => format!("alsp doctor: {n} problems\n"),
    });
    out
}

/// Exit 0 when every required check is ready, 1 otherwise.
pub fn run(args: Args) -> ExitCode {
    let checks = run_checks(&Ctx::from_process(&args.repo));
    if args.json {
        println!("{}", serde_json::to_string_pretty(&to_json(&checks)).expect("json of plain values"));
    } else {
        print!("{}", to_text(&checks));
    }
    if checks.iter().any(Check::is_problem) { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

#[cfg(test)]
mod tests {
    #![allow(non_snake_case)]
    use super::*;
    use std::fs;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn ctx(bin: &Path, repo: Option<PathBuf>, secs: u64) -> Ctx {
        Ctx { path: format!("{}:/usr/bin:/bin", bin.display()).into(), repo, cwd: "/".into(), limit: Duration::from_secs(secs), plugin_limit: Duration::from_secs(secs) }
    }

    #[test]
    fn find_work_tree_walks_up_to_the_dot_git_entry() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        assert_eq!(find_work_tree(&root.join("a/b")), None);
        fs::write(root.join("a/.git"), "gitdir: elsewhere").unwrap();
        assert_eq!(find_work_tree(&root.join("a/b")), Some(root.join("a")));
        assert_eq!(find_work_tree(&root.join("missing")), None);
    }

    #[test]
    fn git_version_floor_is_2_31() {
        for (text, ok) in [("git version 2.31.0", true), ("git version 2.39.5 (Apple Git-154)", true), ("git version 3.0.1", true), ("git version 2.30.9", false), ("git version 1.9", false), ("nonsense", false), ("", false)] {
            assert_eq!(git_is_new_enough(text), ok, "{text:?}");
        }
    }

    #[test]
    fn test_DW_4_5_probe_kills_a_hanging_tool_and_its_children() {
        let d = tempfile::tempdir().unwrap();
        let bin = fs::canonicalize(d.path()).unwrap();
        let marker = bin.join("child-survived");
        let hang = script(&bin, "hang", &format!("(sleep 3; touch {}) &\nsleep 30", marker.display()));
        let t = Instant::now();
        assert!(matches!(probe(&hang, &[], &ctx(&bin, None, 1), Duration::from_secs(1)), Probe::TimedOut));
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        std::thread::sleep(Duration::from_millis(3500));
        assert!(!marker.exists(), "the process group was not killed");
    }

    #[test]
    fn probe_reports_failure_output_and_caps_chatter() {
        let d = tempfile::tempdir().unwrap();
        let bin = fs::canonicalize(d.path()).unwrap();
        let bad = script(&bin, "bad", "echo boom >&2; exit 7");
        let Probe::Failed(e) = probe(&bad, &[], &ctx(&bin, None, 5), Duration::from_secs(5)) else { panic!("expected failure") };
        assert!(e.contains("exit 7: boom"), "{e}");
        let loud = script(&bin, "loud", "yes alsp | head -c 3000000");
        let Probe::Output(o) = probe(&loud, &[], &ctx(&bin, None, 5), Duration::from_secs(5)) else { panic!("expected output") };
        assert!(o.len() <= OUTPUT_CAP, "{}", o.len());
    }

    #[test]
    fn find_skips_relative_entries_and_files_inside_the_repository() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let (repo, good) = (root.join("repo"), root.join("good"));
        script(&repo.join("bin"), "tool", "echo hi");
        script(&good, "tool", "echo hi");
        std::os::unix::fs::symlink(repo.join("bin/tool"), good.join("sneaky")).unwrap();
        let path = std::env::join_paths([PathBuf::from("bin"), repo.join("bin"), good.clone()]).unwrap();
        let c = Ctx { path, repo: Some(repo), cwd: "/".into(), limit: Duration::from_secs(1), plugin_limit: Duration::from_secs(1) };
        assert_eq!(c.find("tool"), Some(good.join("tool")));
        assert_eq!(c.find("sneaky"), None, "a symlink into the repository resolves inside it");
    }

    #[test]
    fn plugin_is_found_only_when_the_list_names_alsp_and_a_failing_list_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let bin = fs::canonicalize(d.path()).unwrap();
        script(&bin, "claude", "echo 'alsp@agent-lsp  enabled'");
        script(&bin, "codex", "echo 'other-plugin'");
        script(&bin, "agy", "echo nope >&2; exit 1");
        let mut checks = Vec::new();
        agent_checks(&ctx(&bin, None, 5), &mut checks);
        let by = |n: &str| checks.iter().find(|c| c.name == n).unwrap();
        assert_eq!(by("claude plugin").status, Status::Ready);
        assert_eq!(by("codex plugin").status, Status::Missing);
        assert!(by("codex plugin").fix.as_deref().unwrap().contains("codex plugin add"));
        assert!(by("agy plugin").detail.contains("failed") && by("agy plugin").status == Status::Missing);
        assert!(checks.iter().all(|c| !c.is_problem()), "agent checks are optional");
    }

    #[test]
    fn a_required_tool_that_hangs_is_a_problem_and_an_optional_one_is_not() {
        let d = tempfile::tempdir().unwrap();
        let bin = fs::canonicalize(d.path()).unwrap();
        script(&bin, "git", "sleep 30");
        script(&bin, "cargo-mutants", "sleep 30");
        let c = ctx(&bin, None, 1);
        let git = git_check(&c);
        assert!(git.status == Status::TimedOut && git.is_problem(), "{git:?}");
        let m = tool_check(&c, "cargo-mutants", false, "cargo-mutants", &["mutants", "--version"], MUTANTS_FIX);
        assert!(m.status == Status::TimedOut && !m.is_problem(), "{m:?}");
    }

    #[test]
    fn text_output_ends_with_the_verdict_and_escapes_details() {
        let ok = vec![Check::ready("a", true, "x"), Check::missing("b", false, "gone", "install b")];
        let t = to_text(&ok);
        assert!(t.ends_with("alsp doctor: ready\n") && t.contains("fix: install b"), "{t}");
        let bad = vec![Check::missing("r", true, "line1\nforged: ready", "fix it"), Check::missing("s", true, "x", "y")];
        let t = to_text(&bad);
        assert!(t.ends_with("alsp doctor: 2 problems\n") && !t.contains("\nforged"), "{t}");
        assert!(to_text(&bad[..1]).ends_with("alsp doctor: 1 problem\n"));
    }
}
