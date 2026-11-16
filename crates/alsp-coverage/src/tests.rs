#![allow(non_snake_case)]
use crate::testkit::{libtest_failures, lcov, pack, sym, Fx};
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

fn runs_of(fx: &Fx) -> Vec<String> {
    fx.calls().into_iter().filter(|c| c.starts_with("llvm-cov --lcov")).collect()
}

fn tool(fx: &Fx) -> CoverageTool {
    CoverageTool { program: fx.cargo.clone(), ..Default::default() }
}

#[test]
fn a_crashed_coverage_run_is_unavailable_with_the_reason_not_zero_and_is_not_cached() {
    let fx = fx();
    fx.flag("fail", true);
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &tool(&fx));
    obs.attach(&mut p, &fx.src);
    assert!(p.symbols.iter().all(|s| s.coverage.is_none()));
    let note = p.notes.iter().find(|n| n.starts_with("coverage unavailable for package fx")).expect("a note");
    assert!(note.contains("exited 101") && note.contains("test t::bad ... FAILED"), "{note}");
    assert_eq!(obs.lines_status("src/lib.rs", 1, 7), Hit::Unknown);
    assert!(obs.runs()[0].error.is_some() && !obs.runs()[0].degraded);
    fx.flag("fail", false);
    assert!(observe(&p, &fx.src, &tool(&fx)).runs()[0].error.is_none(), "the failure was not remembered");
}

#[test]
fn test_DW_1_1_failing_tests_keep_coverage_and_name_the_failures() {
    let fx = fx();
    fx.set_test_output(&libtest_failures(&["a::b", "c::d"], "assertion failed: env var X is set"));
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &tool(&fx));
    obs.attach(&mut p, &fx.src);
    let a = p.symbols[0].coverage.as_ref().expect("coverage is kept");
    assert_eq!((a.lines_total, a.lines_hit, a.branches_total), (4, 3, Some(2)));
    assert_eq!(obs.lines_status("src/lib.rs", 1, 7), Hit::Yes);
    assert!(obs.runs()[0].error.is_none() && obs.runs()[0].degraded && !obs.runs()[0].cached);
    let note = p.notes.iter().find(|n| n.contains("package fx")).expect("a note");
    assert!(note.contains("2 tests failed: a::b, c::d"), "{note}");
    assert!(note.contains("first panic: `assertion failed: env var X is set (at src/lib.rs:3:5)`"), "{note}");
    assert!(note.contains("under-counted") && note.contains("environment variable") && note.contains("rerun"), "{note}");
    assert!(!note.contains("unavailable"), "{note}");
    // 0.9.1: --ignore-run-fail alone keeps the other test binaries running; --no-fail-fast is not passed
    let run = &runs_of(&fx)[0];
    assert!(run.contains("--ignore-run-fail") && !run.contains("--no-fail-fast") && run.contains("--branch"), "{run}");
}

#[test]
fn a_single_failure_and_an_old_style_panic_line_are_read() {
    let fx = fx();
    fx.set_test_output("test only::one ... FAILED\n\nfailures:\n\n---- only::one stdout ----\nthread 'only::one' panicked at 'boom', src/x.rs:1:1\n\nfailures:\n    only::one\n");
    let obs = observe(&pack(symbols()), &fx.src, &tool(&fx));
    let n = &obs.notes()[0];
    assert!(n.contains("1 test failed: only::one") && n.contains("first panic: `'boom', src/x.rs:1:1`"), "{n}");
}

