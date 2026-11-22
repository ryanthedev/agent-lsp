//! Append-only findings ledger, review records, verdicts, stats, and symlink-safe `.alsp/` I/O.

use crate::findings::Finding;
use alsp_evidence::EvidencePack;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerRow {
    pub id: String,
    pub time: String,
    pub base: String,
    pub head: String,
    pub source: String,
    pub file: String,
    pub line: u32,
    #[serde(default)]
    pub severity: String,
    pub text: String,
    pub verdict: Option<String>,
}

fn ledger_path(repo: &Path) -> PathBuf {
    repo.join(".alsp/findings.jsonl")
}

fn refuse(path: &Path, what: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{} is a symlink or not a regular {what}; refusing to write through it", path.display()),
    )
}

/// A directory that exists as a real directory (never a symlink) or is created now.
pub(crate) fn ensure_dir(path: &Path) -> std::io::Result<()> {
    for _ in 0..2 {
        // the second pass re-checks what a concurrent creator left behind
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => return Ok(()),
            Ok(_) => return Err(refuse(path, "directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            },
            Err(e) => return Err(e),
        }
    }
    Err(refuse(path, "directory"))
}

/// Opens a regular file for writing without following a symlink (pre-check for a clear error, O_NOFOLLOW against races on the last component).
pub(crate) fn open_nofollow(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() => return Err(refuse(path, "file")),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).custom_flags(libc::O_NOFOLLOW);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    o.open(path)
}

pub(crate) fn ensure_alsp(repo: &Path) -> std::io::Result<PathBuf> {
    let dir = repo.join(".alsp");
    ensure_dir(&dir)?;
    let ignore = dir.join(".gitignore");
    // Refuse a symlink or non-regular file before reading it: a symlink whose target already holds "*\n" must not be accepted.
    let current = match std::fs::symlink_metadata(&ignore) {
        Ok(m) if !m.is_file() => return Err(refuse(&ignore, "file")),
        Ok(_) => std::fs::read(&ignore).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if current.as_deref() != Some(b"*\n") {
        open_nofollow(&ignore, false)?.write_all(b"*\n")?;
    }
    Ok(dir)
}

fn ensure_ledger(repo: &Path) -> std::io::Result<PathBuf> {
    ensure_alsp(repo)?;
    Ok(ledger_path(repo))
}

fn now() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().to_string()
}

pub fn append_findings(
    repo: &Path,
    base: &str,
    head: &str,
    findings: &[Finding],
) -> std::io::Result<()> {
    let path = ensure_ledger(repo)?;
    let mut f = open_nofollow(&path, true)?;
    for x in findings {
        for source in &x.sources {
            let row = LedgerRow {
                id: x.id.clone(),
                time: now(),
                base: base.into(),
                head: head.into(),
                source: source.clone(),
                file: x.file.clone(),
                line: x.line,
                severity: x.severity.clone(),
                text: x.text.clone(),
                verdict: None,
            };
            writeln!(f, "{}", serde_json::to_string(&row).unwrap())?;
        }
    }
    Ok(())
}

/// What [`ledger_review`] did.
#[derive(Debug)]
pub enum Ledgered {
    /// The pack's index could not serve, so nothing was written.
    Skipped,
    /// Findings were appended. `record_error` is why the review record could not be written, if it
    /// could not (the next ad-hoc review then starts from an earlier point).
    Written { record_error: Option<std::io::Error> },
}

/// Ledgers a finished review: appends `findings` to the findings ledger and, when `record` is given,
/// notes the review so the next ad-hoc run starts after it. Does neither when the pack's index
/// could not serve (`!facts_served`): such a review saw no symbols, so it must neither add rows
/// that read as clean evidence nor move the "base = last review" mark past code nobody looked at.
/// A failure to append findings is an error; a failure to record is reported in the result.
pub fn ledger_review(
    repo: &Path,
    pack: &EvidencePack,
    findings: &[Finding],
    record: Option<ReviewRecord>,
) -> std::io::Result<Ledgered> {
    if !pack.facts_served() {
        return Ok(Ledgered::Skipped);
    }
    append_findings(repo, &pack.base, &pack.head, findings)?;
    let record_error = record.and_then(|rec| record_review(repo, rec).err());
    Ok(Ledgered::Written { record_error })
}

