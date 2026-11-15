#![allow(non_snake_case)]
use crate::testkit::{lcov, pack, sym, Fx};
use crate::*;
use alsp_evidence::ChangeKind::{Added, Modified, Removed};

const LIB: &str = "pub fn a(x: u32) -> u32 {\n    if x > 1 {\n        x\n    } else {\n        0\n    }\n}\npub fn b() {\n    println!();\n}\npub struct S;\n";

fn fx() -> Fx {
    let fx = Fx::new(&[("src/lib.rs", LIB)]);
    fx.set_lcov(&lcov(&[("src/lib.rs", &[(1, 4), (2, 4), (3, 4), (5, 0), (9, 0)], &[(2, &[true, false])])]));
    fx
}

fn symbols() -> Vec<alsp_evidence::SymbolEvidence> {
    vec![
        sym("src/lib.rs", "lib::a", "x a().", Modified, 1, 7),
        sym("src/lib.rs", "lib::b", "x b().", Added, 8, 10),
        sym("src/lib.rs", "lib::S", "x S#", Modified, 11, 11),
        sym("src/lib.rs", "lib", "x lib/", Modified, 1, 11),
        sym("src/lib.rs", "lib::gone", "gone", Removed, 1, 3),
    ]
}

#[test]
fn test_DW_3_1_symbol_lines_and_branches_come_from_the_report_for_the_same_lines() {
    let mut fx = fx();
    fx.src.changed_lines.insert("src/lib.rs".into(), vec![5, 9, 10]);
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    obs.attach(&mut p, &fx.src);
    let a = p.symbols[0].coverage.as_ref().unwrap();
    assert_eq!((a.lines_total, a.lines_hit, a.branches_total, a.branches_hit), (4, 3, Some(2), Some(1)), "lines 1,2,3 hit; 5 not");
    assert_eq!(a.uncovered_changed_lines, vec![5]);
    let b = p.symbols[1].coverage.as_ref().unwrap();
    assert_eq!((b.lines_total, b.lines_hit, b.branches_total, b.branches_hit), (1, 0, Some(0), Some(0)));
    assert_eq!(b.uncovered_changed_lines, vec![9], "line 10 is changed but not code");
    assert!(p.symbols[2].coverage.is_none(), "a struct has no instrumented lines");
    assert!(p.symbols[3].coverage.is_none() && p.symbols[4].coverage.is_none(), "modules and removed symbols get none");
    assert!(obs.notes().is_empty(), "{:?}", obs.notes());
    assert!(obs.runs()[0].error.is_none() && !obs.runs()[0].cached);
}

#[test]
fn run_uses_llvm_cov_for_the_owning_package_in_its_workspace_with_branches_and_the_build_env() {
    let fx = fx();
    let p = pack(symbols());
    observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    let calls = fx.calls();
    let run = calls.iter().find(|c| c.starts_with("llvm-cov --lcov")).expect("a coverage run");
    assert!(run.contains("--branch") && run.contains("-p fx") && run.contains("--output-path"), "{run}");
    assert_eq!(calls.iter().filter(|c| c.starts_with("metadata")).count(), 1, "one metadata call for one manifest directory");
}

#[test]
fn a_cached_report_is_reused_until_the_workspace_content_changes() {
    let fx = fx();
    let p = pack(symbols());
    let tool = CoverageTool { program: fx.cargo.clone(), ..Default::default() };
    let runs = |fx: &Fx| fx.calls().iter().filter(|c| c.starts_with("llvm-cov --lcov")).count();
    assert!(!observe(&p, &fx.src, &tool).runs()[0].cached);
    let again = observe(&p, &fx.src, &tool);
    assert!(again.runs()[0].cached && runs(&fx) == 1, "second look is served from the cache");
    assert_eq!(again.coverage("src/lib.rs", alsp_index::Span { start: 1, end: 7 }, |_| false).unwrap().branches_total, Some(2), "branch flag survives the cache");
    fx.write_source("src/lib.rs", &format!("{LIB}// edit\n"));
    assert!(!observe(&p, &fx.src, &tool).runs()[0].cached && runs(&fx) == 2, "a source change is a new hash");
}

#[test]
fn test_DW_3_5_a_failing_test_run_is_unavailable_with_the_reason_not_zero_and_is_not_cached() {
    let fx = fx();
    fx.flag("fail", true);
    let mut p = pack(symbols());
    let tool = CoverageTool { program: fx.cargo.clone(), ..Default::default() };
    let obs = observe(&p, &fx.src, &tool);
    obs.attach(&mut p, &fx.src);
    assert!(p.symbols.iter().all(|s| s.coverage.is_none()));
    let note = p.notes.iter().find(|n| n.starts_with("coverage unavailable for package fx")).expect("a note");
    assert!(note.contains("exited 101") && note.contains("test t::bad ... FAILED"), "{note}");
    assert_eq!(obs.lines_status("src/lib.rs", 1, 7), Hit::Unknown);
    assert!(obs.runs()[0].error.is_some());
    fx.flag("fail", false);
    assert!(observe(&p, &fx.src, &tool).runs()[0].error.is_none(), "the failure was not remembered");
}

