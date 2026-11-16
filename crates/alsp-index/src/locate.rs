//! Finding a rust-analyzer that runs, without letting the analysed repository choose the program.
//!
//! Order: `ALSP_RUST_ANALYZER` (broken means an error, never a silent replacement); then each
//! `rust-analyzer` on `PATH`; then the copy inside another installed rustup toolchain, run by its
//! real path so the repository's own toolchain still drives cargo.
//!
//! Trust rules. Everything here runs next to an untrusted repository, so:
//! - A `PATH` entry that is relative, or whose file resolves inside the repository, is never run.
//! - Probes run from `/`, never from the repository or a shared temp directory: rustup picks its
//!   toolchain (and cargo its config) from files found searching upward from the cwd. A
//!   repository's `rust-toolchain.toml` may point at a toolchain inside the repository, and
//!   another local user can plant the same files in a world-writable `/tmp`. `/` is root-owned.
//!   Where a pin is meant, `RUSTUP_TOOLCHAIN` is set explicitly; otherwise it is removed.
//! - The repository's pinned toolchain is read as text. A shim is tested under that pin with
//!   `RUSTUP_TOOLCHAIN` instead of being started inside the repository. A name that is not a plain
//!   toolchain name is dropped, so it never reaches a command line or the environment.

use crate::build::tail;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_FALLBACK_TOOLCHAINS: usize = 6;
const FALLBACK_PREFIX: &str = "fallback";
const TOOLCHAIN_FILE_CAP: u64 = 64 * 1024;

/// A rust-analyzer that answered `--version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    pub version: String,
    /// Why this one was chosen: `ALSP_RUST_ANALYZER`, `PATH`, or `fallback: ...`.
    pub source: String,
}

impl Located {
    /// Chosen because the ordinary one was unusable; worth telling the reader.
    pub fn is_fallback(&self) -> bool {
        self.source.starts_with(FALLBACK_PREFIX)
    }
}

/// One candidate that was looked at and why it was not used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tried {
    pub candidate: String,
    pub error: String,
}

/// No rust-analyzer ran. `fix` is the command (or instruction) that makes one available.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocateError {
    pub tried: Vec<Tried>,
    pub fix: String,
}

impl std::fmt::Display for LocateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no working rust-analyzer found")?;
        for (i, t) in self.tried.iter().enumerate() {
            write!(f, "{} {}: {}", if i == 0 { "; tried" } else { ";" }, t.candidate, t.error)?;
        }
        write!(f, "; fix: {}", self.fix)
    }
}

impl std::error::Error for LocateError {}

/// What the lookup reads from the environment; explicit so tests need not touch the process's.
pub(crate) struct Env {
    /// Canonical directories nothing may be run from: the repository and the tree being indexed.
    pub avoid: Vec<PathBuf>,
    pub alsp_ra: Option<OsString>,
    pub path: OsString,
    pub rustup_toolchain: Option<OsString>,
    pub probe_cwd: PathBuf,
}

impl Env {
    fn inside_avoided(&self, real: &Path) -> bool {
        self.avoid.iter().any(|a| real.starts_with(a))
    }

    fn from_process(avoid: Vec<PathBuf>) -> Env {
        Env {
            avoid,
            alsp_ra: std::env::var_os("ALSP_RUST_ANALYZER").filter(|v| !v.is_empty()),
            path: std::env::var_os("PATH").unwrap_or_default(),
            rustup_toolchain: std::env::var_os("RUSTUP_TOOLCHAIN"),
            probe_cwd: "/".into(),
        }
    }
}

/// Find a rust-analyzer for the Cargo tree at `repo`. See the module doc for the order and the
/// trust rules.
pub fn locate_rust_analyzer(repo: &Path) -> Result<Located, LocateError> {
    locate_rust_analyzer_avoiding(repo, &[])
}

/// [`locate_rust_analyzer`] for a tree that is not the repository itself (an exported snapshot):
/// nothing under `repo` or under `also_avoid` is run either.
pub fn locate_rust_analyzer_avoiding(repo: &Path, also_avoid: &[PathBuf]) -> Result<Located, LocateError> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let repo = canon(repo);
    let avoid = std::iter::once(repo.clone()).chain(also_avoid.iter().map(|p| canon(p))).collect();
    locate_with(&repo, &Env::from_process(avoid))
}

