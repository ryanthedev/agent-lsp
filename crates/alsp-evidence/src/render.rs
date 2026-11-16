//! Compact text form of a pack. Written for an agent's context window: one line per fact, lists
//! capped with a count of what was left out (the JSON form has everything).

use crate::types::{Edge, EvidencePack, GapReason, Provenance, SymbolEvidence};
use alsp_index::readable_name;

pub const CALLERS_SHOWN: usize = 5;
pub const ENTRY_POINTS_SHOWN: usize = 3;
pub const GAPS_SHOWN: usize = 5;

/// Text safe to put on one line of the pack: control characters (below 0x20, 0x7f, and the Unicode
/// line and paragraph separators) become visible escapes (`\n`, `\t`, `\u{..}`), so a path, note
/// or mutation containing a newline cannot forge extra lines an agent would read as facts. A literal
/// backslash is doubled (`\\`), so a real `\n` two-character sequence in a path stays distinct from
/// an escaped newline.
pub fn escape(s: &str) -> std::borrow::Cow<'_, str> {
    let bad = |c: char| c == '\\' || c.is_control() || c == '\u{2028}' || c == '\u{2029}';
    if !s.chars().any(bad) {
        return s.into();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if bad(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.into()
}

/// Rough token count: one token per four characters. No tokenizer is involved.
pub fn approx_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// `name file:line`, with a trailing `~` when the edge is not resolved.
fn edge(e: &Edge) -> String {
    let mark = if e.provenance == Provenance::Resolved { "" } else { "~" };
    format!("{} {}:{}{mark}", escape(&readable_name(&e.id)), escape(&e.file), e.line)
}

fn list(label: &str, edges: &[Edge], shown: usize) -> Option<String> {
    if edges.is_empty() {
        return None;
    }
    let mut s = format!("  {label}({}): {}", edges.len(), edges.iter().take(shown).map(edge).collect::<Vec<_>>().join("; "));
    if edges.len() > shown {
        s.push_str(&format!("; +{} more", edges.len() - shown));
    }
    Some(s)
}

fn symbol(s: &SymbolEvidence, out: &mut Vec<String>) {
    let span = if s.span.start == s.span.end { s.span.start.to_string() } else { format!("{}-{}", s.span.start, s.span.end) };
    let tag = if s.kind == crate::types::ChangeKind::Removed { " (removed in the diff; read from base source, heuristic)" } else { "" };
    out.push(format!("{} {} {}:{span}{tag}", s.kind.letter(), escape(&s.name), escape(&s.file)));
    out.extend(list("callers", &s.callers, CALLERS_SHOWN));
    out.extend(list("entry", &s.entry_points, ENTRY_POINTS_SHOWN));
    if let Some(c) = &s.coverage {
        let mut l = format!("  lines {}/{} covered", c.lines_hit, c.lines_total);
        if let (Some(t), Some(h)) = (c.branches_total, c.branches_hit) {
            l.push_str(&format!(", branches {h}/{t}"));
        }
        out.push(l);
        if !c.uncovered_changed_lines.is_empty() {
            out.push(format!("  uncovered changed lines: {}", line_ranges(&c.uncovered_changed_lines)));
        }
    }
    for g in s.gaps.iter().take(GAPS_SHOWN) {
        let tag = if g.reason == GapReason::Uncovered { " uncovered" } else { "" };
        out.push(format!("  gap {}:{} {} p={:.2}{tag}", escape(&g.file), g.line, escape(&g.mutation), g.p_tests_catch));
    }
    if s.gaps.len() > GAPS_SHOWN {
        let rest = &s.gaps[GAPS_SHOWN..];
        out.push(format!("  gaps: +{} more ({} uncovered)", rest.len(), rest.iter().filter(|g| g.reason == GapReason::Uncovered).count()));
    }
}

/// `3,5-7,10` from sorted line numbers.
fn line_ranges(lines: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut j = i;
        while j + 1 < lines.len() && lines[j + 1] == lines[j] + 1 {
            j += 1;
        }
        parts.push(if i == j { lines[i].to_string() } else { format!("{}-{}", lines[i], lines[j]) });
        i = j + 1;
    }
    parts.join(",")
}

impl EvidencePack {
    /// One `Index:` line saying what the facts rest on: the index status, how many Cargo roots are
    /// in it, each root left out with its cause, and every note the indexer left. Printed by `pack`
    /// and `review` so a reader can tell an empty answer from an unknown one.
    pub fn index_line(&self) -> String {
        let ok = self.index.units.iter().filter(|u| u.ok).count();
        let mut line = format!("Index: {}{}, {}/{} roots indexed", self.index.status.as_str(), if self.index.rebuilt { "+rebuilt" } else { "" }, ok, self.index.units.len());
        for u in self.index.units.iter().filter(|u| !u.ok) {
            line.push_str(&format!("; root {} failed: {}", escape(&u.path), escape(u.detail.as_deref().unwrap_or("unknown"))));
        }
        for n in &self.index.notes {
            line.push_str(&format!("; {}", escape(n)));
        }
        line
    }

