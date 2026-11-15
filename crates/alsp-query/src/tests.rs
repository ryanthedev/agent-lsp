//! Queries over an in-memory index (alsp-evidence's testkit) and a fixture tree whose `cargo` is
//! a fake (alsp-coverage's testkit): no rust-analyzer, no cargo-llvm-cov, no model.
#![allow(non_snake_case)]
use super::*;
use alsp_coverage::testkit::{lcov, Fx};
use alsp_coverage::CoverageTool;
use alsp_decide::{DecideError, DecisionModel, Item, ItemScore, Question};
use alsp_evidence::testkit::{index_for, index_for_spanless};
use alsp_evidence::Provenance;
use alsp_gaps::{GapsConfig, MutantsTool};
use std::sync::Mutex;
use std::time::Duration;

/// api 1-3, helper 4-6, (comment on 7), leaf 8-10, S 11.
const LIB: &str = "pub fn api() {\n    helper();\n}\nfn helper() {\n    leaf();\n}\n// between items\nfn leaf() -> u32 {\n    1\n}\nstruct S;\n";
const IT: &str = "#[test]\nfn t_api() {\n    api();\n}\n#[test]\nfn t_leaf() {\n    leaf();\n}\n";

fn world(extra: &[(&str, &str)]) -> (Fx, Workspace) {
    let mut files = vec![("src/lib.rs", LIB), ("tests/it.rs", IT)];
    files.extend_from_slice(extra);
    let fx = Fx::new(&files);
    let t = Duration::from_secs(60);
    let ws = Workspace::new(fx.tree.clone(), fx.src.cache_dir.clone(), index_for(&files))
        .with_build_env(fx.src.build_env.clone())
        .with_tools(CoverageTool { program: fx.cargo.clone(), timeout: t, ..Default::default() }, MutantsTool { program: fx.cargo.clone(), timeout: t });
    (fx, ws)
}

fn names<T>(s: &Section<T>, f: impl Fn(&T) -> String) -> Vec<String> {
    s.items.iter().map(f).collect()
}

struct Stub(Option<f64>);
impl DecisionModel for Stub {
    fn score(&self, _: &Question, items: &[Item]) -> Result<Vec<ItemScore>, DecideError> {
        match self.0 {
            Some(p) => Ok(items.iter().map(|i| ItemScore { id: i.id.clone(), p: Some(p), error: None, input_tokens: 100 }).collect()),
            None => Err(DecideError::Failed("key rejected".into())),
        }
    }
}

#[test]
fn sym_finds_exact_names_unique_suffixes_and_lists_several_matches() {
    let (_fx, ws) = world(&[]);
    let one = ws.sym("lib::leaf").unwrap();
    let m = one.matches.as_ref().unwrap();
    assert_eq!((m.total, m.provenance), (1, Provenance::Resolved));
    let leaf = &m.items[0];
    assert_eq!((leaf.name.as_str(), leaf.kind.as_str(), leaf.file.as_str(), leaf.line, leaf.references), ("lib::leaf", "fn", "src/lib.rs", 8, 2));
    assert!(one.target.is_none(), "sym answers with a list, not a target");
    let (_fx, ws) = world(&[("src/other.rs", "fn leaf() {}\n")]);
    // a bare name matches by suffix: both `leaf`s are listed, none guessed
    let both = ws.sym("leaf").unwrap();
    assert_eq!(names(both.matches.as_ref().unwrap(), |m| m.name.clone()), ["lib::leaf", "other::leaf"]);
    assert!(ws.sym("ib::leaf").is_err(), "a suffix must start at a `::` boundary");
}

#[test]
fn sym_by_file_line_lists_the_definition_around_the_line_or_the_files_definitions() {
    let (_fx, ws) = world(&[]);
    let inside = ws.sym("src/lib.rs:5").unwrap();
    assert_eq!(names(inside.matches.as_ref().unwrap(), |m| m.name.clone()), ["lib::helper"]);
    let outside = ws.sym("src/lib.rs:7").unwrap();
    assert_eq!(names(outside.matches.as_ref().unwrap(), |m| m.name.clone()), ["lib::api", "lib::helper", "lib::leaf", "lib::S"]);
}

#[test]
fn sym_in_a_file_with_no_items_says_so() {
    let (_fx, ws) = world(&[("src/notes.rs", "// nothing\n")]);
    let a = ws.sym("src/notes.rs:1").unwrap();
    assert_eq!(a.matches.as_ref().unwrap().total, 0);
    assert!(a.notes.iter().any(|n| n.contains("holds no symbols for src/notes.rs")), "{:?}", a.notes);
}