#[test]
fn test_DW_1_2_note_lists_five_names_then_and_n_more() {
    let fx = fx();
    let names: Vec<String> = (1..=7).map(|i| format!("t::n{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    fx.set_test_output(&libtest_failures(&refs, "x"));
    let obs = observe(&pack(symbols()), &fx.src, &tool(&fx));
    let n = &obs.notes()[0];
    assert!(n.contains("7 tests failed: t::n1, t::n2, t::n3, t::n4, t::n5 and 2 more"), "{n}");
    assert!(!n.contains("t::n6") && !n.contains("t::n7"), "{n}");
}

#[test]
fn test_DW_1_2_failure_evidence_without_names_says_names_not_parsed() {
    let fx = fx();
    fx.set_test_output("running 1 test\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n");
    let obs = observe(&pack(symbols()), &fx.src, &tool(&fx));
    assert!(obs.runs()[0].degraded);
    assert!(obs.notes()[0].contains("some tests failed (names not parsed)") && !obs.notes()[0].contains("first panic"), "{:?}", obs.notes());
}

#[test]
fn test_DW_1_2_all_passing_has_no_note_and_is_cached() {
    let fx = fx();
    fx.set_test_output("running 2 tests\ntest a ... ok\ntest b ... ignored\n\ntest result: ok. 1 passed; 0 failed; 1 ignored\n");
    let obs = observe(&pack(symbols()), &fx.src, &tool(&fx));
    assert!(obs.notes().is_empty() && !obs.runs()[0].degraded, "{:?}", obs.notes());
    assert!(observe(&pack(symbols()), &fx.src, &tool(&fx)).runs()[0].cached);
}

#[test]
fn a_very_long_panic_message_is_cut_and_stays_one_line() {
    let fx = fx();
    fx.set_test_output(&libtest_failures(&["t"], &"é".repeat(500)));
    let n = &observe(&pack(symbols()), &fx.src, &tool(&fx)).notes()[0].clone();
    assert!(n.matches('é').count() <= MAX_PANIC && !n.contains('\n'), "{n}");
}

#[test]
fn test_DW_1_3_compile_failure_after_noise_is_unavailable_with_the_error_text() {
    let fx = fx();
    fx.flag("compile_fail", true);
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &tool(&fx));
    obs.attach(&mut p, &fx.src);
    assert!(p.symbols.iter().all(|s| s.coverage.is_none()) && obs.runs()[0].error.is_some() && !obs.runs()[0].degraded);
    let note = p.notes.iter().find(|n| n.starts_with("coverage unavailable for package fx")).expect("a note");
    assert!(note.contains("exited 101") && note.contains("error[E0425]: cannot find value `zzz`") && note.contains("could not compile"), "{note}");
    assert!(!note.contains("noise-crate-0 "), "the tail, not the head: {note}");
    assert_eq!(obs.lines_status("src/lib.rs", 1, 7), Hit::Unknown);
}

#[test]
fn test_DW_1_4_old_llvm_cov_without_ignore_run_fail_is_retried_without_it() {
    let fx = fx();
    fx.flag("no_ignore_run_fail", true);
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &tool(&fx));
    obs.attach(&mut p, &fx.src);
    assert!(obs.runs()[0].error.is_none() && p.symbols[0].coverage.is_some() && p.notes.is_empty(), "{:?}", p.notes);
    let runs = runs_of(&fx);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(runs[0].contains("--ignore-run-fail") && runs[0].contains("--branch"), "{runs:?}");
    assert!(!runs[1].contains("--ignore-run-fail") && runs[1].contains("--branch"), "{runs:?}");
}

#[test]
fn test_DW_1_4_ignore_run_fail_and_branch_fallbacks_compose() {
    let fx = fx();
    fx.flag("no_ignore_run_fail", true);
    fx.flag("no_branch", true);
    let mut p = pack(symbols());
    let obs = observe(&p, &fx.src, &tool(&fx));
    obs.attach(&mut p, &fx.src);
    assert!(obs.runs()[0].error.is_none());
    assert!(p.notes.iter().any(|n| n.contains("branch coverage is not supported")), "{:?}", p.notes);
    let runs = runs_of(&fx);
    let has = |i: usize, f: &str| runs[i].contains(f);
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert!(has(0, "--ignore-run-fail") && has(0, "--branch"));
    assert!(!has(1, "--ignore-run-fail") && has(1, "--branch"));
    assert!(!has(2, "--ignore-run-fail") && !has(2, "--branch"));
}

