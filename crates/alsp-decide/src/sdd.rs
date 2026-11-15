//! The decision model: batches go to `bun run <sdd>/src/cli/decide.ts`, which
//! runs sdd's gemini adapter through sdd's own job code (budget check, spend
//! record). Request and results travel as files in a private temp directory.
//! Format: sdd `assets/gemini-decide.ts`, `docs/usage-contract.md` 3a.

use crate::{DecideError, DecisionModel, Item, ItemScore, Question};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const QUESTION_NAME: &str = "q";
const DECIDE_SCRIPT: &str = "src/cli/decide.ts";
/// sdd's default per-job item cap; a bigger batch is split into jobs of this size.
const SDD_DEFAULT_MAX_ITEMS: usize = 500;
/// ~233k tokens at 3 chars/token, under the model's 262,144-token input limit.
const MAX_INPUT_CHARS: usize = 700_000;
const MAX_REASON_CHARS: usize = 300;
const MAX_RESULTS_BYTES: u64 = 32 << 20;
/// Appended to an input that was cut to fit the model's limit.
pub const TRUNCATION_MARKER: &str = "\n[truncated by alsp]";

/// Decision model backed by sdd's `decide` command.
#[derive(Debug, Clone)]
pub struct SddDecider {
    bun: PathBuf,
    decide_ts: PathBuf,
    timeout: Duration,
    /// How long past `timeout` the child may run before alsp kills it
    /// (sdd enforces `timeout` itself; this only guards a wedged sdd).
    grace: Duration,
    /// After SIGTERM (sdd settles the job and writes its spend line), how long before SIGKILL.
    term_wait: Duration,
    max_items: usize,
    /// Run-level notes (a partial run) for the caller to collect with `take_notes`.
    notes: Arc<Mutex<Vec<String>>>,
}

impl SddDecider {
    /// `sdd_root` must be an sdd checkout (holds `src/cli/decide.ts`).
    pub fn new(bun: impl Into<PathBuf>, sdd_root: &Path) -> Self {
        Self {
            bun: bun.into(),
            decide_ts: sdd_root.join(DECIDE_SCRIPT),
            timeout: Duration::from_secs(600),
            grace: Duration::from_secs(30),
            term_wait: Duration::from_secs(10),
            max_items: SDD_DEFAULT_MAX_ITEMS,
            notes: Arc::default(),
        }
    }

