//! `alsp-gaps`: predicted test gaps for the symbols a diff changes, with no mutation run.
//!
//! [`predict`] lists the mutants cargo-mutants would try (`--list` only: nothing is built or run),
//! keeps the ones inside changed functions, and decides for each whether the tests would notice it:
//!
//! * a mutant on lines no test runs is a gap by observation ([`GapReason::Uncovered`]) and costs no
//!   model call;
//! * every other mutant (within the review's budget, highest-value operators first) goes to the
//!   decision model with the enclosing function and the tests the symbol index says reach it, and
//!   is a gap when P(tests catch it) is below the threshold ([`GapReason::PredictedSurvivor`]).
//!
//! Whatever cannot be answered (no mutant listing, a failing model, coverage that could not run)
//! is said in the pack's notes; the rest of the pack is untouched.

#![cfg_attr(test, allow(non_snake_case))]

mod mutants;
mod prompt;

pub use mutants::{ensure_list_only, list as list_mutants, list_command, Mutant, MutantsTool};
pub use prompt::{TestFn, FUNCTION_CHARS_CAP, TEST_CHARS_CAP};

use alsp_coverage::{is_test_support_path, Hit, Observed};
use alsp_decide::{cost_usd, DecisionModel, Item, DEFAULT_GAP_THRESHOLD};
use alsp_evidence::{test_fn_text, ChangeKind, EvidencePack, GapReason, PackSource, PredictedGap};
use alsp_index::Span;
use std::collections::HashMap;
use std::path::{Component, Path};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct GapsConfig {
    /// Most mutants sent to the model in one review. Mutants on uncovered lines are free and not
    /// counted.
    pub budget: usize,
    /// A predicted survivor is reported when P(tests catch) is below this.
    pub threshold: f64,
}

impl Default for GapsConfig {
    fn default() -> Self {
        GapsConfig { budget: 100, threshold: DEFAULT_GAP_THRESHOLD }
    }
}

/// What a run did, for the person watching the spend.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GapsReport {
    /// Mutants cargo-mutants listed for the changed files.
    pub listed: usize,
    /// Listed mutants inside no changed function.
    pub outside: usize,
    /// Listing entries that could not be read.
    pub malformed: usize,
    /// Mutants on lines no test runs: reported without a model call.
    pub uncovered: usize,
    /// Mutants the model answered.
    pub scored: usize,
    /// Mutants dropped for the budget.
    pub dropped: usize,
    /// Mutants sent but not answered (per-item model errors, unreadable source).
    pub unscored: usize,
    pub input_tokens: u64,
    pub cost_usd: f64,
    pub list_millis: u64,
    /// Why predictions are missing altogether (listing failed, model failed).
    pub error: Option<String>,
}

/// The reporting rule: below the threshold, and only strictly (p equal to it is not a gap).
pub fn is_predicted_gap(p_tests_catch: f64, threshold: f64) -> bool {
    p_tests_catch < threshold
}

/// Index of the innermost span of `spans` (file, lines) that holds `lines` of `file` entirely; ties
/// go to the earlier entry. A mutant therefore belongs to exactly one symbol or to none.
pub fn owner_of(spans: &[(&str, Span)], file: &str, lines: (u32, u32)) -> Option<usize> {
    spans
        .iter()
        .enumerate()
        .filter(|(_, (f, s))| *f == file && s.start <= lines.0 && lines.1 <= s.end)
        .min_by_key(|(i, (_, s))| (s.len(), *i))
        .map(|(i, _)| i)
}

/// Operators most likely to reveal a missing assertion come first when the budget forces a choice.
fn genre_rank(genre: &str) -> u8 {
    ["FnValue", "BinaryOperator", "MatchArm", "MatchArmGuard", "UnaryOperator"].iter().position(|g| *g == genre).map_or(5, |i| i as u8)
}

fn safe_rel(rel: &str) -> bool {
    Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)))
}

struct Files<'a> {
    tree: &'a Path,
    cache: HashMap<String, Option<String>>,
}

impl Files<'_> {
    fn get(&mut self, rel: &str) -> Option<&str> {
        let tree = self.tree;
        self.cache.entry(rel.to_string()).or_insert_with(|| if safe_rel(rel) { std::fs::read_to_string(tree.join(rel)).ok() } else { None }).as_deref()
    }
}

struct Candidate {
    sym: usize,
    m: Mutant,
    changed: bool,
}