pub(crate) fn locate_with(repo: &Path, env: &Env) -> Result<Located, LocateError> {
    let mut tried = Vec::new();
    let pinned = pinned_toolchain(repo, env);

    if let Some(v) = &env.alsp_ra {
        let path = PathBuf::from(v);
        return match probe_version(&path, env, None) {
            Ok(version) => Ok(Located { path, version, source: "ALSP_RUST_ANALYZER".into() }),
            Err(e) => {
                tried.push(Tried { candidate: format!("ALSP_RUST_ANALYZER={}", path.display()), error: e });
                Err(LocateError { tried, fix: "fix the path in ALSP_RUST_ANALYZER, or unset it to let alsp search".into() })
            }
        };
    }

    for path in search_path("rust-analyzer", env, &mut tried) {
        match probe_version(&path, env, pinned.as_deref()) {
            Ok(version) => return Ok(Located { path, version, source: "PATH".into() }),
            Err(e) => tried.push(Tried { candidate: path.display().to_string(), error: e }),
        }
    }

    let rustup = search_path("rustup", env, &mut Vec::new()).into_iter().next();
    let Some(rustup) = rustup else {
        return Err(LocateError {
            tried,
            fix: "install rust-analyzer (https://rust-analyzer.github.io/manual.html#installation) or set ALSP_RUST_ANALYZER to its path".into(),
        });
    };
    let (found, default_toolchain) = rustup_fallback(&rustup, env, &mut tried);
    if let Some((path, version, toolchain)) = found {
        let why = match &pinned {
            Some(p) => format!("rust-analyzer on PATH does not run under {p}"),
            None => "no usable rust-analyzer on PATH".into(),
        };
        return Ok(Located { path, version, source: format!("{FALLBACK_PREFIX}: rust-analyzer of toolchain {toolchain}, since {why}") });
    }
    let toolchain = pinned.or(default_toolchain).unwrap_or_else(|| "stable".into());
    let fix = if tried.iter().any(|t| t.error.contains("is not installed")) {
        format!("rustup toolchain install {toolchain} --component rust-analyzer")
    } else {
        format!("rustup component add rust-analyzer --toolchain {toolchain}")
    };
    Err(LocateError { tried, fix })
}

/// The toolchain rustup would pick for the repository: `RUSTUP_TOOLCHAIN`, else the repository's
/// toolchain file (found from the repository upwards, as rustup does). Only a plain name is
/// returned.
fn pinned_toolchain(repo: &Path, env: &Env) -> Option<String> {
    if let Some(v) = &env.rustup_toolchain {
        return v.to_str().filter(|s| valid_toolchain(s)).map(str::to_string);
    }
    repo.ancestors().find_map(|dir| {
        ["rust-toolchain", "rust-toolchain.toml"].iter().map(|f| dir.join(f)).find(|p| p.metadata().is_ok_and(|m| m.is_file())).map(|p| read_channel(&p))
    })?
}

fn read_channel(file: &Path) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(file).ok()?.take(TOOLCHAIN_FILE_CAP).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let lines = || text.lines().map(|l| l.split('#').next().unwrap_or("").trim()).filter(|l| !l.is_empty());
    let name = if text.contains('=') || text.contains('[') {
        lines().find_map(|l| l.strip_prefix("channel").and_then(|r| r.trim_start().strip_prefix('=')).map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string()))?
    } else {
        lines().next()?.to_string()
    };
    valid_toolchain(&name).then_some(name)
}

/// A toolchain name safe to put on a command line or in the environment: no path, no option.
fn valid_toolchain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.starts_with(|c: char| c.is_ascii_alphanumeric()) && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Executable files called `name` in the absolute `PATH` entries, skipping any that resolve inside
/// the repository (recorded in `tried`). Each is returned by the path it was found under: rustup
/// shims choose what to run by their own file name.
fn search_path(name: &str, env: &Env, tried: &mut Vec<Tried>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(&env.path).filter(|d| d.is_absolute()) {
        let cand = dir.join(name);
        if !cand.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) || found.contains(&cand) {
            continue;
        }
        match std::fs::canonicalize(&cand) {
            Ok(real) if env.inside_avoided(&real) => tried.push(Tried { candidate: cand.display().to_string(), error: "skipped: inside the repository".into() }),
            Ok(_) => found.push(cand),
            Err(_) => {}
        }
    }
    found
}

