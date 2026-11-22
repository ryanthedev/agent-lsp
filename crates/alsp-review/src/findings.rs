//! Findings representation, agent output parsing, proof filtering, mechanical heuristics, and merging.

use alsp_decide::DEFAULT_GAP_THRESHOLD;
use alsp_evidence::{ChangeKind, EvidencePack, PackSource};
use alsp_index::Span;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use syn::spanned::Spanned;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub sources: Vec<String>,
    pub file: String,
    pub line: u32,
    pub severity: String,
    pub text: String,
}

impl Finding {
    pub fn new(
        source: impl Into<String>,
        file: impl Into<String>,
        line: u32,
        severity: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let source = source.into();
        let file = file.into();
        let line = line.max(1);
        let severity = normalize_severity(&severity.into());
        let text = text.into();
        let id = finding_id(&file, line, &text);
        Self { id, sources: vec![source], file, line, severity, text }
    }

    pub fn agent(
        source: impl Into<String>,
        file: impl Into<String>,
        line: u32,
        text: impl Into<String>,
    ) -> Self {
        Self::new(source, file, line, "finding", text)
    }
}

pub fn normalize_severity(s: &str) -> String {
    match s.trim().to_ascii_lowercase().as_str() {
        "critical" | "high" => "high".into(),
        "medium" => "medium".into(),
        "warning" | "minor" | "info" | "low" => "low".into(),
        _ => "medium".into(),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProofResult {
    pub kept: Vec<Finding>,
    pub dropped: usize,
}

pub fn prove_agent_findings(
    findings: Vec<Finding>,
    allowed: &BTreeMap<String, BTreeSet<u32>>,
) -> ProofResult {
    let mut result = ProofResult::default();
    for f in findings {
        if allowed.get(&f.file).is_some_and(|lines| lines.contains(&f.line)) {
            result.kept.push(f);
        } else {
            result.dropped += 1;
        }
    }
    result
}

fn finding_id(file: &str, line: u32, text: &str) -> String {
    let normalized = normalize(text);
    let mut h = Sha256::new();
    h.update(file.as_bytes());
    h.update([0]);
    h.update(line.to_le_bytes());
    h.update([0]);
    h.update(normalized.as_bytes());
    format!("{:x}", h.finalize())[..10].to_string()
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub fn parse_agent_output(source: &str, raw: &str) -> (Vec<Finding>, Vec<String>) {
    let (mut findings, mut unparsed) = (Vec::new(), Vec::new());
    for line in raw.lines() {
        let mut candidate = line.trim();
        loop {
            let stripped = candidate.trim_start_matches(['-', '*', '+', '`']).trim_start();
            if stripped.len() == candidate.len() {
                break;
            }
            candidate = stripped;
        }
        candidate = candidate.trim_end_matches('`').trim();
        if candidate == "NO FINDINGS" || !candidate.starts_with("FINDING|") {
            continue;
        }
        // splitn keeps `|` inside the text; the proof step (not this parser) checks the file exists.
        let fields: Vec<_> = candidate.splitn(5, '|').collect();
        if fields.len() == 5
            && !fields[1].trim().is_empty()
            && fields[2].trim().parse::<u32>().is_ok()
            && !fields[3].trim().is_empty()
            && !fields[4].trim().is_empty()
        {
            findings.push(Finding::new(
                source,
                fields[1].trim(),
                fields[2].trim().parse().unwrap(),
                fields[3].trim(),
                fields[4].trim(),
            ));
        } else {
            unparsed.push(candidate.to_string());
        }
    }
    (findings, unparsed)
}

pub fn mechanical_findings(
    pack: &EvidencePack,
    src: &PackSource,
    base_source: impl Fn(&str) -> Option<String>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for symbol in &pack.symbols {
        if let Some(cov) = &symbol.coverage {
            for &line in &cov.uncovered_changed_lines {
                out.push(Finding::new(
                    "coverage",
                    &symbol.file,
                    line,
                    "warning",
                    "changed line is uncovered",
                ));
            }
        }
        for gap in &symbol.gaps {
            if gap.reason == alsp_evidence::GapReason::Uncovered
                || gap.p_tests_catch < DEFAULT_GAP_THRESHOLD
            {
                out.push(Finding::new(
                    "gap",
                    &gap.file,
                    gap.line,
                    "warning",
                    format!(
                        "predicted test gap (P(catch) {:.3}): {}",
                        gap.p_tests_catch, gap.mutation
                    ),
                ));
            }
        }
        if symbol.kind == ChangeKind::Modified {
            if let (Some(old), Some(new)) = (
                base_source(&symbol.file),
                src.tree
                    .join(&symbol.file)
                    .exists()
                    .then(|| std::fs::read_to_string(src.tree.join(&symbol.file)).ok())
                    .flatten(),
            ) {
                let simple_name = symbol.name.rsplit("::").next().unwrap_or(&symbol.name);
                if signature_changed(&old, &new, simple_name, symbol.span) {
                    for caller in &symbol.callers {
                        if !src.is_changed(&caller.file, caller.line) {
                            out.push(Finding::new(
                                "heuristic",
                                &symbol.file,
                                symbol.span.start,
                                "warning",
                                format!(
                                    "signature changed; caller {}:{} is outside the diff",
                                    caller.file, caller.line
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }
    merge_findings(out)
}

/// Enclosing syntactic scope of a function or method.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Enclosing {
    Free,
    Impl {
        self_ty: String,
        trait_path: Option<String>,
    },
    Trait(String),
}

/// Identifies a function/method within a file by module path, enclosing fn path, enclosing impl/trait, and name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FnIdentity {
    mod_path: Vec<String>,
    fn_path: Vec<String>,
    enclosing: Enclosing,
    name: String,
}

struct FnEntry {
    identity: FnIdentity,
    start_line: u32,
    end_line: u32,
    ident_line: u32,
    signature: String,
}

fn tokens(t: &impl quote::ToTokens) -> String {
    quote::ToTokens::to_token_stream(t).to_string()
}

fn collect_functions(source: &str) -> Option<Vec<FnEntry>> {
    let file = syn::parse_file(source).ok()?;
    struct Collector {
        mod_path: Vec<String>,
        fn_path: Vec<String>,
        enclosing: Enclosing,
        entries: Vec<FnEntry>,
    }
    impl<'ast> syn::visit::Visit<'ast> for Collector {
        fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
            self.mod_path.push(i.ident.to_string());
            let prev = std::mem::replace(&mut self.enclosing, Enclosing::Free);
            syn::visit::visit_item_mod(self, i);
            self.enclosing = prev;
            self.mod_path.pop();
        }

        fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
            let prev = std::mem::replace(
                &mut self.enclosing,
                Enclosing::Impl {
                    self_ty: tokens(&i.self_ty),
                    trait_path: i.trait_.as_ref().map(|(_, p, _)| tokens(p)),
                },
            );
            syn::visit::visit_item_impl(self, i);
            self.enclosing = prev;
        }

        fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
            let prev = std::mem::replace(
                &mut self.enclosing,
                Enclosing::Trait(i.ident.to_string()),
            );
            syn::visit::visit_item_trait(self, i);
            self.enclosing = prev;
        }

        fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
            let span = i.span();
            self.entries.push(FnEntry {
                identity: FnIdentity {
                    mod_path: self.mod_path.clone(),
                    fn_path: self.fn_path.clone(),
                    enclosing: Enclosing::Free,
                    name: i.sig.ident.to_string(),
                },
                start_line: span.start().line as u32,
                end_line: span.end().line as u32,
                ident_line: i.sig.ident.span().start().line as u32,
                signature: tokens(&i.sig),
            });
            self.fn_path.push(i.sig.ident.to_string());
            let prev = std::mem::replace(&mut self.enclosing, Enclosing::Free);
            syn::visit::visit_block(self, &i.block);
            self.enclosing = prev;
            self.fn_path.pop();
        }

        fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
            let span = i.span();
            self.entries.push(FnEntry {
                identity: FnIdentity {
                    mod_path: self.mod_path.clone(),
                    fn_path: self.fn_path.clone(),
                    enclosing: self.enclosing.clone(),
                    name: i.sig.ident.to_string(),
                },
                start_line: span.start().line as u32,
                end_line: span.end().line as u32,
                ident_line: i.sig.ident.span().start().line as u32,
                signature: tokens(&i.sig),
            });
            self.fn_path.push(i.sig.ident.to_string());
            let prev = std::mem::replace(&mut self.enclosing, Enclosing::Free);
            syn::visit::visit_block(self, &i.block);
            self.enclosing = prev;
            self.fn_path.pop();
        }

        fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
            let span = i.span();
            self.entries.push(FnEntry {
                identity: FnIdentity {
                    mod_path: self.mod_path.clone(),
                    fn_path: self.fn_path.clone(),
                    enclosing: self.enclosing.clone(),
                    name: i.sig.ident.to_string(),
                },
                start_line: span.start().line as u32,
                end_line: span.end().line as u32,
                ident_line: i.sig.ident.span().start().line as u32,
                signature: tokens(&i.sig),
            });
            if let Some(block) = &i.default {
                self.fn_path.push(i.sig.ident.to_string());
                let prev = std::mem::replace(&mut self.enclosing, Enclosing::Free);
                syn::visit::visit_block(self, block);
                self.enclosing = prev;
                self.fn_path.pop();
            }
        }
    }

    let mut c = Collector {
        mod_path: Vec::new(),
        fn_path: Vec::new(),
        enclosing: Enclosing::Free,
        entries: Vec::new(),
    };
    syn::visit::Visit::visit_file(&mut c, &file);
    Some(c.entries)
}

/// Finds the unique function/method in the head file matching `name` at `span`.
/// Returns `None` if no function matches or if the match is ambiguous.
fn head_signature(source: &str, name: &str, span: Span) -> Option<(FnIdentity, String)> {
    let entries = collect_functions(source)?;
    let (lo, hi) = (span.start.min(span.end), span.start.max(span.end));

    let containing: Vec<&FnEntry> = entries
        .iter()
        .filter(|e| e.identity.name == name && e.start_line <= lo && hi <= e.end_line)
        .collect();
    if let Some(entry) = pick_innermost_unique(&containing) {
        return Some((entry.identity.clone(), entry.signature.clone()));
    }

    let by_ident: Vec<&FnEntry> = entries
        .iter()
        .filter(|e| e.identity.name == name && lo <= e.ident_line && e.ident_line <= hi)
        .collect();
    if by_ident.len() == 1 {
        let entry = by_ident[0];
        return Some((entry.identity.clone(), entry.signature.clone()));
    }

    None
}

fn pick_innermost_unique<'a>(candidates: &[&'a FnEntry]) -> Option<&'a FnEntry> {
    let min_len = candidates.iter().map(|e| e.end_line.saturating_sub(e.start_line)).min()?;
    let mut best = candidates
        .iter()
        .copied()
        .filter(|e| e.end_line.saturating_sub(e.start_line) == min_len);
    let first = best.next()?;
    if best.next().is_some() {
        return None;
    }
    Some(first)
}