/// Fill `gaps` of the changed functions, constants and statics of `pack` (SCIP term symbols end in
/// `.`; functions in `().`). Never fails; see the module docs.
pub fn predict(pack: &mut EvidencePack, src: &PackSource, observed: &Observed, model: &dyn DecisionModel, tool: &MutantsTool, cfg: &GapsConfig) -> GapsReport {
    let mut rep = GapsReport::default();
    let fns: Vec<usize> = (0..pack.symbols.len()).filter(|&i| pack.symbols[i].kind != ChangeKind::Removed && pack.symbols[i].id.ends_with('.') && !is_test_support_path(&pack.symbols[i].file)).collect();
    let mut files: Vec<&str> = fns.iter().map(|&i| pack.symbols[i].file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    if files.is_empty() {
        return rep;
    }

    let started = Instant::now();
    let listed = mutants::list(tool, &src.tree, &files, &src.build_env);
    rep.list_millis = started.elapsed().as_millis() as u64;
    let (all, malformed) = match listed {
        Ok(x) => x,
        Err(e) => {
            pack.notes.push(format!("predicted gaps unavailable: {e}"));
            rep.error = Some(e);
            return rep;
        }
    };
    rep.listed = all.len();
    rep.malformed = malformed;

    // Join mutants to the innermost changed function that holds them.
    let spans: Vec<(&str, Span)> = fns.iter().map(|&i| (pack.symbols[i].file.as_str(), pack.symbols[i].span)).collect();
    let mut covered: Vec<Candidate> = Vec::new();
    let mut gaps: Vec<(usize, PredictedGap)> = Vec::new();
    for m in all {
        let Some(k) = owner_of(&spans, &m.file, (m.start.0, m.end.0)) else {
            rep.outside += 1;
            continue;
        };
        let sym = fns[k];
        if observed.lines_status(&m.file, m.start.0, m.end.0) == Hit::No {
            rep.uncovered += 1;
            gaps.push((sym, PredictedGap { file: m.file.clone(), line: m.start.0, mutation: m.description.clone(), p_tests_catch: 0.0, reason: GapReason::Uncovered }));
        } else {
            let changed = (m.start.0..=m.end.0).any(|l| src.is_changed(&m.file, l));
            covered.push(Candidate { sym, m, changed });
        }
    }

    // Over budget: mutants on changed lines first, then the more valuable operators.
    covered.sort_by(|a, b| (!a.changed, genre_rank(&a.m.genre), &a.m.file, a.m.start).cmp(&(!b.changed, genre_rank(&b.m.genre), &b.m.file, b.m.start)));
    rep.dropped = covered.len().saturating_sub(cfg.budget);
    covered.truncate(cfg.budget);

    let mut fs = Files { tree: &src.tree, cache: HashMap::new() };
    let mut tests: HashMap<usize, Vec<TestFn>> = HashMap::new();
    let mut items: Vec<Item> = Vec::new();
    let mut sent: Vec<&Candidate> = Vec::new();
    for c in &covered {
        let s = &pack.symbols[c.sym];
        let tests = tests.entry(c.sym).or_insert_with(|| {
            let mut seen = std::collections::HashSet::new();
            let mut v = Vec::new();
            for e in &s.entry_points {
                if !seen.insert((e.file.clone(), e.line)) {
                    continue;
                }
                if let Some(text) = fs.get(&e.file).and_then(|t| test_fn_text(t, e.line)) {
                    v.push(TestFn { file: e.file.clone(), line: e.line, text });
                }
            }
            v
        });
        let Some(text) = fs.get(&s.file) else {
            rep.unscored += 1;
            continue;
        };
        items.push(Item { id: items.len().to_string(), input: prompt::build(&c.m, &s.name, text, s.span.start, s.span.end, tests) });
        sent.push(c);
    }

    let mut model_error = None;
    if !items.is_empty() {
        match model.score(&prompt::question(), &items) {
            Ok(scores) if scores.len() == items.len() => {
                let mut first_err = None;
                for (c, sc) in sent.iter().zip(&scores) {
                    rep.input_tokens += sc.input_tokens;
                    match sc.p {
                        Some(p) if p.is_finite() => {
                            rep.scored += 1;
                            if is_predicted_gap(p, cfg.threshold) {
                                gaps.push((c.sym, PredictedGap { file: c.m.file.clone(), line: c.m.start.0, mutation: c.m.description.clone(), p_tests_catch: p, reason: GapReason::PredictedSurvivor }));
                            }
                        }
                        _ => {
                            rep.unscored += 1;
                            first_err.get_or_insert_with(|| sc.error.clone().unwrap_or_else(|| "no score".into()));
                        }
                    }
                }
                if let Some(e) = first_err {
                    pack.notes.push(format!("{} model call(s) returned no score (first: {e})", rep.unscored));
                }
            }
            Ok(scores) => model_error = Some(format!("the model returned {} scores for {} mutants", scores.len(), items.len())),
            Err(e) => model_error = Some(e.to_string()),
        }
    }
    pack.notes.extend(model.take_notes());
    if let Some(e) = model_error {
        rep.unscored += items.len();
        pack.notes.push(format!("predicted gaps unavailable: {e}; gaps on uncovered lines are still listed"));
        rep.error = Some(e);
    }
    rep.cost_usd = cost_usd(rep.input_tokens);

    for (sym, g) in gaps {
        pack.symbols[sym].gaps.push(g);
    }
    for s in &mut pack.symbols {
        // Model predictions first (lowest P first), then the uncovered ones by line.
        s.gaps.sort_by(|a, b| (a.reason == GapReason::Uncovered).cmp(&(b.reason == GapReason::Uncovered)).then(a.p_tests_catch.total_cmp(&b.p_tests_catch)).then(a.line.cmp(&b.line)));
    }
    pack.notes.push(summary(&rep, cfg));
    rep
}

fn summary(r: &GapsReport, cfg: &GapsConfig) -> String {
    let mut s = format!("gaps: {} mutants listed by `cargo mutants --list` (none executed), {} outside the changed functions; {} on lines no test runs (no model call)", r.listed, r.outside, r.uncovered);
    s += &format!("; {} scored by the model ({} input tokens, ${:.4})", r.scored, r.input_tokens, r.cost_usd);
    if r.dropped > 0 {
        s += &format!("; {} not scored: over the budget of {} per review (changed lines and the more valuable operators first)", r.dropped, cfg.budget);
    }
    if r.malformed > 0 {
        s += &format!("; {} listing entries unreadable", r.malformed);
    }
    s
}

#[cfg(test)]
mod tests;
