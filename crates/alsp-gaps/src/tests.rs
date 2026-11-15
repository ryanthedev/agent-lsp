#![allow(non_snake_case)]
use super::*;
use alsp_coverage::testkit::{lcov, pack, sym, Fx};
use alsp_coverage::{observe, CoverageTool};
use alsp_decide::{DecideError, ItemScore, Question};
use alsp_evidence::{Edge, Provenance};
use std::cell::RefCell;

const LIB: &str = "pub fn a(x: u32) -> u32 {\n    if x < 3 {\n        x + 1\n    } else {\n        0\n    }\n}\npub fn never() -> u32 {\n    7\n}\n";
const TESTS: &str = "use fx::a;\n#[test]\nfn t_a() {\n    assert_eq!(a(1), 2);\n}\nfn not_a_test() {}\n";

/// Records every call; answers from `answer(item index, input)`.
struct Stub {
    answer: Box<dyn Fn(usize, &str) -> Option<f64>>,
    fail: bool,
    calls: RefCell<Vec<Vec<Item>>>,
}

impl Stub {
    fn new(answer: impl Fn(usize, &str) -> Option<f64> + 'static) -> Stub {
        Stub { answer: Box::new(answer), fail: false, calls: RefCell::new(vec![]) }
    }
    fn items(&self) -> Vec<Item> {
        self.calls.borrow().iter().flatten().cloned().collect()
    }
}

impl DecisionModel for Stub {
    fn score(&self, _: &Question, items: &[Item]) -> Result<Vec<ItemScore>, DecideError> {
        self.calls.borrow_mut().push(items.to_vec());
        if self.fail {
            return Err(DecideError::Failed("budget exceeded: test".into()));
        }
        Ok(items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let p = (self.answer)(i, &it.input);
                ItemScore { id: it.id.clone(), p, error: p.is_none().then(|| "safety block".to_string()), input_tokens: 1000 }
            })
            .collect())
    }
}

fn mj(file: &str, s: (u32, u32), e: (u32, u32), genre: &str, desc: &str) -> String {
    format!(
        r#"{{"file":"{file}","package":"fx","genre":"{genre}","name":"{file}:{}:{}: {desc}","replacement":"R","span":{{"start":{{"line":{},"column":{}}},"end":{{"line":{},"column":{}}}}},"diff":"D"}}"#,
        s.0, s.1, s.0, s.1, e.0, e.1
    )
}

/// `a` (lines 1-7) has covered mutants on lines 2 and 3; `never` (8-10) is never run.
struct World {
    fx: Fx,
    pack: EvidencePack,
    obs: Observed,
}

fn world(extra_mutants: &[String]) -> World {
    let mut fx = Fx::new(&[("src/lib.rs", LIB), ("tests/it.rs", TESTS)]);
    fx.set_lcov(&lcov(&[("src/lib.rs", &[(1, 4), (2, 4), (3, 3), (5, 1), (9, 0)], &[])]));
    let mut ms = vec![
        mj("src/lib.rs", (2, 10), (2, 11), "BinaryOperator", "replace < with <= in a"),
        mj("src/lib.rs", (3, 11), (3, 12), "BinaryOperator", "replace + with - in a"),
        mj("src/lib.rs", (9, 5), (9, 6), "FnValue", "replace never -> u32 with 0"),
        mj("src/lib.rs", (9, 5), (9, 6), "FnValue", "replace never -> u32 with 1"),
        mj("src/lib.rs", (30, 1), (30, 2), "BinaryOperator", "replace + with * in elsewhere"),
    ];
    ms.extend(extra_mutants.iter().cloned());
    fx.set_mutants(&format!("[{}]", ms.join(",")));
    fx.src.changed_lines.insert("src/lib.rs".into(), vec![2, 3, 9]);
    let mut a = sym("src/lib.rs", "lib::a", "x a().", ChangeKind::Modified, 1, 7);
    a.entry_points = vec![
        Edge { id: "x t_a().".into(), file: "tests/it.rs".into(), line: 3, provenance: Provenance::Resolved },
        Edge { id: "x not_a_test().".into(), file: "tests/it.rs".into(), line: 6, provenance: Provenance::Resolved },
        Edge { id: "x pubfn().".into(), file: "src/lib.rs".into(), line: 1, provenance: Provenance::Heuristic },
    ];
    let never = sym("src/lib.rs", "lib::never", "x never().", ChangeKind::Added, 8, 10);
    let p = pack(vec![a, never]);
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    World { fx, pack: p, obs }
}