    /// Every root is indexed and the indexer left no notes: nothing for the `Index:` line to warn about.
    pub fn index_is_clean(&self) -> bool {
        self.index.complete() && self.index.notes.is_empty() && self.facts_served()
    }

    /// A=added M=modified R=removed. `~` marks facts that are not resolved by the index (heuristic).
    pub fn to_text(&self) -> String {
        let ok = self.index.units.iter().filter(|u| u.ok).count();
        let mut out = vec![format!(
            "pack {}..{} index={}{} roots={}/{} symbols={}",
            short(&self.base),
            short(&self.head),
            self.index.status.as_str(),
            if self.index.rebuilt { "+rebuilt" } else { "" },
            ok,
            self.index.units.len(),
            self.symbols.len()
        )];
        if !self.index_is_clean() {
            out.push(self.index_line());
        }
        for n in &self.notes {
            out.push(format!("! {}", escape(n)));
        }
        for s in &self.symbols {
            symbol(s, &mut out);
        }
        out.join("\n") + "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use alsp_index::{IndexState, IndexStatus, Span, UnitReport};

    const P: &str = "rust-analyzer cargo c 0.0.1 ";

    fn edge_to(name: &str, file: &str, line: u32, p: Provenance) -> Edge {
        Edge { id: format!("{P}{name}()."), file: file.into(), line, provenance: p }
    }
    fn sym(kind: ChangeKind, callers: Vec<Edge>, entries: Vec<Edge>) -> SymbolEvidence {
        SymbolEvidence {
            id: "x".into(),
            name: "frame::Frame::fin".into(),
            file: "crates/p/src/frame.rs".into(),
            span: Span { start: 50, end: 58 },
            kind,
            callers,
            entry_points: entries,
            coverage: None,
            gaps: vec![],
        }
    }
    fn pack(symbols: Vec<SymbolEvidence>) -> EvidencePack {
        EvidencePack {
            base: "0123456789abcdef".into(),
            head: "fedcba9876543210".into(),
            index: IndexState { status: IndexStatus::Fresh, rebuilt: false, units: vec![UnitReport { path: ".".into(), ok: true, detail: None }], millis: 5, notes: vec![] },
            symbols,
            notes: vec![],
        }
    }

    #[test]
    fn index_line_names_failures_notes_and_unknown_symbols() {
        let mut p = pack(vec![]);
        assert!(p.index_is_clean() && !p.to_text().contains("Index:"), "a clean index adds no line");
        p.index.notes.push("tried rust-analyzer at /x".into());
        assert_eq!(p.index_line(), "Index: fresh, 1/1 roots indexed; tried rust-analyzer at /x");
        p.index.units[0].ok = false;
        p.index.units[0].detail = Some("exit 1\nforged".into());
        let l = p.index_line();
        assert!(l.contains("0/1 roots indexed; root . failed: exit 1\\nforged; tried rust-analyzer at /x"), "{l}");
        assert!(!l.contains('\n'), "{l}");
        assert!(p.to_text().contains(&l));
    }

    #[test]
    fn header_symbol_line_callers_and_entries() {
        let s = sym(
            ChangeKind::Modified,
            vec![edge_to("a/b", "x.rs", 12, Provenance::Resolved), edge_to("c/d", "y.rs", 7, Provenance::Heuristic)],
            vec![edge_to("t/e", "z.rs", 3, Provenance::Resolved)],
        );
        let t = pack(vec![s]).to_text();
        assert_eq!(
            t,
            "pack 01234567..fedcba98 index=fresh roots=1/1 symbols=1\n\
             M frame::Frame::fin crates/p/src/frame.rs:50-58\n  \
             callers(2): a::b x.rs:12; c::d y.rs:7~\n  \
             entry(1): t::e z.rs:3\n"
        );
    }

    #[test]
    fn lists_are_capped_with_a_count_and_one_line_spans_are_short() {
        let many: Vec<Edge> = (0..10).map(|i| edge_to(&format!("m/f{i}"), "x.rs", i, Provenance::Resolved)).collect();
        let mut s = sym(ChangeKind::Added, many.clone(), many);
        s.span = Span { start: 9, end: 9 };
        let t = pack(vec![s]).to_text();
        assert!(t.contains("A frame::Frame::fin crates/p/src/frame.rs:9\n"));
        assert!(t.contains("callers(10): ") && t.contains("; +5 more"), "{t}");
        assert!(t.contains("entry(10): ") && t.contains("; +7 more"));
        assert!(!t.contains("m::f6 "));
    }

    #[test]
    fn failed_roots_notes_removed_symbols_coverage_and_gaps_are_shown() {
        let mut p = pack(vec![sym(ChangeKind::Removed, vec![], vec![])]);
        p.index.units.push(UnitReport { path: "bad".into(), ok: false, detail: Some("exit 1".into()) });
        p.index.rebuilt = true;
        p.notes.push("head is not checked out".into());
        p.symbols[0].coverage = Some(Coverage { lines_total: 10, lines_hit: 7, branches_total: None, branches_hit: None, uncovered_changed_lines: vec![] });
        p.symbols[0].gaps.push(PredictedGap { file: "f.rs".into(), line: 3, mutation: "== to !=".into(), p_tests_catch: 0.234, reason: GapReason::PredictedSurvivor });
        let t = p.to_text();
        assert!(t.starts_with("pack 01234567..fedcba98 index=fresh+rebuilt roots=1/2 symbols=1\nIndex: fresh+rebuilt, 1/2 roots indexed; root bad failed: exit 1\n! head is not checked out\n"), "{t}");
        assert!(t.contains("R frame::Frame::fin") && t.contains("heuristic)"));
        assert!(t.contains("  lines 7/10 covered\n  gap f.rs:3 == to != p=0.23\n"), "{t}");
    }

    #[test]
    fn empty_pack_and_short_ids() {
        assert_eq!(pack(vec![]).to_text(), "pack 01234567..fedcba98 index=fresh roots=1/1 symbols=0\n");
        assert_eq!(short("abc"), "abc");
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2);
        assert_eq!(approx_tokens(""), 0);
    }