#[test]
fn test_DW_4_3_no_match_gives_the_nearest_names() {
    let (_fx, ws) = world(&[]);
    match ws.sym("lef").unwrap_err() {
        TargetError::NoMatch { query, nearest } => {
            assert_eq!(query, "lef");
            assert!(nearest.contains(&"lib::leaf".to_string()), "{nearest:?}");
        }
        e => panic!("{e:?}"),
    }
    assert!(matches!(ws.refs("").unwrap_err(), TargetError::NoMatch { .. }));
    // no near name at all: still an error, with an empty list
    assert!(matches!(ws.refs("zzzzzzzzzzzz").unwrap_err(), TargetError::NoMatch { nearest, .. } if nearest.is_empty()));
}

#[test]
fn test_DW_4_3_an_ambiguous_name_lists_candidates_for_every_command_but_sym() {
    let (_fx, ws) = world(&[("src/other.rs", "fn leaf() {}\n")]);
    for e in [ws.refs("leaf").unwrap_err(), ws.tests("leaf", false).unwrap_err(), ws.cover("leaf").unwrap_err()] {
        match e {
            TargetError::Ambiguous { candidates, omitted, .. } => {
                assert_eq!((candidates.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), omitted), (vec!["lib::leaf", "other::leaf"], 0));
                assert_eq!((candidates[0].kind.as_str(), candidates[0].file.as_str(), candidates[0].line), ("fn", "src/lib.rs", 8));
            }
            e => panic!("{e:?}"),
        }
    }
    assert!(matches!(ws.gaps("leaf", &Stub(Some(0.1)), &GapsConfig::default()).unwrap_err(), TargetError::Ambiguous { .. }));
    let capped = ws.with_limit(1);
    assert!(matches!(capped.resolve("leaf").unwrap_err(), TargetError::Ambiguous { candidates, omitted: 1, .. } if candidates.len() == 1));
}

#[test]
fn file_arguments_may_be_absolute_dotted_or_a_unique_suffix_and_must_exist() {
    let (fx, ws) = world(&[("src/a/mod.rs", "fn x() {}\n"), ("src/b/mod.rs", "fn y() {}\n")]);
    let abs = format!("{}:5", fx.tree.join("src/lib.rs").display());
    for q in [abs.as_str(), "./src/lib.rs:5", "lib.rs:5"] {
        assert_eq!(ws.resolve(q).unwrap().name, "lib::helper", "{q}");
    }
    assert!(matches!(ws.resolve("mod.rs:1").unwrap_err(), TargetError::Ambiguous { candidates, .. } if candidates.len() == 2 && candidates[0].kind == "file"));
    assert!(matches!(ws.resolve("nope.rs:1").unwrap_err(), TargetError::NoMatch { .. }));
    assert!(matches!(ws.resolve("../etc/passwd:1").unwrap_err(), TargetError::NoMatch { .. }));
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.rs"), "fn x(){}").unwrap();
    assert!(ws.resolve(&format!("{}:1", outside.path().join("x.rs").display())).is_err(), "a file outside the repository is not a target");
}

#[test]
fn refs_group_references_by_enclosing_symbol_mark_tests_and_name_entry_points() {
    let (_fx, ws) = world(&[]);
    let a = ws.refs("leaf").unwrap();
    let t = a.target.as_ref().unwrap();
    assert_eq!((t.kind.as_str(), t.name.as_str(), t.file.as_str(), t.span.start, t.span.end), ("fn", "lib::leaf", "src/lib.rs", 8, 10));
    assert_eq!(a.reference_count, Some(2));
    let groups = a.refs.as_ref().unwrap();
    assert_eq!(groups.provenance, Provenance::Resolved);
    let g: Vec<(&str, &str, bool, Vec<u32>)> = groups.items.iter().map(|g| (g.symbol.as_str(), g.file.as_str(), g.test, g.lines.clone())).collect();
    assert_eq!(g, vec![("lib::helper", "src/lib.rs", false, vec![5]), ("t_leaf", "tests/it.rs", true, vec![7])]);
    let eps = a.entry_points.as_ref().unwrap();
    let e: Vec<(&str, &str, Provenance)> = eps.items.iter().map(|e| (e.kind.as_str(), e.name.as_str(), e.provenance)).collect();
    assert_eq!(e, vec![("test", "t_leaf", Provenance::Resolved), ("pub", "lib::api", Provenance::Heuristic), ("test", "t_api", Provenance::Resolved)]);
}

