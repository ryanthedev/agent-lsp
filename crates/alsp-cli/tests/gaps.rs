//! `alsp pack --with coverage,gaps` end to end, offline: a real git repository, a fake
//! rust-analyzer (SCIP from alsp-evidence's testkit), a fake cargo that answers `metadata`,
//! `llvm-cov` and `mutants --list` (alsp-coverage's testkit), and a fake `bun` + sdd tree standing in for sdd's `decide` command.
#![allow(non_snake_case)]

use alsp_coverage::testkit::{install_fake_cargo, lcov, libtest_failures, metadata_json};
use alsp_decide::stub::FakeSdd;
use alsp_evidence::testkit::scip_for;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

const LIB1: &str = "pub fn a(x: u32) -> u32 {\n    if x < 3 {\n        x\n    } else {\n        0\n    }\n}\npub fn never() -> u32 {\n    7\n}\n";
const LIB2: &str = "pub fn a(x: u32) -> u32 {\n    if x < 3 {\n        x + 1\n    } else {\n        0\n    }\n}\npub fn never() -> u32 {\n    8\n}\n";
const TESTS: &str = "#[test]\nfn t_a() {\n    assert_eq!(fx::a(1), 2);\n}\n";

struct Env {
    repo: PathBuf,
    _root: tempfile::TempDir,
    fx: PathBuf,
    cache: PathBuf,
    cargo: PathBuf,
    ra: PathBuf,
    config_home: PathBuf,
}

impl Env {
    fn new(lib2: &str, extra: &[(&str, &str)]) -> Env {
        let root = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(root.path()).unwrap();
        let (repo, fx, cache, config_home) = (base.join("repo"), base.join("fx"), base.join("cache"), base.join("config"));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&config_home).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        write(&repo, "Cargo.toml", "[package]\nname = \"fx\"\nversion = \"0.0.1\"\n");
        write(&repo, "src/lib.rs", LIB1);
        write(&repo, "tests/it.rs", TESTS);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "c1"]);
        write(&repo, "src/lib.rs", lib2);
        for (p, c) in extra {
            write(&repo, p, c);
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "c2"]);
        let cargo = install_fake_cargo(&fx);
        fs::write(fx.join("metadata.json"), metadata_json(&repo)).unwrap();
        fs::write(fx.join("out.lcov"), lcov(&[("src/lib.rs", &[(1, 4), (2, 4), (3, 3), (5, 1), (9, 0)], &[(2, &[true, false])])])).unwrap();
        let mj = |l: u32, c: u32, e: u32, genre: &str, d: &str| {
            format!(r#"{{"file":"src/lib.rs","package":"fx","genre":"{genre}","name":"src/lib.rs:{l}:{c}: {d}","replacement":"R","span":{{"start":{{"line":{l},"column":{c}}},"end":{{"line":{l},"column":{e}}}}},"diff":"D"}}"#)
        };
        let ms = [mj(2, 10, 11, "BinaryOperator", "replace < with <= in a"), mj(3, 11, 12, "BinaryOperator", "replace + with - in a"), mj(9, 5, 6, "FnValue", "replace never -> u32 with 0")];
        fs::write(fx.join("mutants.json"), format!("[{}]", ms.join(","))).unwrap();
        let ra = fx.join("ra.sh");
        fs::write(&ra, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fake-ra 1; exit 0; fi\ncp \"$ALSP_FX/default.scip\" \"$4\"\n").unwrap();
        fs::set_permissions(&ra, fs::Permissions::from_mode(0o755)).unwrap();
        let mut files = vec![("src/lib.rs", lib2), ("tests/it.rs", TESTS)];
        files.extend_from_slice(extra);
        fs::write(fx.join("default.scip"), scip_for(&files)).unwrap();
        Env { repo, _root: root, fx, cache, cargo, ra, config_home }
    }

    /// `sdd`: the fake sdd to decide through; `None` points ALSP_SDD_ROOT at nothing.
    fn pack(&self, args: &[&str], sdd: Option<&FakeSdd>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
        c.arg("pack").arg("--repo").arg(&self.repo).arg("--cache-dir").arg(&self.cache).args(args);
        c.env("ALSP_RUST_ANALYZER", &self.ra).env("ALSP_CARGO", &self.cargo).env("ALSP_FX", &self.fx).env("ALSP_TREE", &self.repo);
        // the real sdd must never be used by a test
        match sdd {
            Some(f) => c.env("ALSP_SDD_ROOT", &f.root).env("ALSP_BUN", &f.bun),
            None => c.env("ALSP_SDD_ROOT", self.config_home.join("no-sdd")).env("ALSP_BUN", self.config_home.join("no-bun")),
        };
        c.output().unwrap()
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn test_DW_3_2_pack_with_coverage_and_gaps_shows_lines_gaps_and_the_spend_and_only_lists_mutants() {
    let e = Env::new(LIB2, &[]);
    let stub = { let f = FakeSdd::new("ok"); f.p(0.2); f.tokens(1500); f };
    let o = e.pack(&["--base", "HEAD~1", "--with", "coverage,gaps"], Some(&stub));
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("M lib::a src/lib.rs:1-7") && t.contains("  lines 4/4 covered, branches 1/2\n"), "{t}");
    assert!(t.contains("  uncovered changed lines: 9\n"), "{t}");
    assert!(t.contains("  gap src/lib.rs:2 replace < with <= in a (col 10) p=0.20\n") && t.contains("  gap src/lib.rs:3 replace + with - in a (col 11) p=0.20\n"), "{t}");
    assert!(t.contains("  gap src/lib.rs:9 replace never -> u32 with 0 (col 5) p=0.00 uncovered\n"), "{t}");
    assert!(t.contains("! gaps: 3 mutants listed by `cargo mutants --list` (none executed), 0 outside the changed functions; 1 on lines no test runs (no model call); 2 scored by the model (3000 input tokens, $0.0003)"), "{t}");
    assert_eq!(stub.calls(), 1, "one sdd job for the whole review");
    assert_eq!(stub.request(1).matches("\"id\":").count(), 2, "one item per covered mutant, none for the uncovered one");
    let calls = fs::read_to_string(e.fx.join("calls.log")).unwrap();
    assert!(calls.lines().filter(|c| c.contains("mutants")).all(|c| c.contains("--list")), "{calls}");
    assert!(err(&o).contains("3000 input tokens, $0.0003"), "{}", err(&o));
    // the reaching test, found through the index, was sent
    assert!(stub.request(1).contains("fn t_a()"));
}