fn tool(w: &World) -> MutantsTool {
    MutantsTool { program: w.fx.cargo.clone(), ..Default::default() }
}

fn run(w: &mut World, model: &Stub, cfg: &GapsConfig) -> GapsReport {
    let t = tool(w);
    predict(&mut w.pack, &w.fx.src, &w.obs, model, &t, cfg)
}

#[test]
fn test_DW_3_2_gaps_are_listed_and_cargo_mutants_only_ever_runs_with_list() {
    let mut w = world(&[]);
    let m = Stub::new(|_, _| Some(0.2));
    let rep = run(&mut w, &m, &GapsConfig::default());
    let calls = w.fx.calls();
    let mutants: Vec<&String> = calls.iter().filter(|c| c.starts_with("mutants")).collect();
    assert_eq!(mutants.len(), 1, "one listing for all changed files: {calls:?}");
    assert!(mutants[0].contains("--list") && mutants[0].contains("--file=src/lib.rs"), "{}", mutants[0]);
    assert!(calls.iter().filter(|c| c.contains("mutants")).all(|c| c.contains("--list")), "{calls:?}");
    assert_eq!(rep.listed, 5);
    let gaps: Vec<&PredictedGap> = w.pack.symbols.iter().flat_map(|s| &s.gaps).collect();
    assert_eq!(gaps.iter().filter(|g| g.reason == GapReason::PredictedSurvivor).count(), 2);
    assert!(gaps.iter().all(|g| g.file == "src/lib.rs"));
}

#[test]
fn test_DW_3_3_mutants_on_uncovered_lines_are_uncovered_gaps_and_cost_no_model_call() {
    let mut w = world(&[]);
    let m = Stub::new(|_, _| Some(0.9));
    let rep = run(&mut w, &m, &GapsConfig::default());
    assert_eq!(rep.uncovered, 2);
    let items = m.items();
    assert_eq!(items.len(), 2, "only the two covered mutants are sent");
    assert!(items.iter().all(|i| !i.input.contains("never")), "nothing about the unrun function reaches the model");
    let never = &w.pack.symbols[1];
    assert_eq!(never.gaps.len(), 2);
    assert!(never.gaps.iter().all(|g| g.reason == GapReason::Uncovered && g.p_tests_catch == 0.0 && g.line == 9));
    assert!(w.pack.symbols[0].gaps.is_empty(), "p=0.9 is not a gap");
    assert_eq!((rep.scored, rep.input_tokens), (2, 2000));
    assert!((rep.cost_usd - 0.0002).abs() < 1e-12, "{}", rep.cost_usd);
}

#[test]
fn test_DW_3_3_with_no_covered_mutants_the_model_is_never_called() {
    let mut w = world(&[]);
    w.fx.set_mutants(&format!("[{}]", mj("src/lib.rs", (9, 5), (9, 6), "FnValue", "replace never with 0")));
    let m = Stub::new(|_, _| panic!("must not be called"));
    let rep = run(&mut w, &m, &GapsConfig::default());
    assert!(m.calls.borrow().is_empty());
    assert_eq!((rep.uncovered, rep.scored), (1, 0));
}

