//! What a query answers: one [`Answer`] type for every command, rendered as compact text or as
//! JSON (schema in docs/query.md). A command fills only the sections that apply to it; the others
//! stay `None` and are absent from the JSON.
//!
//! Every section names how its facts were obtained (resolved by the index, observed by running
//! tests, predicted by a model, heuristic). Text shows it in brackets on the section line, and
//! marks a single heuristic item with a trailing `~`, as `alsp pack` does.

use alsp_evidence::{escape, Provenance};
use alsp_index::Span;
use serde::Serialize;

/// Bumped when a key is renamed or removed (adding keys does not bump it).
pub const SCHEMA: u32 = 1;

/// How many reference lines of one group the text shows (JSON has all of them).
const LINES_SHOWN: usize = 8;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Freshness {
    /// `fresh`, `stale`, `missing`: what the cache held when the query started.
    pub status: String,
    /// An indexer ran for this query, so the index matches the working tree now.
    pub rebuilt: bool,
    /// Checked-out commit.
    pub head: String,
    /// Tracked Rust and Cargo files whose working-tree content differs from `head`; the index
    /// describes the working tree, so these edits are in it.
    pub dirty: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TargetInfo {
    /// `fn`, `struct`, ... for a symbol; `module` or `file` for a region of source.
    pub kind: String,
    pub name: String,
    pub file: String,
    pub span: Span,
}

/// A capped list. `total` counts everything found, `omitted` what the cap left out.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Section<T> {
    pub provenance: Provenance,
    pub total: usize,
    pub omitted: usize,
    pub items: Vec<T>,
}