/// Finds the unique function/method in the base file matching `identity`.
/// Returns `None` if missing or ambiguous (e.g. duplicate `#[cfg]` definitions).
fn base_signature(source: &str, identity: &FnIdentity) -> Option<String> {
    let entries = collect_functions(source)?;
    let mut matches = entries.into_iter().filter(|e| e.identity == *identity);
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first.signature)
}

fn signature_changed(old_source: &str, new_source: &str, name: &str, head_span: Span) -> bool {
    let Some((identity, after)) = head_signature(new_source, name, head_span) else {
        return false;
    };
    let Some(before) = base_signature(old_source, &identity) else {
        return false;
    };
    before != after
}

pub fn merge_findings(findings: Vec<Finding>) -> Vec<Finding> {
    let mut merged: Vec<Finding> = Vec::new();
    for mut f in findings {
        let f_mechanical = is_mechanical(&f);
        if let Some(existing) = merged.iter_mut().find(|e| {
            e.file == f.file
                && e.line == f.line
                && (normalize(&e.text) == normalize(&f.text)
                    || f_mechanical != is_mechanical(e))
        }) {
            for source in f.sources.drain(..) {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                }
            }
            existing.sources.sort();
        } else {
            merged.push(f);
        }
    }
    merged.sort_by(|a, b| (&a.file, a.line, &a.id).cmp(&(&b.file, b.line, &b.id)));
    merged
}