#[test]
fn a_reference_with_no_enclosing_symbol_is_grouped_under_a_file_level_row() {
    // Generated or macro-expanded code can carry no spans: the use of `leaf` in `gen.rs` has no
    // definition around it, so it is reported under a file-level row rather than dropped.
    let files = [("src/lib.rs", LIB), ("tests/it.rs", IT), ("src/gen.rs", "use crate::leaf;\n")];
    let fx = Fx::new(&files);
    let ws = Workspace::new(fx.tree.clone(), fx.src.cache_dir.clone(), index_for_spanless(&files, &["src/gen.rs"]));
    let a = ws.refs("leaf").unwrap();
    let g = a.refs.as_ref().unwrap().items.iter().find(|g| g.symbol == "<outside any symbol>").expect("a file-level group");
    assert_eq!((g.kind.as_str(), g.file.as_str(), g.test, g.lines.clone()), ("file", "src/gen.rs", false, vec![1]));
    assert_eq!(a.reference_count, Some(3), "the file-level use counts like any other");
    assert!(a.to_text().contains("<outside any symbol>"), "{}", a.to_text());
}

#[test]
fn refs_by_file_line_outside_any_symbol_answer_for_the_whole_file_from_outside_it() {
    let (_fx, ws) = world(&[]);
    let a = ws.refs("src/lib.rs:7").unwrap();
    let t = a.target.as_ref().unwrap();
    assert_eq!((t.kind.as_str(), t.name.as_str(), t.span.start, t.span.end), ("file", "src/lib.rs", 1, u32::MAX));
    // helper -> leaf stays inside the file and is left out; the tests' uses are the references
    assert_eq!(a.reference_count, Some(2));
    assert_eq!(names(a.refs.as_ref().unwrap(), |g| g.symbol.clone()), ["t_api", "t_leaf"]);
    assert!(a.entry_points.as_ref().unwrap().items.iter().any(|e| e.name == "t_leaf"));
    let t = ws.tests("src/lib.rs:7", false).unwrap();
    assert_eq!(names(t.tests.as_ref().unwrap(), |t| t.name.clone()), ["t_api", "t_leaf"]);
}

#[test]
fn a_module_target_is_the_span_of_the_module_and_what_is_in_it() {
    let (_fx, ws) = world(&[]);
    let t = ws.resolve("lib").unwrap();
    assert_eq!((t.kind.as_str(), t.region, t.defs.len()), ("module", true, 4));
    assert_eq!(ws.refs("lib").unwrap().reference_count, Some(2));
}

#[test]
fn test_DW_4_3_the_result_cap_counts_what_it_leaves_out_and_all_lifts_it() {
    let (_fx, ws) = world(&[]);
    let capped = ws.with_limit(1);
    let a = capped.refs("leaf").unwrap();
    let groups = a.refs.as_ref().unwrap();
    assert_eq!((groups.total, groups.omitted, groups.items.len()), (2, 1, 1));
    assert_eq!(a.reference_count, Some(2), "the site count is of everything, not of what is shown");
    assert_eq!(a.entry_points.as_ref().unwrap().omitted, 2);
    let text = a.to_text();
    assert!(text.contains("refs(2 in 2 symbols) [resolved]") && text.contains("  +1 more (--all lists everything)"), "{text}");
    assert_eq!(capped.sym("src/lib.rs:7").unwrap().matches.as_ref().unwrap().omitted, 3);
    let all = capped.with_limit(usize::MAX);
    assert_eq!(all.refs("leaf").unwrap().refs.as_ref().unwrap().omitted, 0);
}

#[test]
fn tests_tells_direct_from_indirect_tests_in_order_of_distance() {
    let (_fx, ws) = world(&[]);
    let a = ws.tests("leaf", false).unwrap();
    let t = a.tests.as_ref().unwrap();
    assert_eq!(t.provenance, Provenance::Resolved);
    assert_eq!(names(t, |t| format!("{} {}:{} direct={}", t.name, t.file, t.line, t.direct)), ["t_leaf tests/it.rs:6 direct=true", "t_api tests/it.rs:2 direct=false"]);
    assert!(a.executed.is_none() && a.unavailable.is_none());
    assert!(!ws.tests("S", false).unwrap().tests.unwrap().items.iter().any(|_| true), "a type nothing uses has no tests");
}

