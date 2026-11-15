//! The five questions. Each resolves its target, gathers facts from the index (and, for `cover`,
//! `gaps` and `tests --run`, from a coverage or mutant-listing run) and fills an [`Answer`].

use crate::target::{kind_str, Found, Target};
use crate::{Answer, CoverageAnswer, EntryPoint, Executed, GapItem, RefGroup, Section, SymMatch, TargetError, TargetInfo, TestRef, TreeSources, Workspace};
use alsp_coverage::{is_test_support_path, observe, CoverageTool, Observed};
use alsp_decide::DecisionModel;
use alsp_evidence::{ChangeKind, Edge, EvidencePack, GapReason, Graph, PackSource, Provenance, SymbolEvidence};
use alsp_gaps::{predict, GapsConfig};
use alsp_index::{Def, SymKind};
use std::collections::{BTreeMap, HashMap, HashSet};

const OUTSIDE: &str = "<outside any symbol>";

impl Workspace {
    fn answer(&self, command: &str, t: Option<&Target>) -> Answer {
        let target = t.map(|t| TargetInfo { kind: t.kind.clone(), name: t.name.clone(), file: t.file.clone(), span: t.span });
        let mut a = Answer::new(command, target, self.freshness());
        a.notes = self.notes.clone();
        if let Some(t) = t.filter(|t| t.region && t.defs.is_empty() && !self.index.has_file(&t.file)) {
            a.notes.push(self.unindexed_note(&t.file));
        }
        a
    }

    fn match_of(&self, d: &Def) -> SymMatch {
        SymMatch { name: d.name.clone(), kind: kind_str(d.kind).into(), file: d.file.clone(), line: d.name_line, span: d.span, references: self.index.references(&d.key).len() }
    }

    /// Where `query` is defined and how often it is used. Several matches are all listed.
    pub fn sym(&self, query: &str) -> Result<Answer, TargetError> {
        let (defs, empty_file): (Vec<Def>, Option<String>) = match self.lookup(query)? {
            Found::Defs(d) => (d.into_iter().cloned().collect(), None),
            Found::Place(t) => (t.defs, Some(t.file)),
        };
        let mut a = self.answer("sym", None);
        if let Some(file) = empty_file.filter(|_| defs.is_empty()) {
            a.notes.push(self.unindexed_note(&file));
        }
        a.matches = Some(Section::capped(Provenance::Resolved, defs.iter().map(|d| self.match_of(d)).collect(), self.limit));
        Ok(a)
    }