/// One finished `alsp review` of the checked-out branch, so the next ad-hoc run can start where it ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    /// Branch name; `(detached)` when HEAD was detached.
    pub branch: String,
    /// The checked-out commit at review time. The next run uses it to tell whether history was rewritten.
    pub head: String,
    /// The commit the review covered: `head`, or a work-tree snapshot commit when uncommitted work was included.
    pub reviewed: String,
    pub uncommitted: bool,
    #[serde(default)]
    pub time: String,
}

fn reviews_path(repo: &Path) -> PathBuf {
    repo.join(".alsp/reviews.jsonl")
}

/// Appends a review record to `.alsp/reviews.jsonl` (never through a symlink, like the findings ledger).
pub fn record_review(repo: &Path, mut rec: ReviewRecord) -> std::io::Result<()> {
    ensure_alsp(repo)?;
    rec.time = now();
    let mut f = open_nofollow(&reviews_path(repo), true)?;
    writeln!(f, "{}", serde_json::to_string(&rec).map_err(std::io::Error::other)?)
}

/// The most recent record for `branch`, `None` when there is none. A symlinked or non-regular file is an error.
pub fn last_review(repo: &Path, branch: &str) -> std::io::Result<Option<ReviewRecord>> {
    let path = reviews_path(repo);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if !m.is_file() => return Err(refuse(&path, "file")),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    // Unparseable lines (a torn append, an old format) are skipped, never fatal.
    Ok(std::fs::read_to_string(&path)?
        .lines()
        .filter_map(|l| serde_json::from_str::<ReviewRecord>(l).ok())
        .filter(|r| r.branch == branch)
        .next_back())
}

pub fn mark(repo: &Path, id: &str, verdict: &str) -> std::io::Result<usize> {
    let path = ensure_ledger(repo)?;
    let mut latest: BTreeMap<String, LedgerRow> = BTreeMap::new();
    for p in ledger_paths(repo) {
        if p.exists() {
            for r in read_rows(&p)?.into_iter().filter(|r| r.id == id) {
                latest.insert(r.source.clone(), r);
            }
        }
    }
    let matched: Vec<_> = latest.into_values().collect();
    let mut f = open_nofollow(&path, true)?;
    let mut n = 0;
    for r in matched {
        let row = LedgerRow { time: now(), verdict: Some(verdict.into()), ..r };
        writeln!(f, "{}", serde_json::to_string(&row).unwrap())?;
        n += 1;
    }
    Ok(n)
}

pub fn read_rows(path: &Path) -> std::io::Result<Vec<LedgerRow>> {
    let s = std::fs::read_to_string(path)?;
    Ok(s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

pub fn stats(repo: &Path) -> std::io::Result<BTreeMap<String, (usize, usize, usize)>> {
    let mut latest: BTreeMap<(String, String), LedgerRow> = BTreeMap::new();
    for path in ledger_paths(repo) {
        if path.exists() {
            for r in read_rows(&path)? {
                latest.insert((r.id.clone(), r.source.clone()), r);
            }
        }
    }
    let mut out = BTreeMap::new();
    for ((_, source), r) in latest {
        let c = out.entry(source).or_insert((0, 0, 0));
        match r.verdict.as_deref() {
            Some("held") => c.0 += 1,
            Some("not-held") => c.1 += 1,
            _ => c.2 += 1,
        }
    }
    Ok(out)
}

fn ledger_paths(repo: &Path) -> [PathBuf; 2] {
    [repo.join(".alsp/ledger.jsonl"), ledger_path(repo)]
}

pub fn list_findings(
    repo: &Path,
    source: Option<&str>,
    unmarked: bool,
) -> std::io::Result<Vec<LedgerRow>> {
    let mut latest: BTreeMap<(String, String), LedgerRow> = BTreeMap::new();
    for path in ledger_paths(repo) {
        if path.exists() {
            for r in read_rows(&path)? {
                latest.insert((r.id.clone(), r.source.clone()), r);
            }
        }
    }
    let mut grouped: BTreeMap<String, Vec<LedgerRow>> = BTreeMap::new();
    for r in latest.into_values() {
        grouped.entry(r.id.clone()).or_default().push(r);
    }
    let mut rows: Vec<_> = grouped
        .into_values()
        .filter_map(|mut group| {
            let selected: Vec<_> =
                group.iter().filter(|r| source.is_none_or(|s| r.source == s)).collect();
            if selected.is_empty()
                || (unmarked && !selected.iter().any(|r| r.verdict.is_none()))
            {
                return None;
            }
            group.sort_by(|a, b| a.source.cmp(&b.source));
            let mut row = group.remove(0);
            row.source = group.into_iter().fold(row.source, |mut all, r| {
                all.push(',');
                all.push_str(&r.source);
                all
            });
            Some(row)
        })
        .collect();
    rows.sort_by(|a, b| (&a.file, a.line, &a.id).cmp(&(&b.file, b.line, &b.id)));
    Ok(rows)
}

fn split_markdown_row(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.trim().trim_matches('|').chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            cell.push('|');
        } else if c == '|' {
            cells.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(c);
        }
    }
    cells.push(cell.trim().to_string());
    cells
}