#[test]
fn pack_with_coverage_shows_the_failing_tests_note_and_keeps_the_coverage() {
    let e = Env::new(LIB2, &[]);
    fs::write(e.fx.join("stdout.txt"), libtest_failures(&["it::t_a"], "boom")).unwrap();
    let o = e.pack(&["--base", "HEAD~1", "--with", "coverage"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("  lines 4/4 covered, branches 1/2\n"), "{t}");
    assert!(t.contains("! coverage for package fx was observed, but 1 test failed: it::t_a"), "{t}");
    assert!(err(&o).contains("coverage fx: measured, but some tests failed"), "{}", err(&o));
}

#[test]
fn gaps_alone_lists_gaps_without_the_coverage_line() {
    let e = Env::new(LIB2, &[]);
    let stub = { let f = FakeSdd::new("ok"); f.p(0.9); f };
    let o = e.pack(&["--base", "HEAD~1", "--with", "gaps"], Some(&stub));
    let t = out(&o);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!t.contains(" covered") && !t.contains("uncovered changed") && t.contains("  gap src/lib.rs:9 replace never -> u32 with 0 (col 5) p=0.00 uncovered"), "{t}");
    assert!(!t.contains("p=0.90"), "p above the threshold is no gap");
}

#[test]
fn missing_sdd_leaves_a_note_and_the_uncovered_gaps_and_still_exits_0() {
    let e = Env::new(LIB2, &[]);
    let o = e.pack(&["--base", "HEAD~1", "--with", "gaps"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("! predicted gaps unavailable: sdd not found"), "{t}");
    assert!(t.contains("p=0.00 uncovered") && t.contains("M lib::a"), "{t}");
}

#[test]
fn test_DW_3_2_missing_bun_leaves_a_note_with_the_reason() {
    let e = Env::new(LIB2, &[]);
    let f = FakeSdd::new("ok");
    let mut c = Command::new(env!("CARGO_BIN_EXE_alsp"));
    c.arg("pack").arg("--repo").arg(&e.repo).arg("--cache-dir").arg(&e.cache).args(["--base", "HEAD~1", "--with", "gaps"]);
    c.env("ALSP_RUST_ANALYZER", &e.ra).env("ALSP_CARGO", &e.cargo).env("ALSP_FX", &e.fx).env("ALSP_TREE", &e.repo);
    c.env("ALSP_SDD_ROOT", &f.root).env("ALSP_BUN", e.fx.join("no-such-bun"));
    let o = c.output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains("! predicted gaps unavailable: bun not found at"), "{}", out(&o));
}