/// Try the rust-analyzer of each installed toolchain. Returns the first that runs (path, version,
/// toolchain) and the default toolchain's name when rustup said which it is.
fn rustup_fallback(rustup: &Path, env: &Env, tried: &mut Vec<Tried>) -> (Option<(PathBuf, String, String)>, Option<String>) {
    let mut default = None;
    let mut names: Vec<String> = Vec::new();
    match run(Command::new(rustup).args(["toolchain", "list"]), env, None) {
        Ok(list) => {
            for line in list.lines() {
                let mut words = line.split_whitespace();
                let Some(name) = words.next().filter(|n| valid_toolchain(n)) else { continue };
                if line.contains("default") {
                    default = Some(name.to_string());
                }
                names.push(name.to_string());
            }
            // Stable first: it is the toolchain most likely to carry the component.
            names.sort_by_key(|n| !n.starts_with("stable"));
        }
        Err(e) => {
            tried.push(Tried { candidate: format!("{} toolchain list", rustup.display()), error: e });
            names.push("stable".into());
        }
    }
    for name in names.into_iter().take(MAX_FALLBACK_TOOLCHAINS) {
        let label = format!("rustup toolchain {name}");
        let out = run(Command::new(rustup).args(["which", "--toolchain", &name, "rust-analyzer"]), env, None);
        let path = match out.map(|s| PathBuf::from(s.lines().next().unwrap_or("").trim())) {
            Ok(p) if p.is_absolute() => p,
            Ok(p) => {
                tried.push(Tried { candidate: label, error: format!("rustup named {:?}, not an absolute path", p.display().to_string()) });
                continue;
            }
            Err(e) => {
                tried.push(Tried { candidate: label, error: e });
                continue;
            }
        };
        if std::fs::canonicalize(&path).is_ok_and(|real| env.inside_avoided(&real)) {
            tried.push(Tried { candidate: label, error: "skipped: inside the repository".into() });
            continue;
        }
        match probe_version(&path, env, None) {
            Ok(version) => return (Some((path, version, name)), default),
            Err(e) => tried.push(Tried { candidate: format!("{label} ({})", path.display()), error: e }),
        }
    }
    (None, default)
}

fn probe_version(program: &Path, env: &Env, toolchain: Option<&str>) -> Result<String, String> {
    run(Command::new(program).arg("--version"), env, toolchain)
}

/// Run a probe from the neutral directory: PATH as given, the pin (or none) as the toolchain,
/// no auto-install, bounded time. Returns trimmed stdout, or the tail of what went wrong.
fn run(cmd: &mut Command, env: &Env, toolchain: Option<&str>) -> Result<String, String> {
    let program = cmd.get_program().to_os_string();
    cmd.current_dir(&env.probe_cwd).env("PATH", &env.path).env("RUSTUP_AUTO_INSTALL", "0");
    match toolchain {
        Some(t) => cmd.env("RUSTUP_TOOLCHAIN", t),
        None => cmd.env_remove("RUSTUP_TOOLCHAIN"),
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {}: {e}", display(&program)))?;
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            String::from_utf8_lossy(&b).into_owned()
        })
    };
    let (out, err) = (drain(Box::new(child.stdout.take().expect("piped"))), drain(Box::new(child.stderr.take().expect("piped"))));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} timed out after {}s", display(&program), PROBE_TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("waiting for {} failed: {e}", display(&program))),
        }
    };
    let (stdout, stderr) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status.success() {
        let why = if stderr.trim().is_empty() { &stdout } else { &stderr };
        return Err(format!("exit {}: {}", status.code().map_or("signal".into(), |c| c.to_string()), tail(why)));
    }
    match stdout.trim() {
        "" => Err("printed no output".into()),
        s => Ok(s.to_string()),
    }
}