    /// bun from `ALSP_BUN`, else `PATH`, else `~/.bun/bin/bun`; sdd from
    /// `ALSP_SDD_ROOT`, else `~/repos/sdd`. Fails with the reason if either is missing.
    pub fn from_environment() -> Result<Self, DecideError> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let root = resolve_root(env_text("ALSP_SDD_ROOT").as_deref(), home.as_deref())?;
        let bun = resolve_bun(env_text("ALSP_BUN").as_deref(), std::env::var_os("PATH").as_deref(), home.as_deref())?;
        Ok(Self::new(bun, &root))
    }

    /// Deadline passed to sdd (`--timeout-ms`) for every job.
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t.max(Duration::from_millis(1));
        self
    }

    pub fn with_grace(mut self, g: Duration) -> Self {
        self.grace = g;
        self
    }

    pub fn with_term_wait(mut self, w: Duration) -> Self {
        self.term_wait = w;
        self
    }

    /// Items per sdd job, clamped to 1..=10000 (sdd's hard ceiling). Above 500 the
    /// request raises sdd's `maxItems`.
    pub fn with_max_items(mut self, n: usize) -> Self {
        self.max_items = n.clamp(1, 10_000);
        self
    }

    /// One sdd job. `failure` is set when sdd failed but left usable answers (unanswered items
    /// already carry it); `stopped` when sdd stopped early on its own (budget cap, deadline).
    fn run_job(&self, instructions: &str, items: &[Item]) -> Result<Job, DecideError> {
        // Items alsp can refuse itself are never sent (sdd would reject the whole request).
        let sendable: Vec<&Item> = items.iter().filter(|i| !i.input.trim().is_empty()).collect();
        if sendable.is_empty() {
            return Ok(Job { scores: items.iter().map(|i| fail(&i.id, "empty input".into())).collect(), failure: None, stopped: None });
        }
        let dir = tempfile::Builder::new().prefix("alsp-decide-").tempdir().map_err(|e| unavailable(format!("cannot create a private temp dir: {e}")))?;
        // Owner-only whatever the platform default: the request holds function bodies.
        std::fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).map_err(|e| unavailable(format!("cannot secure the temp dir: {e}")))?;
        let (request, results) = (dir.path().join("request.json"), dir.path().join("results.json"));
        let mut req = json!({
            "questions": { QUESTION_NAME: { "type": "noul", "instructions": instructions } },
            "items": sendable.iter().map(|i| json!({ "id": i.id, "input": truncate(&i.input) })).collect::<Vec<_>>(),
        });
        if sendable.len() > SDD_DEFAULT_MAX_ITEMS {
            req["maxItems"] = json!(sendable.len());
        }
        write_private(&request, req.to_string().as_bytes()).map_err(|e| unavailable(format!("cannot write the request file: {e}")))?;

        let failure = self.run_decide(&request, &results)?;
        let (answered, stopped) = match read_results(&results) {
            Ok(r) => r,
            // A clean run whose results cannot be read is a failure; a failed run's file is optional.
            Err(e) if failure.is_none() => return Err(DecideError::Failed(format!("sdd results unusable: {e}"))),
            Err(_) => return Err(DecideError::Failed(failure.unwrap_or_default())),
        };
        let missing = failure.clone().unwrap_or_else(|| "sdd returned no answer for this item".into());
        let scores = items
            .iter()
            .map(|i| match answered.get(&i.id) {
                _ if i.input.trim().is_empty() => fail(&i.id, "empty input".into()),
                Some(Ok((p, tokens))) => ItemScore { id: i.id.clone(), p: Some(*p), error: None, input_tokens: *tokens },
                Some(Err(e)) => fail(&i.id, e.clone()),
                None => fail(&i.id, missing.clone()),
            })
            .collect();
        Ok(Job { scores, failure, stopped })
    }

    /// Runs the command; `Ok(None)` when it exited 0, `Ok(Some(reason))` when the job failed.
    fn run_decide(&self, request: &Path, results: &Path) -> Result<Option<String>, DecideError> {
        let mut child = Command::new(&self.bun)
            .arg("run")
            .arg(&self.decide_ts)
            .arg("--timeout-ms")
            .arg(self.timeout.as_millis().to_string())
            .arg("--")
            .arg(request)
            .arg(results)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => unavailable(format!("bun not found at {}", self.bun.display())),
                _ => unavailable(format!("cannot run {}: {e}", self.bun.display())),
            })?;
        let stderr = drain(child.stderr.take());
        let _stdout = drain(child.stdout.take());
        let deadline = Instant::now() + self.timeout + self.grace;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    self.stop(&mut child);
                    return Ok(Some(format!("timed out after {} ms", self.timeout.as_millis())));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(unavailable(format!("cannot wait for bun: {e}"))),
            }
        };
        if status.success() {
            return Ok(None);
        }
        let text = stderr.join().unwrap_or_default();
        Ok(Some(reason(&text, &status.to_string())))
    }
}