#[test]
fn test_DW_3_7_gap_threshold_boundary() {
    assert!(is_predicted_gap(0.4999, 0.5));
    assert!(!is_predicted_gap(0.5, 0.5), "equal is not below");
    assert!(!is_predicted_gap(0.5001, 0.5));
    assert!(is_predicted_gap(0.0, DEFAULT_GAP_THRESHOLD) && !is_predicted_gap(1.0, DEFAULT_GAP_THRESHOLD));
    // end to end: the model's answers 0.49 / 0.50 are split by the default threshold
    let mut w = world(&[]);
    let m = Stub::new(|i, _| Some(if i == 0 { 0.49 } else { 0.50 }));
    run(&mut w, &m, &GapsConfig::default());
    let a = &w.pack.symbols[0].gaps;
    assert_eq!(a.len(), 1);
    assert_eq!((a[0].reason, a[0].p_tests_catch), (GapReason::PredictedSurvivor, 0.49));
    // a stricter threshold reports nothing
    let mut w = world(&[]);
    run(&mut w, &Stub::new(|_, _| Some(0.2)), &GapsConfig { threshold: 0.2, ..Default::default() });
    assert!(w.pack.symbols[0].gaps.is_empty());
}

#[test]
fn test_DW_3_6_a_mutant_joins_the_innermost_symbol_holding_it_or_none() {
    let s = |a, b| Span { start: a, end: b };
    let spans = [("a.rs", s(1, 20)), ("a.rs", s(5, 9)), ("b.rs", s(1, 20))];
    assert_eq!(owner_of(&spans, "a.rs", (6, 7)), Some(1), "nested function wins");
    assert_eq!(owner_of(&spans, "a.rs", (2, 3)), Some(0));
    assert_eq!(owner_of(&spans, "a.rs", (4, 6)), Some(0), "straddling the inner span: only the outer holds it");
    assert_eq!(owner_of(&spans, "a.rs", (19, 21)), None, "ends outside every span");
    assert_eq!(owner_of(&spans, "b.rs", (1, 1)), Some(2), "files are separate");
    assert_eq!(owner_of(&spans, "c.rs", (1, 1)), None);
    assert_eq!(owner_of(&[("a.rs", s(3, 4)), ("a.rs", s(3, 4))], "a.rs", (3, 3)), Some(0), "identical spans tie to the first");
    // through predict: line 30 is in no changed function
    let mut w = world(&[]);
    let rep = run(&mut w, &Stub::new(|_, _| Some(0.1)), &GapsConfig::default());
    assert_eq!((rep.listed, rep.outside), (5, 1));
    let placed: usize = w.pack.symbols.iter().map(|s| s.gaps.len()).sum();
    assert_eq!(rep.uncovered + rep.scored, 4);
    assert_eq!(placed, 4, "every listed mutant is in exactly one symbol or counted outside");
}

#[test]
fn test_DW_3_5_model_failure_leaves_uncovered_gaps_and_a_note_and_nothing_else_breaks() {
    let mut w = world(&[]);
    let mut m = Stub::new(|_, _| Some(0.1));
    m.fail = true;
    let rep = run(&mut w, &m, &GapsConfig::default());
    assert!(rep.error.as_deref().unwrap().contains("budget exceeded: test"));
    assert_eq!(w.pack.symbols[1].gaps.len(), 2, "uncovered gaps survive");
    assert!(w.pack.symbols[0].gaps.is_empty());
    assert!(w.pack.notes.iter().any(|n| n.starts_with("predicted gaps unavailable: budget exceeded: test")), "{:?}", w.pack.notes);
    assert_eq!((rep.scored, rep.unscored, rep.cost_usd), (0, 2, 0.0));
}

#[test]
fn per_item_model_errors_are_counted_and_noted() {
    let mut w = world(&[]);
    let rep = run(&mut w, &Stub::new(|i, _| if i == 0 { None } else { Some(0.1) }), &GapsConfig::default());
    assert_eq!((rep.scored, rep.unscored), (1, 1));
    assert!(w.pack.notes.iter().any(|n| n.contains("1 model call(s) returned no score (first: safety block)")), "{:?}", w.pack.notes);
    assert_eq!(w.pack.symbols[0].gaps.len(), 1);
}