    /// Entry points that reach the target (all of its definitions, for a region), nearest first.
    fn entry_points(&self, graph: &mut Graph, t: &Target, a: &mut Answer) -> Vec<EntryPoint> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut truncated = Vec::new();
        for d in &t.defs {
            let reach = graph.entry_points(d);
            if reach.truncated {
                truncated.push(d.name.clone());
            }
            for e in reach.entry_points {
                if !seen.insert((e.file.clone(), e.line)) {
                    continue;
                }
                let def = self.index.defs_in(&e.file).find(|x| x.name_line == e.line && x.kind == SymKind::Fn);
                let kind = match def {
                    Some(x) if graph.is_test(x) => "test",
                    _ if e.provenance == Provenance::Resolved => "main",
                    _ => "pub",
                };
                out.push(EntryPoint { name: def.map(|x| x.name.clone()).unwrap_or_else(|| alsp_index::readable_name(&e.id)), kind: kind.into(), file: e.file, line: e.line, provenance: e.provenance });
            }
        }
        if !truncated.is_empty() {
            a.notes.push(format!("the entry-point search hit its limit for {}: the list may be incomplete", truncated.join(", ")));
        }
        out
    }

    /// Notes for definitions whose references can only be attributed within their own file.
    fn ambiguity_note(&self, t: &Target, a: &mut Answer) {
        let amb: Vec<String> = t.defs.iter().filter(|d| d.is_ambiguous()).map(|d| format!("{} ({} unattributed)", d.name, self.index.unattributed_refs(d))).collect();
        if !amb.is_empty() {
            a.notes.push(format!("the index gives {} the same symbol id as items in other crate roots (tests/*.rs, bins): only same-file references are counted, and references from files defining none of the copies are left out", amb.join(", ")));
        }
    }

    /// Every reference to the target, grouped by the symbol that contains it, with the entry
    /// points that reach it.
    pub fn refs(&self, query: &str) -> Result<Answer, TargetError> {
        let t = self.resolve(query)?;
        let src = TreeSources(&self.tree);
        let mut graph = Graph::new(&self.index, &src);
        let mut a = self.answer("refs", Some(&t));
        // One group per enclosing symbol, in the order its first reference is met.
        let mut list: Vec<RefGroup> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for d in &t.defs {
            for site in self.index.references(&d.key) {
                // Inside a region the references made from within it are its own business.
                if t.region && site.file == t.file && t.span.contains(site.line) {
                    continue;
                }
                let (key, symbol, kind, file, test) = match self.index.innermost(&site.file, site.line) {
                    Some(u) => (u.key.clone(), u.name.clone(), kind_str(u.kind), u.file.clone(), graph.is_test(u) || is_test_support_path(&u.file)),
                    None => (format!("{}\u{1}", site.file), OUTSIDE.to_string(), "file", site.file.clone(), is_test_support_path(&site.file)),
                };
                let i = *at.entry(key).or_insert_with(|| {
                    list.push(RefGroup { symbol, kind: kind.into(), file, test, lines: Vec::new() });
                    list.len() - 1
                });
                list[i].lines.push(site.line);
            }
        }
        for g in &mut list {
            g.lines.sort_unstable();
            g.lines.dedup();
        }
        a.reference_count = Some(list.iter().map(|g| g.lines.len()).sum());
        a.refs = Some(Section::capped(Provenance::Resolved, list, self.limit));
        let eps = self.entry_points(&mut graph, &t, &mut a);
        a.entry_points = Some(Section::capped(Provenance::Resolved, eps, self.limit));
        self.ambiguity_note(&t, &mut a);
        Ok(a)
    }

    /// Tests that reach the target. With `run`, also whether the package's tests execute it
    /// (runs `cargo llvm-cov`: the repository's build scripts and tests).
    pub fn tests(&self, query: &str, run: bool) -> Result<Answer, TargetError> {
        let t = self.resolve(query)?;
        let src = TreeSources(&self.tree);
        let mut graph = Graph::new(&self.index, &src);
        let mut a = self.answer("tests", Some(&t));
        let direct: HashSet<String> = t.defs.iter().flat_map(|d| graph.callers(&d.key)).map(|e| e.id).collect();
        let eps = self.entry_points(&mut graph, &t, &mut a);
        let tests: Vec<TestRef> = eps
            .into_iter()
            .filter(|e| e.kind == "test")
            .map(|e| {
                let id = self.index.defs_in(&e.file).find(|x| x.name_line == e.line).map(|x| x.id.clone()).unwrap_or_default();
                TestRef { name: e.name, file: e.file, line: e.line, direct: direct.contains(&id) }
            })
            .collect();
        let none_found = tests.is_empty();
        a.tests = Some(Section::capped(Provenance::Resolved, tests, self.limit));
        self.ambiguity_note(&t, &mut a);
        if run {
            let obs = self.measure(&t, &mut a);
            if let Some(c) = obs.coverage(&t.file, t.span, |_| false) {
                let status = if c.lines_hit > 0 { "yes" } else { "no" };
                a.executed = Some(Executed { provenance: Provenance::Observed, status: status.into(), lines_hit: c.lines_hit, lines_total: c.lines_total });
                if status == "yes" && none_found {
                    a.notes.push("the tests run this code, but none reaches it through resolved references (calls through trait objects, macros or function pointers are not followed)".into());
                } else if status == "no" && !none_found {
                    a.notes.push("tests reference this code but did not run its lines (ignored, feature-gated or failing before reaching it)".into());
                }
            } else if a.unavailable.is_none() {
                a.executed = Some(Executed { provenance: Provenance::Observed, status: "unknown".into(), lines_hit: 0, lines_total: 0 });
            }
            a.notes.push("--run reports whether the package's tests ran the target, not which test did: coverage is measured for the whole package at once".into());
        }
        Ok(a)
    }

    /// Describe `t` to the coverage runner as a one-symbol pack and measure its package.
    fn observe_symbols(&self, symbols: Vec<SymbolEvidence>) -> (EvidencePack, PackSource, Observed) {
        let pack = EvidencePack { base: String::new(), head: self.head.clone(), index: self.state.clone(), symbols, notes: Vec::new() };
        let src = PackSource { tree: self.tree.clone(), cache_dir: self.cache_dir.clone(), build_env: self.build_env.clone(), changed_lines: BTreeMap::new() };
        let tool = CoverageTool { notify: self.progress, ..self.coverage_tool.clone() };
        let obs = observe(&pack, &src, &tool);
        for r in obs.runs() {
            let what = match &r.error {
                None => if r.cached { "cached" } else { "measured" },
                Some(_) => "unavailable",
            };
            (self.progress)(&format!("coverage {}: {what} in {:.1}s", r.package, r.millis as f64 / 1000.0));
        }
        (pack, src, obs)
    }

    /// Coverage of the whole target span; says in `a` when nothing could be measured.
    fn measure(&self, t: &Target, a: &mut Answer) -> Observed {
        let sym = SymbolEvidence { id: "target".into(), name: t.name.clone(), file: t.file.clone(), span: t.span, kind: ChangeKind::Modified, callers: vec![], entry_points: vec![], coverage: None, gaps: vec![] };
        let (_, _, obs) = self.observe_symbols(vec![sym]);
        self.absorb(&obs, a);
        obs
    }

    /// Carry what the run said into the answer: notes, and `unavailable` when no package ran.
    fn absorb(&self, obs: &Observed, a: &mut Answer) {
        a.notes.extend(obs.notes().iter().cloned());
        if !obs.runs().iter().any(|r| r.error.is_none()) {
            if obs.runs().is_empty() && obs.notes().is_empty() {
                a.notes.push("nothing was run: the target is not Rust source, or sits under tests/, benches/ or examples/, which cargo llvm-cov reports nothing for".into());
            }
            a.unavailable = Some("coverage could not be measured (see the notes); nothing here is observed".into());
        }
    }

    /// Observed line and branch coverage of the target's span (runs `cargo llvm-cov`: the
    /// repository's build scripts and tests).
    pub fn cover(&self, query: &str) -> Result<Answer, TargetError> {
        let t = self.resolve(query)?;
        let mut a = self.answer("cover", Some(&t));
        let obs = self.measure(&t, &mut a);
        match obs.coverage(&t.file, t.span, |_| true) {
            Some(c) => {
                a.coverage = Some(CoverageAnswer {
                    provenance: Provenance::Observed,
                    lines_total: c.lines_total,
                    lines_hit: c.lines_hit,
                    branches_total: c.branches_total,
                    branches_hit: c.branches_hit,
                    uncovered_lines: c.uncovered_changed_lines,
                });
            }
            None if a.unavailable.is_none() => a.notes.push("no instrumented lines in the target's span (a type definition, or code compiled out)".into()),
            None => {}
        }
        Ok(a)
    }

    /// Mutants in the target's span, and the predicted chance the tests catch each. Mutants are
    /// only listed (`cargo mutants --list`), never run; `cover`'s test run still happens, to find
    /// the mutants no test reaches. The model sees the function and the tests that reach it.
    pub fn gaps(&self, query: &str, model: &dyn DecisionModel, cfg: &GapsConfig) -> Result<Answer, TargetError> {
        let t = self.resolve(query)?;
        let mut a = self.answer("gaps", Some(&t));
        // Mutants live in code: functions, consts and statics (SCIP term symbols end in `.`).
        let code: Vec<&Def> = t.defs.iter().filter(|d| d.kind.is_code() && d.id.ends_with('.')).collect();
        if code.is_empty() {
            a.notes.push("no function, const or static in the target: mutants only apply to code".into());
            a.gaps = Some(Section::capped(Provenance::Predicted, vec![], self.limit));
            return Ok(a);
        }
        let src = TreeSources(&self.tree);
        let mut graph = Graph::new(&self.index, &src);
        let symbols: Vec<SymbolEvidence> = code
            .iter()
            .map(|d| {
                let entry_points: Vec<Edge> = graph.entry_points(d).entry_points;
                SymbolEvidence { id: d.id.clone(), name: d.name.clone(), file: d.file.clone(), span: d.span, kind: ChangeKind::Modified, callers: vec![], entry_points, coverage: None, gaps: vec![] }
            })
            .collect();
        let (mut pack, psrc, obs) = self.observe_symbols(symbols);
        self.absorb(&obs, &mut a);
        a.unavailable = None; // coverage is optional here: without it every mutant goes to the model
        let r = predict(&mut pack, &psrc, &obs, model, &self.mutants_tool, cfg);
        (self.progress)(&format!(
            "gaps: {} mutants listed in {:.1}s, {} uncovered (free), {} sent to the model, {} dropped over budget; {} input tokens, ${:.4}",
            r.listed,
            r.list_millis as f64 / 1000.0,
            r.uncovered,
            r.scored + r.unscored,
            r.dropped,
            r.input_tokens,
            r.cost_usd
        ));
        a.notes.extend(pack.notes.iter().cloned());
        let mut items: Vec<GapItem> = pack
            .symbols
            .iter()
            .flat_map(|s| &s.gaps)
            .map(|g| GapItem {
                file: g.file.clone(),
                line: g.line,
                mutation: g.mutation.clone(),
                p_tests_catch: g.p_tests_catch,
                provenance: if g.reason == GapReason::Uncovered { Provenance::Observed } else { Provenance::Predicted },
            })
            .collect();
        // Model predictions first (lowest chance first), then lines no test runs.
        items.sort_by(|x, y| (x.provenance == Provenance::Observed).cmp(&(y.provenance == Provenance::Observed)).then(x.p_tests_catch.total_cmp(&y.p_tests_catch)).then((&x.file, x.line).cmp(&(&y.file, y.line))));
        a.gaps = Some(Section::capped(Provenance::Predicted, items, self.limit));
        a.unavailable = r.error;
        Ok(a)
    }
}