impl<T> Section<T> {
    pub fn capped(provenance: Provenance, mut items: Vec<T>, limit: usize) -> Self {
        let total = items.len();
        items.truncate(limit);
        Section { provenance, total, omitted: total - items.len(), items }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SymMatch {
    pub name: String,
    pub kind: String,
    pub file: String,
    /// Line of the name.
    pub line: u32,
    pub span: Span,
    pub references: usize,
}

/// The references to the target made from inside one enclosing symbol.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RefGroup {
    /// Enclosing symbol, or `<outside any symbol>` for references with no enclosing definition.
    pub symbol: String,
    pub kind: String,
    pub file: String,
    /// The enclosing symbol is a `#[test]` function or lives under `tests/`.
    pub test: bool,
    pub lines: Vec<u32>,
    /// `resolved` for a use of the target itself, or of its impl method when the index recorded
    /// the link; `heuristic` when the group is reached across a trait/impl link that was matched
    /// by name, or through the trait (a call that may run another impl).
    pub provenance: Provenance,
    /// The trait or impl method the sites actually resolve to, when that is not the target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EntryPoint {
    pub name: String,
    /// `test`, `main` or `pub` (a public function: only heuristically reachable from outside).
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TestRef {
    pub name: String,
    pub file: String,
    pub line: u32,
    /// The test itself uses the target; otherwise it reaches it through other functions.
    pub direct: bool,
    /// `heuristic` when the test reaches the target only across a name-matched trait/impl link, or
    /// through a call that may run another impl.
    pub provenance: Provenance,
}

/// Whether the package's tests ran the target's lines.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Executed {
    pub provenance: Provenance,
    /// `yes`, `no`, or `unknown` (no instrumented line in the span).
    pub status: String,
    pub lines_hit: u32,
    pub lines_total: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CoverageAnswer {
    pub provenance: Provenance,
    pub lines_total: u32,
    pub lines_hit: u32,
    pub branches_total: Option<u32>,
    pub branches_hit: Option<u32>,
    pub uncovered_lines: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GapItem {
    pub file: String,
    pub line: u32,
    pub mutation: String,
    /// Predicted chance the tests catch the mutation; 0 for lines no test runs.
    pub p_tests_catch: f64,
    /// `observed` (no test runs the line) or `predicted` (a model's estimate).
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Answer {
    pub schema: u32,
    pub command: String,
    /// Absent for `sym`, which answers with a list of matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetInfo>,
    pub index: Freshness,
    /// Caveats a reader needs to weigh the facts.
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches: Option<Section<SymMatch>>,
    /// Reference sites in `refs`, before grouping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs: Option<Section<RefGroup>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_points: Option<Section<EntryPoint>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<Section<TestRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed: Option<Executed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CoverageAnswer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gaps: Option<Section<GapItem>>,
    /// Why a requested measurement is missing (tests could not run, no model). The CLI exits 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

impl Answer {
    pub fn new(command: &str, target: Option<TargetInfo>, index: Freshness) -> Answer {
        Answer {
            schema: SCHEMA,
            command: command.into(),
            target,
            index,
            notes: Vec::new(),
            matches: None,
            reference_count: None,
            refs: None,
            entry_points: None,
            tests: None,
            executed: None,
            coverage: None,
            gaps: None,
            unavailable: None,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("answer serializes") + "\n"
    }

    pub fn to_text(&self) -> String {
        let mut out = Vec::new();
        let f = &self.index;
        let label = self.target.as_ref().map(|t| format!(" {}", escape(&t.name))).unwrap_or_default();
        out.push(format!("{}{label} index={}{} @{}", self.command, f.status, if f.rebuilt { "+rebuilt" } else { "" }, &f.head[..f.head.len().min(8)]));
        if let Some(t) = &self.target {
            out.push(format!("target {} {}:{} [resolved]", t.kind, escape(&t.file), span(t.span)));
        }
        if !f.dirty.is_empty() {
            let shown = f.dirty.iter().take(3).map(|d| escape(d).into_owned()).collect::<Vec<_>>().join(", ");
            out.push(format!("! the working tree has uncommitted changes to {} Rust/Cargo file(s) ({shown}); the index describes the working tree, not {}", f.dirty.len(), &f.head[..f.head.len().min(8)]));
        }
        out.extend(self.notes.iter().map(|n| format!("! {}", escape(n))));
        if let Some(s) = &self.matches {
            section(&mut out, &format!("matches({})", s.total), s, |m| format!("{} {} {}:{} refs={}", m.kind, escape(&m.name), escape(&m.file), m.line, m.references));
        }
        if let Some(s) = &self.refs {
            let sites = self.reference_count.unwrap_or(0);
            let tag = mixed_tag(s.provenance, s.items.iter().map(|g| g.provenance));
            section_tagged(&mut out, &format!("refs({sites} in {} symbols)", s.total), &tag, s, |g| {
                let mark = if g.test { " [test]" } else { "" };
                let via = g.via.as_ref().map(|v| format!(" via {}", escape(v))).unwrap_or_default();
                format!("{} {}:{}{mark}{via}{}", escape(&g.symbol), escape(&g.file), lines(&g.lines), tilde(g.provenance))
            });
        }
        if let Some(s) = &self.entry_points {
            let mix = s.items.iter().any(|e| e.provenance != Provenance::Resolved);
            let tag = if mix && s.items.iter().any(|e| e.provenance == Provenance::Resolved) { "resolved+heuristic".into() } else { s.provenance.as_str().to_string() };
            section_tagged(&mut out, &format!("entry({})", s.total), &tag, s, |e| {
                format!("{} {} {}:{}{}", e.kind, escape(&e.name), escape(&e.file), e.line, if e.provenance == Provenance::Resolved { "" } else { "~" })
            });
        }
        if let Some(s) = &self.tests {
            let tag = mixed_tag(s.provenance, s.items.iter().map(|t| t.provenance));
            section_tagged(&mut out, &format!("tests({})", s.total), &tag, s, |t| format!("{} {}:{} {}{}", escape(&t.name), escape(&t.file), t.line, if t.direct { "direct" } else { "indirect" }, tilde(t.provenance)));
        }
        if let Some(e) = &self.executed {
            out.push(format!("executed: {} ({}/{} lines ran) [{}]", e.status, e.lines_hit, e.lines_total, e.provenance.as_str()));
        }
        if let Some(c) = &self.coverage {
            let b = match (c.branches_total, c.branches_hit) {
                (Some(t), Some(h)) if t > 0 => format!(", branches {h}/{t}"),
                _ => String::new(),
            };
            out.push(format!("lines {}/{} covered{b} [{}]", c.lines_hit, c.lines_total, c.provenance.as_str()));
            if !c.uncovered_lines.is_empty() {
                out.push(format!("uncovered: {} [{}]", line_ranges(&c.uncovered_lines), c.provenance.as_str()));
            }
        }
        if let Some(s) = &self.gaps {
            let tag = gap_tag(s);
            section_tagged(&mut out, &format!("gaps({})", s.total), &tag, s, |g| {
                let how = if g.provenance == Provenance::Observed { "uncovered".to_string() } else { format!("p={:.2}", g.p_tests_catch) };
                format!("{}:{} {} {how} [{}]", escape(&g.file), g.line, escape(&g.mutation), g.provenance.as_str())
            });
        }
        if let Some(u) = &self.unavailable {
            out.push(format!("! unavailable: {}", escape(u)));
        }
        out.join("\n") + "\n"
    }
}

/// A heuristic item ends in `~`.
pub(crate) fn tilde(p: Provenance) -> &'static str {
    if p == Provenance::Heuristic { "~" } else { "" }
}

/// The section's tag: its own provenance, or `resolved+heuristic` when its items differ.
pub(crate) fn mixed_tag(base: Provenance, items: impl Iterator<Item = Provenance>) -> String {
    let items: Vec<Provenance> = items.collect();
    if items.iter().any(|p| *p == Provenance::Heuristic) && items.iter().any(|p| *p == Provenance::Resolved) {
        "resolved+heuristic".into()
    } else {
        items.first().copied().unwrap_or(base).as_str().into()
    }
}

fn gap_tag(s: &Section<GapItem>) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for g in &s.items {
        if !seen.contains(&g.provenance.as_str()) {
            seen.push(g.provenance.as_str());
        }
    }
    if seen.is_empty() { s.provenance.as_str().into() } else { seen.join("+") }
}

fn section<T>(out: &mut Vec<String>, head: &str, s: &Section<T>, line: impl Fn(&T) -> String) {
    section_tagged(out, head, s.provenance.as_str(), s, line);
}

fn section_tagged<T>(out: &mut Vec<String>, head: &str, tag: &str, s: &Section<T>, line: impl Fn(&T) -> String) {
    out.push(format!("{head} [{tag}]"));
    out.extend(s.items.iter().map(|i| format!("  {}", line(i))));
    if s.omitted > 0 {
        out.push(format!("  +{} more (--all lists everything)", s.omitted));
    }
}

pub(crate) fn span(s: Span) -> String {
    if s.end == u32::MAX {
        "1-end".into()
    } else if s.start == s.end {
        s.start.to_string()
    } else {
        format!("{}-{}", s.start, s.end)
    }
}

/// `200,215,+3 more` for a group's reference lines.
pub(crate) fn lines(l: &[u32]) -> String {
    let mut s = l.iter().take(LINES_SHOWN).map(u32::to_string).collect::<Vec<_>>().join(",");
    if l.len() > LINES_SHOWN {
        s.push_str(&format!(",+{} more", l.len() - LINES_SHOWN));
    }
    s
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Freshness {
        Freshness { status: "fresh".into(), rebuilt: false, head: "0123456789abcdef".into(), dirty: vec![] }
    }
    fn target() -> TargetInfo {
        TargetInfo { kind: "fn".into(), name: "m::f".into(), file: "src/m.rs".into(), span: Span { start: 3, end: 9 } }
    }

    #[test]
    fn capped_counts_what_it_leaves_out() {
        let s = Section::capped(Provenance::Resolved, vec![1, 2, 3, 4], 3);
        assert_eq!((s.total, s.omitted, s.items.len()), (4, 1, 3));
        let s = Section::capped(Provenance::Resolved, vec![1], 3);
        assert_eq!((s.total, s.omitted), (1, 0));
    }

    #[test]
    fn line_ranges_and_lines_compress() {
        assert_eq!(line_ranges(&[3, 5, 6, 7, 10]), "3,5-7,10");
        assert_eq!(line_ranges(&[]), "");
        assert_eq!(lines(&[1, 2]), "1,2");
        assert_eq!(lines(&(1..=11).collect::<Vec<_>>()), "1,2,3,4,5,6,7,8,+3 more");
        assert_eq!((span(Span { start: 4, end: 4 }), span(Span { start: 1, end: u32::MAX })), ("4".to_string(), "1-end".to_string()));
    }

    #[test]
    fn header_target_freshness_and_notes_come_first() {
        let mut f = fresh();
        f.rebuilt = true;
        f.dirty = vec!["a.rs".into(), "b\nc.rs".into()];
        let mut a = Answer::new("refs", Some(target()), f);
        a.notes.push("a note".into());
        a.refs = Some(Section::capped(Provenance::Resolved, vec![], 5));
        a.reference_count = Some(0);
        let t = a.to_text();
        let l: Vec<&str> = t.lines().collect();
        assert_eq!(l[0], "refs m::f index=fresh+rebuilt @01234567");
        assert_eq!(l[1], "target fn src/m.rs:3-9 [resolved]");
        assert!(l[2].starts_with("! the working tree has uncommitted changes to 2 Rust/Cargo file(s) (a.rs, b\\nc.rs)"), "{t}");
        assert_eq!((l[3], l[4]), ("! a note", "refs(0 in 0 symbols) [resolved]"));
    }

    #[test]
    fn json_omits_sections_that_do_not_apply_and_names_provenance() {
        let mut a = Answer::new("tests", Some(target()), fresh());
        a.tests = Some(Section::capped(Provenance::Resolved, vec![TestRef { name: "t".into(), file: "f.rs".into(), line: 2, direct: true, provenance: Provenance::Resolved }], 5));
        let v: serde_json::Value = serde_json::from_str(&a.to_json()).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["tests"]["provenance"], "resolved");
        for absent in ["matches", "refs", "coverage", "gaps", "executed", "unavailable", "entry_points", "reference_count"] {
            assert!(v.get(absent).is_none(), "{absent} should be absent");
        }
    }

    #[test]
    fn a_heuristic_entry_point_is_tilde_marked_and_the_header_names_both_provenances() {
        let mut a = Answer::new("refs", Some(target()), fresh());
        let e = |kind: &str, p| EntryPoint { name: "n".into(), kind: kind.into(), file: "f.rs".into(), line: 1, provenance: p };
        a.entry_points = Some(Section::capped(Provenance::Resolved, vec![e("test", Provenance::Resolved), e("pub", Provenance::Heuristic)], 5));
        let t = a.to_text();
        assert!(t.contains("entry(2) [resolved+heuristic]\n  test n f.rs:1\n  pub n f.rs:1~\n"), "{t}");
        a.entry_points = Some(Section::capped(Provenance::Heuristic, vec![e("pub", Provenance::Heuristic)], 5));
        assert!(a.to_text().contains("entry(1) [heuristic]"));
    }

    #[test]
    fn coverage_gaps_executed_and_unavailable_render_with_provenance() {
        let mut a = Answer::new("gaps", Some(target()), fresh());
        a.executed = Some(Executed { provenance: Provenance::Observed, status: "yes".into(), lines_hit: 3, lines_total: 4 });
        a.coverage = Some(CoverageAnswer { provenance: Provenance::Observed, lines_total: 4, lines_hit: 3, branches_total: Some(2), branches_hit: Some(1), uncovered_lines: vec![8, 9] });
        let g = |line, p, prov| GapItem { file: "f.rs".into(), line, mutation: "replace < with <=".into(), p_tests_catch: p, provenance: prov };
        a.gaps = Some(Section::capped(Provenance::Predicted, vec![g(2, 0.2, Provenance::Predicted), g(9, 0.0, Provenance::Observed), g(10, 0.0, Provenance::Observed)], 2));
        a.unavailable = Some("no model".into());
        let t = a.to_text();
        assert!(t.contains("executed: yes (3/4 lines ran) [observed]\n"), "{t}");
        assert!(t.contains("lines 3/4 covered, branches 1/2 [observed]\nuncovered: 8-9 [observed]\n"), "{t}");
        assert!(t.contains("gaps(3) [predicted+observed]\n  f.rs:2 replace < with <= p=0.20 [predicted]\n  f.rs:9 replace < with <= uncovered [observed]\n  +1 more (--all lists everything)\n"), "{t}");
        assert!(t.ends_with("! unavailable: no model\n"));
        let mut b = a.clone();
        b.gaps = Some(Section::capped(Provenance::Predicted, vec![], 2));
        assert!(b.to_text().contains("gaps(0) [predicted]"));
    }

    #[test]
    fn hostile_names_cannot_forge_lines() {
        let mut a = Answer::new("sym", None, fresh());
        a.matches = Some(Section::capped(Provenance::Resolved, vec![SymMatch { name: "x\nforged".into(), kind: "fn".into(), file: "f\n.rs".into(), line: 1, span: Span { start: 1, end: 1 }, references: 0 }], 5));
        let t = a.to_text();
        assert_eq!(t.lines().count(), 3, "{t}");
        assert!(t.contains("x\\nforged"));
    }
}