/// Merge agent findings that point at the same spot: same file, lines within 3, and either inside the same
/// changed symbol (the innermost one containing each line) or both outside every symbol. Sources are unioned (a source may repeat itself), the highest
/// severity and the longest text win.
pub fn merge_agent_findings(
    mut findings: Vec<Finding>,
    symbols: &BTreeMap<String, Vec<(u32, u32, String)>>,
) -> Vec<Finding> {
    findings.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    // A line belongs to the innermost (smallest) changed symbol that contains it, or to none.
    let scope_of = |file: &str, line: u32| {
        symbols.get(file).and_then(|ss| {
            ss.iter()
                .enumerate()
                .filter(|(_, (s, e, _))| *s <= line && line <= *e)
                .min_by_key(|(_, (s, e, _))| e - s)
                .map(|(i, _)| i)
        })
    };
    let same_scope = |file: &str, a: u32, b: u32| scope_of(file, a) == scope_of(file, b);
    let mut merged: Vec<Finding> = Vec::new();
    for mut f in findings {
        if let Some(existing) = merged.iter_mut().find(|e| {
            e.file == f.file
                && e.line.abs_diff(f.line) <= 3
                && same_scope(&f.file, e.line, f.line)
        }) {
            for source in f.sources.drain(..) {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                }
            }
            existing.sources.sort();
            if severity_rank(&f.severity) > severity_rank(&existing.severity) {
                existing.severity = f.severity.clone();
            }
            if f.text.len() > existing.text.len() {
                existing.text = f.text.clone();
            }
            existing.line = existing.line.min(f.line);
            existing.id = finding_id(&existing.file, existing.line, &existing.text);
        } else {
            merged.push(f);
        }
    }
    merged
}

