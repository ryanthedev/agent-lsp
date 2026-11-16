//! `alsp-coverage`: observed line and branch coverage of the symbols a diff changes.
//!
//! [`observe`] is the interface: it finds the Cargo packages that own the changed files, runs
//! `cargo llvm-cov` for each (reusing a cached report while the package's workspace content hash is
//! unchanged), and returns an [`Observed`] that answers two questions: what to write into each
//! symbol's `coverage` ([`Observed::attach`]) and whether a span of lines ran
//! ([`Observed::lines_status`], used to decide which mutants no test reaches).
//!
//! A package whose tests cannot run (it does not compile, `cargo llvm-cov` is missing) is
//! *unavailable*, never zero: its symbols get no coverage, the reason goes into the pack's notes,
//! and its lines answer [`Hit::Unknown`]. A package that has no tests but builds is measured, and
//! honestly shows nothing hit. A package whose tests build but some of them fail is measured too
//! (`--ignore-run-fail`), with a note naming the failing tests; that report is never cached.

pub mod exec;
mod lcov;
mod packages;

pub use lcov::FileCov;

use alsp_evidence::{ChangeKind, Coverage, EvidencePack, PackSource};
use alsp_index::{tree_hash, Span};
use packages::{Package, Resolver};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What cargo prints on stderr just before it blocks on another cargo's lock.
const LOCK_MARKER: &str = "waiting for file lock";

/// Bump when the cached file format or the way it is read changes.
const SCHEMA: &str = "alsp-coverage-v1";

#[derive(Clone, Debug)]
pub struct CoverageTool {
    /// `cargo`; looked up on `PATH` when not a path (`$ALSP_CARGO` overrides the default).
    pub program: PathBuf,
    /// Per package: building with instrumentation and running its tests.
    pub timeout: Duration,
    /// Told (one line, no prefix) before a step that can take minutes starts or stalls: running
    /// the tests, or cargo waiting on another cargo's lock. Silent by default; the CLI prints it.
    pub notify: fn(&str),
}

impl Default for CoverageTool {
    fn default() -> Self {
        CoverageTool { program: std::env::var_os("ALSP_CARGO").map(PathBuf::from).unwrap_or_else(|| "cargo".into()), timeout: Duration::from_secs(45 * 60), notify: |_| {} }
    }
}

/// Files under a `tests`, `benches` or `examples` directory: `cargo llvm-cov` leaves them out of its
/// report and cargo-mutants never mutates them, so there is nothing to measure or list there.
pub fn is_test_support_path(file: &str) -> bool {
    let mut dirs: Vec<&str> = file.split('/').collect();
    dirs.pop();
    dirs.iter().any(|d| matches!(*d, "tests" | "benches" | "examples"))
}

/// Did the tests run these lines?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// At least one instrumented line in the range ran.
    Yes,
    /// There are instrumented lines in the range and none of them ran.
    No,
    /// No answer: no measurement for the file, or no instrumented line in the range.
    Unknown,
}

/// One package's measurement, for the report a human reads.
#[derive(Clone, Debug)]
pub struct Run {
    pub package: String,
    pub cached: bool,
    pub millis: u64,
    /// Why there is no coverage; `None` when measured.
    pub error: Option<String>,
    /// Measured, but some tests failed: the coverage is real and may under-count what they reach.
    pub degraded: bool,
}

#[derive(Debug, Default)]
pub struct Observed {
    files: HashMap<String, FileCov>,
    /// Files whose measurement carries branch counts.
    with_branches: HashSet<String>,
    notes: Vec<String>,
    runs: Vec<Run>,
}

struct Measured {
    files: HashMap<String, FileCov>,
    branches: bool,
    cached: bool,
    /// Tests failed during the run that produced this report (never set for a cached one).
    failed: Option<Failed>,
}

/// How many failing test names a note lists before "and N more".
const MAX_NAMES: usize = 5;
/// Longest panic text a note quotes, in characters.
const MAX_PANIC: usize = 200;

/// The failing tests libtest reported on a run's stdout.
#[derive(Debug, PartialEq, Eq)]
struct Failed {
    /// Empty when only a `test result: FAILED` line was recognised.
    names: Vec<String>,
    /// The first panic message found, with its location.
    first_panic: Option<String>,
}