fn leaf_lcov(fx: &Fx, line8: u64, line9: u64) {
    fx.set_lcov(&lcov(&[("src/lib.rs", &[(2, 1), (5, 1), (8, line8), (9, line9), (10, 0)], &[(8, &[true, false])])]));
}

#[test]
fn tests_run_reports_whether_the_packages_tests_executed_the_target() {
    let (fx, ws) = world(&[]);
    leaf_lcov(&fx, 4, 4);
    let a = ws.tests("leaf", true).unwrap();
    let e = a.executed.as_ref().unwrap();
    assert_eq!((e.status.as_str(), e.lines_hit, e.lines_total, e.provenance), ("yes", 2, 3, Provenance::Observed));
    assert!(a.notes.iter().any(|n| n.contains("not which test did")), "{:?}", a.notes);
    assert!(a.to_text().contains("executed: yes (2/3 lines ran) [observed]"));
    let (fx, ws) = world(&[]);
    leaf_lcov(&fx, 0, 0);
    let a = ws.tests("leaf", true).unwrap();
    assert_eq!(a.executed.as_ref().unwrap().status, "no");
    assert!(a.notes.iter().any(|n| n.contains("did not run its lines")), "{:?}", a.notes);
}

#[test]
fn tests_run_says_when_the_code_runs_but_no_resolved_test_reaches_it() {
    let (fx, ws) = world(&[]);
    fx.set_lcov(&lcov(&[("src/lib.rs", &[(11, 3)], &[])]));
    let a = ws.tests("lib::S", true).unwrap();
    assert!(a.tests.as_ref().unwrap().items.is_empty());
    assert_eq!(a.executed.as_ref().unwrap().status, "yes");
    assert!(a.notes.iter().any(|n| n.contains("none reaches it through resolved references")), "{:?}", a.notes);
}

#[test]
fn tests_run_with_no_instrumented_line_in_the_span_is_unknown_not_no() {
    let (fx, ws) = world(&[]);
    fx.set_lcov(&lcov(&[("src/lib.rs", &[(2, 1)], &[])]));
    assert_eq!(ws.tests("leaf", true).unwrap().executed.unwrap().status, "unknown");
}

#[test]
fn cover_reports_lines_branches_and_the_uncovered_lines_of_the_span() {
    let (fx, ws) = world(&[]);
    leaf_lcov(&fx, 4, 0);
    let a = ws.cover("leaf").unwrap();
    let c = a.coverage.as_ref().unwrap();
    assert_eq!((c.lines_total, c.lines_hit, c.branches_total, c.branches_hit, c.uncovered_lines.clone(), c.provenance), (3, 1, Some(2), Some(1), vec![9, 10], Provenance::Observed));
    assert!(a.unavailable.is_none());
    let t = a.to_text();
    assert!(t.contains("lines 1/3 covered, branches 1/2 [observed]\nuncovered: 9-10 [observed]\n"), "{t}");
    // a file region measures the whole file
    let whole = ws.cover("src/lib.rs:7").unwrap().coverage.unwrap();
    assert_eq!((whole.lines_total, whole.lines_hit), (5, 3));
    // the run is cached: a second question does not run the tests again
    let before = fx.calls().iter().filter(|c| c.contains("llvm-cov") && c.contains("--lcov")).count();
    ws.cover("leaf").unwrap();
    assert_eq!(fx.calls().iter().filter(|c| c.contains("llvm-cov") && c.contains("--lcov")).count(), before);
}

#[test]
fn cover_with_failing_tests_is_unavailable_never_zero() {
    let (fx, ws) = world(&[]);
    fx.flag("fail", true);
    let a = ws.cover("leaf").unwrap();
    assert!(a.coverage.is_none());
    assert!(a.unavailable.as_ref().unwrap().contains("could not be measured"));
    assert!(a.notes.iter().any(|n| n.contains("coverage unavailable for package fx") && n.contains("exited 101")), "{:?}", a.notes);
    assert!(a.to_text().contains("! unavailable: coverage could not be measured"));
}

#[test]
fn cover_of_test_files_and_types_says_why_there_is_nothing() {
    let (fx, ws) = world(&[]);
    let a = ws.cover("t_leaf").unwrap();
    assert!(a.unavailable.is_some() && a.coverage.is_none());
    assert!(a.notes.iter().any(|n| n.contains("under tests/")), "{:?}", a.notes);
    assert!(fx.calls().iter().all(|c| !c.contains("--lcov")), "nothing is run for a test file");
    leaf_lcov(&fx, 1, 1);
    let a = ws.cover("lib::S").unwrap();
    assert!(a.coverage.is_none() && a.unavailable.is_none());
    assert!(a.notes.iter().any(|n| n.contains("no instrumented lines")), "{:?}", a.notes);
}