pub(crate) fn severity_rank(s: &str) -> u8 {
    match normalize_severity(s).as_str() {
        "high" => 3,
        "medium" => 2,
        _ => 1,
    }
}

fn is_mechanical(f: &Finding) -> bool {
    f.sources
        .iter()
        .any(|s| matches!(s.as_str(), "coverage" | "gap" | "heuristic"))
}

pub fn allowed_locations(
    pack: &EvidencePack,
    changed: &BTreeMap<String, Vec<u32>>,
) -> BTreeMap<String, BTreeSet<u32>> {
    let mut allowed: BTreeMap<String, BTreeSet<u32>> =
        changed.iter().map(|(f, ls)| (f.clone(), ls.iter().copied().collect())).collect();
    for s in &pack.symbols {
        allowed.entry(s.file.clone()).or_default().extend(s.span.start..=s.span.end);
        for e in s.callers.iter().chain(&s.entry_points) {
            allowed.entry(e.file.clone()).or_default().insert(e.line);
        }
    }
    allowed
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fresh_index() -> alsp_index::IndexState {
        alsp_index::IndexState {
            status: alsp_index::IndexStatus::Fresh,
            rebuilt: false,
            units: vec![alsp_index::UnitReport { path: ".".into(), ok: true, detail: None }],
            millis: 0,
            notes: vec![],
        }
    }

    #[test]
    fn test_DW_4_6_proof_drops_nonexistent_file_and_line() {
        let allowed = BTreeMap::from([("src/lib.rs".to_string(), BTreeSet::from([7]))]);
        let candidate = Finding::agent("claude", "missing.rs", 2, "bad call");
        assert_eq!(prove_agent_findings(vec![candidate], &allowed).dropped, 1);
        // an existing file with a line outside the allowed set is dropped; an allowed line is kept
        let bad_line = Finding::agent("claude", "src/lib.rs", 99, "bad line");
        let good_line = Finding::agent("claude", "src/lib.rs", 7, "ok line");
        let r = prove_agent_findings(vec![bad_line, good_line], &allowed);
        assert_eq!((r.dropped, r.kept.len()), (1, 1));
        assert_eq!(r.kept[0].line, 7);
    }

    #[test]
    fn test_DW_4_8_agent_chatter_is_not_a_finding_or_malformed_line() {
        let (findings, raw) =
            parse_agent_output("codex", "  I found an issue but formatted it wrong  \n");
        assert!(findings.is_empty());
        assert!(raw.is_empty());
    }

    #[test]
    fn test_DW_4_8_empty_agent_output_has_no_findings() {
        assert!(parse_agent_output("agy", "NO FINDINGS\n").0.is_empty());
    }

    #[test]
    fn test_DW_4_8_agent_parser_accepts_markers_and_counts_only_malformed_findings() {
        let (findings, malformed) = parse_agent_output(
            "agy",
            "thinking\n- `FINDING|src/a.rs|3|CRITICAL|first`\nNO FINDINGS\nFINDING|src/a.rs|x|low|bad\n",
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, "high");
        assert_eq!(malformed.len(), 1);
    }

    #[test]
    fn test_DW_4_8_severity_normalization_maps_aliases_and_unknown_values() {
        assert_eq!(normalize_severity("CRITICAL"), "high");
        assert_eq!(normalize_severity("Warning"), "low");
        assert_eq!(normalize_severity("minor"), "low");
        assert_eq!(normalize_severity("INFO"), "low");
        assert_eq!(normalize_severity("unexpected"), "medium");
    }

    #[test]
    fn test_DW_4_8_merges_nearby_agent_findings_within_symbol_and_prefers_severity_and_text() {
        let symbols =
            BTreeMap::from([("src/a.rs".to_string(), vec![(10, 20, "f".to_string())])]);
        let a = Finding::new("claude", "src/a.rs", 12, "low", "short");
        let b = Finding::new("agy", "src/a.rs", 14, "HIGH", "a longer actionable explanation");
        let merged = merge_agent_findings(vec![a, b], &symbols);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].sources, ["agy", "claude"]);
        assert_eq!(merged[0].severity, "high");
        assert_eq!(merged[0].line, 12);
        assert_eq!(merged[0].text, "a longer actionable explanation");
    }

    #[test]
    fn test_DW_4_5_duplicate_sources_merge_and_keep_sources() {
        let a = Finding::new("coverage", "src/a.rs", 4, "warning", " uncovered   line ");
        let b = Finding::new("claude", "src/a.rs", 4, "warning", "uncovered line");
        let merged = merge_findings(vec![a, b]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].sources, ["claude", "coverage"]);
    }

    #[test]
    fn test_DW_4_4_mechanical_gaps_use_threshold_and_uncovered_reason_without_agent() {
        use alsp_evidence::{Coverage, GapReason, PredictedGap, SymbolEvidence};
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn f(x: u8) {}\n").unwrap();
        let mut p = EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: fresh_index(),
            symbols: vec![],
            notes: vec![],
        };
        let mut s = SymbolEvidence {
            id: "f".into(),
            name: "f".into(),
            file: "a.rs".into(),
            span: Span { start: 1, end: 1 },
            kind: ChangeKind::Modified,
            callers: vec![],
            entry_points: vec![],
            coverage: Some(Coverage {
                lines_total: 1,
                lines_hit: 0,
                branches_total: None,
                branches_hit: None,
                uncovered_changed_lines: vec![1],
            }),
            gaps: vec![
                PredictedGap {
                    file: "a.rs".into(),
                    line: 1,
                    mutation: "x".into(),
                    p_tests_catch: 0.2,
                    reason: GapReason::PredictedSurvivor,
                },
                PredictedGap {
                    file: "a.rs".into(),
                    line: 1,
                    mutation: "y".into(),
                    p_tests_catch: 0.9,
                    reason: GapReason::Uncovered,
                },
            ],
        };
        p.symbols.push(s.clone());
        let src = PackSource {
            tree: root.path().into(),
            cache_dir: PathBuf::new(),
            build_env: vec![],
            changed_lines: BTreeMap::from([("a.rs".into(), vec![1])]),
        };
        let findings = mechanical_findings(&p, &src, |_| None);
        assert_eq!(findings.len(), 3);
        assert!(findings.iter().any(|f| f.sources == ["coverage"]));
        s.gaps[0].p_tests_catch = DEFAULT_GAP_THRESHOLD;
        p.symbols[0] = s;
        assert_eq!(mechanical_findings(&p, &src, |_| None).len(), 2);
    }

    #[test]
    fn test_DW_4_4_signature_change_with_out_of_diff_caller_is_heuristic() {
        use alsp_evidence::{Edge, Provenance, SymbolEvidence};
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn f(x: u16) {}\nfn caller() { f(1); }\n")
            .unwrap();
        let p = EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: fresh_index(),
            symbols: vec![SymbolEvidence {
                id: "f".into(),
                name: "crate::f".into(),
                file: "a.rs".into(),
                span: Span { start: 1, end: 1 },
                kind: ChangeKind::Modified,
                callers: vec![Edge {
                    id: "caller".into(),
                    file: "a.rs".into(),
                    line: 2,
                    provenance: Provenance::Resolved,
                }],
                entry_points: vec![],
                coverage: None,
                gaps: vec![],
            }],
            notes: vec![],
        };
        let src = PackSource {
            tree: root.path().into(),
            cache_dir: PathBuf::new(),
            build_env: vec![],
            changed_lines: BTreeMap::from([("a.rs".into(), vec![1])]),
        };
        let f = mechanical_findings(&p, &src, |_| {
            Some("fn f(x: u8) {}\nfn caller() { f(1); }\n".into())
        });
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].sources, ["heuristic"]);
        assert!(f[0].text.contains("signature changed"));
    }

    #[test]
    fn test_DW_4_1_same_method_name_in_two_impl_blocks_does_not_collide() {
        use alsp_evidence::{Edge, Provenance, SymbolEvidence};

        let base = "\
struct First(u8);
impl First {
    pub fn new(x: u8) -> Self {
        Self(x)
    }
}

struct Second(u8);
impl Second {
    pub fn new(x: u8) -> Self {
        Self(x)
    }
}

fn call_first() { let _ = First::new(1); }
fn call_second() { let _ = Second::new(2); }
";
        // Case 1: First::new changes body only (signature unchanged), Second::new changes signature (u8 -> u16).
        // Under the old bug, signature_for matched the last `fn new` (Second::new) for First::new and emitted a false positive.
        let head_second_sig_changed = "\
struct First(u8);
impl First {
    pub fn new(x: u8) -> Self {
        Self(x.saturating_add(1))
    }
}

struct Second(u16);
impl Second {
    pub fn new(x: u16) -> Self {
        Self(x)
    }
}

fn call_first() { let _ = First::new(1); }
fn call_second() { let _ = Second::new(2); }
";
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), head_second_sig_changed).unwrap();
        let mk_sym = |id: &str, name: &str, span: Span, caller_line: u32| SymbolEvidence {
            id: id.into(),
            name: name.into(),
            file: "lib.rs".into(),
            span,
            kind: ChangeKind::Modified,
            callers: vec![Edge {
                id: format!("caller_{caller_line}"),
                file: "lib.rs".into(),
                line: caller_line,
                provenance: Provenance::Resolved,
            }],
            entry_points: vec![],
            coverage: None,
            gaps: vec![],
        };
        let pack = EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: fresh_index(),
            symbols: vec![
                mk_sym("first_new", "First::new", Span { start: 3, end: 5 }, 15),
                mk_sym("second_new", "Second::new", Span { start: 10, end: 12 }, 16),
            ],
            notes: vec![],
        };
        let src = PackSource {
            tree: root.path().into(),
            cache_dir: PathBuf::new(),
            build_env: vec![],
            changed_lines: BTreeMap::from([("lib.rs".into(), vec![4, 10])]),
        };
        let findings = mechanical_findings(&pack, &src, |_| Some(base.into()));
        assert_eq!(
            findings.len(),
            1,
            "only Second::new changed signature; First::new must not false-positive: {findings:?}"
        );
        assert_eq!(findings[0].line, 10);
        assert!(findings[0].text.contains("lib.rs:16"));

        // Case 2: First::new changes signature (u8 -> u16), Second::new is unchanged.
        // Under the old bug, Second::new was last in file so First::new's signature change was missed.
        let head_first_sig_changed = "\
struct First(u16);
impl First {
    pub fn new(x: u16) -> Self {
        Self(x)
    }
}