#[test]
fn test_DW_3_5_a_crate_with_no_tests_is_measured_as_nothing_hit() {
    let fx = Fx::new(&[("src/lib.rs", LIB)]);
    fx.set_lcov(&lcov(&[("src/lib.rs", &[(1, 0), (2, 0), (3, 0), (9, 0)], &[])]));
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    obs.attach(&mut p, &fx.src);
    let a = p.symbols[0].coverage.as_ref().unwrap();
    assert_eq!((a.lines_total, a.lines_hit), (3, 0));
    assert_eq!(obs.lines_status("src/lib.rs", 1, 7), Hit::No);
    assert!(p.notes.is_empty());
}

#[test]
fn branch_coverage_unsupported_by_the_toolchain_falls_back_to_lines_and_says_so() {
    let fx = fx();
    fx.flag("no_branch", true);
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    obs.attach(&mut p, &fx.src);
    let a = p.symbols[0].coverage.as_ref().unwrap();
    assert_eq!((a.lines_total, a.branches_total, a.branches_hit), (4, None, None));
    assert!(p.notes.iter().any(|n| n.contains("branch coverage is not supported") && n.contains("package fx")), "{:?}", p.notes);
    let runs: Vec<_> = fx.calls().into_iter().filter(|c| c.starts_with("llvm-cov --lcov")).collect();
    assert!(runs[0].contains("--branch") && !runs[1].contains("--branch"), "{runs:?}");
}

#[test]
fn a_missing_cargo_llvm_cov_or_cargo_is_unavailable_with_an_install_hint() {
    let fx = fx();
    fx.flag("no_llvm_cov", true);
    let p = pack(symbols());
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    assert!(obs.notes()[0].contains("cargo install cargo-llvm-cov"), "{:?}", obs.notes());
    let obs = observe(&p, &fx.src, &CoverageTool { program: "/no/such/cargo".into(), ..Default::default() });
    assert!(obs.notes()[0].contains("coverage unavailable") && obs.notes()[0].contains("cannot run"), "{:?}", obs.notes());
}

#[test]
fn a_file_no_package_owns_is_noted_and_other_files_are_still_measured() {
    let fx = fx();
    let mut syms = symbols();
    syms.push(sym("loose/x.rs", "x::f", "x f().", Modified, 1, 2));
    std::fs::create_dir_all(fx.tree.join("loose")).unwrap();
    let p = pack(syms);
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    // loose/x.rs sits under the tree's own Cargo.toml, so it belongs to `fx`; a tree without one does not
    assert!(obs.runs().len() == 1 && obs.runs()[0].error.is_none());
    std::fs::remove_file(fx.tree.join("Cargo.toml")).unwrap();
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    assert!(obs.notes().iter().any(|n| n.contains("no Cargo package owns src/lib.rs")), "{:?}", obs.notes());
    assert!(obs.runs().is_empty());
}

#[test]
fn nothing_to_measure_runs_nothing() {
    let fx = fx();
    let p = pack(vec![sym("src/lib.rs", "lib::gone", "gone", Removed, 1, 3), sym("README.md", "m", "m/", Modified, 1, 1)]);
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    assert!(obs.runs().is_empty() && obs.notes().is_empty() && fx.calls().is_empty());
}