impl Failed {
    /// `None` when the output shows no failure.
    fn parse(stdout: &str) -> Option<Failed> {
        let mut names: Vec<String> = Vec::new();
        for line in stdout.lines() {
            if let Some(n) = line.trim_end().strip_prefix("test ").and_then(|l| l.strip_suffix(" ... FAILED")) {
                if !names.iter().any(|x| x == n) {
                    names.push(n.to_string());
                }
            }
        }
        if names.is_empty() && !stdout.contains("test result: FAILED") {
            return None;
        }
        Some(Failed { first_panic: first_panic(stdout), names })
    }

    /// One line a person or agent can act on.
    fn summary(&self) -> String {
        let tests = match self.names.len() {
            0 => "some tests failed (names not parsed)".to_string(),
            n => {
                let shown = self.names.iter().take(MAX_NAMES).map(String::as_str).collect::<Vec<_>>().join(", ");
                let more = if n > MAX_NAMES { format!(" and {} more", n - MAX_NAMES) } else { String::new() };
                format!("{n} test{} failed: {shown}{more}", if n == 1 { "" } else { "s" })
            }
        };
        match &self.first_panic {
            Some(p) => format!("{tests} - first panic: `{p}`"),
            None => tests,
        }
    }
}

/// The first `panicked at` in a libtest `---- name stdout ----` block: the message, then the location.
/// Rust prints the message on the line after `panicked at <loc>:` (older versions on the same line).
fn first_panic(stdout: &str) -> Option<String> {
    let mut in_block = false;
    let mut lines = stdout.lines();
    while let Some(line) = lines.next() {
        if line.starts_with("---- ") && line.ends_with(" stdout ----") {
            in_block = true;
        } else if line == "failures:" {
            in_block = false;
        } else if in_block {
            let Some((_, rest)) = line.split_once("panicked at ") else { continue };
            let text = match rest.trim_end().strip_suffix(':') {
                Some(loc) => match lines.by_ref().map(str::trim).find(|l| !l.is_empty()) {
                    Some(msg) => format!("{msg} (at {loc})"),
                    None => loc.to_string(),
                },
                None => rest.trim().to_string(),
            };
            return Some(text.chars().take(MAX_PANIC).collect());
        }
    }
    None
}

/// Did cargo-llvm-cov refuse `flag` as an argument it does not know (a version older than the flag)?
fn rejects_flag(stderr: &str, flag: &str) -> bool {
    let e = stderr.to_lowercase();
    e.contains(flag) && ["invalid", "unexpected", "unknown", "unrecognized"].iter().any(|w| e.contains(w))
}