fn display(p: &OsStr) -> String {
    Path::new(p).display().to_string()
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

    /// A sandbox: a repo, a bin dir for fakes, a neutral cwd. PATH holds only `bin` (the fakes use
    /// shell builtins).
    struct Sb {
        _d: tempfile::TempDir,
        repo: PathBuf,
        bin: PathBuf,
        env: Env,
    }

    fn sb() -> Sb {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let (repo, bin, cwd) = (root.join("repo"), root.join("bin"), root.join("cwd"));
        for p in [&repo, &bin, &cwd] {
            fs::create_dir_all(p).unwrap();
        }
        let env = Env { avoid: vec![repo.clone()], alsp_ra: None, path: bin.clone().into_os_string(), rustup_toolchain: None, probe_cwd: cwd };
        Sb { _d: d, repo, bin, env }
    }

    const SHIM_FAILS_UNDER_PIN: &str = "if [ \"$RUSTUP_TOOLCHAIN\" = nightly-2026-07-22 ]; then echo \"error: Unknown binary 'rust-analyzer' in official toolchain '$RUSTUP_TOOLCHAIN'\" >&2; exit 1; fi; echo 'rust-analyzer shim 1'";

    fn pin(s: &Sb) {
        fs::write(s.repo.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"nightly-2026-07-22\"\n").unwrap();
    }

    /// Fake rustup: `toolchain list` prints $LIST; `which --toolchain T rust-analyzer` prints
    /// `<bin>/ra-T` when that exists, else fails.
    fn fake_rustup(s: &Sb, list: &str) {
        script(&s.bin, "rustup", &format!("case \"$1\" in toolchain) printf '%s\\n' '{list}';; which) [ -x \"{b}/ra-$3\" ] && echo \"{b}/ra-$3\" || {{ echo \"error: unknown binary 'rust-analyzer' in toolchain '$3'\" >&2; exit 1; }};; esac", b = s.bin.display()));
    }

    #[test]
    fn test_DW_2_4_path_is_used_when_it_runs_and_is_not_a_fallback() {
        let s = sb();
        let ra = script(&s.bin, "rust-analyzer", "echo 'rust-analyzer 1.2'");
        let l = locate_with(&s.repo, &s.env).unwrap();
        assert_eq!((l.path.as_path(), l.version.as_str(), l.source.as_str(), l.is_fallback()), (ra.as_path(), "rust-analyzer 1.2", "PATH", false));
    }

    #[test]
    fn test_DW_2_4_alsp_rust_analyzer_wins_over_path() {
        let mut s = sb();
        script(&s.bin, "rust-analyzer", "echo path-one");
        let mine = script(&s.bin, "mine", "echo mine-one");
        s.env.alsp_ra = Some(mine.clone().into_os_string());
        let l = locate_with(&s.repo, &s.env).unwrap();
        assert_eq!((l.path, l.version, l.source), (mine, "mine-one".into(), "ALSP_RUST_ANALYZER".into()));
    }

    #[test]
    fn test_DW_2_3_a_broken_alsp_rust_analyzer_is_reported_not_replaced() {
        let mut s = sb();
        script(&s.bin, "rust-analyzer", "echo a-working-one");
        script(&s.bin, "mine", "echo 'boom' >&2; exit 7");
        for broken in [s.bin.join("mine"), s.bin.join("does-not-exist")] {
            s.env.alsp_ra = Some(broken.clone().into_os_string());
            let e = locate_with(&s.repo, &s.env).unwrap_err();
            assert_eq!(e.tried.len(), 1, "{e}");
            assert!(e.tried[0].candidate.starts_with("ALSP_RUST_ANALYZER=") && e.to_string().contains("ALSP_RUST_ANALYZER"), "{e}");
            assert!(e.tried[0].error.contains("exit 7: boom") || e.tried[0].error.contains("cannot run"), "{e}");
            assert!(e.fix.contains("ALSP_RUST_ANALYZER"), "{}", e.fix);
        }
    }

    #[test]
    fn test_DW_2_1_a_shim_failing_under_the_pin_falls_back_to_another_toolchain() {
        let s = sb();
        pin(&s);
        script(&s.bin, "rust-analyzer", SHIM_FAILS_UNDER_PIN);
        let stable = script(&s.bin, "ra-stable-aarch64", "echo 'rust-analyzer stable 9'");
        fake_rustup(&s, "nightly-2026-07-22-aarch64 (default)\nstable-aarch64");
        let l = locate_with(&s.repo, &s.env).unwrap();
        assert_eq!((l.path.as_path(), l.version.as_str()), (stable.as_path(), "rust-analyzer stable 9"));
        assert!(l.is_fallback() && l.source.contains("stable-aarch64") && l.source.contains("nightly-2026-07-22"), "{}", l.source);
    }

    #[test]
    fn test_DW_2_4_the_shim_is_kept_when_it_works_under_the_pin() {
        let s = sb();
        pin(&s);
        script(&s.bin, "rust-analyzer", "[ \"$RUSTUP_TOOLCHAIN\" = nightly-2026-07-22 ] && echo ok-under-pin || exit 1");
        assert_eq!(locate_with(&s.repo, &s.env).unwrap().source, "PATH");
    }

    #[test]
    fn test_DW_2_4_without_a_pin_the_default_is_probed_without_a_toolchain_override() {
        let s = sb();
        script(&s.bin, "rust-analyzer", "[ -z \"$RUSTUP_TOOLCHAIN\" ] && echo no-override || exit 1");
        assert_eq!(locate_with(&s.repo, &s.env).unwrap().version, "no-override");
    }

    #[test]
    fn test_DW_2_4_a_failing_path_entry_lets_a_later_one_win() {
        let s = sb();
        let other = s.bin.parent().unwrap().join("bin2");
        script(&s.bin, "rust-analyzer", "exit 1");
        let good = script(&other, "rust-analyzer", "echo second");
        let mut env = Env { path: std::env::join_paths([&s.bin, &other]).unwrap(), ..s.env };
        env.alsp_ra = None;
        assert_eq!(locate_with(&s.repo, &env).unwrap().path, good);
    }

    #[test]
    fn test_DW_2_2_nothing_works_names_the_repository_toolchain_in_the_fix() {
        let s = sb();
        pin(&s);
        script(&s.bin, "rust-analyzer", SHIM_FAILS_UNDER_PIN);
        fake_rustup(&s, "nightly-2026-07-22-aarch64 (default)\nstable-aarch64");
        let e = locate_with(&s.repo, &s.env).unwrap_err();
        assert_eq!(e.fix, "rustup component add rust-analyzer --toolchain nightly-2026-07-22");
        let text = e.to_string();
        assert!(text.contains("Unknown binary") && text.contains("rustup toolchain stable-aarch64") && text.ends_with(&e.fix), "{text}");
    }

    #[test]
    fn test_DW_2_2_a_toolchain_that_is_not_installed_gets_an_install_fix() {
        let s = sb();
        pin(&s);
        script(&s.bin, "rust-analyzer", "echo \"error: toolchain 'nightly-2026-07-22' is not installed\" >&2; exit 1");
        fake_rustup(&s, "no installed toolchains");
        let e = locate_with(&s.repo, &s.env).unwrap_err();
        assert_eq!(e.fix, "rustup toolchain install nightly-2026-07-22 --component rust-analyzer");
    }

    #[test]
    fn test_DW_2_2_without_a_pin_the_fix_names_the_default_toolchain() {
        let s = sb();
        script(&s.bin, "rust-analyzer", "exit 1");
        fake_rustup(&s, "stable-x\nnightly-x (active, default)");
        assert_eq!(locate_with(&s.repo, &s.env).unwrap_err().fix, "rustup component add rust-analyzer --toolchain nightly-x");
    }

    #[test]
    fn test_DW_2_4_no_rustup_and_no_rust_analyzer_says_how_to_install() {
        let s = sb();
        pin(&s);
        let e = locate_with(&s.repo, &s.env).unwrap_err();
        assert!(e.tried.is_empty() && e.fix.contains("install rust-analyzer") && e.fix.contains("ALSP_RUST_ANALYZER"), "{e}");
    }

    #[test]
    fn test_DW_2_4_no_rustup_with_a_failing_shim_reports_the_failure() {
        let s = sb();
        script(&s.bin, "rust-analyzer", "echo 'proxy broke' >&2; exit 2");
        let e = locate_with(&s.repo, &s.env).unwrap_err();
        assert!(e.to_string().contains("exit 2: proxy broke") && e.fix.contains("install rust-analyzer"), "{e}");
    }

    #[test]
    fn test_DW_2_4_rustup_proxy_errors_other_than_unknown_binary_also_fall_back() {
        let s = sb();
        pin(&s);
        script(&s.bin, "rust-analyzer", "echo 'error: could not download channel manifest' >&2; exit 1");
        script(&s.bin, "ra-stable-x", "echo fb");
        fake_rustup(&s, "stable-x");
        assert!(locate_with(&s.repo, &s.env).unwrap().is_fallback());
    }

    #[test]
    fn test_DW_2_4_rustup_present_but_no_toolchain_has_the_component() {
        let s = sb();
        fake_rustup(&s, "stable-x\nbeta-y");
        let e = locate_with(&s.repo, &s.env).unwrap_err();
        assert_eq!(e.tried.len(), 2, "{e}");
        assert_eq!(e.fix, "rustup component add rust-analyzer --toolchain stable");
    }

    #[test]
    fn test_DW_2_4_stable_is_tried_before_the_other_toolchains() {
        let s = sb();
        script(&s.bin, "ra-nightly-x", "echo from-nightly");
        script(&s.bin, "ra-stable-x", "echo from-stable");
        fake_rustup(&s, "nightly-x (default)\nstable-x");
        assert_eq!(locate_with(&s.repo, &s.env).unwrap().version, "from-stable");
    }

    #[test]
    fn test_DW_2_4_a_rustup_that_fails_to_list_still_tries_stable() {
        let s = sb();
        script(&s.bin, "rustup", &format!("case \"$1\" in toolchain) echo nope >&2; exit 1;; which) echo '{}/ra' ;; esac", s.bin.display()));
        script(&s.bin, "ra", "echo via-which");
        assert_eq!(locate_with(&s.repo, &s.env).unwrap().version, "via-which");
    }

    #[test]
    fn test_DW_2_4_a_relative_rustup_answer_is_refused() {
        let s = sb();
        script(&s.bin, "rustup", "case \"$1\" in toolchain) echo stable-x;; which) echo ./ra;; esac");
        let e = locate_with(&s.repo, &s.env).unwrap_err();
        assert!(e.to_string().contains("not an absolute path"), "{e}");
    }

    #[test]
    fn test_DW_2_4_empty_probe_output_is_a_failure() {
        let s = sb();
        script(&s.bin, "rust-analyzer", "exit 0");
        assert!(locate_with(&s.repo, &s.env).unwrap_err().to_string().contains("printed no output"));
    }

    #[test]
    fn test_DW_2_4_toolchain_file_forms_and_bad_names() {
        let s = sb();
        for (body, want) in [
            ("nightly-2026-07-22\n", Some("nightly-2026-07-22")),
            ("# c\n[toolchain]\nchannel = 'stable' # x\ncomponents = [\"a\"]\n", Some("stable")),
            ("[toolchain]\npath = \"/somewhere\"\n", None),
            ("[toolchain]\nchannel = \"../evil\"\n", None),
            ("[toolchain]\nchannel = \"-x\"\n", None),
            ("", None),
        ] {
            fs::write(s.repo.join("rust-toolchain.toml"), body).unwrap();
            assert_eq!(pinned_toolchain(&s.repo, &s.env).as_deref(), want, "{body:?}");
        }
        fs::remove_file(s.repo.join("rust-toolchain.toml")).unwrap();
        assert_eq!(pinned_toolchain(&s.repo, &s.env), None);
        // The environment variable outranks the file, and is validated too.
        fs::write(s.repo.join("rust-toolchain"), "beta\n").unwrap();
        assert_eq!(pinned_toolchain(&s.repo, &s.env).as_deref(), Some("beta"));
        let env = Env { rustup_toolchain: Some("1.80.0".into()), ..sb().env };
        assert_eq!(pinned_toolchain(&s.repo, &env).as_deref(), Some("1.80.0"));
        let env = Env { rustup_toolchain: Some("/abs".into()), ..sb().env };
        assert_eq!(pinned_toolchain(&s.repo, &env), None);
    }

    #[test]
    fn test_DW_2_4_a_toolchain_file_that_is_not_a_regular_file_is_not_opened() {
        let s = sb();
        let st = Command::new("mkfifo").arg(s.repo.join("rust-toolchain.toml")).status();
        if st.is_ok_and(|x| x.success()) {
            assert_eq!(pinned_toolchain(&s.repo, &s.env), None);
        }
    }

    #[test]
    fn test_DW_2_5_a_rust_analyzer_inside_the_repository_is_never_executed() {
        let s = sb();
        let marker = s.repo.join("marker");
        let mark = format!("echo ran >> '{}'; echo planted", marker.display());
        script(&s.repo.join("bin"), "rust-analyzer", &mark);
        script(&s.repo, "rustup", &mark);
        // absolute entry inside the repo, a relative entry, an empty entry, and a symlink out of a clean dir
        let link_dir = s.bin.parent().unwrap().join("links");
        fs::create_dir_all(&link_dir).unwrap();
        std::os::unix::fs::symlink(s.repo.join("bin/rust-analyzer"), link_dir.join("rust-analyzer")).unwrap();
        let path = std::env::join_paths([s.repo.join("bin"), "bin".into(), "".into(), ".".into(), link_dir, s.repo.clone()]).unwrap();
        let env = Env { path, ..s.env };
        let e = locate_with(&s.repo, &env).unwrap_err();
        assert!(!marker.exists(), "a program inside the repository was executed");
        assert!(e.to_string().contains("skipped: inside the repository"), "{e}");
    }

    #[test]
    fn test_DW_2_5_a_repository_toolchain_override_reaches_no_command() {
        let s = sb();
        let marker = s.repo.join("marker");
        // a toolchain living in the repository, named by the repository's own override file
        script(&s.repo.join("tc/bin"), "rust-analyzer", &format!("echo ran >> '{}'; echo planted", marker.display()));
        fs::write(s.repo.join("rust-toolchain.toml"), format!("[toolchain]\nchannel = \"{}/tc\"\npath = \"{}/tc\"\n", s.repo.display(), s.repo.display())).unwrap();
        let log = s.bin.parent().unwrap().join("rustup.log");
        script(&s.bin, "rustup", &format!("echo \"$RUSTUP_TOOLCHAIN|$@\" >> '{}'; case \"$1\" in toolchain) echo stable-x;; *) exit 1;; esac", log.display()));
        script(&s.bin, "rust-analyzer", &format!("echo \"[$RUSTUP_TOOLCHAIN]\" >> '{}'; exit 1", log.display()));
        let _ = locate_with(&s.repo, &s.env);
        let seen = fs::read_to_string(&log).unwrap();
        assert!(!marker.exists() && !seen.contains(s.repo.to_str().unwrap()) && !seen.contains("/tc"), "{seen}");
        assert!(seen.contains("[]"), "the shim must be probed with no toolchain override: {seen}");
    }

    #[test]
    fn test_DW_2_5_a_rustup_answer_pointing_into_the_repository_is_not_run() {
        let s = sb();
        let marker = s.repo.join("marker");
        let planted = script(&s.repo, "ra", &format!("echo ran >> '{}'; echo planted", marker.display()));
        script(&s.bin, "rustup", &format!("case \"$1\" in toolchain) echo stable-x;; which) echo '{}';; esac", planted.display()));
        let e = locate_with(&s.repo, &s.env).unwrap_err();
        assert!(!marker.exists() && e.to_string().contains("skipped: inside the repository"), "{e}");
    }

    #[test]
    fn test_DW_2_4_public_entry_point_has_the_contract_signature() {
        let f: fn(&Path) -> Result<Located, LocateError> = locate_rust_analyzer;
        let _ = f;
        let l = Located { path: "/x".into(), version: "v".into(), source: "fallback: y".into() };
        assert!(l.is_fallback());
    }

    #[test]
    fn test_DW_2_4_neutral_cwd_is_root_never_the_repository_or_a_temp_dir() {
        let s = sb();
        assert_eq!(Env::from_process(vec![s.repo.clone()]).probe_cwd, PathBuf::from("/"));
        let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
        assert_eq!(Env::from_process(vec![tmp]).probe_cwd, PathBuf::from("/"));
        assert_eq!(Env::from_process(vec![]).probe_cwd, PathBuf::from("/"));
    }

    #[test]
    fn planted_toolchain_and_cargo_files_in_a_shared_dir_never_reach_a_probe() {
        let mut s = sb();
        let planted = s._d.path().join("shared-tmp");
        let marker = planted.join("ran");
        fs::create_dir_all(planted.join(".cargo")).unwrap();
        fs::write(planted.join("rust-toolchain.toml"), format!("[toolchain]\npath = \"{}\"\n", planted.display())).unwrap();
        fs::write(planted.join("rust-toolchain"), "nightly-planted\n").unwrap();
        fs::write(planted.join(".cargo/config.toml"), format!("[build]\nrustc-wrapper = \"{}\"\n", marker.display())).unwrap();
        let ra = script(&s.bin, "rust-analyzer", "echo \"cwd=$(pwd -P)\"; if [ -e rust-toolchain.toml ]; then echo SAW; fi");
        // Control: started in the planted dir, the probe sees the file, so the check below can fail.
        s.env.probe_cwd = planted;
        assert_eq!(run(&mut Command::new(&ra), &s.env, None).unwrap().lines().nth(1), Some("SAW"));
        s.env.probe_cwd = Env::from_process(vec![]).probe_cwd;
        let out = run(&mut Command::new(&ra), &s.env, None).unwrap();
        assert_eq!(out, "cwd=/", "{out}");
        assert!(!marker.exists());
    }
}