#[test]
fn test_DW_3_2_a_budget_refusal_shows_as_gaps_unavailable_budget_exceeded() {
    let e = Env::new(LIB2, &[]);
    let f = FakeSdd::new("budget");
    let o = e.pack(&["--base", "HEAD~1", "--with", "gaps"], Some(&f));
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(t.contains("! predicted gaps unavailable: decision budget exceeded: $5.10 in 24h of $5 daily;"), "{t}");
    assert!(!t.contains("(log:") && !t.contains("gemini-decide"), "{t}");
    assert!(t.contains("p=0.00 uncovered"), "uncovered gaps are still listed: {t}");
    assert_eq!(f.calls(), 1, "the refusal came from the one sdd job, never a direct call");
}

#[test]
fn a_budget_stop_on_a_clean_exit_adds_one_partial_note() {
    let e = Env::new(LIB2, &[]);
    let f = FakeSdd::new("stopped");
    let o = e.pack(&["--base", "HEAD~1", "--with", "coverage,gaps"], Some(&f));
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert_eq!(t.matches("! predicted gaps partial:").count(), 1, "{t}");
    assert!(t.contains("! predicted gaps partial: budget reached after 1 of 2 items"), "{t}");
}

#[test]
fn test_DW_3_5_failing_tests_make_coverage_unavailable_not_zero_and_the_pack_is_still_served() {
    let e = Env::new(LIB2, &[]);
    fs::write(e.fx.join("fail"), "").unwrap();
    let stub = { let f = FakeSdd::new("ok"); f.p(0.2); f };
    let o = e.pack(&["--base", "HEAD~1", "--with", "coverage,gaps"], Some(&stub));
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(!t.contains(" covered") && t.contains("! coverage unavailable for package fx") && t.contains("exited 101"), "{t}");
    assert!(t.contains("M lib::a") && t.contains("callers(1)"), "{t}");
    assert_eq!((stub.calls(), stub.request(1).matches("\"id\":").count()), (1, 3), "with no coverage every mutant goes to the model, in one job");
}

#[test]
fn json_carries_coverage_and_gaps() {
    let e = Env::new(LIB2, &[]);
    let stub = { let f = FakeSdd::new("ok"); f.p(0.2); f };
    let o = e.pack(&["--base", "HEAD~1", "--with", "coverage,gaps", "--format", "json"], Some(&stub));
    let j: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let a = j["symbols"].as_array().unwrap().iter().find(|s| s["name"] == "lib::a").unwrap();
    assert_eq!(a["coverage"]["lines_hit"], 4);
    assert_eq!(a["gaps"][0]["reason"], "PredictedSurvivor");
}

#[test]
fn bad_with_values_and_thresholds_are_rejected_before_anything_runs() {
    let e = Env::new(LIB2, &[]);
    let o = e.pack(&["--base", "HEAD~1", "--with", "mutate"], None);
    assert_eq!(o.status.code(), Some(2));
    let o = e.pack(&["--base", "HEAD~1", "--with", "gaps", "--gap-threshold", "1.5"], None);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("not between 0 and 1"), "{}", err(&o));
    assert!(!e.fx.join("calls.log").exists());
}

#[test]
fn a_path_with_a_newline_cannot_forge_a_line_in_the_text_pack() {
    let weird = "src/we\nird.rs";
    let e = Env::new(LIB2, &[(weird, "pub fn helper() {\n    1;\n}\n")]);
    let o = e.pack(&["--base", "HEAD~1"], None);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    let weird_lines: Vec<&str> = t.lines().filter(|l| l.starts_with("A ") || l.starts_with("M ")).filter(|l| l.contains("helper")).collect();
    assert_eq!(weird_lines.len(), 1, "exactly one line for the symbol: {t}");
    assert!(weird_lines[0].starts_with("A ") && weird_lines[0].contains("src/we\\nird.rs:1-3"), "{t}");
    assert!(!t.lines().any(|l| l.starts_with("ird")), "no forged continuation line: {t}");
    assert_eq!(t.lines().filter(|l| l.starts_with("A ") || l.starts_with("M ")).count(), 3, "three symbols, three lines: {t}");
    let j = out(&e.pack(&["--base", "HEAD~1", "--format", "json"], None));
    assert!(j.contains("src/we\\nird.rs"), "JSON keeps the true name, escaped by serde");
}
