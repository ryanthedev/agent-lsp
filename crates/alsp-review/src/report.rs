//! Changed-symbol summaries and text report rendering.

use crate::agents::AgentRun;
use crate::findings::{severity_rank, Finding};
use alsp_evidence::{escape, EvidencePack};
use serde::Serialize;
use std::collections::BTreeMap;

/// A symbol the reviewed range touched, in the form an agent feeds to `alsp cover` / `alsp gaps`
/// (`name`, or `file:line`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChangedSymbol {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub end_line: u32,
    pub change: &'static str,
}

/// The pack's changed symbols, ordered by file and line. Modules (regions, not code a test reaches) are left out.
pub fn changed_symbols(pack: &EvidencePack) -> Vec<ChangedSymbol> {
    let mut v: Vec<ChangedSymbol> = pack
        .symbols
        .iter()
        .filter(|s| !s.id.ends_with('/'))
        .map(|s| ChangedSymbol {
            name: s.name.clone(),
            file: s.file.clone(),
            line: s.span.start,
            end_line: s.span.end,
            change: match s.kind {
                alsp_evidence::ChangeKind::Added => "added",
                alsp_evidence::ChangeKind::Modified => "modified",
                alsp_evidence::ChangeKind::Removed => "removed",
            },
        })
        .collect();
    v.sort_by(|a, b| (&a.file, a.line, &a.name).cmp(&(&b.file, b.line, &b.name)));
    v
}

/// Rows of changed symbols the text report lists; `--format json` lists up to `CHANGED_SYMBOLS_JSON_CAP`.
pub const CHANGED_SYMBOLS_TEXT_CAP: usize = 12;
pub const CHANGED_SYMBOLS_JSON_CAP: usize = 25;

/// JSON for the report's `changed_symbols` and `changed_symbols_more` fields.
pub fn changed_symbols_json(pack: &EvidencePack) -> (Vec<ChangedSymbol>, usize) {
    let mut all = changed_symbols(pack);
    let more = all.len().saturating_sub(CHANGED_SYMBOLS_JSON_CAP);
    all.truncate(CHANGED_SYMBOLS_JSON_CAP);
    (all, more)
}

/// Pack notes the text report shows; the rest are counted, and `alsp pack` lists them all.
const NOTES_SHOWN: usize = 6;