#[test]
fn test_DW_3_5_over_budget_keeps_changed_lines_and_valuable_operators_and_says_how_many_were_dropped() {
    // two more covered mutants: a FnValue on a changed line, and a UnaryOperator on an unchanged line
    let extra = [mj("src/lib.rs", (1, 28), (7, 2), "FnValue", "replace a -> u32 with 0"), mj("src/lib.rs", (5, 9), (5, 10), "UnaryOperator", "delete - in a")];
    let mut w = world(&extra);
    w.fx.src.changed_lines.insert("src/lib.rs".into(), vec![1, 2, 3, 9]);
    let m = Stub::new(|_, _| Some(0.1));
    let rep = run(&mut w, &m, &GapsConfig { budget: 2, ..Default::default() });
    assert_eq!((rep.scored, rep.dropped), (2, 2));
    let kept: Vec<String> = w.pack.symbols[0].gaps.iter().map(|g| g.mutation.clone()).collect();
    assert!(kept.contains(&"replace a -> u32 with 0 (col 28)".to_string()), "FnValue first: {kept:?}");
    assert!(kept.contains(&"replace < with <= in a (col 10)".to_string()), "then the changed-line operator: {kept:?}");
    assert!(!kept.iter().any(|k| k.starts_with("delete")), "{kept:?}");
    assert!(w.pack.notes.iter().any(|n| n.contains("2 not scored: over the budget of 2")), "{:?}", w.pack.notes);
    assert_eq!(m.items().len(), 2);
    assert_eq!(w.pack.symbols[1].gaps.len(), 2, "uncovered mutants do not count against the budget");
}

#[test]
fn a_listing_failure_is_noted_and_no_gaps_appear() {
    let mut w = world(&[]);
    w.fx.flag("mutants.fail", true);
    let m = Stub::new(|_, _| panic!("no model call without mutants"));
    let rep = run(&mut w, &m, &GapsConfig::default());
    assert!(rep.error.unwrap().contains("cannot list mutants"));
    assert!(w.pack.symbols.iter().all(|s| s.gaps.is_empty()));
    assert!(w.pack.notes[0].starts_with("predicted gaps unavailable: `cargo mutants --list` exited 1"), "{:?}", w.pack.notes);
}

#[test]
fn symbols_that_are_not_changed_functions_trigger_no_listing() {
    let mut w = world(&[]);
    w.pack.symbols = vec![
        sym("src/lib.rs", "lib::gone", "gone().", ChangeKind::Removed, 1, 3),
        sym("src/lib.rs", "lib::S", "x S#", ChangeKind::Modified, 1, 3),
        sym("tests/it.rs", "it::t_a", "x t_a().", ChangeKind::Modified, 1, 4),
    ];
    let m = Stub::new(|_, _| panic!("unused"));
    let rep = run(&mut w, &m, &GapsConfig::default());
    assert_eq!(rep, GapsReport::default());
    assert!(w.fx.calls().iter().all(|c| !c.starts_with("mutants")) && w.pack.notes.is_empty());
}

#[test]
fn the_model_sees_the_tests_the_index_names_and_only_real_test_functions() {
    let mut w = world(&[]);
    let m = Stub::new(|_, _| Some(0.9));
    run(&mut w, &m, &GapsConfig::default());
    let input = &m.items()[0].input;
    assert!(input.contains("// tests/it.rs:3\n```rust\n#[test]\nfn t_a() {\n    assert_eq!(a(1), 2);\n}\n```"), "{input}");
    assert!(!input.contains("not_a_test") && !input.contains("pubfn"), "an index edge that is not a #[test] fn is not shown");
    assert!(input.contains("Item: lib::a (lines 1-7)") && input.contains("-     if x < 3 {\n+     if x R 3 {"), "{input}");
}