fn mutant(file: &str, line: u32, col: u32, genre: &str, d: &str) -> String {
    format!(r#"{{"file":"{file}","package":"fx","genre":"{genre}","name":"{file}:{line}:{col}: {d}","replacement":"R","span":{{"start":{{"line":{line},"column":{col}}},"end":{{"line":{line},"column":{}}}}},"diff":"D"}}"#, col + 1)
}

fn set_mutants(fx: &Fx) {
    let ms = [mutant("src/lib.rs", 8, 4, "BinaryOperator", "replace a with b in leaf"), mutant("src/lib.rs", 9, 5, "FnValue", "replace 1 with 0 in leaf"), mutant("src/lib.rs", 5, 5, "FnValue", "replace in helper")];
    fx.set_mutants(&format!("[{}]", ms.join(",")));
}

#[test]
fn gaps_lists_predicted_survivors_then_unrun_lines_each_with_its_provenance() {
    let (fx, ws) = world(&[]);
    set_mutants(&fx);
    leaf_lcov(&fx, 4, 0);
    let a = ws.gaps("leaf", &Stub(Some(0.2)), &GapsConfig::default()).unwrap();
    let g = a.gaps.as_ref().unwrap();
    let rows: Vec<(u32, f64, Provenance)> = g.items.iter().map(|g| (g.line, g.p_tests_catch, g.provenance)).collect();
    assert_eq!(rows, vec![(8, 0.2, Provenance::Predicted), (9, 0.0, Provenance::Observed)]);
    assert!(a.unavailable.is_none());
    assert!(a.notes.iter().any(|n| n.starts_with("gaps: 3 mutants listed") && n.contains("1 outside the changed functions")), "{:?}", a.notes);
    let t = a.to_text();
    assert!(t.contains("gaps(2) [predicted+observed]\n  src/lib.rs:8 replace a with b in leaf (col 4) p=0.20 [predicted]\n  src/lib.rs:9 replace 1 with 0 in leaf (col 5) uncovered [observed]\n"), "{t}");
    assert!(fx.calls().iter().filter(|c| c.contains("mutants")).all(|c| c.contains("--list")), "mutants are only listed");
    // above the threshold is no gap
    let (fx2, ws2) = world(&[]);
    set_mutants(&fx2);
    leaf_lcov(&fx2, 4, 4);
    assert!(ws2.gaps("leaf", &Stub(Some(0.9)), &GapsConfig::default()).unwrap().gaps.unwrap().items.is_empty());
}

#[test]
fn gaps_with_a_failing_model_keeps_the_observed_gaps_and_says_unavailable() {
    let (fx, ws) = world(&[]);
    set_mutants(&fx);
    leaf_lcov(&fx, 4, 0);
    let a = ws.gaps("leaf", &Stub(None), &GapsConfig::default()).unwrap();
    assert_eq!(a.gaps.as_ref().unwrap().items.len(), 1);
    assert!(a.unavailable.as_ref().unwrap().contains("key rejected"), "{:?}", a.unavailable);
}

#[test]
fn gaps_without_coverage_sends_every_mutant_to_the_model() {
    let (fx, ws) = world(&[]);
    set_mutants(&fx);
    fx.flag("fail", true);
    let a = ws.gaps("leaf", &Stub(Some(0.1)), &GapsConfig::default()).unwrap();
    assert_eq!(a.gaps.as_ref().unwrap().items.len(), 2);
    assert!(a.gaps.as_ref().unwrap().items.iter().all(|g| g.provenance == Provenance::Predicted));
    assert!(a.unavailable.is_none());
    assert!(a.notes.iter().any(|n| n.contains("coverage unavailable")), "{:?}", a.notes);
}

#[test]
fn gaps_when_mutants_cannot_be_listed_is_unavailable() {
    let (fx, ws) = world(&[]);
    fx.flag("mutants.fail", true);
    leaf_lcov(&fx, 4, 4);
    let a = ws.gaps("leaf", &Stub(Some(0.1)), &GapsConfig::default()).unwrap();
    assert!(a.gaps.as_ref().unwrap().items.is_empty());
    assert!(a.unavailable.as_ref().unwrap().contains("cannot list mutants"), "{:?}", a.unavailable);
}

#[test]
fn gaps_of_a_type_runs_nothing_and_says_mutants_apply_to_code() {
    let (fx, ws) = world(&[]);
    let a = ws.gaps("lib::S", &Stub(Some(0.1)), &GapsConfig::default()).unwrap();
    assert!(a.gaps.as_ref().unwrap().items.is_empty() && a.unavailable.is_none());
    assert!(a.notes.iter().any(|n| n.contains("only apply to code")), "{:?}", a.notes);
    assert!(fx.calls().is_empty());
}

#[test]
fn gaps_of_a_file_region_covers_every_function_in_it() {
    let (fx, ws) = world(&[]);
    set_mutants(&fx);
    leaf_lcov(&fx, 4, 0);
    let a = ws.gaps("src/lib.rs:7", &Stub(Some(0.2)), &GapsConfig::default()).unwrap();
    let lines: Vec<u32> = a.gaps.unwrap().items.iter().map(|g| g.line).collect();
    assert_eq!(lines, vec![5, 8, 9], "helper's mutant is now inside the target: {lines:?}");
}

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[test]
fn progress_reports_each_measured_package_and_the_gaps_spend() {
    let (fx, ws) = world(&[]);
    set_mutants(&fx);
    leaf_lcov(&fx, 4, 0);
    let ws = ws.with_progress(|s| LOG.lock().unwrap().push(s.to_string()));
    ws.gaps("leaf", &Stub(Some(0.2)), &GapsConfig::default()).unwrap();
    let log = LOG.lock().unwrap().join("\n");
    assert!(log.contains("coverage fx: no cached report") && log.contains("running its tests"), "the test run is announced before it starts: {log}");
    assert!(log.contains("coverage fx: measured in") && log.contains("gaps: 3 mutants listed") && log.contains("100 input tokens"), "{log}");
}

#[test]
fn open_errors_say_what_to_do_next() {
    let not_git = tempfile::tempdir().unwrap();
    let e = Workspace::open(&OpenOptions { repo: not_git.path().to_path_buf(), no_build: true, cache_root: None, tool: Tool::default(), extra_roots: vec![] }).err().unwrap();
    assert!(matches!(e, OpenError::Repo(_)) && e.to_string().contains("pass --repo <path>"), "{e}");
    let missing = Workspace::open(&OpenOptions { repo: not_git.path().join("nope"), no_build: true, cache_root: None, tool: Tool::default(), extra_roots: vec![] }).err().unwrap();
    assert!(missing.to_string().contains("does not exist"), "{missing}");
    let unit = |ok, d: Option<&str>| UnitReport { path: "crates/x".into(), ok, detail: d.map(String::from) };
    let state = |status, units| IndexState { status, rebuilt: false, units, millis: 0 };
    let ran = |units| IndexState { status: IndexStatus::Missing, rebuilt: true, units, millis: 0 };
    let stale = OpenError::Unavailable(state(IndexStatus::Stale, vec![unit(false, Some("cached; not rebuilt because the indexer is disabled"))])).to_string();
    assert!(stale.contains("index stale") && stale.contains("rerun without --no-build"), "{stale}");
    let broken = OpenError::Unavailable(state(IndexStatus::Missing, vec![unit(false, None)])).to_string();
    assert!(broken.contains("crates/x: unknown"), "{broken}");
    let failed = OpenError::Unavailable(ran(vec![unit(false, Some("exit 1"))])).to_string();
    assert!(failed.contains("rust-analyzer is installed"), "{failed}");
    assert!(OpenError::Index(IndexError::NoCargoRoot("/x".into())).to_string().contains("point --repo (or --root) at a Rust package"));
    assert!(OpenError::Index(IndexError::Io("boom".into())).to_string().contains("boom"));
}

#[test]
fn with_builders_and_a_fresh_workspace_report_a_fresh_index() {
    let (_fx, ws) = world(&[]);
    let f = ws.freshness();
    assert_eq!((f.status.as_str(), f.rebuilt, f.dirty.len(), f.head.len()), ("fresh", false, 0, 40));
}

#[test]
fn tree_sources_read_head_files_only_inside_the_tree() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.rs"), "x").unwrap();
    let s = TreeSources(d.path());
    assert_eq!((s.head("a.rs").as_deref(), s.head("../a.rs"), s.head("/etc/passwd"), s.base("a.rs")), (Some("x"), None, None, None));
}