impl SddDecider {
    /// SIGTERM first: sdd settles the job and writes its spend line on it. SIGKILL only if
    /// the child is still there after `term_wait`.
    fn stop(&self, child: &mut std::process::Child) {
        // SAFETY: plain signal to the pid of a child we own and have not yet reaped.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let until = Instant::now() + self.term_wait;
        while Instant::now() < until {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct Job {
    scores: Vec<ItemScore>,
    failure: Option<String>,
    stopped: Option<String>,
}

impl DecisionModel for SddDecider {
    fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn score(&self, question: &Question, items: &[Item]) -> Result<Vec<ItemScore>, DecideError> {
        if question.instructions.trim().is_empty() {
            return Err(DecideError::InvalidInput("question has empty instructions".into()));
        }
        let mut seen = HashSet::new();
        for it in items {
            if it.id.is_empty() || !seen.insert(it.id.as_str()) {
                return Err(DecideError::InvalidInput(format!("item id {:?} is empty or duplicated", it.id)));
            }
        }
        // sdd's noul question takes instructions only; a criteria text is folded in.
        let instructions = match &question.criteria {
            Some(c) => format!("{}\n\n{c}", question.instructions),
            None => question.instructions.clone(),
        };
        let mut out = Vec::with_capacity(items.len());
        let mut stopped: Option<String> = None;
        let mut sdd_stop: Option<String> = None;
        for chunk in items.chunks(self.max_items) {
            if let Some(why) = &stopped {
                // Once a job fails (budget, timeout) later jobs would fail the same way: don't spend.
                out.extend(chunk.iter().map(|i| fail(&i.id, why.clone())));
                continue;
            }
            match self.run_job(&instructions, chunk) {
                Ok(job) => {
                    out.extend(job.scores);
                    stopped = job.failure;
                    if sdd_stop.is_none() {
                        sdd_stop = job.stopped;
                    }
                }
                Err(e) if out.is_empty() => return Err(e),
                Err(e) => {
                    let why = e.to_string();
                    out.extend(chunk.iter().map(|i| fail(&i.id, why.clone())));
                    stopped = Some(why);
                }
            }
        }
        if let Some(why) = sdd_stop {
            let answered = out.iter().filter(|s| s.p.is_some()).count();
            let note = format!("predicted gaps partial: {why} after {answered} of {} items", out.len());
            self.notes.lock().unwrap_or_else(|e| e.into_inner()).push(note);
        }
        Ok(out)
    }
}

/// Stands in for the model when it cannot be set up (no bun, no sdd): every call fails with the
/// original reason, so a caller can say why gaps are missing and still carry on.
pub struct Unavailable(pub DecideError);

impl DecisionModel for Unavailable {
    fn score(&self, _: &Question, _: &[Item]) -> Result<Vec<ItemScore>, DecideError> {
        Err(self.0.clone())
    }
}

/// The one way alsp builds its decision model: sdd's `decide` command.
pub fn default_model() -> Result<Box<dyn DecisionModel>, DecideError> {
    Ok(Box::new(SddDecider::from_environment()?))
}

/// Like [`default_model`], but a setup failure becomes a model whose every call fails with it.
pub fn default_model_or_unavailable() -> Box<dyn DecisionModel> {
    default_model().unwrap_or_else(|e| Box::new(Unavailable(e)))
}

fn unavailable(m: String) -> DecideError {
    DecideError::Unavailable(m)
}

fn fail(id: &str, error: String) -> ItemScore {
    ItemScore { id: id.to_string(), p: None, error: Some(error), input_tokens: 0 }
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// sdd checkout: `override_root`, else `<home>/repos/sdd`; must hold `src/cli/decide.ts`.
fn resolve_root(override_root: Option<&str>, home: Option<&Path>) -> Result<PathBuf, DecideError> {
    let root = match (override_root, home) {
        (Some(r), _) => PathBuf::from(r),
        (None, Some(h)) => h.join("repos/sdd"),
        (None, None) => return Err(unavailable("sdd not found: set ALSP_SDD_ROOT (HOME is not set)".into())),
    };
    if root.join(DECIDE_SCRIPT).is_file() {
        Ok(root)
    } else {
        Err(unavailable(format!("sdd not found: {} is missing (set ALSP_SDD_ROOT to an sdd checkout)", root.join(DECIDE_SCRIPT).display())))
    }
}

/// bun binary: `override_bun` as given, else the first `bun` on `path`, else `<home>/.bun/bin/bun`.
fn resolve_bun(override_bun: Option<&str>, path: Option<&OsStr>, home: Option<&Path>) -> Result<PathBuf, DecideError> {
    if let Some(b) = override_bun {
        return Ok(PathBuf::from(b));
    }
    let on_path = path.into_iter().flat_map(std::env::split_paths).map(|d| d.join("bun"));
    let fallback = home.map(|h| h.join(".bun/bin/bun"));
    on_path.chain(fallback).find(|p| p.is_file()).ok_or_else(|| unavailable("bun not found on PATH or in ~/.bun/bin (set ALSP_BUN)".into()))
}

/// Cuts an input to the model's limit on a char boundary and marks the cut.
fn truncate(input: &str) -> std::borrow::Cow<'_, str> {
    match input.char_indices().nth(MAX_INPUT_CHARS) {
        None => input.into(),
        Some((cut, _)) => format!("{}{TRUNCATION_MARKER}", &input[..cut]).into(),
    }
}

/// Owner-only file: function bodies are in it.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(bytes)
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = pipe {
            let _ = p.take(64 << 10).read_to_string(&mut s);
        }
        s
    })
}