pub fn import_gaps(repo: &Path, file: &Path) -> Result<usize, String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let mut findings = Vec::new();
    let mut verdicts = Vec::new();
    for line in text.lines().filter(|l| l.starts_with('|')) {
        let cols = split_markdown_row(line);
        if cols.len() < 7 || cols[0].parse::<usize>().is_err() {
            continue;
        }
        let loc = cols[2].split('`').nth(1).unwrap_or(&cols[2]);
        let Some((path, ln)) = loc.rsplit_once(':') else { continue };
        let Ok(n) = ln.split_whitespace().next().unwrap_or(ln).parse() else { continue };
        let verdict = if cols[4].contains("real gap") { "held" } else { "not-held" };
        findings.push(Finding::new("gap", path, n, "warning", &cols[5]));
        verdicts.push(verdict.to_string());
    }
    let ledger = ensure_ledger(repo).map_err(|e| e.to_string())?;
    let mut f = open_nofollow(&ledger, true).map_err(|e| e.to_string())?;
    for (x, v) in findings.iter().zip(verdicts) {
        for source in &x.sources {
            let row = LedgerRow {
                id: x.id.clone(),
                time: now(),
                base: "historical".into(),
                head: "imported".into(),
                source: source.clone(),
                file: x.file.clone(),
                line: x.line,
                severity: x.severity.clone(),
                text: x.text.clone(),
                verdict: Some(v.clone()),
            };
            writeln!(
                f,
                "{}",
                serde_json::to_string(&row).map_err(|e| e.to_string())?
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(findings.len())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use alsp_evidence::{ChangeKind, Coverage, GapReason, PredictedGap, SymbolEvidence};
    use alsp_index::{IndexState, IndexStatus, Span};

    fn report_pack() -> EvidencePack {
        EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: IndexState {
                status: IndexStatus::Fresh,
                rebuilt: false,
                units: vec![alsp_index::UnitReport { path: ".".into(), ok: true, detail: None }],
                millis: 0,
                notes: vec![],
            },
            symbols: vec![SymbolEvidence {
                id: "f".into(),
                name: "f".into(),
                file: "a.rs".into(),
                span: Span { start: 1, end: 4 },
                kind: ChangeKind::Modified,
                callers: vec![],
                entry_points: vec![],
                coverage: Some(Coverage {
                    lines_total: 4,
                    lines_hit: 3,
                    branches_total: None,
                    branches_hit: None,
                    uncovered_changed_lines: vec![2],
                }),
                gaps: vec![PredictedGap {
                    file: "a.rs".into(),
                    line: 3,
                    mutation: "mutant 1".into(),
                    p_tests_catch: 0.1,
                    reason: GapReason::PredictedSurvivor,
                }],
            }],
            notes: vec![],
        }
    }

    fn no_index_pack() -> EvidencePack {
        let mut p = report_pack();
        p.symbols.clear();
        p.index.status = IndexStatus::Missing;
        p.index.units = vec![alsp_index::UnitReport {
            path: ".".into(),
            ok: false,
            detail: Some("rust-analyzer not found".into()),
        }];
        p.index.notes = vec!["tried /x/ra".into()];
        p.notes = vec![
            "no index could be served, so no symbols are reported (an empty list here means unknown, not unchanged)"
                .into(),
        ];
        p
    }

    fn rec(branch: &str, head: &str) -> ReviewRecord {
        ReviewRecord {
            branch: branch.into(),
            head: head.into(),
            reviewed: head.into(),
            uncommitted: false,
            time: String::new(),
        }
    }

    #[test]
    fn test_DW_4_8_new_findings_file_reads_legacy_rows_without_rewriting_them() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join(".alsp");
        std::fs::create_dir_all(&dir).unwrap();
        let old = LedgerRow {
            id: "legacy-id".into(),
            time: "1".into(),
            base: "b".into(),
            head: "h".into(),
            source: "legacy-source".into(),
            file: "old.rs".into(),
            line: 4,
            severity: "high".into(),
            text: "keep me".into(),
            verdict: Some("held".into()),
        };
        let legacy = serde_json::to_string(&old).unwrap() + "\n";
        std::fs::write(dir.join("ledger.jsonl"), &legacy).unwrap();
        append_findings(d.path(), "b", "h", &[Finding::new("codex", "a.rs", 1, "low", "new")])
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("ledger.jsonl")).unwrap(), legacy);
        assert!(dir.join("findings.jsonl").exists());
        assert_eq!(stats(d.path()).unwrap().get("legacy-source"), Some(&(1, 0, 0)));
    }

    #[test]
    fn test_DW_4_5_ledger_appends_verdict_rows_and_stats_by_source() {
        let d = tempfile::tempdir().unwrap();
        let f = Finding::new("coverage", "src/a.rs", 4, "warning", "uncovered");
        append_findings(d.path(), "b", "h", &[f.clone()]).unwrap();
        assert_eq!(mark(d.path(), &f.id, "held").unwrap(), 1);
        assert_eq!(stats(d.path()).unwrap().get("coverage"), Some(&(1, 0, 0)));
        assert_eq!(std::fs::read_to_string(d.path().join(".alsp/.gitignore")).unwrap(), "*\n");
    }

    #[test]
    fn test_DW_4_7_imports_the_ten_historical_gap_verdicts() {
        let d = tempfile::tempdir().unwrap();
        let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/gaps-first-read.md");
        assert_eq!(import_gaps(d.path(), &doc).unwrap(), 10);
        let st = stats(d.path()).unwrap();
        assert_eq!(st.get("gap"), Some(&(5, 5, 0)));
    }

    #[test]
    fn test_DW_4_5_repeated_mark_appends_one_latest_verdict_per_source() {
        let d = tempfile::tempdir().unwrap();
        let f = Finding::new("claude", "src/a.rs", 9, "bug", "x");
        append_findings(d.path(), "b", "h", &[f.clone()]).unwrap();
        mark(d.path(), &f.id, "held").unwrap();
        mark(d.path(), &f.id, "not-held").unwrap();
        let rows = read_rows(&ledger_path(d.path())).unwrap();
        assert_eq!(rows.iter().filter(|r| r.verdict.is_some()).count(), 2);
        assert_eq!(stats(d.path()).unwrap().get("claude"), Some(&(0, 1, 0)));
    }

    #[test]
    fn list_findings_groups_sources_filters_and_honours_unmarked() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Finding::new("claude", "src/a.rs", 4, "high", "shared");
        f.sources.push("coverage".into());
        let g = Finding::new("gap", "src/b.rs", 1, "low", "solo");
        append_findings(d.path(), "b", "h", &[f.clone(), g.clone()]).unwrap();
        let all = list_findings(d.path(), None, false).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].source, "claude,coverage");
        assert_eq!(all[1].source, "gap");
        assert_eq!(list_findings(d.path(), Some("gap"), false).unwrap().len(), 1);
        assert!(list_findings(d.path(), Some("agy"), false).unwrap().is_empty());
        mark(d.path(), &f.id, "held").unwrap();
        let unmarked = list_findings(d.path(), None, true).unwrap();
        assert_eq!(unmarked.len(), 1);
        assert_eq!(unmarked[0].id, g.id);
    }

    #[test]
    fn split_markdown_row_handles_escaped_pipes_and_edges() {
        assert_eq!(split_markdown_row("| 1 | a\\|b |  c  |"), ["1", "a|b", "c"]);
        assert_eq!(split_markdown_row("|x|"), ["x"]);
        assert_eq!(split_markdown_row(""), [""]);
    }

    #[test]
    fn symlinked_ledger_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        std::fs::create_dir(d.path().join(".alsp")).unwrap();
        std::os::unix::fs::symlink(&victim, d.path().join(".alsp/findings.jsonl")).unwrap();
        assert!(
            append_findings(d.path(), "b", "h", &[Finding::new("claude", "a.rs", 1, "low", "x")])
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn symlinked_gitignore_with_matching_content_is_refused_and_target_untouched() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("victim.txt");
        std::fs::write(&victim, "*\n").unwrap();
        std::fs::create_dir(d.path().join(".alsp")).unwrap();
        std::os::unix::fs::symlink(&victim, d.path().join(".alsp/.gitignore")).unwrap();
        let e =
            append_findings(d.path(), "b", "h", &[Finding::new("claude", "a.rs", 1, "low", "x")])
                .unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "*\n");
        assert!(!d.path().join(".alsp/findings.jsonl").exists());
    }

    #[test]
    fn review_records_round_trip_per_branch_and_the_latest_wins() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(last_review(d.path(), "main").unwrap(), None, "no file");
        record_review(d.path(), rec("main", "a")).unwrap();
        record_review(d.path(), rec("feat", "x")).unwrap();
        record_review(
            d.path(),
            ReviewRecord { uncommitted: true, reviewed: "snap".into(), ..rec("main", "b") },
        )
        .unwrap();
        let m = last_review(d.path(), "main").unwrap().unwrap();
        assert_eq!((m.head.as_str(), m.reviewed.as_str(), m.uncommitted), ("b", "snap", true));
        assert!(!m.time.is_empty());
        assert_eq!(last_review(d.path(), "feat").unwrap().unwrap().head, "x");
        assert_eq!(last_review(d.path(), "other").unwrap(), None);
        assert_eq!(std::fs::read_to_string(d.path().join(".alsp/.gitignore")).unwrap(), "*\n");
    }

    #[test]
    fn last_review_skips_unparseable_lines() {
        let d = tempfile::tempdir().unwrap();
        record_review(d.path(), rec("main", "a")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(d.path().join(".alsp/reviews.jsonl"))
            .unwrap()
            .write_all(b"{torn\nnot json\n")
            .unwrap();
        assert_eq!(last_review(d.path(), "main").unwrap().unwrap().head, "a");
    }

    #[test]
    fn test_DW_1_6_a_no_index_review_writes_neither_the_findings_ledger_nor_the_review_record() {
        let d = tempfile::tempdir().unwrap();
        let f = vec![Finding::new("coverage", "a.rs", 2, "medium", "changed line is uncovered")];
        assert!(matches!(
            ledger_review(d.path(), &no_index_pack(), &f, Some(rec("main", "a"))).unwrap(),
            Ledgered::Skipped
        ));
        assert!(!d.path().join(".alsp").exists(), "nothing was written, not even the directory");
        assert_eq!(last_review(d.path(), "main").unwrap(), None);
        // the same call with a servable index writes both
        assert!(matches!(
            ledger_review(d.path(), &report_pack(), &f, Some(rec("main", "a"))).unwrap(),
            Ledgered::Written { record_error: None }
        ));
        assert_eq!(read_rows(&ledger_path(d.path())).unwrap().len(), 1);
        assert_eq!(last_review(d.path(), "main").unwrap().unwrap().head, "a");
        // no record requested (--print, or a range that is not recorded): findings only
        let e = tempfile::tempdir().unwrap();
        assert!(matches!(
            ledger_review(e.path(), &report_pack(), &f, None).unwrap(),
            Ledgered::Written { record_error: None }
        ));
        assert_eq!(last_review(e.path(), "main").unwrap(), None);
    }

    #[test]
    fn review_records_refuse_symlinks_in_both_directions() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        std::fs::create_dir(d.path().join(".alsp")).unwrap();
        std::os::unix::fs::symlink(&target, d.path().join(".alsp/reviews.jsonl")).unwrap();
        assert_eq!(
            record_review(d.path(), rec("main", "a")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            last_review(d.path(), "main").unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }
}