struct Second(u8);
impl Second {
    pub fn new(x: u8) -> Self {
        Self(x)
    }
}

fn call_first() { let _ = First::new(1); }
fn call_second() { let _ = Second::new(2); }
";
        std::fs::write(root.path().join("lib.rs"), head_first_sig_changed).unwrap();
        let pack_first = EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: fresh_index(),
            symbols: vec![mk_sym("first_new", "First::new", Span { start: 3, end: 5 }, 15)],
            notes: vec![],
        };
        let findings_first = mechanical_findings(&pack_first, &src, |_| Some(base.into()));
        assert_eq!(findings_first.len(), 1, "First::new signature change must be detected");
        assert_eq!(findings_first[0].line, 3);
        assert!(findings_first[0].text.contains("lib.rs:15"));

        // Case 3: Neither changes signature (both modified in body only) -> zero findings.
        let head_body_only = "\
struct First(u8);
impl First {
    pub fn new(x: u8) -> Self {
        Self(x + 1)
    }
}

struct Second(u8);
impl Second {
    pub fn new(x: u8) -> Self {
        Self(x + 2)
    }
}

fn call_first() { let _ = First::new(1); }
fn call_second() { let _ = Second::new(2); }
";
        std::fs::write(root.path().join("lib.rs"), head_body_only).unwrap();
        let findings_none = mechanical_findings(&pack, &src, |_| Some(base.into()));
        assert!(findings_none.is_empty(), "body-only edits must emit zero findings: {findings_none:?}");
    }

    #[test]
    fn test_DW_4_1_ambiguous_base_impl_returns_none_without_false_positive() {
        let base = "\
struct Dup;
#[cfg(unix)]
impl Dup {
    pub fn new(x: u8) -> Self { let _ = x; Self }
}
#[cfg(not(unix))]
impl Dup {
    pub fn new(x: u16) -> Self { let _ = x; Self }
}
";
        let head = "\
struct Dup;
#[cfg(unix)]
impl Dup {
    pub fn new(x: u8) -> Self { let _ = x + 1; Self }
}
#[cfg(not(unix))]
impl Dup {
    pub fn new(x: u16) -> Self { let _ = x; Self }
}
";
        let (id, _) = head_signature(head, "new", Span { start: 4, end: 4 }).unwrap();
        assert_eq!(base_signature(base, &id), None, "ambiguous base impl must return None");
        assert!(!signature_changed(base, head, "new", Span { start: 4, end: 4 }));
    }

    #[test]
    fn test_DW_4_1_trait_impl_inherent_impl_and_free_fn_do_not_collide() {
        let base = "\
fn run(x: u8) {}
struct S;
impl S {
    fn run(&self, x: u8) {}
}
trait A { fn run(&self, x: u8); }
trait B { fn run(&self, x: u8); }
impl A for S {
    fn run(&self, x: u8) {}
}
impl B for S {
    fn run(&self, x: u8) {}
}
";
        let head = "\
fn run(x: u8) { let _ = x; }
struct S;
impl S {
    fn run(&self, x: u8) { let _ = x; }
}
trait A { fn run(&self, x: u8); }
trait B { fn run(&self, x: u16); }
impl A for S {
    fn run(&self, x: u8) { let _ = x; }
}
impl B for S {
    fn run(&self, x: u16) {}
}
";
        assert!(!signature_changed(base, head, "run", Span { start: 1, end: 1 }));
        assert!(!signature_changed(base, head, "run", Span { start: 4, end: 4 }));
        assert!(!signature_changed(base, head, "run", Span { start: 9, end: 9 }));
        assert!(signature_changed(base, head, "run", Span { start: 12, end: 12 }));
    }

    #[test]
    fn test_DW_4_5_mechanical_and_agent_same_location_merge_even_with_different_text() {
        let a = Finding::new("coverage", "src/a.rs", 4, "warning", "changed line is uncovered");
        let b = Finding::new("codex", "src/a.rs", 4, "bug", "returns a stale value");
        let m = merge_findings(vec![a, b]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].sources, ["codex", "coverage"]);
    }

    #[test]
    fn parser_keeps_pipes_in_text_and_accepts_repo_root_files() {
        let (f, bad) = parse_agent_output(
            "claude",
            "FINDING|src/a.rs|3|high|returns a || b wrongly\nFINDING|build.rs|10|high|root file bug\nFINDING|Cargo.toml|2|low|dep\n",
        );
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].text, "returns a || b wrongly");
        assert_eq!(f[1].file, "build.rs");
        assert_eq!(f[2].file, "Cargo.toml");
        let (f, bad) = parse_agent_output(
            "claude",
            "FINDING||3|high|no file\nFINDING|a.rs|3|high|\nFINDING|a.rs|3|high\n",
        );
        assert!(f.is_empty());
        assert_eq!(bad.len(), 3);
    }

    #[test]
    fn allowed_locations_unions_changed_lines_symbol_spans_callers_and_entry_points() {
        use alsp_evidence::{Edge, Provenance, SymbolEvidence};
        let mut pack = EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: fresh_index(),
            symbols: vec![SymbolEvidence {
                id: "f".into(),
                name: "f".into(),
                file: "a.rs".into(),
                span: Span { start: 1, end: 4 },
                kind: ChangeKind::Modified,
                callers: vec![],
                entry_points: vec![],
                coverage: None,
                gaps: vec![],
            }],
            notes: vec![],
        };
        let edge = |file: &str, line| Edge {
            id: "e".into(),
            file: file.into(),
            line,
            provenance: Provenance::Resolved,
        };
        pack.symbols[0].callers = vec![edge("c.rs", 9)];
        pack.symbols[0].entry_points = vec![edge("main.rs", 2)];
        let changed = BTreeMap::from([("z.rs".to_string(), vec![5, 6])]);
        let a = allowed_locations(&pack, &changed);
        assert_eq!(a["z.rs"], BTreeSet::from([5, 6]));
        assert_eq!(a["a.rs"], BTreeSet::from([1, 2, 3, 4]));
        assert_eq!(a["c.rs"], BTreeSet::from([9]));
        assert_eq!(a["main.rs"], BTreeSet::from([2]));
        assert!(!a.contains_key("other.rs"));
    }

    fn merge_symbols() -> BTreeMap<String, Vec<(u32, u32, String)>> {
        BTreeMap::from([(
            "src/ui.rs".to_string(),
            vec![(480, 490, "draw".to_string()), (500, 510, "other".to_string())],
        )])
    }

    #[test]
    fn merge_same_source_same_line_near_duplicates_into_one_row() {
        let a = Finding::new("claude", "src/ui.rs", 483, "medium", "layout width is computed twice");
        let b = Finding::new(
            "claude",
            "src/ui.rs",
            483,
            "high",
            "layout width is computed twice and can overflow",
        );
        let m = merge_agent_findings(vec![a, b], &merge_symbols());
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].sources, ["claude"]);
        assert_eq!(m[0].severity, "high");
        assert!(m[0].text.ends_with("can overflow"));
        assert_eq!(m[0].id, finding_id("src/ui.rs", 483, &m[0].text));
    }

    #[test]
    fn merge_findings_outside_any_symbol_within_three_lines_from_different_sources() {
        let a = Finding::new("codex", "SCHEMA.md", 133, "low", "field name differs from code");
        let b = Finding::new(
            "claude",
            "SCHEMA.md",
            135,
            "high",
            "documented field `x` no longer exists in the code",
        );
        let m = merge_agent_findings(vec![b, a], &merge_symbols());
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].sources, ["claude", "codex"]);
        assert_eq!(m[0].severity, "high");
        assert_eq!(m[0].line, 133);
        assert!(m[0].text.contains("no longer exists"));
    }

    #[test]
    fn merge_keeps_findings_apart_when_far_in_different_symbols_files_or_scope() {
        let s = merge_symbols();
        let f = |file: &str, line| Finding::new("claude", file, line, "low", format!("t{file}{line}"));
        assert_eq!(
            merge_agent_findings(vec![f("SCHEMA.md", 133), f("SCHEMA.md", 137)], &s).len(),
            2,
            "4 lines apart"
        );
        assert_eq!(
            merge_agent_findings(vec![f("src/ui.rs", 489), f("src/ui.rs", 500)], &s).len(),
            2,
            "different symbols"
        );
        assert_eq!(
            merge_agent_findings(vec![f("src/ui.rs", 490), f("src/ui.rs", 492)], &s).len(),
            2,
            "symbol vs outside"
        );
        assert_eq!(
            merge_agent_findings(vec![f("a.md", 5), f("b.md", 5)], &s).len(),
            2,
            "different files"
        );
    }
}