pub fn render_report(
    pack: &EvidencePack,
    findings: &[Finding],
    dropped: usize,
    skipped: &[String],
    runs: &[AgentRun],
    split_crates: usize,
) -> String {
    const LINE_BUDGET: usize = 40 + CHANGED_SYMBOLS_TEXT_CAP + 2 + 1 + NOTES_SHOWN + 1;
    let mut out = vec![format!("Review {}..{}", escape(&pack.base), escape(&pack.head))];
    if !pack.index_is_clean() {
        out.push(pack.index_line());
    }
    out.extend(pack.notes.iter().take(NOTES_SHOWN).map(|n| format!("! {}", cap_text(n, 300))));
    if pack.notes.len() > NOTES_SHOWN {
        out.push(format!(
            "+{} more notes (alsp pack --base {})",
            pack.notes.len() - NOTES_SHOWN,
            escape(&pack.base)
        ));
    }
    let mut agent: Vec<_> = findings
        .iter()
        .filter(|f| f.sources.iter().any(|s| matches!(s.as_str(), "claude" | "codex" | "agy")))
        .collect();
    agent.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then_with(|| (&a.file, a.line).cmp(&(&b.file, b.line)))
    });
    let mech: Vec<_> = findings
        .iter()
        .filter(|f| f.sources.iter().any(|s| matches!(s.as_str(), "coverage" | "heuristic")))
        .collect();
    out.push("Agent findings:".into());
    let agent_budget = 16;
    for f in agent.iter().take(agent_budget) {
        out.push(format!(
            "{} {}:{} [{}] {} — {}",
            f.id,
            escape(&f.file),
            f.line,
            escape(&f.severity),
            f.sources.join(","),
            cap_text(&f.text, 220)
        ));
    }
    if agent.len() > agent_budget {
        out.push(format!("+{} more (alsp ledger list)", agent.len() - agent_budget));
    }
    let changed = changed_symbols(pack);
    out.push(if !pack.facts_served() {
        "Changed symbols: unknown (no index could be served; unknown, not unchanged)".into()
    } else if changed.is_empty() {
        "Changed symbols: none".into()
    } else {
        "Changed symbols (pass the name, or file:line, to alsp cover / alsp gaps):".into()
    });
    for c in changed.iter().take(CHANGED_SYMBOLS_TEXT_CAP) {
        out.push(format!(
            "{} {} {}:{}-{}",
            c.change,
            escape(&c.name),
            escape(&c.file),
            c.line,
            c.end_line
        ));
    }
    if changed.len() > CHANGED_SYMBOLS_TEXT_CAP {
        out.push(format!(
            "+{} more (alsp pack --base {})",
            changed.len() - CHANGED_SYMBOLS_TEXT_CAP,
            escape(&pack.base)
        ));
    }
    out.push("Mechanical findings:".into());
    let mut mechanical_by_symbol: BTreeMap<String, Vec<&Finding>> = BTreeMap::new();
    for f in mech {
        let name = symbol_for(pack, &f.file, f.line).map(|s| s.name.as_str()).unwrap_or("changed code");
        mechanical_by_symbol.entry(name.into()).or_default().push(f);
    }
    let mechanical: Vec<_> = mechanical_by_symbol
        .into_iter()
        .map(|(name, fs)| {
            format!(
                "{}: {}",
                escape(&name),
                fs.iter()
                    .map(|f| format!(
                        "{} {}:{} [{}] {}",
                        f.id,
                        escape(&f.file),
                        f.line,
                        f.sources.join(","),
                        cap_text(&f.text, 160)
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
        .collect();
    let mech_budget = 6;
    out.extend(mechanical.iter().take(mech_budget).cloned());
    if mechanical.len() > mech_budget {
        out.push(format!(
            "+{} more mechanical symbol(s) (alsp ledger list)",
            mechanical.len() - mech_budget
        ));
    }
    out.push("Predicted gaps:".into());
    let gap_lines: Vec<_> = pack
        .symbols
        .iter()
        .filter(|s| !s.gaps.is_empty())
        .map(|s| {
            let mut sorted: Vec<_> = s.gaps.iter().collect();
            sorted.sort_by(|a, b| a.p_tests_catch.total_cmp(&b.p_tests_catch));
            let uncovered =
                sorted.iter().filter(|g| g.reason == alsp_evidence::GapReason::Uncovered).count();
            let lowest = sorted
                .first()
                .map(|g| format!("{:.3}", g.p_tests_catch))
                .unwrap_or_else(|| "n/a".into());
            let mutations = sorted
                .iter()
                .take(2)
                .map(|g| cap_text(&g.mutation, 100))
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "{} {}:{}-{}: {} predicted gaps, {uncovered} uncovered, lowest P(catch) {lowest}; {mutations}",
                escape(&s.name),
                escape(&s.file),
                s.span.start,
                s.span.end,
                s.gaps.len()
            )
        })
        .collect();
    let gap_budget = 6;
    out.extend(gap_lines.iter().take(gap_budget).cloned());
    if gap_lines.len() > gap_budget {
        out.push(format!("+{} more gap symbol(s) (alsp ledger list)", gap_lines.len() - gap_budget));
    }
    if split_crates > 0 {
        out.push(format!(
            "Diff split into {split_crates} scopes (by crate, then by file when a crate is still too large)"
        ));
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in findings {
        for s in &f.sources {
            *counts.entry(s.clone()).or_default() += 1;
        }
    }
    out.push(format!(
        "Findings: {}; counts by source: {}",
        findings.len(),
        if counts.is_empty() {
            "none".into()
        } else {
            counts.iter().map(|(s, n)| format!("{s} {n}")).collect::<Vec<_>>().join(", ")
        }
    ));
    out.push(format!(
        "Dropped by proof: {dropped}; malformed FINDING lines: {}",
        runs.iter().map(|r| r.malformed).sum::<usize>()
    ));
    out.push(format!(
        "Agents skipped: {}",
        if skipped.is_empty() {
            "none".into()
        } else {
            skipped.iter().map(|s| escape(s).into_owned()).collect::<Vec<_>>().join("; ")
        }
    ));
    out.push(format!(
        "Agent time: {}",
        runs.iter()
            .map(|r| format!(
                "{} {}ms{}{}{}",
                escape(&r.agent),
                r.millis,
                if r.unparsed { " unparsed" } else { "" },
                r.raw_path.as_ref().map(|p| format!(" raw={}", escape(p))).unwrap_or_default(),
                if r.notes.is_empty() {
                    String::new()
                } else {
                    format!(
                        " [{}]",
                        r.notes.iter().map(|n| escape(n).into_owned()).collect::<Vec<_>>().join("; ")
                    )
                }
            ))
            .collect::<Vec<_>>()
            .join("; ")
    ));
    debug_assert!(out.len() <= LINE_BUDGET, "report budget exceeded: {} lines", out.len());
    out.join("\n") + "\n"
}

fn symbol_for<'a>(
    pack: &'a EvidencePack,
    file: &str,
    line: u32,
) -> Option<&'a alsp_evidence::SymbolEvidence> {
    pack.symbols.iter().find(|s| s.file == file && s.span.start <= line && line <= s.span.end)
}

pub fn cap_text(s: &str, max: usize) -> String {
    let clean = escape(s);
    let clean = clean.as_ref();
    if clean.chars().count() <= max {
        clean.into()
    } else {
        format!("{}…", clean.chars().take(max.saturating_sub(1)).collect::<String>())
    }
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

    #[test]
    fn test_DW_4_8_text_report_prioritizes_agent_findings_then_groups_mechanical_and_gaps() {
        let pack = report_pack();
        let findings = vec![
            Finding::new("gap", "a.rs", 3, "low", "mutant 1"),
            Finding::new("claude", "a.rs", 2, "high", "agent defect"),
            Finding::new("coverage", "a.rs", 2, "low", "changed line is uncovered"),
        ];
        let report = render_report(&pack, &findings, 0, &[], &[], 0);
        let lines: Vec<_> = report.lines().collect();
        assert!(lines[2].contains("claude — agent defect"));
        assert!(lines.iter().any(|l| l.contains("f a.rs:1-4:")));
        assert!(report.contains("mutant 1"));
    }

    #[test]
    fn test_DW_4_8_report_explicitly_counts_agent_findings_over_budget() {
        let pack = report_pack();
        let findings = (0..21)
            .map(|n| Finding::new("codex", "a.rs", 1, "low", format!("issue {n}")))
            .collect::<Vec<_>>();
        let report = render_report(&pack, &findings, 0, &[], &[], 0);
        assert!(report.contains("+5 more (alsp ledger list)"));
        assert!(report.lines().count() <= 40);
    }

    #[test]
    fn symbol_for_matches_file_and_span_only() {
        let p = report_pack();
        assert_eq!(symbol_for(&p, "a.rs", 2).map(|s| s.name.as_str()), Some("f"));
        assert!(symbol_for(&p, "a.rs", 1).is_some() && symbol_for(&p, "a.rs", 4).is_some());
        assert!(symbol_for(&p, "a.rs", 5).is_none());
        assert!(symbol_for(&p, "b.rs", 2).is_none());
    }

    #[test]
    fn cap_text_escapes_control_characters_and_truncates_with_an_ellipsis() {
        assert_eq!(cap_text("a\x1b[31mb\n", 50), "a\\u{1b}[31mb\\n");
        assert_eq!(cap_text("short", 5), "short");
        assert_eq!(cap_text("abcdefgh", 4), "abc…");
        assert_eq!(cap_text("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn report_escapes_control_characters_in_files_and_symbols() {
        let mut p = report_pack();
        p.symbols[0].file = "a\x1b[31mEVIL.rs".into();
        p.symbols[0].name = "f\x1b[2J".into();
        let fs = vec![
            Finding::new("coverage", "a\x1b[31mEVIL.rs", 2, "low", "changed line is uncovered"),
            Finding::new("claude", "a\x1b[31mEVIL.rs", 2, "high", "agent\x07"),
        ];
        let report = render_report(&p, &fs, 0, &["codex: \x1b bad".to_string()], &[], 0);
        assert!(!report.chars().any(|c| c == '\x1b' || c == '\x07'), "{report:?}");
        assert!(report.contains("EVIL.rs"));
    }

    #[test]
    fn report_states_split_notes_and_timeouts_within_the_line_budget() {
        let run = AgentRun {
            agent: "agy".into(),
            millis: 5,
            notes: vec!["stdout truncated at 4194304 bytes".into()],
            raw_path: Some(".alsp/raw/x-agy.txt".into()),
            ..Default::default()
        };
        let report = render_report(
            &report_pack(),
            &[],
            0,
            &["agy: timed out after 600s".to_string()],
            &[run],
            3,
        );
        assert!(
            report.contains("Diff split into 3 scopes")
                && report.contains("agy: timed out after 600s")
                && report.contains("[stdout truncated at 4194304 bytes]"),
            "{report}"
        );
        assert!(report.lines().count() <= 40);
    }

    #[test]
    fn test_DW_1_1_report_without_an_index_says_unknown_and_shows_the_index_line_and_notes() {
        let r = render_report(&no_index_pack(), &[], 0, &[], &[], 0);
        assert!(
            r.contains(
                "Index: missing, 0/1 roots indexed; root . failed: rust-analyzer not found; tried /x/ra\n"
            ),
            "{r}"
        );
        assert!(r.contains("unknown, not unchanged"), "{r}");
        assert!(r.contains("Changed symbols: unknown") && !r.contains("Changed symbols: none"), "{r}");
    }

    #[test]
    fn test_DW_1_2_report_with_a_good_index_and_no_changes_says_none_and_adds_no_index_line() {
        let mut p = report_pack();
        p.symbols.clear();
        let r = render_report(&p, &[], 0, &[], &[], 0);
        assert!(r.contains("Changed symbols: none") && !r.contains("Index:"), "{r}");
    }

    #[test]
    fn report_shows_a_partial_index_and_caps_pack_notes() {
        let mut p = report_pack();
        p.index.units.push(alsp_index::UnitReport {
            path: "bad".into(),
            ok: false,
            detail: Some("exit 1".into()),
        });
        p.notes = (0..9).map(|i| format!("note {i}")).collect();
        let r = render_report(&p, &[], 0, &[], &[], 0);
        assert!(r.contains("Index: fresh, 1/2 roots indexed; root bad failed: exit 1"), "{r}");
        assert!(r.contains("! note 5\n") && !r.contains("! note 6") && r.contains("+3 more notes"), "{r}");
        assert!(r.contains("Changed symbols (pass"), "a partial index still reports its symbols: {r}");
    }
}