    #[test]
    fn five_worst_case_symbols_fit_the_token_budget() {
        // Caps bound the size: 5 symbols, every list overflowing, long names and paths.
        let long_edge = |i: u32| Edge {
            id: format!("{P}ui/SurfaceState#handle_event_{i}()."),
            file: "crates/render/src/surface/state.rs".into(),
            line: 1000 + i,
            provenance: Provenance::Resolved,
        };
        let edges: Vec<Edge> = (0..60).map(long_edge).collect();
        let syms: Vec<SymbolEvidence> = (0..5)
            .map(|_| {
                let mut s = sym(ChangeKind::Modified, edges.clone(), edges.clone());
                s.name = "surface::SurfaceState::handle_pointer_event".into();
                s
            })
            .collect();
        let tokens = approx_tokens(&pack(syms).to_text());
        assert!(tokens < 1500, "{tokens} tokens");
    }

    #[test]
    fn control_characters_in_paths_names_and_notes_cannot_forge_lines() {
        assert_eq!(escape("a\nb\tc\r\u{7f}\u{2028}\u{1b}"), "a\\nb\\tc\\r\\u{7f}\\u{2028}\\u{1b}");
        assert!(matches!(escape("plain/path.rs"), std::borrow::Cow::Borrowed(_)));
        // a real backslash-n in a path is distinguishable from an escaped newline
        assert_eq!(escape("a\\nb"), "a\\\\nb");
        assert_ne!(escape("a\\nb"), escape("a\nb"));
        assert_eq!(escape("\\"), "\\\\");
        let mut s = sym(ChangeKind::Modified, vec![edge_to("a/b", "x\nM forged crates/x.rs:1", 12, Provenance::Resolved)], vec![]);
        s.file = "evil\nM forged src/x.rs:1".into();
        s.name = "n\u{2028}m".into();
        s.gaps.push(PredictedGap { file: "f\n.rs".into(), line: 1, mutation: "a\nb".into(), p_tests_catch: 0.1, reason: GapReason::PredictedSurvivor });
        let mut p = pack(vec![s]);
        p.notes.push("note\nM forged".into());
        p.index.units.push(UnitReport { path: "r\nx".into(), ok: false, detail: Some("d\ny".into()) });
        let t = p.to_text();
        // header + failed root + note + symbol + callers + gap: exactly six lines, no forged ones
        assert_eq!(t.lines().count(), 6, "{t}");
        assert!(t.lines().all(|l| !l.starts_with("M forged")), "{t}");
        assert!(t.contains("M n\\u{2028}m evil\\nM forged src/x.rs:1"));
    }

    #[test]
    fn coverage_branches_uncovered_lines_and_gap_caps() {
        let mut s = sym(ChangeKind::Modified, vec![], vec![]);
        s.coverage = Some(Coverage { lines_total: 10, lines_hit: 7, branches_total: Some(4), branches_hit: Some(3), uncovered_changed_lines: vec![3, 5, 6, 7, 10] });
        for i in 0..8u32 {
            let reason = if i < 5 { GapReason::PredictedSurvivor } else { GapReason::Uncovered };
            s.gaps.push(PredictedGap { file: "f.rs".into(), line: i, mutation: "m".into(), p_tests_catch: 0.0, reason });
        }
        let t = pack(vec![s]).to_text();
        assert!(t.contains("  lines 7/10 covered, branches 3/4\n  uncovered changed lines: 3,5-7,10\n"), "{t}");
        assert_eq!(t.matches("  gap ").count(), GAPS_SHOWN);
        assert!(t.contains("  gaps: +3 more (3 uncovered)\n"), "{t}");
        assert!(!t.contains("p=0.00 uncovered") || t.matches(" uncovered\n").count() >= 1);
        assert_eq!(line_ranges(&[]), "");
    }
}