/// Bounded one-line reason from sdd's stderr. Real shape:
/// `decide: gemini: exited with code 1: gemini-decide: decision budget exceeded: $X ... (log: /p)`
/// shows as `decision budget exceeded: $X ...`. Control characters are dropped (it is text from
/// another process).
fn reason(stderr: &str, status: &str) -> String {
    let line = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut line = line.strip_prefix("decide:").unwrap_or(line).trim_start();
    if let Some(rest) = line.strip_prefix("gemini:").map(str::trim_start).and_then(|r| r.strip_prefix("exited with code ")) {
        // `N:` then the script's own text; without the colon keep the line as it was.
        if let Some((_, tail)) = rest.split_once(':').filter(|(c, _)| c.bytes().all(|b| b.is_ascii_digit())) {
            line = tail.trim_start();
        }
    }
    let line = line.strip_prefix("gemini-decide:").unwrap_or(line).trim_start();
    let mut line = line.trim_end();
    // Trailing `(log: ...)` / `(results: ...)` pointers into sdd's own files.
    while let Some(open) = line.rfind(" (").filter(|&i| line.ends_with(')') && (line[i + 2..].starts_with("log: ") || line[i + 2..].starts_with("results: "))) {
        line = line[..open].trim_end();
    }
    let clean: String = line.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        return format!("sdd decide failed ({status})");
    }
    let mut s: String = clean.chars().take(MAX_REASON_CHARS).collect();
    if clean.chars().count() > MAX_REASON_CHARS {
        s.push('…');
    }
    s
}

/// How sdd's top-level `stopped` value reads in a note.
fn stop_text(stopped: &str) -> String {
    match stopped {
        "budget" => "budget reached".into(),
        "deadline" => "deadline reached".into(),
        other => format!("stopped ({})", other.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(20).collect::<String>()),
    }
}

type Answer = Result<(f64, u64), String>;

/// The barricade for sdd's results file: id -> `(P(yes), input tokens)` or that item's error.
/// Unknown ids are the caller's to ignore; a repeated id keeps its first entry.
fn read_results(path: &Path) -> Result<(HashMap<String, Answer>, Option<String>), String> {
    let len = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?.len();
    if len > MAX_RESULTS_BYTES {
        return Err(format!("results file is {len} bytes"));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read results: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("not JSON ({e})"))?;
    let list = v.get("results").and_then(Value::as_array).ok_or("no `results` array")?;
    let mut out = HashMap::new();
    for r in list {
        let Some(id) = r.get("id").and_then(Value::as_str) else { continue };
        out.entry(id.to_string()).or_insert_with(|| parse_one(r));
    }
    let stopped = v.get("stopped").and_then(Value::as_str).map(stop_text);
    Ok((out, stopped))
}