/// `cargo llvm-cov` for one package, cached under `<cache_dir>/coverage/`.
fn measure(tool: &CoverageTool, pkg: &Package, src: &PackSource) -> Result<Measured, String> {
    let env = src.build_env.iter().map(|(k, v)| (k, v));
    let mut version = Command::new(&tool.program);
    version.args(["llvm-cov", "--version"]).current_dir(&pkg.workspace_root).envs(env.clone());
    let v = exec::run(&mut version, Duration::from_secs(60), 400)?;
    if !v.success() {
        return Err(format!("cargo-llvm-cov is not usable (install with `cargo install cargo-llvm-cov`): {}", v.stderr));
    }
    let salt = format!("{SCHEMA}\n{}\n{}", v.stdout.trim(), pkg.name);
    let hash = tree_hash(&pkg.workspace_root, &salt).map_err(|e| format!("cannot hash {}: {e}", pkg.workspace_root.display()))?;
    let dir = src.cache_dir.join("coverage");
    let cached = dir.join(format!("{}.{hash}.lcov", pkg.name));
    if let Ok(text) = std::fs::read_to_string(&cached) {
        let branches = text.starts_with("# alsp branches=1");
        return Ok(Measured { files: lcov::parse(&text), branches, cached: true, failed: None });
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    (tool.notify)(&format!(
        "coverage {}: no cached report for the current sources; running its tests under `cargo llvm-cov` (a cold instrumented build takes minutes, the report is then cached for unchanged code)",
        pkg.name
    ));
    let out_path = dir.join(format!("{}.{hash}.lcov.tmp-{}", pkg.name, std::process::id()));
    let attempt = |branch: bool, ignore_run_fail: bool| -> Result<exec::Output, String> {
        let mut cmd = Command::new(&tool.program);
        cmd.arg("llvm-cov").args(["--lcov", "--output-path"]).arg(&out_path).args(["-p", &pkg.name]);
        if ignore_run_fail {
            // Failing tests still yield a report (and the other test binaries still run).
            cmd.arg("--ignore-run-fail");
        }
        if branch {
            cmd.arg("--branch");
        }
        cmd.current_dir(&pkg.workspace_root).envs(src.build_env.iter().map(|(k, v)| (k, v)));
        let (notify, name) = (tool.notify, pkg.name.clone());
        let lock = exec::Watch {
            marker: LOCK_MARKER,
            notify: Box::new(move || notify(&format!("coverage {name}: cargo is waiting for a file lock held by another cargo process (a build or test run in the same target directory); it continues when that one ends"))),
        };
        exec::run_watching(&mut cmd, tool.timeout, 1500, Some(lock))
    };
    // Each optional flag is dropped at most once, when this cargo-llvm-cov rejects it.
    let (mut branches, mut ignore_run_fail) = (true, true);
    let mut out = attempt(branches, ignore_run_fail)?;
    while !out.success() {
        if ignore_run_fail && rejects_flag(&out.stderr, "--ignore-run-fail") {
            // An older cargo-llvm-cov: a failing test then costs the report, as it always did.
            ignore_run_fail = false;
        } else if branches && out.stderr.contains("--branch") && out.stderr.to_lowercase().contains("nightly") {
            // The toolchain cannot count branches: line coverage is still worth having.
            branches = false;
        } else {
            break;
        }
        out = attempt(branches, ignore_run_fail)?;
    }
    let failed = if out.success() { Failed::parse(&out.stdout) } else { None };
    if !out.success() {
        let _ = std::fs::remove_file(&out_path);
        let tests = Failed::parse(&out.stdout).map(|f| format!("; {}", f.summary())).unwrap_or_default();
        return Err(match out.code {
            Some(c) => format!("`cargo llvm-cov -p {}` exited {c}: {}{tests}", pkg.name, out.stderr),
            None => format!("`cargo llvm-cov -p {}` was killed by a signal: {}{tests}", pkg.name, out.stderr),
        });
    }
    let body = std::fs::read_to_string(&out_path).map_err(|e| format!("cargo llvm-cov wrote no report: {e}"))?;
    let _ = std::fs::remove_file(&out_path);
    if failed.is_none() {
        // Only a clean run is cached: a report made with failing tests must not be served later as
        // if the tests passed (the next run, perhaps in a fixed environment, measures afresh).
        let stored = format!("# alsp branches={}\n{body}", u8::from(branches));
        let tmp = dir.join(format!("{}.{hash}.lcov.stage-{}", pkg.name, std::process::id()));
        if std::fs::write(&tmp, &stored).and_then(|_| std::fs::rename(&tmp, &cached)).is_err() {
            let _ = std::fs::remove_file(&tmp); // an unwritable cache costs only the next run's time
        }
    }
    Ok(Measured { files: lcov::parse(&body), branches, cached: false, failed })
}

/// Measure every package that owns a changed, non-removed symbol of `pack`. Never fails: whatever
/// could not be measured is explained in [`Observed::notes`].
pub fn observe(pack: &EvidencePack, src: &PackSource, tool: &CoverageTool) -> Observed {
    let mut obs = Observed::default();
    let mut files: Vec<&str> = pack.symbols.iter().filter(|s| s.kind != ChangeKind::Removed && !s.id.ends_with('/') && s.file.ends_with(".rs")).map(|s| s.file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    let before = files.len();
    files.retain(|f| !is_test_support_path(f));
    if files.len() < before {
        obs.notes.push(format!("coverage is not measured for {} changed file(s) under tests/, benches/ or examples/ (cargo llvm-cov excludes them)", before - files.len()));
    }
    if files.is_empty() {
        return obs;
    }
    let canon = std::fs::canonicalize(&src.tree).unwrap_or_else(|_| src.tree.clone());
    let mut resolver = Resolver::new(&tool.program, &src.build_env, &src.tree);
    let mut by_pkg: BTreeMap<(String, PathBuf), (Package, Vec<&str>)> = BTreeMap::new();
    for f in files {
        match resolver.package_of(f) {
            Ok(p) => by_pkg.entry((p.name.clone(), p.workspace_root.clone())).or_insert_with(|| (p, Vec::new())).1.push(f),
            Err(e) => obs.notes.push(format!("coverage unavailable for {f}: {e}")),
        }
    }
    for (_, (pkg, pkg_files)) in by_pkg {
        let started = Instant::now();
        let result = measure(tool, &pkg, src);
        let millis = started.elapsed().as_millis() as u64;
        match result {
            Ok(m) => {
                obs.runs.push(Run { package: pkg.name.clone(), cached: m.cached, millis, error: None, degraded: m.failed.is_some() });
                if let Some(f) = &m.failed {
                    obs.notes.push(format!(
                        "coverage for package {} was observed, but {}; lines those tests reach may be under-counted; fix the cause (often a missing environment variable the tests expect) and rerun",
                        pkg.name,
                        f.summary()
                    ));
                }
                if !m.branches {
                    obs.notes.push(format!("branch coverage is not supported by this toolchain: package {} has line coverage only", pkg.name));
                }
                for (abs, cov) in m.files {
                    let p = Path::new(&abs);
                    if let Some(rel) = p.strip_prefix(&canon).or_else(|_| p.strip_prefix(&src.tree)).ok().and_then(|r| r.to_str()) {
                        let rel = rel.replace('\\', "/");
                        if m.branches {
                            obs.with_branches.insert(rel.clone());
                        }
                        obs.files.insert(rel, cov);
                    }
                }
            }
            Err(reason) => {
                obs.runs.push(Run { package: pkg.name.clone(), cached: false, millis, error: Some(reason.clone()), degraded: false });
                obs.notes.push(format!("coverage unavailable for package {} ({} changed file(s)): {reason}", pkg.name, pkg_files.len()));
            }
        }
    }
    obs
}

impl Observed {
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// Whether the tests ran the lines `from..=to` of `file`.
    pub fn lines_status(&self, file: &str, from: u32, to: u32) -> Hit {
        let Some(f) = self.files.get(file) else { return Hit::Unknown };
        let mut seen = false;
        for (_, &count) in f.lines.range(from..=to) {
            if count > 0 {
                return Hit::Yes;
            }
            seen = true;
        }
        if seen { Hit::No } else { Hit::Unknown }
    }

    /// Coverage of the instrumented lines inside `span`; `None` when there are none (a struct, a
    /// file nothing was measured for). `changed` marks the diff's lines for `uncovered_changed_lines`.
    pub fn coverage(&self, file: &str, span: Span, changed: impl Fn(u32) -> bool) -> Option<Coverage> {
        let f = self.files.get(file)?;
        let in_span = f.lines.range(span.start..=span.end);
        let (mut total, mut hit, mut uncovered) = (0u32, 0u32, Vec::new());
        for (&line, &count) in in_span {
            total += 1;
            if count > 0 {
                hit += 1;
            } else if changed(line) {
                uncovered.push(line);
            }
        }
        if total == 0 {
            return None;
        }
        let (branches_total, branches_hit) = if self.with_branches.contains(file) {
            let b: Vec<&bool> = f.branches.range(span.start..=span.end).flat_map(|(_, v)| v).collect();
            (Some(b.len() as u32), Some(b.iter().filter(|h| ***h).count() as u32))
        } else {
            (None, None)
        };
        Some(Coverage { lines_total: total, lines_hit: hit, branches_total, branches_hit, uncovered_changed_lines: uncovered })
    }

    /// Fill `coverage` of every changed symbol the measurement covers, and add this run's notes.
    pub fn attach(&self, pack: &mut EvidencePack, src: &PackSource) {
        for s in pack.symbols.iter_mut().filter(|s| s.kind != ChangeKind::Removed && !s.id.ends_with('/')) {
            s.coverage = self.coverage(&s.file, s.span, |l| src.is_changed(&s.file, l));
        }
        pack.notes.extend(self.notes.iter().cloned());
    }
}

#[cfg(feature = "testkit")]
pub mod testkit;

#[cfg(test)]
mod tests;