#[test]
fn a_symbol_with_no_reaching_test_tells_the_model_so() {
    let mut w = world(&[]);
    w.pack.symbols[0].entry_points.clear();
    let m = Stub::new(|_, _| Some(0.9));
    run(&mut w, &m, &GapsConfig::default());
    assert!(m.items()[0].input.ends_with("No test function reaches this function through the index.\n"));
}

#[test]
fn when_coverage_is_unavailable_every_mutant_is_scored_by_the_model() {
    let mut w = world(&[]);
    w.fx.flag("fail", true);
    w.fx.src.cache_dir = w.fx.src.cache_dir.join("other");
    w.obs = observe(&w.pack, &w.fx.src, &CoverageTool { program: w.fx.cargo.clone(), ..Default::default() });
    let m = Stub::new(|_, _| Some(0.9));
    let rep = run(&mut w, &m, &GapsConfig::default());
    assert_eq!((rep.uncovered, rep.scored), (0, 4), "unknown is not uncovered");
}

#[test]
fn symbols_without_mutants_get_no_gaps_and_the_summary_note_counts_what_happened() {
    let mut w = world(&[]);
    w.fx.set_mutants("[]");
    let rep = run(&mut w, &Stub::new(|_, _| Some(0.1)), &GapsConfig::default());
    assert_eq!((rep.listed, rep.scored, rep.uncovered), (0, 0, 0));
    assert!(w.pack.symbols.iter().all(|s| s.gaps.is_empty()));
    assert!(w.pack.notes.last().unwrap().starts_with("gaps: 0 mutants listed by `cargo mutants --list` (none executed)"));
    let mut w = world(&[]);
    run(&mut w, &Stub::new(|_, _| Some(0.1)), &GapsConfig::default());
    let note = w.pack.notes.last().unwrap();
    assert!(note.contains("5 mutants listed") && note.contains("1 outside") && note.contains("2 on lines no test runs") && note.contains("2 scored by the model (2000 input tokens, $0.0002)"), "{note}");
}

#[test]
fn gaps_are_ordered_predicted_first_by_lowest_p_then_uncovered_by_line() {
    let mut w = world(&[]);
    // put the never() mutants into `a`'s span too by widening it
    w.pack.symbols[0].span = Span { start: 1, end: 10 };
    w.pack.symbols.truncate(1);
    run(&mut w, &Stub::new(|i, _| Some(if i == 0 { 0.4 } else { 0.1 })), &GapsConfig::default());
    let g = &w.pack.symbols[0].gaps;
    let order: Vec<(GapReason, f64)> = g.iter().map(|g| (g.reason, g.p_tests_catch)).collect();
    assert_eq!(order, vec![(GapReason::PredictedSurvivor, 0.1), (GapReason::PredictedSurvivor, 0.4), (GapReason::Uncovered, 0.0), (GapReason::Uncovered, 0.0)]);
}

#[test]
fn unsafe_paths_are_never_read() {
    assert!(safe_rel("src/a.rs") && !safe_rel("../a.rs") && !safe_rel("/etc/passwd") && !safe_rel("a/../../b"));
    let d = tempfile::tempdir().unwrap();
    let mut f = Files { tree: d.path(), cache: HashMap::new() };
    assert!(f.get("../escape").is_none());
}

#[test]
fn changed_constants_and_statics_get_mutants_too_but_types_do_not() {
    let mut w = world(&[]);
    w.pack.symbols[1].id = "x NEVER.".into(); // a const/static: SCIP term symbols end in `.`
    let rep = run(&mut w, &Stub::new(|_, _| Some(0.9)), &GapsConfig::default());
    assert_eq!(rep.uncovered, 2, "the mutants inside the constant's span are listed and joined to it");
    assert_eq!(w.pack.symbols[1].gaps.len(), 2);
    let mut w = world(&[]);
    w.pack.symbols[1].id = "x Never#".into();
    let rep = run(&mut w, &Stub::new(|_, _| Some(0.9)), &GapsConfig::default());
    assert_eq!(rep.uncovered, 0, "a type is not a mutation target; its mutants fall outside the changed items");
}