fn parse_one(r: &Value) -> Answer {
    if let Some(e) = r.get("error") {
        let text = e.as_str().map_or_else(|| e.to_string(), str::to_string);
        return Err(reason(&text, "item error"));
    }
    let a = r.pointer(&format!("/answers/{QUESTION_NAME}")).filter(|a| a.is_object()).ok_or("no answer for the question")?;
    let p = a
        .get("p")
        .or_else(|| a.get("noul"))
        .and_then(Value::as_f64)
        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
        .ok_or("answer has no probability in [0,1]")?;
    // sdd reports null (never 0) when the API gave no usage: counted as 0 here.
    Ok((p, r.get("inputTokens").and_then(Value::as_u64).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_strips_prefixes_bounds_and_cleans() {
        assert_eq!(reason("decide: gemini-decide: budget exceeded: $5\n", "x"), "budget exceeded: $5");
        let real = "decide: gemini: exited with code 1: gemini-decide: decision budget exceeded: $5.10 in 24h of $5 daily (log: /tmp/a b/job.log)\n";
        assert_eq!(reason(real, "x"), "decision budget exceeded: $5.10 in 24h of $5 daily");
        assert_eq!(reason("decide: gemini: exited with code 2: boom (results: /r) (log: /l)", "x"), "boom");
        assert_eq!(reason("decide: gemini: exited with code x: odd", "x"), "gemini: exited with code x: odd");
        assert_eq!(reason("decide: cancelled by SIGTERM (log: /l)", "x"), "cancelled by SIGTERM");
        assert_eq!(reason("decide: keep (parenthetical) here", "x"), "keep (parenthetical) here");
        assert_eq!(reason("noise\ndecide: boom\n\n", "x"), "boom");
        assert_eq!(reason("a\u{1b}[31mb\u{7}", "x"), "a[31mb");
        assert_eq!(reason("", "exit status: 1"), "sdd decide failed (exit status: 1)");
        let long = reason(&"x".repeat(1000), "x");
        assert_eq!(long.chars().count(), MAX_REASON_CHARS + 1);
    }

    #[test]
    fn truncate_cuts_on_a_char_boundary_and_marks() {
        let big = "é".repeat(MAX_INPUT_CHARS + 5);
        let t = truncate(&big);
        assert_eq!(t.chars().count(), MAX_INPUT_CHARS + TRUNCATION_MARKER.chars().count());
        assert!(t.ends_with(TRUNCATION_MARKER));
        assert_eq!(truncate("abc"), "abc");
    }

    #[test]
    fn resolve_bun_prefers_override_then_path_then_home() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, home) = (d.path().join("a"), d.path().join("b"), d.path().join("home"));
        std::fs::create_dir_all(home.join(".bun/bin")).unwrap();
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(b.join("bun"), "").unwrap();
        std::fs::write(home.join(".bun/bin/bun"), "").unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(resolve_bun(Some("/x/bun"), Some(&path), None).unwrap(), PathBuf::from("/x/bun"));
        assert_eq!(resolve_bun(None, Some(&path), Some(&home)).unwrap(), b.join("bun"));
        assert_eq!(resolve_bun(None, None, Some(&home)).unwrap(), home.join(".bun/bin/bun"));
        let only_a = std::env::join_paths([&a]).unwrap();
        let e = resolve_bun(None, Some(&only_a), Some(&d.path().join("nohome"))).unwrap_err();
        assert!(e.to_string().contains("bun not found"), "{e}");
    }

    #[test]
    fn resolve_root_needs_decide_ts() {
        let d = tempfile::tempdir().unwrap();
        let e = resolve_root(Some(d.path().to_str().unwrap()), None).unwrap_err();
        assert!(e.to_string().contains("sdd not found") && e.to_string().contains("decide.ts"), "{e}");
        std::fs::create_dir_all(d.path().join("repos/sdd/src/cli")).unwrap();
        std::fs::write(d.path().join("repos/sdd/src/cli/decide.ts"), "").unwrap();
        assert_eq!(resolve_root(None, Some(d.path())).unwrap(), d.path().join("repos/sdd"));
        assert!(resolve_root(None, None).unwrap_err().to_string().contains("ALSP_SDD_ROOT"));
    }

    #[test]
    fn read_results_keeps_first_duplicate_and_reports_shape_errors() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("r.json");
        std::fs::write(&p, r#"{"results":[{"id":"a","answers":{"q":{"p":0.5}}},{"id":"a","error":"late"},{"error":"no id"},{"id":"b","answers":{}}]}"#).unwrap();
        let (m, stopped) = read_results(&p).unwrap();
        assert_eq!(stopped, None);
        assert_eq!(m["a"], Ok((0.5, 0)));
        assert!(m["b"].is_err() && m.len() == 2);
        std::fs::write(&p, "{}").unwrap();
        assert!(read_results(&p).is_err());
        assert!(read_results(&d.path().join("missing.json")).is_err());
    }

    #[test]
    fn unavailable_model_fails_every_call_with_the_stored_error() {
        let e = DecideError::Unavailable("why".into());
        let q = Question { instructions: "q".into(), criteria: None };
        assert_eq!(Unavailable(e.clone()).score(&q, &[]), Err(e));
    }
}