#[test]
fn lines_status_distinguishes_hit_miss_and_unknown() {
    let fx = fx();
    let obs = observe(&pack(symbols()), &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    assert_eq!(obs.lines_status("src/lib.rs", 1, 7), Hit::Yes, "any hit line makes the range hit");
    assert_eq!(obs.lines_status("src/lib.rs", 5, 5), Hit::No);
    assert_eq!(obs.lines_status("src/lib.rs", 5, 6), Hit::No);
    assert_eq!(obs.lines_status("src/lib.rs", 6, 8), Hit::Unknown, "no instrumented line there");
    assert_eq!(obs.lines_status("src/other.rs", 1, 9), Hit::Unknown, "file never measured");
}

#[test]
fn paths_outside_the_tree_are_ignored() {
    let fx = fx();
    fx.set_lcov(&format!("SF:/somewhere/else/dep.rs\nDA:1,1\nend_of_record\n{}", lcov(&[("src/lib.rs", &[(1, 1)], &[])])));
    let obs = observe(&pack(symbols()), &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    assert_eq!(obs.lines_status("/somewhere/else/dep.rs", 1, 1), Hit::Unknown);
    assert_eq!(obs.lines_status("src/lib.rs", 1, 1), Hit::Yes);
}

#[test]
fn test_files_are_not_measured_and_trigger_no_run_but_are_noted() {
    assert!(is_test_support_path("crates/x/tests/host.rs") && is_test_support_path("tests/a/b.rs") && is_test_support_path("benches/b.rs") && is_test_support_path("x/examples/e.rs"));
    assert!(!is_test_support_path("src/tests.rs") && !is_test_support_path("src/testsuite/a.rs") && !is_test_support_path("tests.rs"));
    let fx = fx();
    let mut p = pack(vec![sym("tests/it.rs", "it::t", "x t().", Modified, 1, 3)]);
    let obs = observe(&p, &fx.src, &CoverageTool { program: fx.cargo.clone(), ..Default::default() });
    obs.attach(&mut p, &fx.src);
    assert!(fx.calls().is_empty() && obs.runs().is_empty(), "no package is built for test files");
    assert_eq!(p.notes.len(), 1);
    assert!(p.notes[0].contains("1 changed file(s) under tests/") && p.symbols[0].coverage.is_none(), "{:?}", p.notes);
}

// The notices go through a plain `fn(&str)`, so the tests share one buffer; this lock keeps the
// two tests that read it from interleaving.
static NOTICES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static NOTICES_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn notices_tool(fx: &Fx) -> CoverageTool {
    CoverageTool { program: fx.cargo.clone(), notify: |m| NOTICES.lock().unwrap().push(m.to_string()), ..Default::default() }
}

#[test]
fn test_DW_U_2_running_the_tests_is_announced_on_a_miss_and_not_on_a_cache_hit() {
    let _serial = NOTICES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = fx();
    let p = pack(symbols());
    NOTICES.lock().unwrap().clear();
    observe(&p, &fx.src, &notices_tool(&fx));
    {
        let n = NOTICES.lock().unwrap();
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(n[0].starts_with("coverage fx:") && n[0].contains("running its tests") && n[0].contains("minutes") && n[0].contains("cached"), "{}", n[0]);
        assert!(!n[0].contains('\n'), "one line");
    }
    NOTICES.lock().unwrap().clear();
    assert!(observe(&p, &fx.src, &notices_tool(&fx)).runs()[0].cached);
    assert!(NOTICES.lock().unwrap().is_empty(), "a cache hit runs nothing and says nothing");
}

#[test]
fn test_DW_U_2_waiting_on_a_cargo_lock_is_announced_while_cargo_is_still_waiting() {
    let _serial = NOTICES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fx = fx();
    fx.flag("lock_wait", true);
    NOTICES.lock().unwrap().clear();
    let obs = observe(&pack(symbols()), &fx.src, &notices_tool(&fx));
    assert!(obs.runs()[0].error.is_none());
    let n = NOTICES.lock().unwrap();
    assert_eq!(n.len(), 2, "the test run, then the lock: {n:?}");
    assert!(n[0].contains("running its tests"), "{}", n[0]);
    assert!(n[1].starts_with("coverage fx: cargo is waiting for a file lock") && !n[1].contains('\n'), "{}", n[1]);
}

/// The cache key is the workspace's sources and the llvm-cov version: nothing about how cargo is
/// invoked. An agent session differs from a plain shell in exactly such things (its own target
/// directory, rustflags, profile file, colour and terminal variables), and none of them may turn a
/// repeat look at unchanged code into another test run. (Guard, not a reproduction of the live
/// hang: see the discovery file.)
#[test]
fn test_DW_U_1_a_repeat_look_at_unchanged_code_is_a_cache_hit_whatever_the_cargo_environment() {
    let mut fx = fx();
    let p = pack(symbols());
    let tool = CoverageTool { program: fx.cargo.clone(), ..Default::default() };
    let runs = |fx: &Fx| fx.calls().iter().filter(|c| c.starts_with("llvm-cov --lcov")).count();
    assert!(!observe(&p, &fx.src, &tool).runs()[0].cached);
    for (k, v) in [("CARGO_TARGET_DIR", "/agent/target"), ("RUSTFLAGS", "-Awarnings"), ("LLVM_PROFILE_FILE", "/tmp/x-%p.profraw"), ("CARGO_TERM_COLOR", "never"), ("CARGO_INCREMENTAL", "0")] {
        fx.src.build_env.push((k.into(), v.into()));
        assert!(observe(&p, &fx.src, &tool).runs()[0].cached, "{k} must not change the cache key");
    }
    assert_eq!(runs(&fx), 1, "tests ran once");
}