#[test]
fn with_an_old_llvm_cov_failing_tests_are_unavailable_and_still_named() {
    let fx = fx();
    fx.flag("no_ignore_run_fail", true);
    fx.flag("fail", true);
    fx.set_test_output(&libtest_failures(&["a::b"], "boom"));
    let obs = observe(&pack(symbols()), &fx.src, &tool(&fx));
    assert!(obs.runs()[0].error.is_some(), "no report is produced by an old cargo-llvm-cov");
    assert!(obs.notes()[0].contains("coverage unavailable for package fx") && obs.notes()[0].contains("1 test failed: a::b"), "{:?}", obs.notes());
}

#[test]
fn test_DW_1_5_failing_then_passing_run_measures_afresh_and_has_no_note() {
    let fx = fx();
    let p = pack(symbols());
    fx.set_test_output(&libtest_failures(&["a::b"], "boom"));
    let first = observe(&p, &fx.src, &tool(&fx));
    assert!(first.runs()[0].degraded && first.notes().len() == 1);
    let again = observe(&p, &fx.src, &tool(&fx));
    assert!(again.runs()[0].degraded && !again.runs()[0].cached && again.notes().len() == 1, "a repeated failing run shows the note again");
    assert_eq!(runs_of(&fx).len(), 2, "a failing report is not served from the cache");
    std::fs::remove_file(fx.fx.join("stdout.txt")).unwrap();
    let clean = observe(&p, &fx.src, &tool(&fx));
    assert!(!clean.runs()[0].cached && !clean.runs()[0].degraded && clean.notes().is_empty(), "{:?}", clean.notes());
    assert_eq!(runs_of(&fx).len(), 3, "measured afresh");
    assert!(observe(&p, &fx.src, &tool(&fx)).runs()[0].cached, "the clean report is cached");
}

#[test]
fn failed_parse_is_none_without_failure_lines_and_dedupes_names() {
    assert_eq!(Failed::parse("test a ... ok\ntest result: ok. 1 passed\n", ""), None);
    let f = Failed::parse("test a::b ... FAILED\ntest a::b ... FAILED\ntest c ... ok\n", "").unwrap();
    assert_eq!((f.names, f.first_panic), (vec!["a::b".to_string()], None));
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

#[test]
fn test_DW_1_2_a_crashed_test_binary_is_degraded_noted_and_never_cached() {
    let fx = fx();
    fx.flag("crash", true);
    fx.set_test_output("running 1 test\n");
    let p = pack(symbols());
    let first = observe(&p, &fx.src, &tool(&fx));
    assert!(first.runs()[0].error.is_none() && first.runs()[0].degraded, "{:?}", first.runs());
    assert!(first.notes()[0].contains("some tests failed (names not parsed)"), "{:?}", first.notes());
    assert_eq!(first.lines_status("src/lib.rs", 1, 7), Hit::Yes, "coverage is kept");
    assert!(observe(&p, &fx.src, &tool(&fx)).runs()[0].degraded, "the crash is shown again, not served from the cache");
    assert_eq!(runs_of(&fx).len(), 2);
    fx.flag("crash", false);
    fx.set_test_output("running 1 test\ntest a ... ok\n");
    let clean = observe(&p, &fx.src, &tool(&fx));
    assert!(!clean.runs()[0].cached && !clean.runs()[0].degraded && clean.notes().is_empty(), "{:?}", clean.notes());
    assert_eq!(runs_of(&fx).len(), 3, "measured afresh");
}

#[test]
fn a_crash_marker_in_stderr_adds_to_names_parsed_from_stdout() {
    let f = Failed::parse("test a::b ... FAILED\n", "error: 2 targets failed:\n").unwrap();
    assert_eq!(f.names, vec!["a::b".to_string()]);
    assert!(Failed::parse("test a ... ok\n", "   Compiling x\nerror: test failed, to rerun pass `-p x`").is_some());
    assert!(Failed::parse("test a ... ok\n", "warning: unused\n").is_none());
}
